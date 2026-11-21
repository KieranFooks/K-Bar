//! Battery readers for wireless peripherals over raw HID.
//!
//! - Logitech HID++ 2.0 (VID 046D) vendor page 0xFF00, 20-byte long reports (0x11):
//!   direct Root 0x0000 feature lookup, 0x0005 device name, and battery features
//!   0x1004 (UnifiedBattery) / 0x1000 (BatteryStatus) / 0x1001 (BatteryVoltage).
//! - HyperX Cloud III Wireless (VID 03F0, PID 05B7) vendor page 0xFF13:
//!   62-byte packets, report id 0x66, cmd 0x89 = battery (0/0 when powered off),
//!   cmd 0x8A = charging state.

use std::ffi::c_void;
use std::time::{Duration, Instant};

use windows::core::{Error, HRESULT, PCWSTR, PWSTR};
use windows::Win32::Devices::HumanInterfaceDevice::{
    HidD_FreePreparsedData, HidD_GetPreparsedData, HidD_GetProductString, HidD_SetFeature,
    HidP_GetCaps, HIDP_CAPS, PHIDP_PREPARSED_DATA,
};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_IO_PENDING, GENERIC_READ, GENERIC_WRITE, HANDLE, WAIT_OBJECT_0,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, WriteFile, FILE_FLAG_OVERLAPPED, FILE_SHARE_READ, FILE_SHARE_WRITE,
    OPEN_EXISTING,
};
use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::Win32::UI::Input::{
    GetRawInputDeviceInfoW, GetRawInputDeviceList, RAWINPUTDEVICELIST, RIDI_DEVICEINFO,
    RIDI_DEVICENAME, RID_DEVICE_INFO, RIM_TYPEHID,
};

const ERR_WAIT_TIMEOUT: i32 = 0x800704C7u32 as i32;
const ERR_PROTOCOL: i32 = 0x8007001Fu32 as i32;

/// Non-zero HID++ software ID to isolate our replies from Logitech G HUB
/// traffic and unsolicited device notifications (which use sw_id = 0).
pub const HIDPP_SW_ID: u8 = 0x0A;

