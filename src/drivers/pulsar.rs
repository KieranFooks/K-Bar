//! Pulsar wireless & wired gaming mice battery driver (`VID 0x3710`, `VID 0x3554`).
//!
//! Covers X2 CrazyLight (`0x3710:0x5406` 8K dongle / `0x3710:0x3414` wired),
//! X2 v3 / Xlite v4 / X3 (`0x3710`), and X2 V2 / X2H / X2A / Xlite V3 (`0x3554`).

use crate::drivers::io::{
    get_feature, make_hid_id, recv_report, send_report_padded, set_feature, strip_zero_report_id,
    vp_u32,
};
use crate::hid::{Battery, Kind, Source};
use crate::win32::{GetTickCount64, HANDLE};

const REPORT_ID_CMD: u8 = 0x08;
const REPORT_ID_ALT: u8 = 0x09;
const CMD_POWER: u8 = 0x04;
const CHECKSUM_MAGIC: u8 = 0x55;
const PAYLOAD_LEN: usize = 17;

pub fn is_candidate(vid: u16, pid: u16, usage_page: u16, _usage: u16) -> bool {
    if usage_page < 0xFF00 {
        return false;
    }
    match vid {
        // Pulsar Gaming Gears (X2 CrazyLight, X2 v3, Xlite v4, X3, etc.)
        0x3710 => matches!(pid & 0xFF00, 0x1400 | 0x3400 | 0x5400),
        // Pulsar X2 V2 / X2H / X2A / Xlite V3 (wired, 1K, 4K dongles)
        0x3554 => matches!(pid, 0xF507..=0xF509),
        _ => false,
    }
}

/// Compute CompX/Pulsar 17-byte packet checksum (`0x55 - sum(bytes[0..16])`).
pub fn pulsar_checksum(pkt: &[u8]) -> u8 {
    let mut sum = 0u8;
    let mut i = 0usize;
    while i < 16 && i < pkt.len() {
        sum = sum.wrapping_add(pkt[i]);
        i += 1;
    }
    CHECKSUM_MAGIC.wrapping_sub(sum)
}

/// Build a 17-byte Pulsar HID command packet (`Report ID 0x08`).
pub fn build_cmd(cmd: u8) -> [u8; PAYLOAD_LEN] {
    let mut pkt = [0u8; PAYLOAD_LEN];
    pkt[0] = REPORT_ID_CMD;
    pkt[1] = cmd;
    pkt[16] = pulsar_checksum(&pkt);
    pkt
}

/// Parse a Pulsar `CMD_POWER` (`0x04`) reply into `Some(Some((pct, charging)))`,
/// `Some(None)` when the mouse is asleep/offline on its wireless dongle, or `None` if unrelated.
pub fn parse_power_reply(raw: &[u8]) -> Option<Option<(u8, bool)>> {
    let r = strip_zero_report_id(raw);
    if r.len() < 10 || (r[0] != REPORT_ID_CMD && r[0] != REPORT_ID_ALT) || r[1] != CMD_POWER {
        return None;
    }
    if r[0] == REPORT_ID_CMD && r.len() >= PAYLOAD_LEN && r[16] != pulsar_checksum(&r[..PAYLOAD_LEN])
    {
        return None;
    }
    let pct = r[6];
    if pct > 100 || r[7] > 0x01 {
        return None;
    }
    let charging = r[7] == 0x01;
    let mv = u16::from_be_bytes([r[8], r[9]]);
    if pct == 0 && mv == 0 {
        return Some(None);
    }
    Some(Some((pct, charging)))
}

/// Poll a Pulsar mouse or wireless dongle via `CMD_POWER` (`0x08 0x04`).
///
/// # Safety
/// `h` must be an opened HID HANDLE and `ev` a valid manual-reset event HANDLE.
pub unsafe fn poll_handle(h: HANDLE, ev: HANDLE, vid: u16, pid: u16) -> Option<Battery> {
    let req = build_cmd(CMD_POWER);
    let sent_out = send_report_padded(h, ev, &req, &[17, 32, 64, 65], 200);
    if !sent_out && !set_feature(h, ev, &req, 200) {
        return None;
    }

    let make_batt = |pct: u8, charging: bool| {
        Battery::with_id(
            make_hid_id(7, vid, pid, 1),
            vp_u32(vid, pid),
            Kind::Mouse,
            Some(pct),
            charging,
            true,
            Source::NativeHid,
        )
    };

    let mut pkt = [0u8; 128];
    let deadline = GetTickCount64().wrapping_add(220);
    loop {
        let now = GetTickCount64();
        if now >= deadline {
            break;
        }
        let rem = (deadline - now).clamp(1, 220) as u32;
        let Some(n) = recv_report(h, ev, rem, &mut pkt) else {
            break;
        };
        if let Some(parsed) = parse_power_reply(&pkt[..n]) {
            return parsed.map(|(pct, chg)| make_batt(pct, chg));
        }
    }

    let mut feat = [0u8; PAYLOAD_LEN];
    feat[0] = REPORT_ID_CMD;
    if let Some(n) = get_feature(h, ev, &mut feat, 200) {
        if let Some(Some((pct, chg))) = parse_power_reply(&feat[..n]) {
            return Some(make_batt(pct, chg));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pulsar_candidate_and_protocol_parsing() {
        // Pulsar X2 CrazyLight wireless dongle (0x5406) & wired (0x3414)
        assert!(is_candidate(0x3710, 0x5406, 0xFF02, 0x0001));
        assert!(is_candidate(0x3710, 0x3414, 0xFF00, 0x0001));
        assert!(!is_candidate(0x3710, 0x5406, 0x0001, 0x0002));
        // Older Pulsar X2 V2 / Xlite V3
        assert!(is_candidate(0x3554, 0xF508, 0xFF02, 0x0001));
        assert!(!is_candidate(0x3554, 0x0001, 0xFF02, 0x0001));

        let cmd = build_cmd(CMD_POWER);
        assert_eq!(cmd[0], 0x08);
        assert_eq!(cmd[1], 0x04);
        assert_eq!(cmd[16], 0x49);

        // Standard 17-byte 0x08 CMD_POWER reply: 84%, charging, 4080 mV
        let mut resp = [0u8; 17];
        resp[0] = 0x08;
        resp[1] = 0x04;
        resp[5] = 0x04;
        resp[6] = 84;
        resp[7] = 0x01;
        resp[8..10].copy_from_slice(&4080u16.to_be_bytes());
        resp[16] = pulsar_checksum(&resp);
        assert_eq!(parse_power_reply(&resp), Some(Some((84, true))));

        // Bad checksum on 0x08 is rejected
        resp[16] ^= 0xFF;
        assert_eq!(parse_power_reply(&resp), None);

        // Sleeping/offline mouse on dongle (0% and 0 mV)
        let mut sleep_resp = [0u8; 17];
        sleep_resp[0] = 0x08;
        sleep_resp[1] = 0x04;
        sleep_resp[16] = pulsar_checksum(&sleep_resp);
        assert_eq!(parse_power_reply(&sleep_resp), Some(None));

        // 0x09 reply variant
        assert_eq!(
            parse_power_reply(&[0x09, 0x04, 0x00, 0x00, 0x00, 0x02, 55, 0x00, 0x0E, 0xD8]),
            Some(Some((55, false)))
        );
    }
}
