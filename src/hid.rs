//! Battery readers for wireless peripherals over raw HID (`#![no_std]`, zero heap allocations).
//!
//! - Uses `user32` Raw Input + `kernel32` `CreateFileW`/`DeviceIoControl` directly (zero `setupapi.dll`, zero `hid.dll`).
//! - Logitech HID++ 2.0 (VID 046D) vendor page 0xFF00 / usage 0x0002 (20-byte long reports `0x11`):
//!   `sw_id = 0x0A`, Root `0x0000` lookup, `0x0005` name, and `0x1004`/`0x1000`/`0x1001` battery.
//! - HyperX Cloud III Wireless (VID 03F0, PID 05B7) vendor page 0xFF13:
//!   62-byte packets, report id 0x66, cmd 0x89 = battery (0/0 when powered off), cmd 0x8A = charging.

use core::ffi::c_void;
use core::ptr::{null, null_mut};

use crate::win32::{
    CancelIoEx, CloseHandle, CreateEventW, CreateFileW, DeviceIoControl, GetLastError,
    GetOverlappedResult, GetRawInputDeviceInfoW, GetRawInputDeviceList, GetTickCount64, ReadFile,
    ResetEvent, WaitForSingleObject, WriteFile, ERROR_IO_PENDING, FILE_FLAG_OVERLAPPED,
    FILE_SHARE_READ, FILE_SHARE_WRITE, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
    IOCTL_HID_GET_PRODUCT_STRING, IOCTL_HID_SET_FEATURE, OPEN_EXISTING, OVERLAPPED,
    RAWINPUTDEVICELIST, RIDI_DEVICEINFO, RIDI_DEVICENAME, RID_DEVICE_INFO, RIM_TYPEHID,
    WAIT_OBJECT_0,
};

/// Non-zero HID++ software ID to isolate our replies from Logitech G HUB
/// traffic and unsolicited device notifications (which use sw_id = 0).
pub const HIDPP_SW_ID: u8 = 0x0A;

const HID_PATH_CAP: usize = 256;
const RAW_LIST_CAP: usize = 64;
const LOGI_CAND_CAP: usize = 2;

/// Non-linear Li-ion discharge table (100% down to 1%, mV) from Linux `hid-logitech-hidpp.c`.
const LOGITECH_VOLTAGES_MV: [u16; 100] = [
    4186, 4156, 4143, 4133, 4122, 4113, 4103, 4094, 4086, 4075, 4067, 4059, 4051, 4043, 4035, 4027,
    4019, 4011, 4003, 3997, 3989, 3983, 3976, 3969, 3961, 3955, 3949, 3942, 3935, 3929, 3922, 3916,
    3909, 3902, 3896, 3890, 3883, 3877, 3870, 3865, 3859, 3853, 3848, 3842, 3837, 3833, 3828, 3824,
    3819, 3815, 3810, 3806, 3802, 3798, 3794, 3790, 3786, 3782, 3778, 3774, 3770, 3766, 3762, 3758,
    3754, 3750, 3746, 3741, 3737, 3732, 3728, 3723, 3718, 3713, 3708, 3702, 3697, 3691, 3685, 3679,
    3673, 3666, 3660, 3653, 3646, 3638, 3631, 3623, 3615, 3606, 3597, 3588, 3578, 3568, 3557, 3545,
    3533, 3520, 3505, 3485,
];

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub enum Kind {
    Mouse,
    Headset,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Battery {
    pub name: [u8; 64],
    pub name_len: u8,
    pub kind: Kind,
    pub pct: Option<u8>,
    pub charging: bool,
    pub online: bool,
}

impl Battery {
    pub const fn new(kind: Kind, pct: Option<u8>, charging: bool, online: bool) -> Self {
        Self {
            name: [0u8; 64],
            name_len: 0,
            kind,
            pct,
            charging,
            online,
        }
    }

    pub fn with_name(
        name: &str,
        kind: Kind,
        pct: Option<u8>,
        charging: bool,
        online: bool,
    ) -> Self {
        let mut b = Self::new(kind, pct, charging, online);
        let bytes = name.as_bytes();
        let n = bytes.len().min(b.name.len());
        b.name[..n].copy_from_slice(&bytes[..n]);
        b.name_len = n as u8;
        b
    }

    pub fn name_str(&self) -> &str {
        core::str::from_utf8(&self.name[..self.name_len as usize]).unwrap_or("")
    }
}

/// Deterministic 2-slot container: index `0` = `Mouse`, index `1` = `Headset`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct BatteryList {
    pub slots: [Option<Battery>; 2],
}

