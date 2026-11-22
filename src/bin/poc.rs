//! Live hardware diagnostic probe for katt (no `setupapi.dll`, no `hid.dll`, no `windows` crate).

use core::ptr::{null, null_mut};

use katt::drivers::{corsair, hyperx, logitech, pulsar, razer, steelseries};
use katt::hid::{
    self, hidpp_request, parse_1000_status, parse_1001_voltage, parse_1004_status,
};
use katt::win32::{
    CloseHandle, CreateEventW, CreateFileW, FILE_FLAG_OVERLAPPED, FILE_SHARE_READ,
    FILE_SHARE_WRITE, GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE, OPEN_EXISTING,
};

fn hex(buf: &[u8]) -> String {
    buf.iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn driver_family(vid: u16, pid: u16, up: u16, u: u16) -> &'static str {
    if logitech::is_candidate(vid, pid, up, u) {
        "Logitech"
    } else if hyperx::is_candidate(vid, pid, up) {
        "HyperX"
    } else if razer::is_candidate(vid, pid, up, u) {
        "Razer"
    } else if steelseries::is_candidate(vid, pid, up, u) {
        "SteelSeries"
    } else if corsair::is_candidate(vid, pid, up, u) {
        "Corsair"
    } else if pulsar::is_candidate(vid, pid, up, u) {
        "Pulsar"
    } else {
        "Unknown"
    }
}

fn main() {
    println!("=== Raw Input HID targets (zero setupapi.dll / zero hid.dll) ===");
    let (cands, count) = hid::probe_raw_candidates();
    println!("Matched target HID interfaces: {count}");
    unsafe {
        let ev = CreateEventW(null(), 1, 0, null());
        for cand in &cands[..count] {
            let pstr = String::from_utf16_lossy(cand.path.as_slice());
            let fam = driver_family(cand.vid, cand.pid, cand.usage_page, cand.usage);
            let h = CreateFileW(
                cand.path.buf.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                null(),
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED,
                null_mut(),
            );
            let mut prod = String::new();
            if h != INVALID_HANDLE_VALUE && !ev.is_null() {
                let mut wbuf = [0u16; 128];
                let plen = hid::probe_product_string_utf16(h, ev, &mut wbuf);
                prod = String::from_utf16_lossy(&wbuf[..plen]);
            }
            println!(
                "  [{fam}] VID={:04X} PID={:04X} UP={:04X} U={:04X} product='{}' path={pstr}",
                cand.vid, cand.pid, cand.usage_page, cand.usage, prod
            );

            if h != INVALID_HANDLE_VALUE {
                if cand.vid == 0x046D && cand.usage_page == 0xFF00 && cand.usage == 0x0002 && !ev.is_null() {
                    let mut r = [0u8; 128];
                    let slots: &[u8] = if (0xC500..=0xC5FF).contains(&cand.pid)
                        && !logitech::is_single_receiver_pid(cand.pid)
                    {
                        &[1, 2, 3, 4, 5, 6]
                    } else {
                        &[1]
                    };
                    for &slot in slots {
                        if let Some(pn) =
                            hidpp_request(h, ev, (slot, 0x00, 0x01), &[0, 0, 0x5A], 180, &mut r)
                        {
                            if pn >= 7 && r[6] == 0x5A {
                                println!(
                                    "    slot={slot} HID++ {}.{} ping={}",
                                    r[4],
                                    r[5],
                                    hex(&r[..pn.min(8)])
                                );
                                for (fid, label) in [
                                    (0x1004u16, "UnifiedBattery"),
                                    (0x1000u16, "BatteryStatus"),
                                    (0x1001u16, "BatteryVoltage"),
                                    (0x1F20u16, "ADCMeasurement"),
                                ] {
                                    if let Some(fn_) = hidpp_request(
                                        h,
                                        ev,
                                        (slot, 0x00, 0x00),
                                        &fid.to_be_bytes(),
                                        180,
                                        &mut r,
                                    ) {
                                        if fn_ >= 5 && r[4] != 0 {
                                            let idx = r[4];
                                            let func = if fid == 0x1004 { 1 } else { 0 };
                                            if let Some(bn) = hidpp_request(
                                                h,
                                                ev,
                                                (slot, idx, func),
                                                &[],
                                                180,
                                                &mut r,
                                            ) {
                                                let parsed = match fid {
                                                    0x1004 => format!("{:?}", parse_1004_status(&r[..bn])),
                                                    0x1000 => format!("{:?}", parse_1000_status(&r[..bn])),
                                                    _ => format!("{:?}", parse_1001_voltage(&r[..bn])),
                                                };
                                                println!(
                                                    "    feature {fid:04X} ({label}) idx={idx} raw={} -> {parsed}",
                                                    hex(&r[..bn.min(10)])
                                                );
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                let _ = CloseHandle(h);
            }
        }
        if !ev.is_null() {
            let _ = CloseHandle(ev);
        }
    }

    println!("\n=== Bluetooth PnP Battery Probe (cfgmgr32.dll) ===");
    hid::probe_bluetooth_diagnostics(&mut |id, batt, conn, kind, name| {
        if batt.is_some() || conn == Some(true) || kind.is_some() {
            println!(
                "  name='{}' conn={:?} batt={:?} kind={:?} id={}",
                String::from_utf16_lossy(name),
                conn,
                batt,
                kind,
                String::from_utf16_lossy(id)
            );
        }
    });

    println!("\n=== HidScanner::read_all() ===");
    let list = hid::read_all();
    for b in list.iter() {
        println!(
            "  {:?} [{:?}]: pct={:?} | charging={} | online={} | id={:#018X} | vid_pid={:08X}",
            b.kind, b.source, b.pct, b.charging, b.online, b.id, b.vid_pid
        );
    }
}
