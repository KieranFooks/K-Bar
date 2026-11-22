use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn find_windres() -> Option<PathBuf> {
    for candidate in ["windres", "x86_64-w64-mingw32-windres"] {
        if Command::new(candidate)
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
        {
            return Some(PathBuf::from(candidate));
        }
    }
    let msys_windres = Path::new(r"C:\msys64\mingw64\bin\windres.exe");
    if msys_windres.is_file() {
        return Some(msys_windres.to_path_buf());
    }
    None
}

fn parse_win_version(ver: &str) -> [u16; 4] {
    let mut parts = [0u16; 4];
    for (slot, raw) in parts.iter_mut().zip(ver.trim().trim_start_matches('v').split('.')) {
        let digits: String = raw.chars().take_while(|c| c.is_ascii_digit()).collect();
        if let Ok(n) = digits.parse::<u32>() {
            *slot = n.min(u16::MAX as u32) as u16;
        }
    }
    parts
}

fn sanitize_rc_string(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii() && !matches!(c, '"' | '\\' | '\r' | '\n'))
        .collect()
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=KATT_VERSION");
    println!("cargo:rustc-link-arg-bin=katt=-nostdlib");
    println!(
        "cargo:rustc-link-arg-bin=katt=-Wl,--subsystem,windows,-e,mainCRTStartup,--gc-sections"
    );

    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR missing"));
    let version_str = env::var("KATT_VERSION")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string());
    let [v0, v1, v2, v3] = parse_win_version(&version_str);
    let safe_ver = sanitize_rc_string(version_str.trim().trim_start_matches('v'));

    if let Some(windres) = find_windres() {
        let rc_path = out_dir.join("katt.rc");
        let res_obj_path = out_dir.join("katt_res.o");
        let rc_content = format!(
            r#"1 VERSIONINFO
FILEVERSION {v0},{v1},{v2},{v3}
PRODUCTVERSION {v0},{v1},{v2},{v3}
FILEFLAGSMASK 0x3fL
FILEFLAGS 0x0L
FILEOS 0x40004L
FILETYPE 0x1L
FILESUBTYPE 0x0L
BEGIN
    BLOCK "StringFileInfo"
    BEGIN
        BLOCK "040904B0"
        BEGIN
            VALUE "CompanyName", "KieranFooks"
            VALUE "FileDescription", "katt - Wireless peripheral battery indicator"
            VALUE "FileVersion", "{safe_ver}"
            VALUE "InternalName", "katt"
            VALUE "OriginalFilename", "katt.exe"
            VALUE "ProductName", "katt"
            VALUE "ProductVersion", "{safe_ver}"
        END
    END
    BLOCK "VarFileInfo"
    BEGIN
        VALUE "Translation", 0x0409, 1200
    END
END
"#
        );

        fs::write(&rc_path, rc_content).expect("failed to write .rc file");
        let status = Command::new(&windres)
            .args(["--target=pe-x86-64", "-O", "coff", "-i"])
            .arg(&rc_path)
            .arg("-o")
            .arg(&res_obj_path)
            .status()
            .expect("failed to invoke windres");

        if status.success() {
            println!("cargo:rustc-link-arg-bin=katt={}", res_obj_path.display());
        } else {
            println!("cargo:warning=windres failed with status {status}; skipping VERSIONINFO");
        }
    } else {
        println!("cargo:warning=windres not found; building without VERSIONINFO resource");
    }
}