impl Default for BatteryList {
    fn default() -> Self {
        Self::new()
    }
}

impl BatteryList {
    pub const fn new() -> Self {
        Self {
            slots: [None, None],
        }
    }

    pub fn set(&mut self, b: Battery) {
        let idx = match b.kind {
            Kind::Mouse => 0,
            Kind::Headset => 1,
        };
        self.slots[idx] = Some(b);
    }

    pub fn get(&self, kind: Kind) -> Option<&Battery> {
        let idx = match kind {
            Kind::Mouse => 0,
            Kind::Headset => 1,
        };
        self.slots[idx].as_ref()
    }

    pub fn is_empty(&self) -> bool {
        self.slots[0].is_none() && self.slots[1].is_none()
    }

    pub fn count(&self) -> usize {
        usize::from(self.slots[0].is_some()) + usize::from(self.slots[1].is_some())
    }

    pub fn any_charging(&self) -> bool {
        self.slots[0].is_some_and(|b| b.charging) || self.slots[1].is_some_and(|b| b.charging)
    }
}

// --------------------------------------------------------- Pure parsers -----

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MatchReply {
    Match,
    ProtocolError,
    Ignore,
}

/// Match an incoming HID++ 2.0 packet against our outgoing request parameters.
pub fn match_hidpp_reply(r: &[u8], dev_idx: u8, feat_idx: u8, func: u8) -> MatchReply {
    if r.len() < 5 || r[0] != 0x11 || r[1] != dev_idx {
        return MatchReply::Ignore;
    }
    let expected_fn_sw = ((func << 4) & 0xF0) | (HIDPP_SW_ID & 0x0F);
    if r[2] == feat_idx && r[3] == expected_fn_sw {
        return MatchReply::Match;
    }
    if r[2] == 0xFF && r[3] == feat_idx && r[4] == expected_fn_sw {
        return MatchReply::ProtocolError;
    }
    if r[2] == 0x8F {
        return MatchReply::ProtocolError;
    }
    MatchReply::Ignore
}

/// Map battery voltage (mV) to 0..=100% using the non-linear Li-ion curve.
pub fn voltage_to_pct(mv: u16) -> u8 {
    for (i, &threshold) in LOGITECH_VOLTAGES_MV.iter().enumerate() {
        if mv >= threshold {
            return (100 - i) as u8;
        }
    }
    0
}

