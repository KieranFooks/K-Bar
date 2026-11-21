//! SteelSeries Arctis & Nova wireless headsets battery driver (`VID 0x1038`).

use crate::drivers::io::{
    make_hid_id, recv_report, send_report_padded, strip_zero_report_id, vp_u32,
};
use crate::hid::{Battery, Kind, Source};
use crate::win32::{GetTickCount64, HANDLE};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SteelProto {
    Arctis7Plus,
    Nova7Discrete,
    Nova7Pct,
    Nova5,
    NovaPro,
    Arctis7,
    Arctis9,
}

const STEEL_PIDS: &[(u16, SteelProto)] = &[
    // Arctis 7+
    (0x220E, SteelProto::Arctis7Plus),
    (0x2212, SteelProto::Arctis7Plus),
    (0x2216, SteelProto::Arctis7Plus),
    (0x2236, SteelProto::Arctis7Plus),
    // Arctis Nova 7 discrete (0..=4)
    (0x2202, SteelProto::Nova7Discrete),
    (0x2206, SteelProto::Nova7Discrete),
    (0x220A, SteelProto::Nova7Discrete),
    (0x223A, SteelProto::Nova7Discrete),
    (0x227A, SteelProto::Nova7Discrete),
    (0x22A4, SteelProto::Nova7Discrete),
    // Arctis Nova 7 percentage (0..=100)
    (0x22A1, SteelProto::Nova7Pct),
    (0x227E, SteelProto::Nova7Pct),
    (0x2258, SteelProto::Nova7Pct),
    (0x229E, SteelProto::Nova7Pct),
    (0x22AD, SteelProto::Nova7Pct),
    (0x22A9, SteelProto::Nova7Pct),
    (0x22A5, SteelProto::Nova7Pct),
    // Arctis Nova 5 / 5X
    (0x2232, SteelProto::Nova5),
    (0x2253, SteelProto::Nova5),
    // Arctis Nova Pro Wireless / X
    (0x12E0, SteelProto::NovaPro),
    (0x12E5, SteelProto::NovaPro),
    // Arctis 7 / 2019 / Pro
    (0x1260, SteelProto::Arctis7),
    (0x12AD, SteelProto::Arctis7),
    (0x1252, SteelProto::Arctis7),
    (0x1280, SteelProto::Arctis7),
    // Arctis 9
    (0x12C2, SteelProto::Arctis9),
    (0x12C4, SteelProto::Arctis9),
];

pub fn lookup(pid: u16) -> Option<SteelProto> {
    STEEL_PIDS.iter().find(|&&(p, _)| p == pid).map(|&(_, proto)| proto)
}

pub fn is_candidate(vid: u16, pid: u16, usage_page: u16, usage: u16) -> bool {
    if vid != 0x1038 {
        return false;
    }
    let Some(proto) = lookup(pid) else {
        return false;
    };
    match proto {
        SteelProto::Arctis7 => usage_page == 0xFF43 && usage == 0x0202,
        SteelProto::NovaPro => usage_page >= 0xFF00,
        _ => usage_page == 0xFFC0 && usage == 0x0001,
    }
}

