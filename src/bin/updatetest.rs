//! Mocked auto-update test workspace (`updatetest.exe`).
//!
//! Creates a sandbox directory (`target/update-workspace` by default), launches an
//! ephemeral local HTTP mock server on `127.0.0.1`, runs `katt::updater` against mocked
//! CalVer release data, shows the English Windows 11 TaskDialog with a live release link,
//! verifies SHA-256 via `bcrypt.dll`, performs the atomic `.old`/`.new` PE swap, and
//! relaunches the sandboxed `katt.exe` to verify cleanup.

use std::env;
use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use katt::updater::{
    apply_update_and_spawn, check_latest_tag, cleanup_old_binary, download_and_verify_pe,
    fetch_sha256, is_newer, parse_calver, show_update_dialog, PromptAction,
};
use katt::win32::{CloseHandle, WaitForSingleObject, INFINITE};

const MOCK_MARKER_PREFIX: &[u8] = b"\nKATT_MOCK_VERSION=";

struct MockConfig {
    workspace: PathBuf,
    local_tag: String,
    remote_tag: String,
    bad_hash: bool,
    auto_accept: bool,
    no_ui: bool,
}

fn to_utf16z(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn sha256_hex_via_windows_mock(bytes: &[u8]) -> String {
    // Compute SHA-256 via `bcrypt.dll` in a temp path or pure Rust SHA-256?
    // Writing a 60-line pure-Rust SHA-256 here avoids depending on any crate while
    // keeping the mock server independent of `katt::updater`'s verifier!
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
        0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
        0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
        0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
        0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
        0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
        0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a,
        0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
    ];
    let bit_len = (bytes.len() as u64) * 8;
    let mut padded = bytes.to_vec();
    padded.push(0x80);
    while (padded.len() % 64) != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in padded.as_chunks::<64>().0 {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                chunk[i * 4],
                chunk[i * 4 + 1],
                chunk[i * 4 + 2],
                chunk[i * 4 + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = hh.wrapping_add(s1).wrapping_add(ch).wrapping_add(K[i]).wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        let deltas = [a, b, c, d, e, f, g, hh];
        for (slot, delta) in h.iter_mut().zip(deltas) {
            *slot = slot.wrapping_add(delta);
        }
    }
    let mut out = String::with_capacity(64);
    for word in h {
        out.push_str(&format!("{word:08x}"));
    }
    out
}

fn handle_mock_client(
    mut stream: TcpStream,
    addr: SocketAddr,
    remote_tag: &str,
    payload: &[u8],
    sha_line: &str,
) {
    let mut req_buf = [0u8; 2048];
    let n = match stream.read(&mut req_buf) {
        Ok(n) if n > 0 => n,
        _ => return,
    };
    let req = String::from_utf8_lossy(&req_buf[..n]);
    let first_line = req.lines().next().unwrap_or("");
    let mut parts = first_line.split_whitespace();
    let method = parts.next().unwrap_or("GET");
    let path = parts.next().unwrap_or("/");

    let base = format!("http://127.0.0.1:{}", addr.port());
    let tag_path = format!("/releases/tag/{remote_tag}");
    let exe_path = format!("/releases/download/{remote_tag}/katt.exe");
    let sha_path = format!("/releases/download/{remote_tag}/katt.exe.sha256");

    if path == "/releases/latest" {
        let resp = format!(
            "HTTP/1.1 302 Found\r\nLocation: {base}{tag_path}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
        let _ = stream.write_all(resp.as_bytes());
    } else if path == tag_path {
        let body = format!(
            "<!doctype html><html><head><meta charset=\"utf-8\"><title>katt {remote_tag}</title>\
             <style>body{{font-family:'Segoe UI',sans-serif;background:#111318;color:#e6edf3;max-width:720px;margin:48px auto;padding:0 24px;line-height:1.6}}\
             .card{{background:#1b1f27;border:1px solid #30363d;border-radius:10px;padding:20px;margin-top:16px}}\
             code{{background:#0d1117;padding:2px 6px;border-radius:4px;font-size:0.9em}}\
             .badge{{display:inline-block;background:#238636;color:#fff;padding:2px 10px;border-radius:999px;font-size:12px}}</style></head>\
             <body><span class=\"badge\">Mock Release</span><h1>katt {remote_tag}</h1>\
             <p>Local mock GitHub Release served by <code>updatetest</code>.</p>\
             <div class=\"card\"><h3>Changelog</h3><ul>\
             <li>feat(updater): lightweight WinHTTP CalVer check + TaskDialog notification</li>\
             <li>fix(hid): faster wireless peripheral wake handling</li></ul>\
             <h3>Assets</h3><ul><li><code>katt.exe</code> ({} bytes)</li>\
             <li>SHA-256: <code>{}</code></li></ul></div></body></html>",
            payload.len(),
            sha_line.split_whitespace().next().unwrap_or("")
        );
        let header = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let _ = stream.write_all(header.as_bytes());
        if method != "HEAD" {
            let _ = stream.write_all(body.as_bytes());
        }
    } else if path == exe_path {
        let header = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            payload.len()
        );
        let _ = stream.write_all(header.as_bytes());
        if method != "HEAD" {
            let _ = stream.write_all(payload);
        }
    } else if path == sha_path {
        let header = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            sha_line.len()
        );
        let _ = stream.write_all(header.as_bytes());
        if method != "HEAD" {
            let _ = stream.write_all(sha_line.as_bytes());
        }
    } else {
        let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
    }
}

