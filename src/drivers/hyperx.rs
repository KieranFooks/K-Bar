//! HyperX wireless headset battery driver (`VID 0x03F0` HP & `VID 0x0951` Kingston).
//!
//! Covers Cloud III Wireless, Cloud Alpha Wireless, Cloud II Wireless (HP & Kingston),
//! and Cloud Flight.

use crate::drivers::io::{
    make_hid_id, recv_report, send_report, send_report_padded, set_feature, voltage_to_pct,
    vp_u32,
};
use crate::hid::{Battery, Kind, Source};
use crate::win32::{GetTickCount64, HANDLE};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HyperxProto {
    Cloud3,
    CloudAlpha,
    Cloud2Hp,
    Cloud2Kingston,
    CloudFlight,
}

const HYPERX_PROFILES: &[(u16, u16, u16, HyperxProto)] = &[
    (0x03F0, 0x05B7, 0xFF13, HyperxProto::Cloud3),
    (0x03F0, 0x098D, 0, HyperxProto::CloudAlpha),
    (0x03F0, 0x0696, 0xFF90, HyperxProto::Cloud2Hp),
    (0x03F0, 0x018B, 0xFF90, HyperxProto::Cloud2Hp),
    (0x0951, 0x1718, 0, HyperxProto::Cloud2Kingston),
    (0x0951, 0x16C4, 0, HyperxProto::CloudFlight),
    (0x0951, 0x1723, 0, HyperxProto::CloudFlight),
];

pub fn match_profile(vid: u16, pid: u16, usage_page: u16) -> Option<HyperxProto> {
    for &(v, p, up, proto) in HYPERX_PROFILES {
        if vid == v && pid == p && ((up != 0 && usage_page == up) || (up == 0 && usage_page >= 0xFF00)) {
            return Some(proto);
        }
    }
    None
}

pub fn is_candidate(vid: u16, pid: u16, usage_page: u16) -> bool {
    match_profile(vid, pid, usage_page).is_some()
}

/// Parse HyperX Cloud III Wireless `0x89` response.
pub fn parse_hyperx_battery(r: &[u8]) -> Option<Option<u8>> {
    if r.len() < 5 || r[0] != 0x66 || r[1] != 0x89 {
        return None;
    }
    if r[2] == 0 && r[3] == 0 {
        Some(None)
    } else {
        Some(Some(r[4].min(100)))
    }
}

/// Parse HyperX Cloud III Wireless `0x8A` charging state response.
pub fn parse_hyperx_charging(r: &[u8]) -> Option<bool> {
    if r.len() < 3 || r[0] != 0x66 || r[1] != 0x8A {
        return None;
    }
    Some(r[2] != 0)
}

/// Parse Cloud Alpha Wireless `0x21 0xBB` responses (`0x03` conn, `0x0C` charge, `0x0B` pct).
pub fn parse_alpha_reply(r: &[u8], cmd: u8) -> Option<u8> {
    if r.len() >= 4 && r[0] == 0x21 && r[1] == 0xBB && r[2] == cmd {
        Some(r[3])
    } else {
        None
    }
}

/// Parse Cloud II Wireless (HP) `0x06 0xFF 0xBB` battery (`cmd=2`) / charging (`cmd=3`).
pub fn parse_cloud2_hp(r: &[u8], cmd: u8) -> Option<Option<u8>> {
    if r.len() < 8 || r[0] != 0x06 || r[1] != 0xFF || r[2] != 0xBB || r[3] != cmd {
        return None;
    }
    if cmd == 0x02 {
        if r[7] == 0 && r[5] == 0 && r[6] == 0 {
            Some(None)
        } else {
            Some(Some(r[7].min(100)))
        }
    } else {
        Some(Some(u8::from(r[4] == 1)))
    }
}

/// Parse Cloud II Wireless (Kingston) `0x0B ... 0xBB` battery (`cmd=2`) / charging (`cmd=3`).
pub fn parse_cloud2_kingston(r: &[u8], cmd: u8) -> Option<Option<u8>> {
    if r.len() < 8 || r[0] != 0x0B || r[2] != 0xBB || r[3] != cmd {
        return None;
    }
    if cmd == 0x02 {
        if r[7] == 0 {
            Some(None)
        } else {
            Some(Some(r[7].min(100)))
        }
    } else {
        Some(Some(u8::from(r[4] == 1)))
    }
}

