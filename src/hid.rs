//! Battery readers for wireless peripherals over raw HID (`#![no_std]`, zero heap allocations).
//!
//! - Uses `user32` Raw Input + overlapped `kernel32` `CreateFileW`/`DeviceIoControl` (zero `setupapi.dll`, zero `hid.dll`).
//! - Logitech HID++ 2.0 (VID 046D) vendor page 0xFF00 / usage 0x0002 (20-byte long reports `0x11`):
//!   `sw_id = 0x0A`, Root `0x0000` lookup, `0x0005` `getDeviceType == 3` (Mouse), and `0x1004`/`0x1000`/`0x1001`.
//! - HyperX Cloud III Wireless (VID 03F0, PID 05B7) vendor page 0xFF13:
//!   62-byte packets, report id 0x66, cmd 0x89 = battery (0/0 when powered off), cmd 0x8A = charging.

use core::ffi::c_void;
use core::ptr::{addr_of_mut, null, null_mut};

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
const RAW_LIST_CAP: usize = 128;
const LOGI_CAND_CAP: usize = 4;

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
    pub kind: Kind,
    pub pct: Option<u8>,
    pub charging: bool,
    pub online: bool,
}

impl Battery {
    pub const fn new(kind: Kind, pct: Option<u8>, charging: bool, online: bool) -> Self {
        Self {
            kind,
            pct,
            charging,
            online,
        }
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
    // HID++ 1.0 error frame matching our feature index and sw_id
    if r[2] == 0x8F && r[3] == feat_idx && r[4] == expected_fn_sw {
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

/// 4-byte-aligned UTF-16 device interface path buffer.
///
/// `win32k!NtUserGetRawInputDeviceInfo` probes the output buffer for 4-byte alignment
/// even on `RIDI_DEVICENAME`, failing with `ERROR_NOACCESS` (`998`) when `opt-level = "z"`
/// packs a `#[repr(align(2))]` `[u16; 256]` stack local at `addr % 4 == 2`.
#[derive(Clone, Copy)]
#[repr(align(4))]
pub struct HidPath {
    pub buf: [u16; HID_PATH_CAP],
}

const _: () = assert!(core::mem::align_of::<HidPath>() >= 4);

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
    let h = CreateFileW(
        path,
        GENERIC_READ | GENERIC_WRITE,
        FILE_SHARE_READ | FILE_SHARE_WRITE,
        null(),
        OPEN_EXISTING,
        FILE_FLAG_OVERLAPPED,
        null_mut(),
    );
    if h != INVALID_HANDLE_VALUE {
        Some(h)
    } else {
        None
    }
}

unsafe fn finish_overlapped(h: HANDLE, ev: HANDLE, ov: *mut OVERLAPPED, timeout_ms: u32) -> Option<u32> {
    if GetLastError() != ERROR_IO_PENDING {
        return None;
    }
    if WaitForSingleObject(ev, timeout_ms) == WAIT_OBJECT_0 {
        let mut got = 0u32;
        if GetOverlappedResult(h, ov, &mut got, 0) != 0 {
            Some(got)
        } else {
            None
        }
    } else {
        let _ = CancelIoEx(h, ov);
        let mut dummy = 0u32;
        let _ = GetOverlappedResult(h, ov, &mut dummy, 1);
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
    finish_overlapped(h, ev, &mut ov, timeout_ms).is_some()
}

unsafe fn recv_report(h: HANDLE, ev: HANDLE, timeout_ms: u32, out: &mut [u8; 64]) -> Option<usize> {
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
    finish_overlapped(h, ev, &mut ov, timeout_ms).map(|n| n as usize)
}

unsafe fn hid_ioctl(
    h: HANDLE,
    ev: HANDLE,
    code: u32,
    input: (*const c_void, u32),
    output: (*mut c_void, u32),
    timeout_ms: u32,
) -> Option<u32> {
    if ev.is_null() {
        return None;
    }
    let _ = ResetEvent(ev);
    let mut ov = OVERLAPPED::with_event(ev);
    let mut got = 0u32;
    if DeviceIoControl(
        h,
        code,
        input.0,
        input.1,
        output.0,
        output.1,
        &mut got,
        &mut ov,
    ) != 0
    {
        return Some(got);
    }
    finish_overlapped(h, ev, &mut ov, timeout_ms)
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

/// Check HID++ `0x0005` `func 2` (`getDeviceType`) so multi-device Unifying/Bolt
/// receivers skip keyboards/keypads/remotes (`3 == Mouse`). Falls back to `true`
/// if `0x0005` is unavailable.
unsafe fn hidpp_is_mouse(h: HANDLE, ev: HANDLE, dev_idx: u8) -> bool {
    let Some(name_idx) = hidpp_get_feature_index(h, ev, dev_idx, 0x0005) else {
        return true;
    };
    let mut r = [0u8; 64];
    let Some(n) = hidpp_request(h, ev, (dev_idx, name_idx, 0x02), &[], 250, &mut r) else {
        return true;
    };
    if n >= 5 {
        r[4] == 3
    } else {
        true
    }
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

    if !send_report(h, ev, &out, timeout_ms)
        && hid_ioctl(
            h,
            ev,
            IOCTL_HID_SET_FEATURE,
            (out.as_ptr() as *const c_void, out.len() as u32),
            (null_mut(), 0),
            timeout_ms,
        )
        .is_none()
    {
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
        if n >= 3 && reply_out[0] == 0x66 && reply_out[1] == cmd {
            return Some(n);
        }
    }
}

unsafe fn hyperx_read_handle(h: HANDLE, ev: HANDLE) -> Option<Battery> {
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
    Some(Battery::new(Kind::Headset, pct, charging, online))
}

// -------------------------------------------------------- Cached scanner ----

#[derive(Clone, Copy)]
struct LogitechEndpoint {
    cand_idx: u8,
    dev_idx: u8,
    batt_feat_id: u16,
    batt_feat_idx: u8,
}

#[derive(Clone, Copy)]
struct HyperXEndpoint {
    active: bool,
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
        self.close();
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

    /// Initialize `HidScanner` directly in static `.bss` storage without stack temporaries.
    ///
    /// # Safety
    /// `dst` must point to valid zero-initialized memory for `Self`.
    pub unsafe fn init_in_place(dst: *mut Self) {
        addr_of_mut!((*dst).io_event).write(null_mut());
        addr_of_mut!((*dst).logitech).write(None);
        addr_of_mut!((*dst).hyperx).write(None);
        addr_of_mut!((*dst).candidates_cached).write(false);
        addr_of_mut!((*dst).cached_logi_count).write(0);
        addr_of_mut!((*dst).cached_hyperx).write(None);
    }

    /// Release cached `io_event` handle cleanly in place without moving `Self`.
    pub fn close(&mut self) {
        if !self.io_event.is_null() {
            unsafe {
                let _ = CloseHandle(self.io_event);
            }
            self.io_event = null_mut();
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

        let mut skip_logi_discover = false;
        let mut skip_hyperx_discover = false;

        // 1. Poll cached Logitech endpoint if present
        if let Some(ep) = self.logitech {
            let idx = ep.cand_idx as usize;
            if idx < self.cached_logi_count as usize {
                unsafe {
                    if let Some(h) = open_hid(self.cached_logi[idx].path.buf.as_ptr()) {
                        if let Some((pct, charging)) =
                            hidpp_read_battery(h, ev, ep.dev_idx, ep.batt_feat_id, ep.batt_feat_idx)
                        {
                            results.set(Battery::new(Kind::Mouse, Some(pct), charging, true));
                        }
                        let _ = CloseHandle(h);
                    } else {
                        self.logitech = None;
                        self.candidates_cached = false;
                        skip_logi_discover = true;
                    }
                }
            } else {
                self.logitech = None;
            }
        }

        // 2. Poll cached HyperX endpoint if present
        if let Some(ep) = self.hyperx {
            if ep.active {
                if let Some(ref cand) = self.cached_hyperx {
                    unsafe {
                        if let Some(h) = open_hid(cand.path.buf.as_ptr()) {
                            if let Some(b) = hyperx_read_handle(h, ev) {
                                results.set(b);
                            } else {
                                self.hyperx = None;
                                self.candidates_cached = false;
                                skip_hyperx_discover = true;
                            }
                            let _ = CloseHandle(h);
                        } else {
                            self.hyperx = None;
                            self.candidates_cached = false;
                            skip_hyperx_discover = true;
                        }
                    }
                } else {
                    self.hyperx = None;
                }
            }
        }

        // 3. Discover missing devices without blocking again on a device that just failed
        let need_logi = self.logitech.is_none() && !skip_logi_discover;
        let need_hyperx = self.hyperx.is_none() && !skip_hyperx_discover;
        if need_logi || need_hyperx {
            unsafe { self.discover_missing(ev, need_logi, need_hyperx, &mut results) };
        }

        results
    }

    #[inline(never)]
    unsafe fn refresh_raw_candidates(&mut self) {
        self.cached_logi_count = 0;
        self.cached_hyperx = None;

        let elem_size = core::mem::size_of::<RAWINPUTDEVICELIST>() as u32;
        let mut list = [RAWINPUTDEVICELIST::default(); RAW_LIST_CAP];
        let mut cap = list.len() as u32;
        let got = GetRawInputDeviceList(list.as_mut_ptr(), &mut cap, elem_size);
        if got == u32::MAX {
            return;
        }
        self.candidates_cached = true;

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
            let is_hyperx = vid == 0x03F0 && pid == 0x05B7 && h.us_usage_page == 0xFF13;
            if !is_logi && !is_hyperx {
                continue;
            }

            let mut path = HidPath::empty();
            let mut char_len = HID_PATH_CAP as u32;
            let res = GetRawInputDeviceInfoW(
                item.h_device,
                RIDI_DEVICENAME,
                path.buf.as_mut_ptr() as *mut c_void,
                &mut char_len,
            );
            if res == u32::MAX || res == 0 {
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
        let is_single = |pid: u16| {
            matches!(
                pid,
                0xC539 | 0xC53A | 0xC53F | 0xC541 | 0xC545 | 0xC547
            )
        };
        let n = self.cached_logi_count as usize;
        for i in 1..n {
            let mut j = i;
            while j > 0
                && is_single(self.cached_logi[j].pid)
                && !is_single(self.cached_logi[j - 1].pid)
            {
                self.cached_logi.swap(j - 1, j);
                j -= 1;
            }
        }
    }

    unsafe fn discover_missing(
        &mut self,
        ev: HANDLE,
        need_logi: bool,
        need_hyperx: bool,
        results: &mut BatteryList,
    ) {
        if !self.candidates_cached {
            self.refresh_raw_candidates();
        }

        if need_logi && self.logitech.is_none() {
            let n = self.cached_logi_count as usize;
            for i in 0..n {
                let path_ptr = self.cached_logi[i].path.buf.as_ptr();
                let pid = self.cached_logi[i].pid;
                let Some(handle) = open_hid(path_ptr) else {
                    self.candidates_cached = false;
                    continue;
                };
                let indices = logitech_dev_indices(pid);
                let ping_timeout = if indices.len() == 1 { 250 } else { 120 };

                let mut found = false;
                for &dev_idx in indices {
                    if !hidpp_ping(handle, ev, dev_idx, ping_timeout) {
                        continue;
                    }
                    if indices.len() > 1 && !hidpp_is_mouse(handle, ev, dev_idx) {
                        continue;
                    }
                    let mut found_feat = None;
                    for feat_id in [0x1004u16, 0x1000u16, 0x1001u16] {
                        if let Some(feat_idx) =
                            hidpp_get_feature_index(handle, ev, dev_idx, feat_id)
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
                        self.logitech = Some(LogitechEndpoint {
                            cand_idx: i as u8,
                            dev_idx,
                            batt_feat_id,
                            batt_feat_idx,
                        });
                        results.set(Battery::new(Kind::Mouse, Some(pct), charging, true));
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

        if need_hyperx && self.hyperx.is_none() {
            if let Some(ref cand) = self.cached_hyperx {
                if let Some(handle) = open_hid(cand.path.buf.as_ptr()) {
                    if let Some(b) = hyperx_read_handle(handle, ev) {
                        self.hyperx = Some(HyperXEndpoint { active: true });
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

// -------------------------------------------------- CLI diagnostics (poc) ---

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
        let mut list = [RAWINPUTDEVICELIST::default(); RAW_LIST_CAP];
        let mut cap = list.len() as u32;
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
            let res = GetRawInputDeviceInfoW(
                item.h_device,
                RIDI_DEVICENAME,
                path.buf.as_mut_ptr() as *mut c_void,
                &mut char_len,
            );
            if res != u32::MAX && res != 0 && out_n < out.len() {
                path.buf[HID_PATH_CAP - 1] = 0;
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

/// Read USB HID product string via overlapped `IOCTL_HID_GET_PRODUCT_STRING` (for `poc`).
///
/// # Safety
/// `h` must be an opened HID HANDLE and `ev` a valid manual-reset event HANDLE.
pub unsafe fn probe_product_string_utf16(h: HANDLE, ev: HANDLE, out: &mut [u16; 128]) -> usize {
    let Some(got) = hid_ioctl(
        h,
        ev,
        IOCTL_HID_GET_PRODUCT_STRING,
        (null(), 0),
        (out.as_mut_ptr() as *mut c_void, (out.len() * 2) as u32),
        300,
    ) else {
        return 0;
    };
    let units = (got as usize / 2).min(out.len());
    out[..units]
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(units)
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
        assert_eq!(
            match_hidpp_reply(&err20, 1, 7, 0),
            MatchReply::ProtocolError
        );

        let err10_ours = [0x11, 0x01, 0x8F, 0x07, HIDPP_SW_ID, 0x08];
        assert_eq!(
            match_hidpp_reply(&err10_ours, 1, 7, 0),
            MatchReply::ProtocolError
        );

        // HID++ 1.0 error from G HUB (different sw_id) must be ignored
        let err10_other = [0x11, 0x01, 0x8F, 0x07, 0x01, 0x08];
        assert_eq!(match_hidpp_reply(&err10_other, 1, 7, 0), MatchReply::Ignore);
    }

    #[test]
    fn logitech_battery_parsers() {
        assert_eq!(
            parse_1004_status(&[0x11, 1, 5, 0x1A, 86, 0x04, 1]),
            Some((86, true))
        );
        assert_eq!(
            parse_1004_status(&[0x11, 1, 5, 0x1A, 40, 0x04, 4]),
            Some((40, true))
        );
        assert_eq!(
            parse_1004_status(&[0x11, 1, 5, 0x1A, 100, 0x04, 3]),
            Some((100, false))
        );

        assert_eq!(
            parse_1000_status(&[0x11, 1, 5, 0x0A, 45, 40, 4]),
            Some((45, true))
        );
        assert_eq!(
            parse_1000_status(&[0x11, 1, 5, 0x0A, 45, 40, 0]),
            Some((45, false))
        );

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
        assert_eq!(
            parse_hyperx_battery(&[0x66, 0x89, 0x0F, 0xEB, 0x5D]),
            Some(Some(93))
        );
        assert_eq!(
            parse_hyperx_battery(&[0x66, 0x89, 0x00, 0x00, 0x00]),
            Some(None)
        );
        assert_eq!(
            parse_hyperx_battery(&[0x66, 0x8A, 0x01, 0x00, 0x00]),
            None
        );

        assert_eq!(parse_hyperx_charging(&[0x66, 0x8A, 0x01]), Some(true));
        assert_eq!(parse_hyperx_charging(&[0x66, 0x8A, 0x00]), Some(false));
    }

    #[test]
    fn hid_path_is_4_byte_aligned_for_raw_input() {
        assert!(core::mem::align_of::<HidPath>() >= 4);

        let mut pair = core::hint::black_box((1u16, HidPath::empty()));
        assert_eq!(pair.0, 1);
        let addr = pair.1.buf.as_ptr() as usize;
        assert_eq!(addr & 3, 0, "HidPath must be 4-byte aligned, got {addr:#x}");

        unsafe {
            let elem_size = core::mem::size_of::<RAWINPUTDEVICELIST>() as u32;
            let mut list = [RAWINPUTDEVICELIST::default(); RAW_LIST_CAP];
            let mut cap = list.len() as u32;
            let got = GetRawInputDeviceList(list.as_mut_ptr(), &mut cap, elem_size);
            if got != u32::MAX {
                if let Some(item) = list[..got as usize]
                    .iter()
                    .find(|d| d.dw_type == RIM_TYPEHID)
                {
                    let mut char_len = HID_PATH_CAP as u32;
                    let res = GetRawInputDeviceInfoW(
                        item.h_device,
                        RIDI_DEVICENAME,
                        pair.1.buf.as_mut_ptr() as *mut c_void,
                        &mut char_len,
                    );
                    assert_ne!(
                        res,
                        u32::MAX,
                        "RIDI_DEVICENAME failed (err={})",
                        GetLastError()
                    );
                    assert!(res > 0);
                }
            }
        }
    }
}
