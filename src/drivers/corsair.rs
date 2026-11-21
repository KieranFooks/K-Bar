//! Corsair Bragi (`0xFF42`) & Void/HS70 (`0xFFC5`) wireless battery driver (`VID 0x1B1C`).

use crate::drivers::io::{
    make_hid_id, recv_report, send_report_padded, strip_zero_report_id, vp_u32,
};
use crate::hid::{Battery, Kind, Source};
use crate::win32::{GetTickCount64, HANDLE};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CorsairProto {
    Bragi,
    VoidRich,
}

const VOID_PIDS: &[u16] = &[
    0x1B1C, 0x1B27, 0x0A14, 0x0A16, 0x0A17, 0x0A1D, 0x0A1A, 0x1B2A, 0x1B23, 0x1B29, 0x0A55, 0x0A51,
    0x0A52, 0x0A38, 0x0A4F, 0x0A2B, 0x0A75, 0x0A56,
];

/// Non-headset Corsair wireless PIDs (`Kind::Mouse` / `Kind::Keyboard`); `0x0Axx` defaults to `Headset`.
const CORSAIR_KINDS: &[(u16, Kind)] = &[
    // Mice
    (0x1B4B, Kind::Mouse), // Dark Core SE
    (0x1B51, Kind::Mouse), // Dark Core RGB
    (0x1B5E, Kind::Mouse), // Harpoon RGB Wireless
    (0x1B66, Kind::Mouse), // Ironclaw RGB Wireless
    (0x1B7E, Kind::Mouse), // Dark Core RGB Pro SE
    (0x1B80, Kind::Mouse), // Dark Core RGB Pro
    (0x1BA4, Kind::Mouse), // Katar Pro Wireless
    (0x1BC0, Kind::Mouse), // M65 RGB Ultra Wireless
    (0x1BCE, Kind::Mouse), // Nightsabre Wireless
    (0x1BDC, Kind::Mouse), // Scimitar Elite Wireless
    (0x1BE1, Kind::Mouse), // M75 Wireless
    // Keyboards
    (0x1B6E, Kind::Keyboard), // K57 RGB Wireless
    (0x1B8F, Kind::Keyboard), // K70 Pro Mini Wireless
    (0x1BAA, Kind::Keyboard), // K100 Air Wireless
    (0x1BDD, Kind::Keyboard), // K65 Plus Wireless
];

pub fn classify_pid(pid: u16) -> Kind {
    for &(p, k) in CORSAIR_KINDS {
        if p == pid {
            return k;
        }
    }
    if (pid & 0xFF00) == 0x0A00 || VOID_PIDS.contains(&pid) {
        Kind::Headset
    } else {
        Kind::Mouse
    }
}

pub fn match_proto(vid: u16, pid: u16, usage_page: u16, usage: u16) -> Option<CorsairProto> {
    if vid != 0x1B1C {
        return None;
    }
    if usage_page == 0xFF42 && usage == 0x0001 {
        return Some(CorsairProto::Bragi);
    }
    if usage_page == 0xFFC5 && usage == 0x0001 && VOID_PIDS.contains(&pid) {
        return Some(CorsairProto::VoidRich);
    }
    None
}

pub fn is_candidate(vid: u16, pid: u16, usage_page: u16, usage: u16) -> bool {
    match_proto(vid, pid, usage_page, usage).is_some()
}

pub fn parse_bragi_prop(r: &[u8]) -> Option<Result<u32, u8>> {
    if r.len() < 8 || r[0] != 0x01 {
        return None;
    }
    if r[3] != 0x00 {
        return Some(Err(r[3]));
    }
    Some(Ok(u32::from_le_bytes([r[4], r[5], r[6], r[7]])))
}

pub fn parse_void_battery(r: &[u8]) -> Option<(Option<u8>, bool)> {
    if r.len() < 5 || r[0] != 100 || r[1] != 0 || r[3] != 177 {
        return None;
    }
    if r[4] == 0 {
        return Some((None, false));
    }
    let charging = matches!(r[4], 4 | 5);
    let pct = (r[2] & 0x7F).min(100);
    Some((Some(pct), charging))
}