/// Parse HID++ 2.0 `0x1004` UnifiedBattery `func 1` (get_status) response.
pub fn parse_1004_status(r: &[u8]) -> Option<(u8, bool)> {
    if r.len() < 7 {
        return None;
    }
    Some((r[4].min(100), matches!(r[6], 1 | 2)))
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

// ------------------------------------------------------------ HID helpers ---

#[derive(Clone, Copy)]
pub struct HidPath {
    pub buf: [u16; HID_PATH_CAP],
}

impl HidPath {
    pub const fn empty() -> Self {
        Self {
            buf: [0u16; HID_PATH_CAP],
        }
    }

    pub fn as_slice(&self) -> &[u16] {
        let len = self
            .buf
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(self.buf.len());
        &self.buf[..len]
    }
}

#[derive(Clone, Copy)]
struct CachedCandidate {
    path: HidPath,
    pid: u16,
}

impl CachedCandidate {
    const fn empty() -> Self {
        Self {
            path: HidPath::empty(),
            pid: 0,
        }
    }
}

unsafe fn open_hid(path: *const u16) -> Option<HANDLE> {
    let try_open = |access: u32| {
        CreateFileW(
            path,
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            null(),
            OPEN_EXISTING,
            FILE_FLAG_OVERLAPPED,
            null_mut(),
        )
    };
    let h = try_open(GENERIC_READ | GENERIC_WRITE);
    if h != INVALID_HANDLE_VALUE {
        return Some(h);
    }
    let h0 = try_open(0);
    if h0 != INVALID_HANDLE_VALUE {
        Some(h0)
    } else {
        None
    }
}

unsafe fn send_report(h: HANDLE, ev: HANDLE, buf: &[u8], timeout_ms: u32) -> bool {
    if ev.is_null() {
        return false;
    }
    let _ = ResetEvent(ev);
    let mut ov = OVERLAPPED::with_event(ev);
    let mut written = 0u32;
    if WriteFile(h, buf.as_ptr(), buf.len() as u32, &mut written, &mut ov) != 0 {
        return true;
    }
    if GetLastError() != ERROR_IO_PENDING {
        return false;
    }
    if WaitForSingleObject(ev, timeout_ms) == WAIT_OBJECT_0 {
        let mut got = 0u32;
        GetOverlappedResult(h, &ov, &mut got, 0) != 0
    } else {
        let _ = CancelIoEx(h, &ov);
        let mut dummy = 0u32;
        let _ = GetOverlappedResult(h, &ov, &mut dummy, 1);
        false
    }
}

unsafe fn recv_report(
    h: HANDLE,
    ev: HANDLE,
    timeout_ms: u32,
    out: &mut [u8; 64],
) -> Option<usize> {
    if ev.is_null() {
        return None;
    }
    let _ = ResetEvent(ev);
    out.fill(0);
    let mut ov = OVERLAPPED::with_event(ev);
    let mut read = 0u32;
    if ReadFile(h, out.as_mut_ptr(), out.len() as u32, &mut read, &mut ov) != 0 {
        return Some(read as usize);
    }
    if GetLastError() != ERROR_IO_PENDING {
        return None;
    }
    if WaitForSingleObject(ev, timeout_ms) == WAIT_OBJECT_0 {
        let mut got = 0u32;
        if GetOverlappedResult(h, &ov, &mut got, 0) != 0 {
            Some(got as usize)
        } else {
            None
        }
    } else {
        let _ = CancelIoEx(h, &ov);
        let mut dummy = 0u32;
        let _ = GetOverlappedResult(h, &ov, &mut dummy, 1);
        None
    }
}

fn copy_utf16_into_utf8(wbuf: &[u16], dst: &mut [u8; 64]) -> u8 {
    let end = wbuf.iter().position(|&c| c == 0).unwrap_or(wbuf.len());
    let mut pos = 0usize;
    let mut tmp = [0u8; 4];
    for r in char::decode_utf16(wbuf[..end].iter().copied()) {
        let ch = r.unwrap_or('\u{FFFD}');
        let s = ch.encode_utf8(&mut tmp).as_bytes();
        if pos + s.len() > dst.len() {
            break;
        }
        dst[pos..pos + s.len()].copy_from_slice(s);
        pos += s.len();
    }
    // Trim leading/trailing ASCII whitespace
    while pos > 0 && dst[pos - 1].is_ascii_whitespace() {
        pos -= 1;
    }
    let mut start = 0usize;
    while start < pos && dst[start].is_ascii_whitespace() {
        start += 1;
    }
    if start > 0 {
        let len = pos - start;
        dst.copy_within(start..pos, 0);
        pos = len;
    }
    pos as u8
}

unsafe fn read_product_string(h: HANDLE, dst: &mut [u8; 64]) -> u8 {
    let mut wbuf = [0u16; 128];
    let mut got = 0u32;
    if DeviceIoControl(
        h,
        IOCTL_HID_GET_PRODUCT_STRING,
        null(),
        0,
        wbuf.as_mut_ptr() as *mut c_void,
        (wbuf.len() * 2) as u32,
        &mut got,
        null_mut(),
    ) == 0
    {
        return 0;
    }
    let code_units = (got as usize / 2).min(wbuf.len());
    copy_utf16_into_utf8(&wbuf[..code_units], dst)
}

// ------------------------------------------------------- Logitech HID++ ---

/// Send a HID++ 2.0 long (0x11, 20B) request (`cmd = (dev_idx, feat_idx, func)`)
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
    reply_out: &mut [u8; 64],
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

unsafe fn hidpp_ping(h: HANDLE, ev: HANDLE, dev_idx: u8, timeout_ms: u32) -> bool {
    const MARKER: u8 = 0x5A;
    let mut r = [0u8; 64];
    match hidpp_request(h, ev, (dev_idx, 0x00, 0x01), &[0, 0, MARKER], timeout_ms, &mut r) {
        Some(n) => n >= 7 && r[6] == MARKER,
        None => false,
    }
}

unsafe fn hidpp_get_feature_index(h: HANDLE, ev: HANDLE, dev_idx: u8, feat_id: u16) -> Option<u8> {
    let [hi, lo] = feat_id.to_be_bytes();
    let mut r = [0u8; 64];
    let n = hidpp_request(h, ev, (dev_idx, 0x00, 0x00), &[hi, lo], 250, &mut r)?;
    if n >= 5 && r[4] != 0 {
        Some(r[4])
    } else {
        None
    }
}

unsafe fn hidpp_device_name(h: HANDLE, ev: HANDLE, dev_idx: u8, dst: &mut [u8; 64]) -> u8 {
    let Some(name_idx) = hidpp_get_feature_index(h, ev, dev_idx, 0x0005) else {
        return 0;
    };
    let mut r = [0u8; 64];
    let Some(n) = hidpp_request(h, ev, (dev_idx, name_idx, 0x00), &[], 250, &mut r) else {
        return 0;
    };
    if n < 5 {
        return 0;
    }
    let total_len = (r[4] as usize).min(dst.len());
    if total_len == 0 {
        return 0;
    }
    let mut offset = 0usize;
    while offset < total_len {
        let Some(cn) = hidpp_request(
            h,
            ev,
            (dev_idx, name_idx, 0x01),
            &[offset as u8],
            250,
            &mut r,
        ) else {
            return 0;
        };
        if cn <= 4 {
            break;
        }
        let payload = &r[4..cn];
        let copy_len = payload.len().min(total_len - offset);
        dst[offset..offset + copy_len].copy_from_slice(&payload[..copy_len]);
        offset += copy_len;
    }
    let mut len = offset.min(total_len);
    while len > 0 && (dst[len - 1] == 0 || dst[len - 1].is_ascii_whitespace()) {
        len -= 1;
    }
    len as u8
}

unsafe fn hidpp_read_battery(
    h: HANDLE,
    ev: HANDLE,
    dev_idx: u8,
    feat_id: u16,
    feat_idx: u8,
) -> Option<(u8, bool)> {
    let mut r = [0u8; 64];
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
        _ => None,
    }
}