/// Parse a raw HID reply for SteelSeries protocols (`(Option<pct>, charging)`).
pub fn parse_reply(raw: &[u8], proto: SteelProto) -> Option<(Option<u8>, bool)> {
    match proto {
        SteelProto::Arctis7Plus => {
            let d = strip_zero_report_id(raw);
            if d.len() < 4 || d[0] != 0xB0 {
                return None;
            }
            if d[1] == 0x01 {
                return Some((None, false));
            }
            let pct = ((d[2] as u16) * 100 / 4).min(100) as u8;
            Some((Some(pct), d[3] == 0x01))
        }
        SteelProto::Nova7Discrete | SteelProto::Nova7Pct => {
            let d = strip_zero_report_id(raw);
            if d.len() < 4 || d[0] != 0xB0 {
                return None;
            }
            if d[3] == 0x00 {
                return Some((None, false));
            }
            let charging = matches!(d[3], 1 | 2);
            let pct = if matches!(proto, SteelProto::Nova7Discrete) {
                ((d[2] as u16) * 100 / 4).min(100) as u8
            } else {
                d[2].min(100)
            };
            Some((Some(pct), charging))
        }
        SteelProto::Nova5 => {
            let d = strip_zero_report_id(raw);
            if d.len() < 5 || d[0] != 0xB0 {
                return None;
            }
            if d[1] == 0x02 {
                return Some((None, false));
            }
            Some((Some(d[3].min(100)), d[4] == 0x01))
        }
        SteelProto::NovaPro => {
            if raw.len() < 16 || raw[0] != 0x06 || raw[1] != 0xB0 {
                return None;
            }
            if raw[15] == 0x01 {
                return Some((None, false));
            }
            let pct = ((raw[6] as u16) * 100 / 8).min(100) as u8;
            Some((Some(pct), raw[15] == 0x02))
        }
        SteelProto::Arctis7 => {
            if raw.len() < 3 || raw[0] != 0x06 || raw[1] != 0x18 {
                return None;
            }
            if raw[2] == 0 {
                return Some((None, false));
            }
            Some((Some(raw[2].min(100)), false))
        }
        SteelProto::Arctis9 => {
            let d = strip_zero_report_id(raw);
            if d.len() < 5 || d[0] != 0x20 {
                return None;
            }
            let raw_lvl = d[3];
            if raw_lvl == 0 {
                return Some((None, false));
            }
            let clamped = raw_lvl.clamp(0x64, 0x9A);
            let pct = (((clamped - 0x64) as u16) * 100 / (0x9A - 0x64)) as u8;
            Some((Some(pct.min(100)), d[4] == 0x01))
        }
    }
}

/// Poll a SteelSeries Arctis or Nova wireless headset.
///
/// # Safety
/// `h` must be an opened HID HANDLE and `ev` a valid manual-reset event HANDLE.
pub unsafe fn poll_handle(h: HANDLE, ev: HANDLE, pid: u16, proto: SteelProto) -> Option<Battery> {
    let (prefix, sizes): (&[u8], &[usize]) = match proto {
        SteelProto::NovaPro => (&[0x06, 0xB0], &[31, 64, 32]),
        SteelProto::Arctis7 => (&[0x06, 0x18], &[31, 32, 64]),
        SteelProto::Arctis9 => (&[0x00, 0x20], &[65, 64, 32, 31]),
        _ => (&[0x00, 0xB0], &[65, 64, 32, 31]),
    };

    if !send_report_padded(h, ev, prefix, sizes, 250) {
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
        if let Some((pct, chg)) = parse_reply(&pkt[..n], proto) {
            return Some(Battery::with_id(
                make_hid_id(5, 0x1038, pid, 1),
                vp_u32(0x1038, pid),
                Kind::Headset,
                pct,
                chg,
                pct.is_some(),
                Source::NativeHid,
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steelseries_parsers_cover_all_variants() {
        assert_eq!(parse_reply(&[0x00, 0xB0, 0x03, 0x03, 0x00], SteelProto::Arctis7Plus), Some((Some(75), false)));
        assert_eq!(parse_reply(&[0x00, 0xB0, 0x01, 0x00, 0x00], SteelProto::Arctis7Plus), Some((None, false)));
        assert_eq!(parse_reply(&[0x00, 0xB0, 0x00, 0x02, 0x01], SteelProto::Nova7Discrete), Some((Some(50), true)));
        assert_eq!(parse_reply(&[0x00, 0xB0, 0x00, 78, 0x03], SteelProto::Nova7Pct), Some((Some(78), false)));
        assert_eq!(parse_reply(&[0x00, 0xB0, 0x00, 0x00, 0x00], SteelProto::Nova7Pct), Some((None, false)));
        assert_eq!(parse_reply(&[0x00, 0xB0, 0x00, 0x00, 64, 0x01], SteelProto::Nova5), Some((Some(64), true)));

        let mut np = [0u8; 16];
        np[0] = 0x06;
        np[1] = 0xB0;
        np[6] = 6;
        np[15] = 0x02;
        assert_eq!(parse_reply(&np, SteelProto::NovaPro), Some((Some(75), true)));

        assert_eq!(parse_reply(&[0x00, 0x20, 0, 0, 0x7F, 0x01], SteelProto::Arctis9), Some((Some(50), true)));
    }
}