unsafe fn read_bragi_prop(h: HANDLE, ev: HANDLE, target: u8, prop: u8) -> Option<u32> {
    let numbered = [0x02, target, 0x02, prop];
    let unnumbered = [0x00, 0x02, target, 0x02, prop];
    if !send_report_padded(h, ev, &numbered, &[64], 200)
        && !send_report_padded(h, ev, &unnumbered, &[65], 200)
    {
        return None;
    }
    let mut pkt = [0u8; 128];
    let deadline = GetTickCount64().wrapping_add(220);
    loop {
        let now = GetTickCount64();
        if now >= deadline {
            return None;
        }
        let rem = (deadline - now).clamp(1, 220) as u32;
        let n = recv_report(h, ev, rem, &mut pkt)?;
        let d = strip_zero_report_id(&pkt[..n]);
        if d.first() == Some(&0x0E) {
            continue;
        }
        if let Some(res) = parse_bragi_prop(d) {
            return res.ok();
        }
    }
}

/// Poll a Corsair wireless peripheral for battery status.
///
/// # Safety
/// `h` must be an opened HID HANDLE and `ev` a valid manual-reset event HANDLE.
pub unsafe fn poll_handle(h: HANDLE, ev: HANDLE, pid: u16, proto: CorsairProto) -> Option<Battery> {
    let kind = classify_pid(pid);
    let id = make_hid_id(6, 0x1B1C, pid, 1);
    let vp = vp_u32(0x1B1C, pid);

    match proto {
        CorsairProto::Bragi => {
            for target in [0x09u8, 0x08u8] {
                if let Some(level) = read_bragi_prop(h, ev, target, 0x0F) {
                    if level <= 1000 {
                        let chg = read_bragi_prop(h, ev, target, 0x10).unwrap_or(0) == 1;
                        return Some(Battery::with_id(
                            id,
                            vp,
                            kind,
                            Some((level / 10) as u8),
                            chg,
                            true,
                            Source::NativeHid,
                        ));
                    }
                }
            }
            None
        }
        CorsairProto::VoidRich => {
            if !send_report_padded(h, ev, &[0xC9, 0x64], &[64, 32, 2], 250) {
                return None;
            }
            let mut pkt = [0u8; 128];
            let deadline = GetTickCount64().wrapping_add(250);
            loop {
                let now = GetTickCount64();
                if now >= deadline {
                    return None;
                }
                let rem = (deadline - now).clamp(1, 250) as u32;
                let n = recv_report(h, ev, rem, &mut pkt)?;
                if let Some((pct, chg)) = parse_void_battery(&pkt[..n]) {
                    return Some(Battery::with_id(
                        id,
                        vp,
                        kind,
                        pct,
                        chg,
                        pct.is_some(),
                        Source::NativeHid,
                    ));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corsair_bragi_and_void_parsers() {
        let lvl = 850u32.to_le_bytes();
        assert_eq!(
            parse_bragi_prop(&[0x01, 0x01, 0x00, 0x00, lvl[0], lvl[1], lvl[2], lvl[3]]),
            Some(Ok(850))
        );
        assert_eq!(
            parse_bragi_prop(strip_zero_report_id(&[
                0x00, 0x01, 0x01, 0x00, 0x00, lvl[0], lvl[1], lvl[2], lvl[3]
            ])),
            Some(Ok(850))
        );
        assert_eq!(parse_bragi_prop(&[0x01, 0x01, 0x00, 0x05, 0, 0, 0, 0]), Some(Err(5)));
        assert_eq!(parse_bragi_prop(&[0x0E, 0x00, 0, 0, 0, 0, 0, 0]), None);

        assert_eq!(parse_void_battery(&[100, 0, 0x80 | 75, 177, 4]), Some((Some(75), true)));
        assert_eq!(parse_void_battery(&[100, 0, 0, 177, 0]), Some((None, false)));
        assert_eq!(classify_pid(0x0A64), Kind::Headset);
        assert_eq!(classify_pid(0x1B80), Kind::Mouse);
        assert_eq!(classify_pid(0x1BDD), Kind::Keyboard);
    }
}