fn run_sandbox_child() {
    let exe = env::current_exe().expect("current_exe");
    let exe_w = to_utf16z(&exe.to_string_lossy());
    let cleaned = cleanup_old_binary(&exe_w);

    let bytes = fs::read(&exe).unwrap_or_default();
    let version = bytes
        .windows(MOCK_MARKER_PREFIX.len())
        .rposition(|w| w == MOCK_MARKER_PREFIX)
        .map(|pos| {
            let tail = &bytes[pos + MOCK_MARKER_PREFIX.len()..];
            String::from_utf8_lossy(tail).trim().to_string()
        })
        .unwrap_or_else(|| "unknown".to_string());

    if let Some(dir) = exe.parent() {
        let report = format!(
            "status=updated\nactive_exe={}\nversion={}\ncleaned_old={}\n",
            exe.display(),
            version,
            cleaned
        );
        let _ = fs::write(dir.join("update_result.txt"), report);
    }

    println!("[sandbox katt.exe] Relaunched as {version} (cleaned katt.exe.old: {cleaned})");
}

fn parse_args() -> MockConfig {
    let mut cfg = MockConfig {
        workspace: PathBuf::from("target").join("update-workspace"),
        local_tag: "v2026.11.22".to_string(),
        remote_tag: "v2026.11.23".to_string(),
        bad_hash: false,
        auto_accept: false,
        no_ui: false,
    };
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--workspace" => {
                if let Some(v) = args.next() {
                    cfg.workspace = PathBuf::from(v);
                }
            }
            "--local" => {
                if let Some(v) = args.next() {
                    cfg.local_tag = v;
                }
            }
            "--remote" => {
                if let Some(v) = args.next() {
                    cfg.remote_tag = v;
                }
            }
            "--bad-hash" => cfg.bad_hash = true,
            "--auto-accept" => cfg.auto_accept = true,
            "--no-ui" => {
                cfg.no_ui = true;
                cfg.auto_accept = true;
            }
            "--mock" => {}
            other => {
                eprintln!("Unknown option: {other}");
            }
        }
    }
    cfg
}

fn prepare_sandbox(workspace: &Path) -> (PathBuf, Vec<u8>) {
    fs::create_dir_all(workspace).expect("create update workspace");
    for leftover in ["katt.exe.new", "katt.exe.old", "update_result.txt", "skipped_version.txt"] {
        let _ = fs::remove_file(workspace.join(leftover));
    }
    let self_exe = env::current_exe().expect("current_exe");
    let self_bytes = fs::read(&self_exe).expect("read updatetest.exe");
    let sandbox_exe = workspace.join("katt.exe");
    fs::write(&sandbox_exe, &self_bytes).expect("seed sandbox katt.exe");
    (sandbox_exe, self_bytes)
}