fn logitech_dev_indices(pid: u16) -> &'static [u8] {
    match pid {
        0xC539 | 0xC53A | 0xC53F | 0xC541 | 0xC545 | 0xC547 => &[1],
        0xC500..=0xC5FF => &[1, 2, 3, 4, 5, 6],
        _ => &[0xFF, 1],
    }
}

unsafe fn set_logitech_fallback_name(h: HANDLE, out: &mut Battery) {
    let plen = read_product_string(h, &mut out.name);
    let ps = core::str::from_utf8(&out.name[..plen as usize]).unwrap_or("");
    if plen == 0 || ps == "USB Receiver" || ps == "LIGHTSPEED Receiver" {
        let fb = b"Souris Logitech";
        out.name[..fb.len()].copy_from_slice(fb);
        out.name_len = fb.len() as u8;
    } else {
        out.name_len = plen;
    }
}

// -------------------------------------------------------------- HyperX ---

unsafe fn hyperx_transact(
    h: HANDLE,
    ev: HANDLE,
    cmd: u8,
    timeout_ms: u32,
    reply_out: &mut [u8; 64],
) -> Option<usize> {
    let mut out = [0u8; 62];
    out[0] = 0x66;
    out[1] = cmd;

    if !send_report(h, ev, &out, timeout_ms) {
        let mut dummy = 0u32;
        if DeviceIoControl(
            h,
            IOCTL_HID_SET_FEATURE,
            out.as_ptr() as *const c_void,
            out.len() as u32,
            null_mut(),
            0,
            &mut dummy,
            null_mut(),
        ) == 0
        {
            return None;
        }
    }

    let deadline = GetTickCount64().wrapping_add(timeout_ms as u64);
    loop {
        let now = GetTickCount64();
        if now >= deadline {
            return None;
        }
        let rem = (deadline - now).clamp(1, timeout_ms as u64) as u32;
        let n = recv_report(h, ev, rem, reply_out)?;
        if n >= 3 && reply_out[0] == 0x66 && reply_out[1] == cmd {
            return Some(n);
        }
    }
}

