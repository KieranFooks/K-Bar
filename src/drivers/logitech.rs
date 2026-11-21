//! Logitech HID++ 2.0, HID++ 1.0, and G PRO X 2 vendor battery driver (`VID 0x046D`).
//!
//! Supports mice, keyboards, and headsets across Unifying, Bolt, LIGHTSPEED,
//! direct USB, and Logitech G headset interfaces.

use crate::drivers::io::{
    make_hid_id, open_hid, recv_report, send_report, send_report_padded, voltage_to_pct, vp_u32,
    RawCandidate,
};
use crate::hid::{Battery, BatteryList, Kind, Source};
use crate::win32::{CloseHandle, GetTickCount64, HANDLE};

/// Non-zero HID++ software ID to isolate our replies from Logitech G HUB
/// traffic and unsolicited device notifications (which use sw_id = 0).
pub const HIDPP_SW_ID: u8 = 0x0A;

pub const LOGI_EP_CAP: usize = 6;

pub fn is_candidate(vid: u16, pid: u16, usage_page: u16, usage: u16) -> bool {
    if vid != 0x046D {
        return false;
    }
    (usage_page == 0xFF00 && (usage == 0x0002 || usage == 0x0001))
        || (usage_page == 0xFF43 && (usage == 0x0202 || usage == 0x0002))
        || (pid == 0x0AC4 && usage_page == 0x000C && usage == 0x0001)
        || (pid == 0x0AF7 && usage_page == 0xFFA0 && usage == 0x0001)
}

