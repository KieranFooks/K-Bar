//! Razer wireless mice and keyboards battery driver (`VID 0x1532`, 90-byte feature report).
//!
//! Covers Viper, DeathAdder, Basilisk, Cobra, Naga, Orochi, Pro Click, Lancehead,
//! Mamba, BlackWidow, DeathStalker, and Joro families.

use crate::drivers::io::{get_feature, make_hid_id, set_feature, vp_u32};
use crate::hid::{Battery, Kind, Source};
use crate::win32::{Sleep, HANDLE};

/// `(pid, kind, transaction_id)` table from OpenRazer.
const RAZER_DEVICES: &[(u16, Kind, u8)] = &[
    // Wireless mice (tx_id = 0x1F)
    (0x0062, Kind::Mouse, 0x1F), // Atheris
    (0x0077, Kind::Mouse, 0x1F), // Pro Click Receiver
    (0x007A, Kind::Mouse, 0x1F), // Viper Ultimate Wired
    (0x007B, Kind::Mouse, 0x1F), // Viper Ultimate Wireless
    (0x0080, Kind::Mouse, 0x1F), // Pro Click Wired
    (0x0083, Kind::Mouse, 0x1F), // Basilisk X HyperSpeed
    (0x0086, Kind::Mouse, 0x1F), // Basilisk Ultimate Wired
    (0x0088, Kind::Mouse, 0x1F), // Basilisk Ultimate Receiver
    (0x008F, Kind::Mouse, 0x1F), // Naga Pro Wired
    (0x0090, Kind::Mouse, 0x1F), // Naga Pro Wireless
    (0x0094, Kind::Mouse, 0x1F), // Orochi V2 Receiver
    (0x0095, Kind::Mouse, 0x1F), // Orochi V2 Bluetooth
    (0x009A, Kind::Mouse, 0x1F), // Pro Click Mini Receiver
    (0x009C, Kind::Mouse, 0x1F), // DeathAdder V2 X HyperSpeed
    (0x009E, Kind::Mouse, 0x1F), // Viper Mini SE Wired
    (0x009F, Kind::Mouse, 0x1F), // Viper Mini SE Wireless
    (0x00A5, Kind::Mouse, 0x1F), // Viper V2 Pro Wired
    (0x00A6, Kind::Mouse, 0x1F), // Viper V2 Pro Wireless
    (0x00A7, Kind::Mouse, 0x1F), // Naga V2 Pro Wired
    (0x00A8, Kind::Mouse, 0x1F), // Naga V2 Pro Wireless
    (0x00AA, Kind::Mouse, 0x1F), // Basilisk V3 Pro Wired
    (0x00AB, Kind::Mouse, 0x1F), // Basilisk V3 Pro Wireless
    (0x00AF, Kind::Mouse, 0x1F), // Cobra Pro Wired
    (0x00B0, Kind::Mouse, 0x1F), // Cobra Pro Wireless
    (0x00B3, Kind::Mouse, 0x1F), // HyperPolling Wireless Dongle
    (0x00B4, Kind::Mouse, 0x1F), // Naga V2 HyperSpeed Receiver
    (0x00B6, Kind::Mouse, 0x1F), // DeathAdder V3 Pro Wired
    (0x00B7, Kind::Mouse, 0x1F), // DeathAdder V3 Pro Wireless
    (0x00B8, Kind::Mouse, 0x1F), // Viper V3 HyperSpeed
    (0x00B9, Kind::Mouse, 0x1F), // Basilisk V3 X HyperSpeed
    (0x00BE, Kind::Mouse, 0x1F), // DeathAdder V4 Pro Wired
    (0x00BF, Kind::Mouse, 0x1F), // DeathAdder V4 Pro Wireless
    (0x00C0, Kind::Mouse, 0x1F), // Viper V3 Pro Wired
    (0x00C1, Kind::Mouse, 0x1F), // Viper V3 Pro Wireless
    (0x00C2, Kind::Mouse, 0x1F), // DeathAdder V3 Pro Wired Alt
    (0x00C3, Kind::Mouse, 0x1F), // DeathAdder V3 Pro Wireless Alt
    (0x00C4, Kind::Mouse, 0x1F), // DeathAdder V3 HyperSpeed Wired
    (0x00C5, Kind::Mouse, 0x1F), // DeathAdder V3 HyperSpeed Wireless
    (0x00C7, Kind::Mouse, 0x1F), // Pro Click V2 Vertical Wired
    (0x00C8, Kind::Mouse, 0x1F), // Pro Click V2 Vertical Wireless
    (0x00CB, Kind::Mouse, 0x1F), // Basilisk V3 35K
    (0x00CC, Kind::Mouse, 0x1F), // Basilisk V3 Pro 35K Wired
    (0x00CD, Kind::Mouse, 0x1F), // Basilisk V3 Pro 35K Wireless
    (0x00D0, Kind::Mouse, 0x1F), // Pro Click V2 Wired
    (0x00D1, Kind::Mouse, 0x1F), // Pro Click V2 Wireless
    (0x00D3, Kind::Mouse, 0x1F), // Basilisk Mobile Wired
    (0x00D4, Kind::Mouse, 0x1F), // Basilisk Mobile Receiver
    (0x00D6, Kind::Mouse, 0x1F), // Basilisk V3 Pro 35K Phantom Wired
    (0x00D7, Kind::Mouse, 0x1F), // Basilisk V3 Pro 35K Phantom Wireless
    (0x00DA, Kind::Mouse, 0x1F), // Cobra HyperSpeed Wired
    (0x00DB, Kind::Mouse, 0x1F), // Cobra HyperSpeed Wireless
    (0x00DE, Kind::Mouse, 0x1F), // Viper V3 Pro SE Wired
    (0x00DF, Kind::Mouse, 0x1F), // Viper V3 Pro SE Wireless
    (0x00E0, Kind::Mouse, 0x1F), // Orochi V2 2026 Receiver
    (0x00E5, Kind::Mouse, 0x1F), // Viper V4 Pro Wired
    (0x00E6, Kind::Mouse, 0x1F), // Viper V4 Pro Wireless
    // Wireless mice (tx_id = 0x3F)
    (0x0044, Kind::Mouse, 0x3F), // Mamba Wired
    (0x0045, Kind::Mouse, 0x3F), // Mamba Wireless
    (0x0059, Kind::Mouse, 0x3F), // Lancehead Wired
    (0x005A, Kind::Mouse, 0x3F), // Lancehead Wireless
    (0x006F, Kind::Mouse, 0x3F), // Lancehead Wireless Receiver
    (0x0070, Kind::Mouse, 0x3F), // Lancehead Wireless Wired
    (0x0072, Kind::Mouse, 0x3F), // Mamba Wireless Receiver
    (0x0073, Kind::Mouse, 0x3F), // Mamba Wireless Wired
    (0x007C, Kind::Mouse, 0x3F), // DeathAdder V2 Pro Wired
    (0x007D, Kind::Mouse, 0x3F), // DeathAdder V2 Pro Wireless
    // Wireless keyboards
    (0x0258, Kind::Keyboard, 0x1F), // BlackWidow V3 Mini HyperSpeed Wired
    (0x025A, Kind::Keyboard, 0x3F), // BlackWidow V3 Pro Wired
    (0x025C, Kind::Keyboard, 0x9F), // BlackWidow V3 Pro Wireless
    (0x0271, Kind::Keyboard, 0x9F), // BlackWidow V3 Mini HyperSpeed Wireless
    (0x0290, Kind::Keyboard, 0x9F), // DeathStalker V2 Pro Wireless
    (0x0292, Kind::Keyboard, 0x1F), // DeathStalker V2 Pro Wired
    (0x0296, Kind::Keyboard, 0x9F), // DeathStalker V2 Pro TKL Wireless
    (0x0298, Kind::Keyboard, 0x1F), // DeathStalker V2 Pro TKL Wired
    (0x02B9, Kind::Keyboard, 0x1F), // BlackWidow V4 Mini HyperSpeed Wired
    (0x02BA, Kind::Keyboard, 0x9F), // BlackWidow V4 Mini HyperSpeed Wireless
    (0x02CD, Kind::Keyboard, 0x1F), // Joro
    (0x02D2, Kind::Keyboard, 0x9F), // BlackWidow V4 Low-Profile TKL Wireless
    (0x02D4, Kind::Keyboard, 0x1F), // BlackWidow V4 Low-Profile TKL Wired
    (0x02D5, Kind::Keyboard, 0x9F), // BlackWidow V4 TKL HyperSpeed Wireless
    (0x02D7, Kind::Keyboard, 0x1F), // BlackWidow V4 TKL HyperSpeed Wired
];