/// Parse Cloud Flight `0x21 0xFF` response into `(Option<pct>, charging)`.
pub fn parse_cloud_flight(r: &[u8]) -> Option<(Option<u8>, bool)> {
    if r.len() < 6 || r[0] != 0x21 || r[1] != 0xFF {
        return None;
    }
    let mv = u16::from_be_bytes([r[3], r[4]]);
    if mv == 0 {
        return Some((None, false));
    }
    if mv > 0x100B {
        return Some((None, true));
    }
    if mv > 3975 {
        return Some((Some(100), false));
    }
    Some((Some(voltage_to_pct(mv)), false))
}

#[allow(clippy::too_many_arguments)]
unsafe fn transact_match(
    h: HANDLE,
    ev: HANDLE,
    req: &[u8],
    sizes: &[usize],
    timeout_ms: u32,
    pkt: &mut [u8; 128],
    pred: fn(&[u8], u8) -> bool,
    tag: u8,
) -> Option<usize> {
    if sizes.is_empty() {
        if !send_report(h, ev, req, timeout_ms) && !set_feature(h, ev, req, timeout_ms) {
            return None;
        }
    } else if !send_report_padded(h, ev, req, sizes, timeout_ms) {
        return None;
    }

    let deadline = GetTickCount64().wrapping_add(timeout_ms as u64);
    loop {
        let now = GetTickCount64();
        if now >= deadline {
            return None;
        }
        let rem = (deadline - now).clamp(1, timeout_ms as u64) as u32;
        let n = recv_report(h, ev, rem, pkt)?;
        if pred(&pkt[..n], tag) {
            return Some(n);
        }
    }
}

fn pred_cloud3(r: &[u8], cmd: u8) -> bool {
    r.len() >= 3 && r[0] == 0x66 && r[1] == cmd
}
fn pred_alpha(r: &[u8], cmd: u8) -> bool {
    r.len() >= 4 && r[0] == 0x21 && r[1] == 0xBB && r[2] == cmd
}
fn pred_c2hp(r: &[u8], cmd: u8) -> bool {
    r.len() >= 5 && r[0] == 0x06 && r[1] == 0xFF && r[2] == 0xBB && r[3] == cmd
}
fn pred_c2k(r: &[u8], cmd: u8) -> bool {
    r.len() >= 5 && r[0] == 0x0B && r[2] == 0xBB && r[3] == cmd
}
fn pred_flight(r: &[u8], _: u8) -> bool {
    r.len() >= 6 && r[0] == 0x21 && r[1] == 0xFF
}

