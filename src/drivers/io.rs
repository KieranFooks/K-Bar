//! Shared `#![no_std]` raw HID I/O primitives, Li-ion curve, and 4-byte-aligned path buffer.

use core::ffi::c_void;
use core::ptr::{null, null_mut};

use crate::win32::{
    CancelIoEx, CreateFileW, DeviceIoControl, GetLastError, GetOverlappedResult, ReadFile,
    ResetEvent, WaitForSingleObject, WriteFile, ERROR_INVALID_PARAMETER,
    ERROR_INVALID_USER_BUFFER, ERROR_IO_PENDING, FILE_FLAG_OVERLAPPED, FILE_SHARE_READ,
    FILE_SHARE_WRITE, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
    IOCTL_HID_GET_FEATURE, IOCTL_HID_GET_PRODUCT_STRING, IOCTL_HID_SET_FEATURE, OPEN_EXISTING,
    OVERLAPPED, WAIT_OBJECT_0,
};

pub const HID_PATH_CAP: usize = 256;

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

/// Map battery voltage (mV) to 0..=100% using the non-linear Li-ion curve.
pub fn voltage_to_pct(mv: u16) -> u8 {
    for (i, &threshold) in LOGITECH_VOLTAGES_MV.iter().enumerate() {
        if mv >= threshold {
            return (100 - i) as u8;
        }
    }
    0
}

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

pub const fn vp_u32(vid: u16, pid: u16) -> u32 {
    ((vid as u32) << 16) | (pid as u32)
}

pub const fn make_hid_id(driver_tag: u8, vid: u16, pid: u16, sub_id: u16) -> u64 {
    ((driver_tag as u64) << 48) | ((vid as u64) << 32) | ((pid as u64) << 16) | (sub_id as u64)
}

pub fn strip_zero_report_id(raw: &[u8]) -> &[u8] {
    if raw.first() == Some(&0) {
        &raw[1..]
    } else {
        raw
    }
}

/// Open an HID handle for read/write reports.
///
/// # Safety
/// `path` must point to a valid null-terminated UTF-16 device interface path.
pub unsafe fn open_hid(path: *const u16) -> Option<HANDLE> {
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

/// Open an HID handle for feature reports (falls back to access `0` for locked mouse/keyboard collections).
///
/// # Safety
/// `path` must point to a valid null-terminated UTF-16 device interface path.
pub unsafe fn open_hid_feature(path: *const u16) -> Option<HANDLE> {
    if let Some(h) = open_hid(path) {
        return Some(h);
    }
    let h = CreateFileW(
        path,
        0,
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

/// Write an exact HID output report using overlapped I/O.
///
/// # Safety
/// `h` must be an opened HID HANDLE and `ev` a valid manual-reset event HANDLE.
pub unsafe fn send_report(h: HANDLE, ev: HANDLE, buf: &[u8], timeout_ms: u32) -> bool {
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

/// Write an HID output report padded to the first accepted report length in `sizes`.
///
/// # Safety
/// `h` must be an opened HID HANDLE and `ev` a valid manual-reset event HANDLE.
pub unsafe fn send_report_padded(
    h: HANDLE,
    ev: HANDLE,
    prefix: &[u8],
    sizes: &[usize],
    timeout_ms: u32,
) -> bool {
    if ev.is_null() {
        return false;
    }
    for &sz in sizes {
        if sz < prefix.len() || sz > 96 {
            continue;
        }
        let mut buf = [0u8; 96];
        buf[..prefix.len()].copy_from_slice(prefix);
        let _ = ResetEvent(ev);
        let mut ov = OVERLAPPED::with_event(ev);
        let mut written = 0u32;
        if WriteFile(h, buf.as_ptr(), sz as u32, &mut written, &mut ov) != 0 {
            return true;
        }
        let err = GetLastError();
        if err == ERROR_IO_PENDING {
            return finish_overlapped(h, ev, &mut ov, timeout_ms).is_some();
        }
        if err != ERROR_INVALID_USER_BUFFER && err != ERROR_INVALID_PARAMETER {
            return false;
        }
    }
    false
}

/// Read an HID input report into a 128-byte buffer using overlapped I/O.
///
/// # Safety
/// `h` must be an opened HID HANDLE and `ev` a valid manual-reset event HANDLE.
pub unsafe fn recv_report(h: HANDLE, ev: HANDLE, timeout_ms: u32, out: &mut [u8; 128]) -> Option<usize> {
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

/// Issue an overlapped HID `DeviceIoControl` request.
///
/// # Safety
/// `h` must be an opened HID HANDLE and `ev` a valid manual-reset event HANDLE.
pub unsafe fn hid_ioctl(
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

/// Send an HID Feature Report via `IOCTL_HID_SET_FEATURE`.
///
/// # Safety
/// `h` must be an opened HID HANDLE and `ev` a valid manual-reset event HANDLE.
pub unsafe fn set_feature(h: HANDLE, ev: HANDLE, buf: &[u8], timeout_ms: u32) -> bool {
    hid_ioctl(
        h,
        ev,
        IOCTL_HID_SET_FEATURE,
        (buf.as_ptr() as *const c_void, buf.len() as u32),
        (null_mut(), 0),
        timeout_ms,
    )
    .is_some()
}

/// Read an HID Feature Report via `IOCTL_HID_GET_FEATURE`.
/// Caller must initialize `out[0]` with the target Report ID before calling.
///
/// # Safety
/// `h` must be an opened HID HANDLE and `ev` a valid manual-reset event HANDLE.
pub unsafe fn get_feature(h: HANDLE, ev: HANDLE, out: &mut [u8], timeout_ms: u32) -> Option<usize> {
    hid_ioctl(
        h,
        ev,
        IOCTL_HID_GET_FEATURE,
        (null(), 0),
        (out.as_mut_ptr() as *mut c_void, out.len() as u32),
        timeout_ms,
    )
    .map(|n| n as usize)
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