pub fn lookup(pid: u16) -> Option<(Kind, u8)> {
    RAZER_DEVICES
        .iter()
        .find(|&&(p, _, _)| p == pid)
        .map(|&(_, k, tx)| (k, tx))
}

pub fn is_candidate(vid: u16, pid: u16, usage_page: u16, usage: u16) -> bool {
    vid == 0x1532
        && lookup(pid).is_some()
        && (usage_page >= 0xFF00 || (usage_page == 0x0001 && matches!(usage, 0x0002 | 0x0006)))
}

/// Compute XOR CRC over bytes `2..88` of a 90-byte `razer_report`.
pub fn razer_crc(report90: &[u8; 90]) -> u8 {
    let mut crc = 0u8;
    let mut i = 2usize;
    while i < 88 {
        crc ^= report90[i];
        i += 1;
    }
    crc
}

pub fn build_razer_feature_report(tx_id: u8, cmd: u8) -> [u8; 91] {
    let mut out = [0u8; 91];
    let mut r = [0u8; 90];
    r[1] = tx_id;
    r[5] = 0x02;
    r[6] = 0x07;
    r[7] = cmd;
    r[88] = razer_crc(&r);
    out[1..91].copy_from_slice(&r);
    out
}

pub fn parse_razer_reply(buf: &[u8], cmd: u8) -> Option<u8> {
    let r = if buf.len() >= 91 && buf[0] == 0x00 {
        &buf[1..91]
    } else if buf.len() >= 90 {
        &buf[..90]
    } else {
        return None;
    };
    if r[0] == 0x02 && r[6] == 0x07 && r[7] == cmd {
        Some(r[9])
    } else {
        None
    }
}