/// Poll a HyperX wireless headset for battery status.
///
/// # Safety
/// `h` must be an opened HID HANDLE and `ev` a valid manual-reset event HANDLE.
pub unsafe fn poll_handle(h: HANDLE, ev: HANDLE, vid: u16, pid: u16, proto: HyperxProto) -> Option<Battery> {
    let id = make_hid_id(3, vid, pid, 1);
    let vp = vp_u32(vid, pid);
    let mut pkt = [0u8; 128];

    match proto {
        HyperxProto::Cloud3 => {
            let mut req = [0u8; 62];
            req[0] = 0x66;
            req[1] = 0x89;
            let n = transact_match(h, ev, &req, &[], 500, &mut pkt, pred_cloud3, 0x89)?;
            let pct = parse_hyperx_battery(&pkt[..n])?;
            let (online, charging) = match pct {
                Some(_) => {
                    req[1] = 0x8A;
                    let chg = transact_match(h, ev, &req, &[], 400, &mut pkt, pred_cloud3, 0x8A)
                        .and_then(|cn| parse_hyperx_charging(&pkt[..cn]))
                        .unwrap_or(false);
                    (true, chg)
                }
                None => (false, false),
            };
            Some(Battery::with_id(id, vp, Kind::Headset, pct, charging, online, Source::NativeHid))
        }
        HyperxProto::CloudAlpha => {
            let n = transact_match(h, ev, &[0x21, 0xBB, 0x03], &[31, 52, 64], 300, &mut pkt, pred_alpha, 0x03)?;
            if parse_alpha_reply(&pkt[..n], 0x03) == Some(0x01) {
                return Some(Battery::with_id(id, vp, Kind::Headset, None, false, false, Source::NativeHid));
            }
            let nb = transact_match(h, ev, &[0x21, 0xBB, 0x0B], &[31, 52, 64], 300, &mut pkt, pred_alpha, 0x0B)?;
            let pct = parse_alpha_reply(&pkt[..nb], 0x0B)?.min(100);
            let chg = transact_match(h, ev, &[0x21, 0xBB, 0x0C], &[31, 52, 64], 250, &mut pkt, pred_alpha, 0x0C)
                .and_then(|nc| parse_alpha_reply(&pkt[..nc], 0x0C))
                == Some(0x01);
            Some(Battery::with_id(id, vp, Kind::Headset, Some(pct), chg, true, Source::NativeHid))
        }
        HyperxProto::Cloud2Hp => {
            let n = transact_match(h, ev, &[0x06, 0xFF, 0xBB, 0x02], &[52, 64, 31], 300, &mut pkt, pred_c2hp, 0x02)?;
            let pct = parse_cloud2_hp(&pkt[..n], 0x02)?;
            let (online, charging) = match pct {
                Some(_) => {
                    let chg = transact_match(h, ev, &[0x06, 0xFF, 0xBB, 0x03], &[52, 64, 31], 250, &mut pkt, pred_c2hp, 0x03)
                        .and_then(|nc| parse_cloud2_hp(&pkt[..nc], 0x03))
                        .flatten()
                        == Some(1);
                    (true, chg)
                }
                None => (false, false),
            };
            Some(Battery::with_id(id, vp, Kind::Headset, pct, charging, online, Source::NativeHid))
        }
        HyperxProto::Cloud2Kingston => {
            let req2 = [0x06, 0x00, 0x02, 0x00, 0x9A, 0x00, 0x00, 0x68, 0x4A, 0x8E, 0x0A, 0x00, 0x00, 0x00, 0xBB, 0x02];
            let n = transact_match(h, ev, &req2, &[20, 64, 52], 300, &mut pkt, pred_c2k, 0x02)?;
            let pct = parse_cloud2_kingston(&pkt[..n], 0x02)?;
            let (online, charging) = match pct {
                Some(_) => {
                    let mut req3 = req2;
                    req3[15] = 0x03;
                    let chg = transact_match(h, ev, &req3, &[20, 64, 52], 250, &mut pkt, pred_c2k, 0x03)
                        .and_then(|nc| parse_cloud2_kingston(&pkt[..nc], 0x03))
                        .flatten()
                        == Some(1);
                    (true, chg)
                }
                None => (false, false),
            };
            Some(Battery::with_id(id, vp, Kind::Headset, pct, charging, online, Source::NativeHid))
        }
        HyperxProto::CloudFlight => {
            let n = transact_match(h, ev, &[0x21, 0xFF, 0x05], &[20, 64], 300, &mut pkt, pred_flight, 0)?;
            let (pct, chg) = parse_cloud_flight(&pkt[..n])?;
            Some(Battery::with_id(id, vp, Kind::Headset, pct, chg, pct.is_some(), Source::NativeHid))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hyperx_battery_and_charging_parsers() {
        assert_eq!(parse_hyperx_battery(&[0x66, 0x89, 0x0F, 0xEB, 0x5D]), Some(Some(93)));
        assert_eq!(parse_hyperx_battery(&[0x66, 0x89, 0x00, 0x00, 0x00]), Some(None));
        assert_eq!(parse_hyperx_battery(&[0x66, 0x8A, 0x01, 0x00, 0x00]), None);
        assert_eq!(parse_hyperx_charging(&[0x66, 0x8A, 0x01]), Some(true));
        assert_eq!(parse_hyperx_charging(&[0x66, 0x8A, 0x00]), Some(false));

        assert_eq!(parse_alpha_reply(&[0x21, 0xBB, 0x03, 0x01], 0x03), Some(1));
        assert_eq!(parse_alpha_reply(&[0x21, 0xBB, 0x0B, 78], 0x0B), Some(78));
        assert_eq!(parse_cloud2_hp(&[0x06, 0xFF, 0xBB, 0x02, 0, 0x0F, 0xA0, 84], 0x02), Some(Some(84)));
        assert_eq!(parse_cloud2_hp(&[0x06, 0xFF, 0xBB, 0x02, 0, 0, 0, 0], 0x02), Some(None));
        assert_eq!(parse_cloud2_kingston(&[0x0B, 0, 0xBB, 0x02, 0, 0, 0, 72], 0x02), Some(Some(72)));

        let mv_full = 3980u16.to_be_bytes();
        assert_eq!(
            parse_cloud_flight(&[0x21, 0xFF, 0, mv_full[0], mv_full[1], 0]),
            Some((Some(100), false))
        );
        let mv_chg = 4120u16.to_be_bytes();
        assert_eq!(
            parse_cloud_flight(&[0x21, 0xFF, 0, mv_chg[0], mv_chg[1], 0]),
            Some((None, true))
        );
    }
}