unsafe fn hyperx_read_handle(
    h: HANDLE,
    ev: HANDLE,
    name: &[u8; 64],
    name_len: u8,
) -> Option<Battery> {
    let mut pkt = [0u8; 64];
    let n = hyperx_transact(h, ev, 0x89, 500, &mut pkt)?;
    let pct = parse_hyperx_battery(&pkt[..n])?;
    let (online, charging) = match pct {
        Some(_) => {
            let mut chg_pkt = [0u8; 64];
            let chg = hyperx_transact(h, ev, 0x8A, 400, &mut chg_pkt)
                .and_then(|cn| parse_hyperx_charging(&chg_pkt[..cn]))
                .unwrap_or(false);
            (true, chg)
        }
        None => (false, false),
    };
    let mut b = Battery::new(Kind::Headset, pct, charging, online);
    b.name = *name;
    b.name_len = name_len;
    Some(b)
}

// -------------------------------------------------------- Cached scanner ----

#[derive(Clone, Copy)]
struct LogitechEndpoint {
    path: HidPath,
    dev_idx: u8,
    name: [u8; 64],
    name_len: u8,
    batt_feat_id: u16,
    batt_feat_idx: u8,
}

#[derive(Clone, Copy)]
struct HyperXEndpoint {
    path: HidPath,
    name: [u8; 64],
    name_len: u8,
}

/// Stateful, allocation-free HID scanner caching active endpoints, sleeping candidates,
/// and a single reusable overlapped I/O event handle.
pub struct HidScanner {
    io_event: HANDLE,
    logitech: Option<LogitechEndpoint>,
    hyperx: Option<HyperXEndpoint>,
    candidates_cached: bool,
    cached_logi_count: u8,
    cached_logi: [CachedCandidate; LOGI_CAND_CAP],
    cached_hyperx: Option<CachedCandidate>,
}

impl Default for HidScanner {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for HidScanner {
    fn drop(&mut self) {
        if !self.io_event.is_null() {
            unsafe {
                let _ = CloseHandle(self.io_event);
            }
            self.io_event = null_mut();
        }
    }
}

impl HidScanner {
    pub const fn new() -> Self {
        Self {
            io_event: null_mut(),
            logitech: None,
            hyperx: None,
            candidates_cached: false,
            cached_logi_count: 0,
            cached_logi: [CachedCandidate::empty(); LOGI_CAND_CAP],
            cached_hyperx: None,
        }
    }

    unsafe fn ensure_event(&mut self) -> HANDLE {
        if self.io_event.is_null() {
            self.io_event = CreateEventW(null(), 1, 0, null());
        }
        self.io_event
    }