pub fn is_single_receiver_pid(pid: u16) -> bool {
    matches!(pid, 0xC539 | 0xC53A | 0xC53F | 0xC541 | 0xC545 | 0xC547)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MatchReply {
    Match,
    ProtocolError,
    Ignore,
}

/// Match an incoming HID++ 2.0 packet against our outgoing request parameters.
pub fn match_hidpp_reply(r: &[u8], dev_idx: u8, feat_idx: u8, func: u8) -> MatchReply {
    if r.len() < 5 || (r[0] != 0x11 && r[0] != 0x10) || r[1] != dev_idx {
        return MatchReply::Ignore;
    }
    let expected_fn_sw = ((func << 4) & 0xF0) | (HIDPP_SW_ID & 0x0F);
    if r[2] == feat_idx && r[3] == expected_fn_sw {
        return MatchReply::Match;
    }
    if r[2] == 0xFF && r[3] == feat_idx && r[4] == expected_fn_sw {
        return MatchReply::ProtocolError;
    }
    if r[2] == 0x8F && r[3] == feat_idx && r[4] == expected_fn_sw {
        return MatchReply::ProtocolError;
    }
    MatchReply::Ignore
}

/// Parse HID++ 2.0 `0x1004` UnifiedBattery `func 1` (get_status) response.
pub fn parse_1004_status(r: &[u8]) -> Option<(u8, bool)> {
    if r.len() < 7 {
        return None;
    }
    Some((r[4].min(100), matches!(r[6], 1 | 2 | 4)))
}

/// Parse HID++ 2.0 `0x1000` BatteryLevelStatus `func 0` response.
pub fn parse_1000_status(r: &[u8]) -> Option<(u8, bool)> {
    if r.len() < 7 {
        return None;
    }
    Some((r[4].min(100), matches!(r[6], 1 | 2 | 4)))
}

/// Parse HID++ 2.0 `0x1001` BatteryVoltage `func 0` response.
pub fn parse_1001_voltage(r: &[u8]) -> Option<(u8, bool)> {
    if r.len() < 7 {
        return None;
    }
    let mv = u16::from_be_bytes([r[4], r[5]]);
    if !(2500..=5000).contains(&mv) {
        return None;
    }
    Some((voltage_to_pct(mv), (r[6] & 0x80) != 0))
}

/// Parse HID++ 2.0 `0x1F20` ADCMeasurement `func 0` response (headsets).
pub fn parse_1f20_adc(r: &[u8]) -> Option<(u8, bool)> {
    if r.len() < 7 {
        return None;
    }
    let mv = u16::from_be_bytes([r[4], r[5]]);
    if !(2500..=5000).contains(&mv) {
        return None;
    }
    let charging = r[6] == 0x03;
    Some((voltage_to_pct(mv), charging))
}

/// Parse HID++ 1.0 `0x0D` BatteryMileage short reply.
pub fn parse_hidpp10_mileage(r: &[u8]) -> Option<(u8, bool)> {
    if r.len() < 7 || r[0] != 0x10 || r[2] != 0x81 || r[3] != 0x0D {
        return None;
    }
    if r[4] > 100 {
        return None;
    }
    Some((r[4], matches!(r[6], 1 | 2)))
}

/// Parse HID++ 1.0 `0x07` BatteryStatus short reply.
pub fn parse_hidpp10_status(r: &[u8]) -> Option<(u8, bool)> {
    if r.len() < 6 || r[0] != 0x10 || r[2] != 0x81 || r[3] != 0x07 {
        return None;
    }
    let pct = match r[4] {
        1 => 5,
        3 => 20,
        5 => 50,
        7 => 90,
        _ => return None,
    };
    let charging = matches!(r[5], 0x22 | 0x25);
    Some((pct, charging))
}

/// Parse Logitech G PRO X 2 LIGHTSPEED (`0x0AF7`, `UP 0xFFA0`) 64-byte response.
pub fn parse_gprox2_battery(r: &[u8]) -> Option<(u8, bool)> {
    if r.len() < 13 || r[0] != 0x51 || r[1] != 0x0B || r[8] != 0x04 {
        return None;
    }
    Some((r[10].min(100), r[12] == 0x02))
}

/// Map HID++ `0x0005` `getDeviceType` byte to `Kind`.
pub fn device_type_to_kind(dev_type: u8, fallback: Kind) -> Kind {
    match dev_type {
        0 | 2 => Kind::Keyboard,
        3..=5 => Kind::Mouse,
        8 => Kind::Headset,
        _ => fallback,
    }
}

/// Send a HID++ 2.0 long (`0x11`, 20B) request (`cmd = (dev_idx, feat_idx, func)`)
/// and discard unrelated packets until deadline.
///
/// # Safety
/// `h` must be an opened HID HANDLE and `ev` a valid manual-reset event HANDLE.
pub unsafe fn hidpp_request(
    h: HANDLE,
    ev: HANDLE,
    cmd: (u8, u8, u8),
    params: &[u8],
    timeout_ms: u32,
    reply_out: &mut [u8; 128],
) -> Option<usize> {
    let (dev_idx, feat_idx, func) = cmd;
    let mut req = [0u8; 20];
    req[0] = 0x11;
    req[1] = dev_idx;
    req[2] = feat_idx;
    req[3] = ((func << 4) & 0xF0) | (HIDPP_SW_ID & 0x0F);
    for (i, &p) in params.iter().enumerate() {
        if 4 + i < req.len() {
            req[4 + i] = p;
        }
    }
    if !send_report(h, ev, &req, timeout_ms) {
        return None;
    }

    let deadline = GetTickCount64().wrapping_add(timeout_ms as u64);
    loop {
        let now = GetTickCount64();
        if now >= deadline {
            return None;
        }
        let rem = (deadline - now).clamp(1, timeout_ms as u64) as u32;
        let n = recv_report(h, ev, rem, reply_out)?;
        match match_hidpp_reply(&reply_out[..n], dev_idx, feat_idx, func) {
            MatchReply::Match => return Some(n),
            MatchReply::ProtocolError => return None,
            MatchReply::Ignore => continue,
        }
    }
}

#[allow(clippy::too_many_arguments)]
unsafe fn hidpp10_request(
    h: HANDLE,
    ev: HANDLE,
    dev_idx: u8,
    sub_id: u8,
    addr: u8,
    p0: u8,
    timeout_ms: u32,
    reply_out: &mut [u8; 128],
) -> Option<usize> {
    let req = [0x10, dev_idx, sub_id, addr, p0, 0, 0];
    if !send_report(h, ev, &req, timeout_ms) {
        return None;
    }
    let deadline = GetTickCount64().wrapping_add(timeout_ms as u64);
    loop {
        let now = GetTickCount64();
        if now >= deadline {
            return None;
        }
        let rem = (deadline - now).clamp(1, timeout_ms as u64) as u32;
        let n = recv_report(h, ev, rem, reply_out)?;
        if n >= 7 && reply_out[0] == 0x10 && reply_out[1] == dev_idx {
            if reply_out[2] == sub_id && reply_out[3] == addr {
                return Some(n);
            }
            if reply_out[2] == 0x8F && reply_out[3] == sub_id && reply_out[4] == addr {
                return None;
            }
        }
    }
}

unsafe fn hidpp_ping(h: HANDLE, ev: HANDLE, dev_idx: u8, timeout_ms: u32) -> bool {
    const MARKER: u8 = 0x5A;
    let mut r = [0u8; 128];
    match hidpp_request(h, ev, (dev_idx, 0x00, 0x01), &[0, 0, MARKER], timeout_ms, &mut r) {
        Some(n) => n >= 7 && r[6] == MARKER,
        None => false,
    }
}

/// Fast Unifying/HID++ 1.0 check on `Col01` (`UP 0xFF00, U 0x0001`):
/// - Returns `Some(false)` if receiver reports `paired_count == 0` OR slot ping gets `0x8F ... 0x09` (offline).
/// - Returns `Some(true)` if short interface is responsive, or `None` if short interface unavailable.
unsafe fn short_slot_check(h_short: HANDLE, ev: HANDLE, dev_idx: u8) -> Option<bool> {
    let req = [0x10, dev_idx, 0x00, (0x01 << 4) | HIDPP_SW_ID, 0x00, 0x00, 0x5A];
    if !send_report(h_short, ev, &req, 60) {
        return None;
    }
    let mut r = [0u8; 128];
    let n = recv_report(h_short, ev, 60, &mut r)?;
    if n >= 6 && r[0] == 0x10 && r[1] == dev_idx && r[2] == 0x8F {
        // Error 0x09 = ERR_RESOURCE_ERROR (sleeping/unreachable)
        if r[5] == 0x09 {
            return Some(false);
        }
    }
    Some(true)
}

unsafe fn short_paired_count(h_short: HANDLE, ev: HANDLE) -> Option<u8> {
    let mut r = [0u8; 128];
    let _n = hidpp10_request(h_short, ev, 0xFF, 0x81, 0x02, 0x00, 80, &mut r)?;
    Some(r[5])
}

unsafe fn hidpp_get_feature_index(h: HANDLE, ev: HANDLE, dev_idx: u8, feat_id: u16) -> Option<u8> {
    let [hi, lo] = feat_id.to_be_bytes();
    let mut r = [0u8; 128];
    let n = hidpp_request(h, ev, (dev_idx, 0x00, 0x00), &[hi, lo], 250, &mut r)?;
    if n >= 5 && r[4] != 0 {
        Some(r[4])
    } else {
        None
    }
}

unsafe fn hidpp_device_kind(h: HANDLE, ev: HANDLE, dev_idx: u8, fallback: Kind) -> Kind {
    let Some(name_idx) = hidpp_get_feature_index(h, ev, dev_idx, 0x0005) else {
        return fallback;
    };
    let mut r = [0u8; 128];
    let Some(n) = hidpp_request(h, ev, (dev_idx, name_idx, 0x02), &[], 250, &mut r) else {
        return fallback;
    };
    if n >= 5 {
        device_type_to_kind(r[4], fallback)
    } else {
        fallback
    }
}

unsafe fn hidpp_read_battery(
    h: HANDLE,
    ev: HANDLE,
    dev_idx: u8,
    feat_id: u16,
    feat_idx: u8,
) -> Option<(u8, bool)> {
    let mut r = [0u8; 128];
    match feat_id {
        0x1004 => {
            let n = hidpp_request(h, ev, (dev_idx, feat_idx, 0x01), &[], 250, &mut r)?;
            parse_1004_status(&r[..n])
        }
        0x1000 => {
            let n = hidpp_request(h, ev, (dev_idx, feat_idx, 0x00), &[], 250, &mut r)?;
            parse_1000_status(&r[..n])
        }
        0x1001 => {
            let n = hidpp_request(h, ev, (dev_idx, feat_idx, 0x00), &[], 250, &mut r)?;
            parse_1001_voltage(&r[..n])
        }
        0x1F20 => {
            let n = hidpp_request(h, ev, (dev_idx, feat_idx, 0x00), &[], 250, &mut r)?;
            parse_1f20_adc(&r[..n])
        }
        _ => None,
    }
}

unsafe fn gprox2_read_battery(h: HANDLE, ev: HANDLE) -> Option<(u8, bool)> {
    let req = [0x51, 0x08, 0x00, 0x03, 0x1A, 0x00, 0x03, 0x00, 0x04, 0x0A];
    if !send_report_padded(h, ev, &req, &[64], 250) {
        return None;
    }
    let mut r = [0u8; 128];
    let deadline = GetTickCount64().wrapping_add(250);
    loop {
        let now = GetTickCount64();
        if now >= deadline {
            return None;
        }
        let rem = (deadline - now).clamp(1, 250) as u32;
        let n = recv_report(h, ev, rem, &mut r)?;
        if let Some(parsed) = parse_gprox2_battery(&r[..n]) {
            return Some(parsed);
        }
    }
}

fn logitech_dev_indices(pid: u16) -> &'static [u8] {
    if is_single_receiver_pid(pid) {
        &[1]
    } else if (0xC500..=0xC5FF).contains(&pid) {
        &[1, 2, 3, 4, 5, 6]
    } else {
        &[0xFF, 1]
    }
}