/// Non-linear Li-ion discharge table (100% down to 1%, mV) from Linux `hid-logitech-hidpp.c`.
const LOGITECH_VOLTAGES_MV: [u16; 100] = [
    4186, 4156, 4143, 4133, 4122, 4113, 4103, 4094, 4086, 4075,
    4067, 4059, 4051, 4043, 4035, 4027, 4019, 4011, 4003, 3997,
    3989, 3983, 3976, 3969, 3961, 3955, 3949, 3942, 3935, 3929,
    3922, 3916, 3909, 3902, 3896, 3890, 3883, 3877, 3870, 3865,
    3859, 3853, 3848, 3842, 3837, 3833, 3828, 3824, 3819, 3815,
    3810, 3806, 3802, 3798, 3794, 3790, 3786, 3782, 3778, 3774,
    3770, 3766, 3762, 3758, 3754, 3750, 3746, 3741, 3737, 3732,
    3728, 3723, 3718, 3713, 3708, 3702, 3697, 3691, 3685, 3679,
    3673, 3666, 3660, 3653, 3646, 3638, 3631, 3623, 3615, 3606,
    3597, 3588, 3578, 3568, 3557, 3545, 3533, 3520, 3505, 3485,
];

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub enum Kind {
    Mouse,
    Headset,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Battery {
    pub name: String,
    pub kind: Kind,
    pub pct: Option<u8>,
    pub charging: bool,
    pub online: bool,
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
    // HID++ 2.0 error frame: [0x11, dev, 0xFF, feat_idx, fn_sw, err_code, ...]
    if r[2] == 0xFF && r[3] == feat_idx && r[4] == expected_fn_sw {
        return MatchReply::ProtocolError;
    }
    // HID++ 1.0 error frame: [0x11, dev, 0x8F, ...]
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
/// `r[4] = state_of_charge (%)`, `r[6] = charging_status` (1=recharging, 2=slow).
pub fn parse_1004_status(r: &[u8]) -> Option<(u8, bool)> {
    if r.len() < 7 {
        return None;
    }
    let pct = r[4].min(100);
    let charging = matches!(r[6], 1 | 2);
    Some((pct, charging))
}

/// Parse HID++ 2.0 `0x1000` BatteryLevelStatus `func 0` response.
/// `r[4] = level %`, `r[6] = status` (1=recharging, 2=almost_full, 4=slow_recharge).
pub fn parse_1000_status(r: &[u8]) -> Option<(u8, bool)> {
    if r.len() < 7 {
        return None;
    }
    let pct = r[4].min(100);
    let charging = matches!(r[6], 1 | 2 | 4);
    Some((pct, charging))
}

/// Parse HID++ 2.0 `0x1001` BatteryVoltage `func 0` response.
/// `r[4..6]` big-endian mV (valid 2500..=5000), `r[6] & 0x80` = recharging bit.
pub fn parse_1001_voltage(r: &[u8]) -> Option<(u8, bool)> {
    if r.len() < 7 {
        return None;
    }
    let mv = u16::from_be_bytes([r[4], r[5]]);
    if !(2500..=5000).contains(&mv) {
        return None;
    }
    let charging = (r[6] & 0x80) != 0;
    Some((voltage_to_pct(mv), charging))
}

/// Parse HyperX Cloud III Wireless `0x89` response.
/// Returns `None` if packet does not match `(0x66, 0x89)`.
/// Returns `Some(None)` if headset is off/unreachable (`r[2] == 0 && r[3] == 0`),
/// or `Some(Some(pct))` when online.
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

unsafe fn open_hid(path: &str) -> Option<HANDLE> {
    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    let open = |access: u32| {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_OVERLAPPED,
            None,
        )
    };
    match open(GENERIC_READ.0 | GENERIC_WRITE.0) {
        Ok(h) => Some(h),
        Err(_) => open(0).ok(),
    }
}

unsafe fn send_report(h: HANDLE, buf: &[u8], timeout_ms: u32) -> std::result::Result<(), Error> {
    let ev = CreateEventW(None, true, false, PCWSTR::null())?;
    let mut ov = OVERLAPPED {
        hEvent: ev,
        ..Default::default()
    };
    let mut written = 0u32;
    let res = match WriteFile(h, Some(buf), Some(&mut written), Some(&mut ov)) {
        Ok(()) => Ok(()),
        Err(e) if e.code() == ERROR_IO_PENDING.to_hresult() => {
            if WaitForSingleObject(ev, timeout_ms) == WAIT_OBJECT_0 {
                let mut got = 0u32;
                GetOverlappedResult(h, &ov, &mut got, false)
            } else {
                let _ = CancelIoEx(h, Some(&ov));
                let mut dummy = 0u32;
                let _ = GetOverlappedResult(h, &ov, &mut dummy, true);
                Err(Error::from_hresult(HRESULT(ERR_WAIT_TIMEOUT)))
            }
        }
        Err(e) => Err(e),
    };
    let _ = CloseHandle(ev);
    res
}

unsafe fn recv_report(h: HANDLE, len: usize, timeout_ms: u32) -> std::result::Result<Vec<u8>, Error> {
    let ev = CreateEventW(None, true, false, PCWSTR::null())?;
    let mut ov = OVERLAPPED {
        hEvent: ev,
        ..Default::default()
    };
    let mut buf = vec![0u8; len.max(64)];
    let mut read = 0u32;
    let res = match ReadFile(h, Some(&mut buf), Some(&mut read), Some(&mut ov)) {
        Ok(()) => Ok(buf[..read as usize].to_vec()),
        Err(e) if e.code() == ERROR_IO_PENDING.to_hresult() => {
            if WaitForSingleObject(ev, timeout_ms) == WAIT_OBJECT_0 {
                let mut got = 0u32;
                GetOverlappedResult(h, &ov, &mut got, false)?;
                Ok(buf[..got as usize].to_vec())
            } else {
                let _ = CancelIoEx(h, Some(&ov));
                let mut dummy = 0u32;
                let _ = GetOverlappedResult(h, &ov, &mut dummy, true);
                Err(Error::from_hresult(HRESULT(ERR_WAIT_TIMEOUT)))
            }
        }
        Err(e) => Err(e),
    };
    let _ = CloseHandle(ev);
    res
}

// ------------------------------------------------------- Logitech HID++ ---

/// Send a HID++ 2.0 long (0x11, 20B) request and discard unrelated packets until deadline.
///
/// # Safety
/// `h` must be a valid opened HID device `HANDLE` supporting overlapped I/O.
pub unsafe fn hidpp_request(
    h: HANDLE,
    in_len: usize,
    dev_idx: u8,
    feat_idx: u8,
    func: u8,
    params: &[u8],
    timeout_ms: u32,
) -> std::result::Result<Vec<u8>, Error> {
    let mut buf = [0u8; 20];
    buf[0] = 0x11;
    buf[1] = dev_idx;
    buf[2] = feat_idx;
    buf[3] = ((func << 4) & 0xF0) | (HIDPP_SW_ID & 0x0F);
    for (i, &p) in params.iter().enumerate() {
        if 4 + i < buf.len() {
            buf[4 + i] = p;
        }
    }
    send_report(h, &buf, timeout_ms)?;

    let deadline = Instant::now() + Duration::from_millis(timeout_ms as u64);
    while Instant::now() < deadline {
        let rem_ms = deadline
            .saturating_duration_since(Instant::now())
            .as_millis()
            .clamp(1, timeout_ms as u128) as u32;
        let r = recv_report(h, in_len, rem_ms)?;
        match match_hidpp_reply(&r, dev_idx, feat_idx, func) {
            MatchReply::Match => return Ok(r),
            MatchReply::ProtocolError => return Err(Error::from_hresult(HRESULT(ERR_PROTOCOL))),
            MatchReply::Ignore => continue,
        }
    }
    Err(Error::from_hresult(HRESULT(ERR_WAIT_TIMEOUT)))
}

unsafe fn hidpp_ping(h: HANDLE, in_len: usize, dev_idx: u8, timeout_ms: u32) -> bool {
    const MARKER: u8 = 0x5A;
    match hidpp_request(h, in_len, dev_idx, 0x00, 0x01, &[0, 0, MARKER], timeout_ms) {
        Ok(r) => r.len() >= 7 && r[6] == MARKER,
        Err(_) => false,
    }
}

unsafe fn hidpp_get_feature_index(h: HANDLE, in_len: usize, dev_idx: u8, feat_id: u16) -> Option<u8> {
    let [hi, lo] = feat_id.to_be_bytes();
    let r = hidpp_request(h, in_len, dev_idx, 0x00, 0x00, &[hi, lo], 250).ok()?;
    if r.len() >= 5 && r[4] != 0 {
        Some(r[4])
    } else {
        None
    }
}

unsafe fn hidpp_device_name(h: HANDLE, in_len: usize, dev_idx: u8) -> Option<String> {
    let name_idx = hidpp_get_feature_index(h, in_len, dev_idx, 0x0005)?;
    let r = hidpp_request(h, in_len, dev_idx, name_idx, 0x00, &[], 250).ok()?;
    let total_len = (*r.get(4)? as usize).min(64);
    if total_len == 0 {
        return None;
    }
    let mut raw = Vec::with_capacity(total_len);
    let mut offset = 0usize;
    while offset < total_len {
        let chunk = hidpp_request(h, in_len, dev_idx, name_idx, 0x01, &[offset as u8], 250).ok()?;
        let payload = chunk.get(4..)?;
        if payload.is_empty() {
            break;
        }
        raw.extend_from_slice(payload);
        offset += payload.len();
    }
    raw.truncate(total_len);
    while raw.last() == Some(&0) {
        raw.pop();
    }
    let s = String::from_utf8_lossy(&raw).trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

unsafe fn hidpp_read_battery(
    h: HANDLE,
    in_len: usize,
    dev_idx: u8,
    feat_id: u16,
    feat_idx: u8,
) -> Option<(u8, bool)> {
    match feat_id {
        0x1004 => {
            let r = hidpp_request(h, in_len, dev_idx, feat_idx, 0x01, &[], 250).ok()?;
            parse_1004_status(&r)
        }
        0x1000 => {
            let r = hidpp_request(h, in_len, dev_idx, feat_idx, 0x00, &[], 250).ok()?;
            parse_1000_status(&r)
        }
        0x1001 => {
            let r = hidpp_request(h, in_len, dev_idx, feat_idx, 0x00, &[], 250).ok()?;
            parse_1001_voltage(&r)
        }
        _ => None,
    }
}

fn logitech_dev_indices(pid: u16) -> &'static [u8] {
    match pid {
        // Single-device LIGHTSPEED receivers
        0xC539 | 0xC53A | 0xC53F | 0xC541 | 0xC545 | 0xC547 => &[1],
        // Multi-device receivers (Unifying / Logi Bolt / combo)
        0xC500..=0xC5FF => &[1, 2, 3, 4, 5, 6],
        // Direct wired USB HID++ device
        _ => &[0xFF, 1],
    }
}

fn clean_logitech_name(dev_name: Option<String>, product: &str) -> String {
    if let Some(n) = dev_name {
        return n;
    }
    let p = product.trim();
    if p.is_empty() || p == "USB Receiver" || p == "LIGHTSPEED Receiver" {
        "Souris Logitech".to_string()
    } else {
        p.to_string()
    }
}

// -------------------------------------------------------------- HyperX ---

unsafe fn hyperx_transact(
    h: HANDLE,
    out_len: usize,
    in_len: usize,
    cmd: u8,
    timeout_ms: u32,
) -> Option<Vec<u8>> {
    let mut out = vec![0u8; out_len];
    out[0] = 0x66;
    out[1] = cmd;

    if send_report(h, &out, timeout_ms).is_err()
        && !HidD_SetFeature(h, out.as_ptr() as *mut c_void, out.len() as u32)
    {
        return None;
    }

    let deadline = Instant::now() + Duration::from_millis(timeout_ms as u64);
    while Instant::now() < deadline {
        let rem_ms = deadline
            .saturating_duration_since(Instant::now())
            .as_millis()
            .clamp(1, timeout_ms as u128) as u32;
        let r = recv_report(h, in_len, rem_ms).ok()?;
        if r.len() >= 3 && r[0] == 0x66 && r[1] == cmd {
            return Some(r);
        }
    }
    None
}

unsafe fn hyperx_read_endpoint(ep: &HyperXEndpoint) -> Option<Battery> {
    let h = open_hid(&ep.path)?;
    let res = hyperx_read_handle(h, ep.out_len, ep.in_len, &ep.name);
    let _ = CloseHandle(h);
    res
}

unsafe fn hyperx_read_handle(
    h: HANDLE,
    out_len: usize,
    in_len: usize,
    name: &str,
) -> Option<Battery> {
    let batt_pkt = hyperx_transact(h, out_len, in_len, 0x89, 500)?;
    let pct = parse_hyperx_battery(&batt_pkt)?;
    let (online, charging) = match pct {
        Some(_) => {
            let chg = hyperx_transact(h, out_len, in_len, 0x8A, 400)
                .and_then(|r| parse_hyperx_charging(&r))
                .unwrap_or(false);
            (true, chg)
        }
        None => (false, false),
    };
    Some(Battery {
        name: name.to_string(),
        kind: Kind::Headset,
        pct,
        charging,
        online,
    })
}

// -------------------------------------------------------- Cached scanner ----

#[derive(Clone, Debug)]
struct LogitechEndpoint {
    path: String,
    dev_idx: u8,
    name: String,
    batt_feat_id: u16,
    batt_feat_idx: u8,
    in_len: usize,
}

#[derive(Clone, Debug)]
struct HyperXEndpoint {
    path: String,
    name: String,
    out_len: usize,
    in_len: usize,
}

#[derive(Clone, Debug)]
pub struct RawHidDevice {
    pub path: String,
    pub vid: u16,
    pub pid: u16,
    pub usage_page: u16,
    pub usage: u16,
}

/// Enumerate present Raw Input HID interfaces via `user32.dll` (avoids loading `setupapi.dll`).
pub fn enumerate_raw_hid() -> Vec<RawHidDevice> {
    unsafe {
        let elem_size = std::mem::size_of::<RAWINPUTDEVICELIST>() as u32;
        let mut count = 0u32;
        if GetRawInputDeviceList(None, &mut count, elem_size) == u32::MAX || count == 0 {
            return Vec::new();
        }

        let mut list = vec![RAWINPUTDEVICELIST::default(); count as usize];
        let got = GetRawInputDeviceList(Some(list.as_mut_ptr()), &mut count, elem_size);
        if got == u32::MAX {
            return Vec::new();
        }
        list.truncate(got as usize);

        let mut out = Vec::new();
        for item in list {
            if item.dwType != RIM_TYPEHID {
                continue;
            }

            let mut info = RID_DEVICE_INFO {
                cbSize: std::mem::size_of::<RID_DEVICE_INFO>() as u32,
                ..Default::default()
            };
            let mut info_sz = info.cbSize;
            if GetRawInputDeviceInfoW(
                Some(item.hDevice),
                RIDI_DEVICEINFO,
                Some(&mut info as *mut RID_DEVICE_INFO as *mut c_void),
                &mut info_sz,
            ) == u32::MAX
            {
                continue;
            }
            let hid_info = info.Anonymous.hid;

            let mut char_len = 0u32;
            if GetRawInputDeviceInfoW(Some(item.hDevice), RIDI_DEVICENAME, None, &mut char_len)
                == u32::MAX
                || char_len == 0
            {
                continue;
            }
            let mut wbuf = vec![0u16; char_len as usize];
            if GetRawInputDeviceInfoW(
                Some(item.hDevice),
                RIDI_DEVICENAME,
                Some(wbuf.as_mut_ptr() as *mut c_void),
                &mut char_len,
            ) == u32::MAX
            {
                continue;
            }
            let end = wbuf.iter().position(|&c| c == 0).unwrap_or(wbuf.len());
            let path = String::from_utf16_lossy(&wbuf[..end]);
            if path.is_empty() {
                continue;
            }

            out.push(RawHidDevice {
                path,
                vid: hid_info.dwVendorId as u16,
                pid: hid_info.dwProductId as u16,
                usage_page: hid_info.usUsagePage,
                usage: hid_info.usUsage,
            });
        }
        out
    }
}

/// Stateful HID battery scanner that caches discovered device interfaces and
/// feature indices so routine polls only exchange 1-3 packets (~15 ms total).
#[derive(Default)]
pub struct HidScanner {
    logitech: Option<LogitechEndpoint>,
    hyperx: Option<HyperXEndpoint>,
}

impl HidScanner {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn poll(&mut self, force_rescan: bool) -> Vec<Battery> {
        if force_rescan {
            self.logitech = None;
            self.hyperx = None;
        }

        let mut results: Vec<Battery> = Vec::new();

        // 1. Poll cached Logitech endpoint if present
        if let Some(ep) = self.logitech.clone() {
            unsafe {
                if let Some(h) = open_hid(&ep.path) {
                    if let Some((pct, charging)) =
                        hidpp_read_battery(h, ep.in_len, ep.dev_idx, ep.batt_feat_id, ep.batt_feat_idx)
                    {
                        results.push(Battery {
                            name: ep.name,
                            kind: Kind::Mouse,
                            pct: Some(pct),
                            charging,
                            online: true,
                        });
                    }
                    // If the receiver opened fine but the mouse didn't answer, it is asleep:
                    // keep `self.logitech` cached and let UI preserve its last reading.
                    let _ = CloseHandle(h);
                } else {
                    self.logitech = None;
                }
            }
        }

        // 2. Poll cached HyperX endpoint if present
        if let Some(ep) = self.hyperx.clone() {
            unsafe {
                if let Some(b) = hyperx_read_endpoint(&ep) {
                    results.push(b);
                } else {
                    self.hyperx = None;
                }
            }
        }

        // 3. Discover any missing device kind via Raw Input
        if self.logitech.is_none() || self.hyperx.is_none() {
            unsafe { self.discover_missing(&mut results) };
        }

        results.sort_by_key(|b| b.kind);
        results
    }

    unsafe fn discover_missing(&mut self, results: &mut Vec<Battery>) {
        let mut candidates = enumerate_raw_hid();
        candidates.retain(|d| {
            (self.logitech.is_none() && d.vid == 0x046D && d.usage_page == 0xFF00)
                || (self.hyperx.is_none()
                    && d.vid == 0x03F0
                    && d.pid == 0x05B7
                    && d.usage_page == 0xFF13)
        });
        // Probe single-device LIGHTSPEED receivers & HID++ 2.0 (usage 0x0002) first.
        candidates.sort_by_key(|d| {
            let is_single_lightspeed = matches!(
                d.pid,
                0xC539 | 0xC53A | 0xC53F | 0xC541 | 0xC545 | 0xC547
            );
            (
                u8::from(!is_single_lightspeed),
                u8::from(d.usage != 0x0002),
            )
        });

        for dev in candidates {
            if self.logitech.is_some() && self.hyperx.is_some() {
                break;
            }

            let Some(handle) = open_hid(&dev.path) else {
                continue;
            };

            let mut caps = HIDP_CAPS::default();
            let mut preparsed = PHIDP_PREPARSED_DATA::default();
            if HidD_GetPreparsedData(handle, &mut preparsed) {
                let _ = HidP_GetCaps(preparsed, &mut caps);
                HidD_FreePreparsedData(preparsed);
            }

            if self.logitech.is_none()
                && dev.vid == 0x046D
                && dev.usage_page == 0xFF00
                && caps.OutputReportByteLength == 20
            {
                let in_len = caps.InputReportByteLength as usize;
                let product = read_product_string(handle);
                let indices = logitech_dev_indices(dev.pid);
                let ping_timeout = if indices.len() == 1 { 250 } else { 120 };

                for &dev_idx in indices {
                    if !hidpp_ping(handle, in_len, dev_idx, ping_timeout) {
                        continue;
                    }
                    let mut found_feat = None;
                    for feat_id in [0x1004u16, 0x1000u16, 0x1001u16] {
                        if let Some(feat_idx) = hidpp_get_feature_index(handle, in_len, dev_idx, feat_id)
                        {
                            found_feat = Some((feat_id, feat_idx));
                            break;
                        }
                    }
                    let Some((batt_feat_id, batt_feat_idx)) = found_feat else {
                        continue;
                    };
                    let name = clean_logitech_name(
                        hidpp_device_name(handle, in_len, dev_idx),
                        &product,
                    );
                    let ep = LogitechEndpoint {
                        path: dev.path.clone(),
                        dev_idx,
                        name: name.clone(),
                        batt_feat_id,
                        batt_feat_idx,
                        in_len,
                    };
                    if let Some((pct, charging)) =
                        hidpp_read_battery(handle, in_len, dev_idx, batt_feat_id, batt_feat_idx)
                    {
                        self.logitech = Some(ep);
                        results.push(Battery {
                            name,
                            kind: Kind::Mouse,
                            pct: Some(pct),
                            charging,
                            online: true,
                        });
                        break;
                    }
                }
            } else if self.hyperx.is_none()
                && dev.vid == 0x03F0
                && dev.pid == 0x05B7
                && dev.usage_page == 0xFF13
            {
                let out_len = if caps.OutputReportByteLength > 0 {
                    caps.OutputReportByteLength as usize
                } else {
                    62
                };
                let in_len = if caps.InputReportByteLength > 0 {
                    caps.InputReportByteLength as usize
                } else {
                    62
                };
                let product = read_product_string(handle);
                let name = if product.is_empty() {
                    "HyperX Cloud III Wireless".to_string()
                } else {
                    product
                };
                if let Some(b) = hyperx_read_handle(handle, out_len, in_len, &name) {
                    self.hyperx = Some(HyperXEndpoint {
                        path: dev.path,
                        name,
                        out_len,
                        in_len,
                    });
                    results.push(b);
                }
            }

            let _ = CloseHandle(handle);
        }
    }
}

unsafe fn read_product_string(handle: HANDLE) -> String {
    let mut product = [0u16; 256];
    let _ = HidD_GetProductString(
        handle,
        product.as_mut_ptr() as *mut c_void,
        (product.len() * 2) as u32,
    );
    PWSTR(product.as_mut_ptr())
        .to_string()
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// Convenience one-shot scan for CLI tools and tests.
pub fn read_all() -> Vec<Battery> {
    HidScanner::new().poll(true)
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
        // Matching packet with sw_id = 0x0A
        let ok = [0x11, 0x01, 0x07, HIDPP_SW_ID, 0x0F, 0xA0, 0x00];
        assert_eq!(match_hidpp_reply(&ok, 1, 7, 0), MatchReply::Match);

        // Stale getFeature reply (feat 1 func 1) or G HUB traffic with different sw_id
        let other_sw = [0x11, 0x01, 0x07, 0x01, 0x1E, 0x22, 0x00];
        assert_eq!(match_hidpp_reply(&other_sw, 1, 7, 0), MatchReply::Ignore);

        let wrong_feat = [0x11, 0x01, 0x01, (0x01 << 4) | HIDPP_SW_ID, 0x1E, 0x22, 0x00];
        assert_eq!(match_hidpp_reply(&wrong_feat, 1, 7, 0), MatchReply::Ignore);

        // HID++ 2.0 error frame targeting our request
        let err20 = [0x11, 0x01, 0xFF, 0x07, HIDPP_SW_ID, 0x02];
        assert_eq!(match_hidpp_reply(&err20, 1, 7, 0), MatchReply::ProtocolError);

        // HID++ 1.0 error frame
        let err10 = [0x11, 0x01, 0x8F, 0x00, 0x00, 0x08];
        assert_eq!(match_hidpp_reply(&err10, 1, 0, 1), MatchReply::ProtocolError);
    }

    #[test]
    fn logitech_battery_parsers() {
        // 0x1004: state_of_charge=86, charging=1 -> (86, true)
        assert_eq!(parse_1004_status(&[0x11, 1, 5, 0x1A, 86, 0x04, 1]), Some((86, true)));
        assert_eq!(parse_1004_status(&[0x11, 1, 5, 0x1A, 100, 0x04, 3]), Some((100, false)));

        // 0x1000: level=45, next=40, status=4 (slow recharge) -> (45, true)
        assert_eq!(parse_1000_status(&[0x11, 1, 5, 0x0A, 45, 40, 4]), Some((45, true)));
        assert_eq!(parse_1000_status(&[0x11, 1, 5, 0x0A, 45, 40, 0]), Some((45, false)));

        // 0x1001: 4067 mV = 90%, flags=0x80 -> charging
        let mv = 4067u16.to_be_bytes();
        assert_eq!(
            parse_1001_voltage(&[0x11, 1, 7, 0x0A, mv[0], mv[1], 0x80]),
            Some((90, true))
        );
        // Reject corrupted 7714 mV packet (like the old getFeature desync 0x1E 0x22)
        assert_eq!(
            parse_1001_voltage(&[0x11, 1, 1, 0x10, 0x1E, 0x22, 0x60]),
            None
        );
    }

    #[test]
    fn hyperx_battery_and_charging_parsers() {
        // Valid online battery = 93% (0x5D)
        assert_eq!(parse_hyperx_battery(&[0x66, 0x89, 0x0F, 0xEB, 0x5D]), Some(Some(93)));
        // Powered off (r[2] == 0 && r[3] == 0)
        assert_eq!(parse_hyperx_battery(&[0x66, 0x89, 0x00, 0x00, 0x00]), Some(None));
        // Unrelated packet ignored
        assert_eq!(parse_hyperx_battery(&[0x66, 0x8A, 0x01, 0x00, 0x00]), None);

        assert_eq!(parse_hyperx_charging(&[0x66, 0x8A, 0x01]), Some(true));
        assert_eq!(parse_hyperx_charging(&[0x66, 0x8A, 0x00]), Some(false));
    }
}