fn main() {
    if env::args().any(|a| a == "--sandbox-child") {
        run_sandbox_child();
        return;
    }

    let cfg = parse_args();
    let local_ver = parse_calver(&cfg.local_tag).expect("invalid --local CalVer tag");
    let (sandbox_exe, base_pe) = prepare_sandbox(&cfg.workspace);

    // Build mocked updated PE binary by appending a harmless overlay marker so its SHA-256 differs.
    let mut remote_pe = base_pe;
    remote_pe.extend_from_slice(MOCK_MARKER_PREFIX);
    remote_pe.extend_from_slice(cfg.remote_tag.as_bytes());
    let real_sha = sha256_hex_via_windows_mock(&remote_pe);
    let served_sha = if cfg.bad_hash {
        "0000000000000000000000000000000000000000000000000000000000000000".to_string()
    } else {
        real_sha.clone()
    };
    let sha_line = format!("{served_sha}  katt.exe\n");

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock HTTP server");
    let addr = listener.local_addr().expect("local_addr");
    listener.set_nonblocking(true).expect("set_nonblocking");

    let stop = Arc::new(AtomicBool::new(false));
    let stop_srv = Arc::clone(&stop);
    let remote_tag_srv = cfg.remote_tag.clone();
    let remote_pe_arc = Arc::new(remote_pe);
    let sha_line_arc = Arc::new(sha_line);

    let srv_thread = thread::spawn(move || {
        while !stop_srv.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((stream, _)) => {
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                    handle_mock_client(
                        stream,
                        addr,
                        &remote_tag_srv,
                        &remote_pe_arc,
                        &sha_line_arc,
                    );
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(15));
                }
                Err(_) => break,
            }
        }
    });

    let base_url = format!("http://127.0.0.1:{}", addr.port());
    let latest_url = format!("{base_url}/releases/latest");
    println!("=== katt Auto-Update Mock Workspace ===");
    println!("Workspace  : {}", cfg.workspace.display());
    println!("Mock server: {base_url}");
    println!("Local ver  : {}", cfg.local_tag);
    println!("Remote ver : {}", cfg.remote_tag);
    println!("Real SHA256: {real_sha}");
    if cfg.bad_hash {
        println!("Bad hash   : ENABLED (serving all-zero SHA-256 to test rejection)");
    }
    println!();

    println!("[1/5] Checking {latest_url} via WinHTTP HEAD (no redirects)...");
    let check = match check_latest_tag(&latest_url) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Check failed: {e:?}");
            stop.store(true, Ordering::Relaxed);
            let _ = srv_thread.join();
            std::process::exit(1);
        }
    };
    println!(
        "      -> Found tag '{}' (release URL: {})",
        check.tag_str(),
        check.release_url_str()
    );

    if !is_newer(check.version, local_ver) {
        println!("[done] Already up to date ({} >= {}).", cfg.local_tag, check.tag_str());
        stop.store(true, Ordering::Relaxed);
        let _ = srv_thread.join();
        return;
    }

    let action = if cfg.no_ui {
        println!("[2/5] --no-ui enabled: auto-selecting 'Update now'.");
        PromptAction::UpdateNow
    } else {
        println!("[2/5] Opening English TaskDialog notification (click 'View release notes' to test browser link)...");
        let outcome = show_update_dialog(
            0,
            &cfg.local_tag,
            check.tag_str(),
            check.release_url_str(),
        );
        println!(
            "      -> Dialog result: {:?}, skip_version={}",
            outcome.action, outcome.skip_version
        );
        if outcome.skip_version {
            let _ = fs::write(cfg.workspace.join("skipped_version.txt"), check.tag_str());
        }
        if cfg.auto_accept {
            PromptAction::UpdateNow
        } else {
            outcome.action
        }
    };

    if action != PromptAction::UpdateNow {
        println!("[done] User deferred update.");
        stop.store(true, Ordering::Relaxed);
        let _ = srv_thread.join();
        return;
    }

    let sha_url = format!("{base_url}/releases/download/{}/katt.exe.sha256", check.tag_str());
    let dl_url = format!("{base_url}/releases/download/{}/katt.exe", check.tag_str());
    let new_exe = cfg.workspace.join("katt.exe.new");
    let new_exe_w = to_utf16z(&new_exe.to_string_lossy());
    let sandbox_exe_w = to_utf16z(&sandbox_exe.to_string_lossy());

    println!("[3/5] Fetching SHA-256 from {sha_url}...");
    let expected_sha = fetch_sha256(&sha_url).expect("fetch_sha256");

    println!("[4/5] Streaming {dl_url} to {} & verifying BCrypt SHA-256 + PE header...", new_exe.display());
    match download_and_verify_pe(&dl_url, &new_exe_w, &expected_sha) {
        Ok(bytes) => println!("      -> Verified {bytes} bytes (SHA-256 + PE OK)."),
        Err(e) => {
            println!("      -> Rejected download as expected/errored: {e:?}");
            println!("      -> katt.exe.new cleaned up: {}", !new_exe.exists());
            stop.store(true, Ordering::Relaxed);
            let _ = srv_thread.join();
            if cfg.bad_hash {
                return;
            }
            std::process::exit(1);
        }
    }

    println!("[5/5] Atomic MoveFileExW swap (.old / .new) and launching updated sandbox katt.exe...");
    let child_proc = apply_update_and_spawn(&sandbox_exe_w, &new_exe_w, "--sandbox-child")
        .expect("apply_update_and_spawn");
    unsafe {
        let _ = WaitForSingleObject(child_proc, INFINITE);
        let _ = CloseHandle(child_proc);
    }

    let result_txt = fs::read_to_string(cfg.workspace.join("update_result.txt")).unwrap_or_default();
    println!();
    println!("=== Sandbox Result ===");
    println!("{result_txt}");
    println!("katt.exe.old present? {}", cfg.workspace.join("katt.exe.old").exists());
    println!("katt.exe.new present? {}", new_exe.exists());

    stop.store(true, Ordering::Relaxed);
    let _ = srv_thread.join();
}