#[derive(Clone, Copy)]
#[repr(u8)]
enum LogiProto {
    None = 0,
    Hidpp20 = 1,
    Hidpp10Reg = 2,
    GProX2 = 3,
}

fn path_hash8(path: &[u16]) -> u8 {
    let mut h = 0x811C_9DC5u32;
    for &u in path {
        if u == 0 {
            break;
        }
        h ^= (u & 0xFF) as u32;
        h = h.wrapping_mul(0x0100_0193);
        h ^= (u >> 8) as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    ((h ^ (h >> 8) ^ (h >> 16) ^ (h >> 24)) as u8).max(1)
}

#[derive(Clone, Copy)]
pub struct LogitechEndpoint {
    cand_idx: u8,
    path_hash: u8,
    dev_idx: u8,
    kind: Kind,
    proto: LogiProto,
    batt_feat_id: u16,
    batt_feat_idx: u8,
    pid: u16,
}

impl LogitechEndpoint {
    pub const fn empty() -> Self {
        Self {
            cand_idx: 0,
            path_hash: 0,
            dev_idx: 0,
            kind: Kind::Mouse,
            proto: LogiProto::None,
            batt_feat_id: 0,
            batt_feat_idx: 0,
            pid: 0,
        }
    }
}

/// Returns `true` when any Logitech non-short candidate is either a multi-slot receiver
/// (where another paired device may wake later) or has no cached endpoint yet.
pub fn has_undiscovered_slots(
    cands: &[RawCandidate],
    eps: &[LogitechEndpoint; LOGI_EP_CAP],
    ep_count: u8,
) -> bool {
    let n = (ep_count as usize).min(LOGI_EP_CAP);
    if n >= LOGI_EP_CAP {
        return false;
    }
    for (ci, c) in cands.iter().enumerate() {
        if c.vid != 0x046D || (c.usage_page == 0xFF00 && c.usage == 0x0001) {
            continue;
        }
        let is_multi = (0xC500..=0xC5FF).contains(&c.pid) && !is_single_receiver_pid(c.pid);
        let cached = eps[..n]
            .iter()
            .any(|e| !matches!(e.proto, LogiProto::None) && e.cand_idx == ci as u8);
        if is_multi || !cached {
            return true;
        }
    }
    false
}

/// Poll all previously discovered Logitech endpoints.
///
/// # Safety
/// `ev` must be a valid manual-reset event HANDLE.
pub unsafe fn poll_cached(
    ev: HANDLE,
    cands: &[RawCandidate],
    eps: &mut [LogitechEndpoint; LOGI_EP_CAP],
    ep_count: &mut u8,
    results: &mut BatteryList,
) -> bool {
    let mut had_open_failure = false;
    let count = (*ep_count as usize).min(LOGI_EP_CAP);
    let mut i = 0;
    while i < count {
        let ep = eps[i];
        let ci = ep.cand_idx as usize;
        if ci >= cands.len() {
            eps[i].proto = LogiProto::None;
            i += 1;
            continue;
        }
        let Some(h) = open_hid(cands[ci].path.buf.as_ptr()) else {
            had_open_failure = true;
            eps[i].proto = LogiProto::None;
            i += 1;
            continue;
        };
        let reading = match ep.proto {
            LogiProto::Hidpp20 => {
                hidpp_read_battery(h, ev, ep.dev_idx, ep.batt_feat_id, ep.batt_feat_idx)
            }
            LogiProto::Hidpp10Reg => {
                let mut r = [0u8; 128];
                hidpp10_request(h, ev, ep.dev_idx, 0x81, ep.batt_feat_idx, 0, 200, &mut r)
                    .and_then(|n| {
                        if ep.batt_feat_idx == 0x0D {
                            parse_hidpp10_mileage(&r[..n])
                        } else {
                            parse_hidpp10_status(&r[..n])
                        }
                    })
            }
            LogiProto::GProX2 => gprox2_read_battery(h, ev),
            LogiProto::None => None,
        };
        let _ = CloseHandle(h);
        if let Some((pct, charging)) = reading {
            let sub = ((ep.path_hash as u16) << 8) | (ep.dev_idx as u16);
            results.upsert(Battery::with_id(
                make_hid_id(1, 0x046D, ep.pid, sub),
                vp_u32(0x046D, ep.pid),
                ep.kind,
                Some(pct),
                charging,
                true,
                Source::NativeHid,
            ));
        }
        i += 1;
    }

    // Compact dropped endpoints
    let mut w = 0usize;
    for r in 0..count {
        if !matches!(eps[r].proto, LogiProto::None) {
            eps[w] = eps[r];
            w += 1;
        }
    }
    *ep_count = w as u8;
    had_open_failure
}

fn has_endpoint(eps: &[LogitechEndpoint], count: usize, cand_idx: u8, dev_idx: u8) -> bool {
    eps[..count]
        .iter()
        .any(|e| !matches!(e.proto, LogiProto::None) && e.cand_idx == cand_idx && e.dev_idx == dev_idx)
}

/// Discover Logitech HID++ / vendor devices across all slots and receivers.
///
/// # Safety
/// `ev` must be a valid manual-reset event HANDLE.
#[inline(never)]
pub unsafe fn discover(
    ev: HANDLE,
    cands: &[RawCandidate],
    eps: &mut [LogitechEndpoint; LOGI_EP_CAP],
    ep_count: &mut u8,
    results: &mut BatteryList,
) -> bool {
    let mut any_open_fail = false;

    for (ci, cand) in cands.iter().enumerate() {
        if cand.vid != 0x046D {
            continue;
        }
        if (*ep_count as usize) >= LOGI_EP_CAP {
            break;
        }

        // G PRO X 2 LIGHTSPEED vendor collection
        if cand.pid == 0x0AF7 && cand.usage_page == 0xFFA0 {
            if has_endpoint(eps, *ep_count as usize, ci as u8, 1) {
                continue;
            }
            let Some(h) = open_hid(cand.path.buf.as_ptr()) else {
                any_open_fail = true;
                continue;
            };
            if let Some((pct, charging)) = gprox2_read_battery(h, ev) {
                let ph = path_hash8(cand.path.as_slice());
                let idx = *ep_count as usize;
                eps[idx] = LogitechEndpoint {
                    cand_idx: ci as u8,
                    path_hash: ph,
                    dev_idx: 1,
                    kind: Kind::Headset,
                    proto: LogiProto::GProX2,
                    batt_feat_id: 0,
                    batt_feat_idx: 0,
                    pid: cand.pid,
                };
                *ep_count += 1;
                results.upsert(Battery::with_id(
                    make_hid_id(1, 0x046D, cand.pid, ((ph as u16) << 8) | 1),
                    vp_u32(0x046D, cand.pid),
                    Kind::Headset,
                    Some(pct),
                    charging,
                    true,
                    Source::NativeHid,
                ));
            }
            let _ = CloseHandle(h);
            continue;
        }

        // Only probe HID++ 2.0 long / headset collections directly; short Col01 is used as helper
        if cand.usage_page == 0xFF00 && cand.usage == 0x0001 {
            continue;
        }

        let is_headset_iface = cand.usage_page == 0xFF43 || cand.pid == 0x0AC4;
        let fallback_kind = if is_headset_iface {
            Kind::Headset
        } else {
            Kind::Mouse
        };

        let indices = logitech_dev_indices(cand.pid);
        let ping_timeout = if indices.len() == 1 { 250 } else { 120 };

        // Optional fast paired-count check via matching short (UP 0xFF00, U 0x0001) handle
        let short_handle = if indices.len() > 1 {
            cands
                .iter()
                .find(|c| c.vid == 0x046D && c.pid == cand.pid && c.usage_page == 0xFF00 && c.usage == 0x0001)
                .and_then(|c| open_hid(c.path.buf.as_ptr()))
        } else {
            None
        };

        if let Some(sh) = short_handle {
            if (0xC500..=0xC5FF).contains(&cand.pid)
                && !is_single_receiver_pid(cand.pid)
                && short_paired_count(sh, ev) == Some(0)
            {
                let _ = CloseHandle(sh);
                continue;
            }
        }

        let Some(handle) = open_hid(cand.path.buf.as_ptr()) else {
            if let Some(sh) = short_handle {
                let _ = CloseHandle(sh);
            }
            any_open_fail = true;
            continue;
        };

        for &dev_idx in indices {
            if (*ep_count as usize) >= LOGI_EP_CAP {
                break;
            }
            if has_endpoint(eps, *ep_count as usize, ci as u8, dev_idx) {
                continue;
            }
            if indices.len() > 1 {
                if let Some(sh) = short_handle {
                    if short_slot_check(sh, ev, dev_idx) == Some(false) {
                        continue;
                    }
                }
            }

            if hidpp_ping(handle, ev, dev_idx, ping_timeout) {
                let kind = hidpp_device_kind(handle, ev, dev_idx, fallback_kind);
                let mut found_feat = None;
                for feat_id in [0x1004u16, 0x1000u16, 0x1001u16, 0x1F20u16] {
                    if let Some(feat_idx) = hidpp_get_feature_index(handle, ev, dev_idx, feat_id) {
                        found_feat = Some((feat_id, feat_idx));
                        break;
                    }
                }
                if let Some((batt_feat_id, batt_feat_idx)) = found_feat {
                    if let Some((pct, charging)) =
                        hidpp_read_battery(handle, ev, dev_idx, batt_feat_id, batt_feat_idx)
                    {
                        let ph = path_hash8(cand.path.as_slice());
                        let idx = *ep_count as usize;
                        eps[idx] = LogitechEndpoint {
                            cand_idx: ci as u8,
                            path_hash: ph,
                            dev_idx,
                            kind,
                            proto: LogiProto::Hidpp20,
                            batt_feat_id,
                            batt_feat_idx,
                            pid: cand.pid,
                        };
                        *ep_count += 1;
                        let sub = ((ph as u16) << 8) | (dev_idx as u16);
                        results.upsert(Battery::with_id(
                            make_hid_id(1, 0x046D, cand.pid, sub),
                            vp_u32(0x046D, cand.pid),
                            kind,
                            Some(pct),
                            charging,
                            true,
                            Source::NativeHid,
                        ));
                        continue;
                    }
                }
            } else if let Some(sh) = short_handle {
                // HID++ 1.0 fallback on multi-device Unifying slots (0x0D mileage, then 0x07 status)
                let mut r = [0u8; 128];
                for reg in [0x0Du8, 0x07u8] {
                    if let Some(n) = hidpp10_request(sh, ev, dev_idx, 0x81, reg, 0, 120, &mut r) {
                        let parsed = if reg == 0x0D {
                            parse_hidpp10_mileage(&r[..n])
                        } else {
                            parse_hidpp10_status(&r[..n])
                        };
                        if let Some((pct, charging)) = parsed {
                            if let Some((short_ci, short_cand)) = cands.iter().enumerate().find(|(_, c)| {
                                c.vid == 0x046D
                                    && c.pid == cand.pid
                                    && c.usage_page == 0xFF00
                                    && c.usage == 0x0001
                            }) {
                                let ph = path_hash8(short_cand.path.as_slice());
                                let idx = *ep_count as usize;
                                eps[idx] = LogitechEndpoint {
                                    cand_idx: short_ci as u8,
                                    path_hash: ph,
                                    dev_idx,
                                    kind: fallback_kind,
                                    proto: LogiProto::Hidpp10Reg,
                                    batt_feat_id: 0,
                                    batt_feat_idx: reg,
                                    pid: cand.pid,
                                };
                                *ep_count += 1;
                                let sub = ((ph as u16) << 8) | (dev_idx as u16);
                                results.upsert(Battery::with_id(
                                    make_hid_id(1, 0x046D, cand.pid, sub),
                                    vp_u32(0x046D, cand.pid),
                                    fallback_kind,
                                    Some(pct),
                                    charging,
                                    true,
                                    Source::NativeHid,
                                ));
                                break;
                            }
                        }
                    }
                }
            }
        }

        let _ = CloseHandle(handle);
        if let Some(sh) = short_handle {
            let _ = CloseHandle(sh);
        }
    }

    any_open_fail
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hidpp_reply_filtering_ignores_other_sw_ids_and_catches_errors() {
        let ok = [0x11, 0x01, 0x07, HIDPP_SW_ID, 0x0F, 0xA0, 0x00];
        assert_eq!(match_hidpp_reply(&ok, 1, 7, 0), MatchReply::Match);

        let other_sw = [0x11, 0x01, 0x07, 0x01, 0x1E, 0x22, 0x00];
        assert_eq!(match_hidpp_reply(&other_sw, 1, 7, 0), MatchReply::Ignore);

        let wrong_feat = [0x11, 0x01, 0x01, (0x01 << 4) | HIDPP_SW_ID, 0x1E, 0x22, 0x00];
        assert_eq!(match_hidpp_reply(&wrong_feat, 1, 7, 0), MatchReply::Ignore);

        let err20 = [0x11, 0x01, 0xFF, 0x07, HIDPP_SW_ID, 0x02];
        assert_eq!(match_hidpp_reply(&err20, 1, 7, 0), MatchReply::ProtocolError);

        let err10_ours = [0x11, 0x01, 0x8F, 0x07, HIDPP_SW_ID, 0x08];
        assert_eq!(match_hidpp_reply(&err10_ours, 1, 7, 0), MatchReply::ProtocolError);

        let err10_other = [0x11, 0x01, 0x8F, 0x07, 0x01, 0x08];
        assert_eq!(match_hidpp_reply(&err10_other, 1, 7, 0), MatchReply::Ignore);
    }

    #[test]
    fn logitech_battery_parsers() {
        assert_eq!(parse_1004_status(&[0x11, 1, 5, 0x1A, 86, 0x04, 1]), Some((86, true)));
        assert_eq!(parse_1004_status(&[0x11, 1, 5, 0x1A, 100, 0x04, 3]), Some((100, false)));
        assert_eq!(parse_1000_status(&[0x11, 1, 5, 0x0A, 45, 40, 4]), Some((45, true)));
        assert_eq!(parse_1000_status(&[0x11, 1, 5, 0x0A, 45, 40, 0]), Some((45, false)));

        let mv = 4067u16.to_be_bytes();
        assert_eq!(parse_1001_voltage(&[0x11, 1, 7, 0x0A, mv[0], mv[1], 0x80]), Some((90, true)));
        assert_eq!(parse_1001_voltage(&[0x11, 1, 1, 0x10, 0x1E, 0x22, 0x60]), None);

        assert_eq!(parse_1f20_adc(&[0x11, 0xFF, 5, 0x0A, mv[0], mv[1], 0x03]), Some((90, true)));
        assert_eq!(parse_1f20_adc(&[0x11, 0xFF, 5, 0x0A, mv[0], mv[1], 0x01]), Some((90, false)));

        assert_eq!(parse_hidpp10_mileage(&[0x10, 1, 0x81, 0x0D, 65, 0, 0]), Some((65, false)));
        assert_eq!(parse_hidpp10_status(&[0x10, 1, 0x81, 0x07, 7, 0x22, 0]), Some((90, true)));
        assert_eq!(
            parse_gprox2_battery(&[0x51, 0x0B, 0, 0, 0, 0, 0, 0, 0x04, 0, 82, 0, 0x02]),
            Some((82, true))
        );
        assert_eq!(device_type_to_kind(0, Kind::Mouse), Kind::Keyboard);
        assert_eq!(device_type_to_kind(3, Kind::Keyboard), Kind::Mouse);
        assert_eq!(device_type_to_kind(8, Kind::Mouse), Kind::Headset);
    }

    #[test]
    fn path_hash_is_stable_and_undiscovered_slots_detects_multi_receiver() {
        let p1: Vec<u16> = "\\\\?\\HID#VID_046D&PID_C539#1".encode_utf16().collect();
        let p2: Vec<u16> = "\\\\?\\HID#VID_046D&PID_C52B#2".encode_utf16().collect();
        assert_eq!(path_hash8(&p1), path_hash8(&p1));
        assert_ne!(path_hash8(&p1), path_hash8(&p2));

        let mut c1 = RawCandidate::empty();
        c1.vid = 0x046D;
        c1.pid = 0xC539;
        c1.usage_page = 0xFF00;
        c1.usage = 0x0002;
        let mut c2 = RawCandidate::empty();
        c2.vid = 0x046D;
        c2.pid = 0xC52B;
        c2.usage_page = 0xFF00;
        c2.usage = 0x0002;

        let mut eps = [LogitechEndpoint::empty(); LOGI_EP_CAP];
        eps[0].cand_idx = 0;
        eps[0].proto = LogiProto::Hidpp20;
        eps[0].pid = 0xC539;

        // Only single receiver cached -> done
        assert!(!has_undiscovered_slots(&[c1], &eps, 1));
        // Multi-device C52B present -> undiscovered slots remain
        assert!(has_undiscovered_slots(&[c1, c2], &eps, 1));
    }
}