    pub fn poll(&mut self, force_rescan: bool) -> BatteryList {
        if force_rescan {
            self.logitech = None;
            self.hyperx = None;
            self.candidates_cached = false;
            self.cached_logi_count = 0;
            self.cached_hyperx = None;
        }

        let mut results = BatteryList::new();
        let ev = unsafe { self.ensure_event() };
        if ev.is_null() {
            return results;
        }

        // 1. Poll cached Logitech endpoint if present
        if let Some(ep) = self.logitech {
            unsafe {
                if let Some(h) = open_hid(ep.path.buf.as_ptr()) {
                    if let Some((pct, charging)) =
                        hidpp_read_battery(h, ev, ep.dev_idx, ep.batt_feat_id, ep.batt_feat_idx)
                    {
                        let mut b = Battery::new(Kind::Mouse, Some(pct), charging, true);
                        b.name = ep.name;
                        b.name_len = ep.name_len;
                        results.set(b);
                    }
                    let _ = CloseHandle(h);
                } else {
                    self.logitech = None;
                }
            }
        }

        // 2. Poll cached HyperX endpoint if present
        if let Some(ep) = self.hyperx {
            unsafe {
                if let Some(h) = open_hid(ep.path.buf.as_ptr()) {
                    if let Some(b) = hyperx_read_handle(h, ev, &ep.name, ep.name_len) {
                        results.set(b);
                    } else {
                        self.hyperx = None;
                    }
                    let _ = CloseHandle(h);
                } else {
                    self.hyperx = None;
                }
            }
        }

        // 3. Discover any missing device kind (enumerate Raw Input only when needed)
        if self.logitech.is_none() || self.hyperx.is_none() {
            unsafe { self.discover_missing(ev, &mut results) };
        }

        results
    }

    unsafe fn refresh_raw_candidates(&mut self) {
        self.cached_logi_count = 0;
        self.cached_hyperx = None;
        self.candidates_cached = true;

        let elem_size = core::mem::size_of::<RAWINPUTDEVICELIST>() as u32;
        let mut count = 0u32;
        if GetRawInputDeviceList(null_mut(), &mut count, elem_size) == u32::MAX || count == 0 {
            return;
        }

        let mut list = [RAWINPUTDEVICELIST::default(); RAW_LIST_CAP];
        let mut cap = list.len().min(count as usize) as u32;
        let got = GetRawInputDeviceList(list.as_mut_ptr(), &mut cap, elem_size);
        if got == u32::MAX {
            return;
        }

        for item in &list[..got as usize] {
            if item.dw_type != RIM_TYPEHID {
                continue;
            }
            let mut info = RID_DEVICE_INFO::default();
            let mut info_sz = core::mem::size_of::<RID_DEVICE_INFO>() as u32;
            if GetRawInputDeviceInfoW(
                item.h_device,
                RIDI_DEVICEINFO,
                &mut info as *mut RID_DEVICE_INFO as *mut c_void,
                &mut info_sz,
            ) == u32::MAX
            {
                continue;
            }
            let h = info.u.hid;
            let vid = h.dw_vendor_id as u16;
            let pid = h.dw_product_id as u16;
            let is_logi = vid == 0x046D && h.us_usage_page == 0xFF00 && h.us_usage == 0x0002;
            let is_hyperx =
                vid == 0x03F0 && pid == 0x05B7 && h.us_usage_page == 0xFF13;
            if !is_logi && !is_hyperx {
                continue;
            }

            let mut path = HidPath::empty();
            let mut char_len = HID_PATH_CAP as u32;
            if GetRawInputDeviceInfoW(
                item.h_device,
                RIDI_DEVICENAME,
                path.buf.as_mut_ptr() as *mut c_void,
                &mut char_len,
            ) == u32::MAX
                || char_len == 0
            {
                continue;
            }
            path.buf[HID_PATH_CAP - 1] = 0;

            if is_logi && (self.cached_logi_count as usize) < LOGI_CAND_CAP {
                self.cached_logi[self.cached_logi_count as usize] = CachedCandidate { path, pid };
                self.cached_logi_count += 1;
            } else if is_hyperx && self.cached_hyperx.is_none() {
                self.cached_hyperx = Some(CachedCandidate { path, pid });
            }
        }

        // Prioritize single-device LIGHTSPEED receivers before multi-device Unifying/Bolt receivers
        if self.cached_logi_count == 2 {
            let is_single = |pid: u16| {
                matches!(
                    pid,
                    0xC539 | 0xC53A | 0xC53F | 0xC541 | 0xC545 | 0xC547
                )
            };
            if !is_single(self.cached_logi[0].pid) && is_single(self.cached_logi[1].pid) {
                self.cached_logi.swap(0, 1);
            }
        }
    }