pub fn scale_razer_pct(raw: u8) -> Option<u8> {
    if raw == 0 {
        None
    } else {
        Some((((raw as u16) * 100 + 127) / 255).min(100) as u8)
    }
}

unsafe fn query_razer_cmd(h: HANDLE, ev: HANDLE, tx_id: u8, cmd: u8) -> Option<u8> {
    let req = build_razer_feature_report(tx_id, cmd);
    if !set_feature(h, ev, &req, 200) {
        return None;
    }
    for _ in 0..2 {
        Sleep(20);
        let mut reply = [0u8; 91];
        reply[0] = 0x00;
        let n = get_feature(h, ev, &mut reply, 200)?;
        if let Some(val) = parse_razer_reply(&reply[..n], cmd) {
            return Some(val);
        }
        let status = if n >= 91 && reply[0] == 0 {
            reply[1]
        } else {
            reply[0]
        };
        if status != 0x01 {
            return None;
        }
    }
    None
}

/// Poll a Razer wireless mouse or keyboard via 90-byte feature reports.
///
/// # Safety
/// `h` must be an opened HID HANDLE and `ev` a valid manual-reset event HANDLE.
pub unsafe fn poll_handle(h: HANDLE, ev: HANDLE, pid: u16, kind: Kind, tx_id: u8) -> Option<Battery> {
    let raw_level = query_razer_cmd(h, ev, tx_id, 0x80)?;
    let pct = scale_razer_pct(raw_level)?;
    let charging = query_razer_cmd(h, ev, tx_id, 0x84).unwrap_or(0) != 0;
    Some(Battery::with_id(
        make_hid_id(4, 0x1532, pid, 1),
        vp_u32(0x1532, pid),
        kind,
        Some(pct),
        charging,
        true,
        Source::NativeHid,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn razer_crc_and_battery_parsing() {
        let req = build_razer_feature_report(0x1F, 0x80);
        assert_eq!(req[0], 0x00);
        assert_eq!(req[2], 0x1F);
        assert_eq!(req[6], 0x02);
        assert_eq!(req[7], 0x07);
        assert_eq!(req[8], 0x80);
        assert_eq!(req[89], 0x02 ^ 0x07 ^ 0x80);

        let mut resp = [0u8; 91];
        resp[1] = 0x02;
        resp[7] = 0x07;
        resp[8] = 0x80;
        resp[10] = 204; // 80%
        assert_eq!(parse_razer_reply(&resp, 0x80).and_then(scale_razer_pct), Some(80));
        assert_eq!(scale_razer_pct(0), None);
        assert_eq!(scale_razer_pct(255), Some(100));

        assert_eq!(lookup(0x00C1), Some((Kind::Mouse, 0x1F)));
        assert_eq!(lookup(0x0290), Some((Kind::Keyboard, 0x9F)));
    }
}