    unsafe fn discover_missing(&mut self, ev: HANDLE, results: &mut BatteryList) {
        if !self.candidates_cached {
            self.refresh_raw_candidates();
        }

        if self.logitech.is_none() {
            let n = self.cached_logi_count as usize;
            for i in 0..n {
                let cand = self.cached_logi[i];
                let Some(handle) = open_hid(cand.path.buf.as_ptr()) else {
                    self.candidates_cached = false;
                    continue;
                };
                let indices = logitech_dev_indices(cand.pid);
                let ping_timeout = if indices.len() == 1 { 250 } else { 120 };

                let mut found = false;
                for &dev_idx in indices {
                    if !hidpp_ping(handle, ev, dev_idx, ping_timeout) {
                        continue;
                    }
                    let mut found_feat = None;
                    for feat_id in [0x1004u16, 0x1000u16, 0x1001u16] {
                        if let Some(feat_idx) = hidpp_get_feature_index(handle, ev, dev_idx, feat_id)
                        {
                            found_feat = Some((feat_id, feat_idx));
                            break;
                        }
                    }
                    let Some((batt_feat_id, batt_feat_idx)) = found_feat else {
                        continue;
                    };
                    if let Some((pct, charging)) =
                        hidpp_read_battery(handle, ev, dev_idx, batt_feat_id, batt_feat_idx)
                    {
                        let mut b = Battery::new(Kind::Mouse, Some(pct), charging, true);
                        b.name_len = hidpp_device_name(handle, ev, dev_idx, &mut b.name);
                        if b.name_len == 0 {
                            set_logitech_fallback_name(handle, &mut b);
                        }
                        self.logitech = Some(LogitechEndpoint {
                            path: cand.path,
                            dev_idx,
                            name: b.name,
                            name_len: b.name_len,
                            batt_feat_id,
                            batt_feat_idx,
                        });
                        results.set(b);
                        found = true;
                        break;
                    }
                }
                let _ = CloseHandle(handle);
                if found {
                    break;
                }
            }
        }

        if self.hyperx.is_none() {
            if let Some(cand) = self.cached_hyperx {
                if let Some(handle) = open_hid(cand.path.buf.as_ptr()) {
                    let mut name = [0u8; 64];
                    let mut name_len = read_product_string(handle, &mut name);
                    if name_len == 0 {
                        let fb = b"HyperX Cloud III Wireless";
                        name[..fb.len()].copy_from_slice(fb);
                        name_len = fb.len() as u8;
                    }
                    if let Some(b) = hyperx_read_handle(handle, ev, &name, name_len) {
                        self.hyperx = Some(HyperXEndpoint {
                            path: cand.path,
                            name,
                            name_len,
                        });
                        results.set(b);
                    }
                    let _ = CloseHandle(handle);
                } else {
                    self.candidates_cached = false;
                }
            }
        }
    }
}

/// Convenience one-shot scan for CLI tools and tests.
pub fn read_all() -> BatteryList {
    HidScanner::new().poll(true)
}

#[derive(Clone, Copy)]
pub struct RawCandidate {
    pub path: HidPath,
    pub vid: u16,
    pub pid: u16,
    pub usage_page: u16,
    pub usage: u16,
}

impl RawCandidate {
    pub const fn empty() -> Self {
        Self {
            path: HidPath::empty(),
            vid: 0,
            pid: 0,
            usage_page: 0,
            usage: 0,
        }
    }
}

/// Inspect active Raw Input HID candidate paths & PIDs (for diagnostic CLI `poc`).
pub fn probe_raw_candidates() -> ([RawCandidate; 4], usize) {
    let mut out = [RawCandidate::empty(); 4];
    let mut out_n = 0usize;
    unsafe {
        let elem_size = core::mem::size_of::<RAWINPUTDEVICELIST>() as u32;
        let mut count = 0u32;
        if GetRawInputDeviceList(null_mut(), &mut count, elem_size) == u32::MAX || count == 0 {
            return (out, 0);
        }
        let mut list = [RAWINPUTDEVICELIST::default(); RAW_LIST_CAP];
        let mut cap = list.len().min(count as usize) as u32;
        let got = GetRawInputDeviceList(list.as_mut_ptr(), &mut cap, elem_size);
        if got == u32::MAX {
            return (out, 0);
        }
        for item in &list[..got as usize] {
            if item.dw_type != RIM_TYPEHID {
                continue;
            }
            let mut info = RID_DEVICE_INFO::default();
            let mut info_sz = core::mem::size_of::<RID_DEVICE_INFO>() as u32;
            if GetRawInputDeviceInfoW(
                item.h_device,
                RIDI_DEVICEINFO,
                &mut info as *mut RID_DEVICE_INFO as *mut c_void,
                &mut info_sz,
            ) == u32::MAX
            {
                continue;
            }
            let h = info.u.hid;
            let vid = h.dw_vendor_id as u16;
            let pid = h.dw_product_id as u16;
            if !((vid == 0x046D && h.us_usage_page == 0xFF00 && h.us_usage == 0x0002)
                || (vid == 0x03F0 && pid == 0x05B7 && h.us_usage_page == 0xFF13))
            {
                continue;
            }
            let mut path = HidPath::empty();
            let mut char_len = HID_PATH_CAP as u32;
            if GetRawInputDeviceInfoW(
                item.h_device,
                RIDI_DEVICENAME,
                path.buf.as_mut_ptr() as *mut c_void,
                &mut char_len,
            ) != u32::MAX
                && out_n < out.len()
            {
                out[out_n] = RawCandidate {
                    path,
                    vid,
                    pid,
                    usage_page: h.us_usage_page,
                    usage: h.us_usage,
                };
                out_n += 1;
            }
        }
    }
    (out, out_n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn voltage_curve_boundaries() {
        assert_eq!(voltage_to_pct(4200), 100);
        assert_eq!(voltage_to_pct(4186), 100);
        assert_eq!(voltage_to_pct(4156), 99);
        assert_eq!(voltage_to_pct(3815), 51);
        assert_eq!(voltage_to_pct(3485), 1);
        assert_eq!(voltage_to_pct(3400), 0);
    }

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

        let err10 = [0x11, 0x01, 0x8F, 0x00, 0x00, 0x08];
        assert_eq!(match_hidpp_reply(&err10, 1, 0, 1), MatchReply::ProtocolError);
    }

    #[test]
    fn logitech_battery_parsers() {
        assert_eq!(parse_1004_status(&[0x11, 1, 5, 0x1A, 86, 0x04, 1]), Some((86, true)));
        assert_eq!(parse_1004_status(&[0x11, 1, 5, 0x1A, 100, 0x04, 3]), Some((100, false)));

        assert_eq!(parse_1000_status(&[0x11, 1, 5, 0x0A, 45, 40, 4]), Some((45, true)));
        assert_eq!(parse_1000_status(&[0x11, 1, 5, 0x0A, 45, 40, 0]), Some((45, false)));

        let mv = 4067u16.to_be_bytes();
        assert_eq!(
            parse_1001_voltage(&[0x11, 1, 7, 0x0A, mv[0], mv[1], 0x80]),
            Some((90, true))
        );
        assert_eq!(
            parse_1001_voltage(&[0x11, 1, 1, 0x10, 0x1E, 0x22, 0x60]),
            None
        );
    }

    #[test]
    fn hyperx_battery_and_charging_parsers() {
        assert_eq!(parse_hyperx_battery(&[0x66, 0x89, 0x0F, 0xEB, 0x5D]), Some(Some(93)));
        assert_eq!(parse_hyperx_battery(&[0x66, 0x89, 0x00, 0x00, 0x00]), Some(None));
        assert_eq!(parse_hyperx_battery(&[0x66, 0x8A, 0x01, 0x00, 0x00]), None);

        assert_eq!(parse_hyperx_charging(&[0x66, 0x8A, 0x01]), Some(true));
        assert_eq!(parse_hyperx_charging(&[0x66, 0x8A, 0x00]), Some(false));
    }
}
