//! Battery readers for wireless peripherals over raw HID.
//!
//! - Logitech (VID 046D) vendor page 0xFF00: HID++ long reports (0x11),
//!   feature-set enumeration, battery features 0x1004 / 0x1000 / 0x1001.
//! - HyperX Cloud III Wireless (VID 03F0, PID 05B7) vendor page 0xFF13:
//!   62-byte packets, report id 0x66, cmd 0x89 = battery level, 0x8A = charging.

use std::ffi::c_void;

use windows::core::{Error, GUID, HRESULT, PCWSTR, PWSTR};
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW,
    SetupDiGetDeviceInterfaceDetailW, DIGCF_DEVICEINTERFACE, DIGCF_PRESENT,
    SP_DEVICE_INTERFACE_DATA, SP_DEVICE_INTERFACE_DETAIL_DATA_W,
};
use windows::Win32::Devices::HumanInterfaceDevice::{
    HidD_FreePreparsedData, HidD_GetAttributes, HidD_GetPreparsedData, HidD_GetProductString,
    HidD_SetFeature, HidP_GetCaps, HIDD_ATTRIBUTES, HIDP_CAPS, PHIDP_PREPARSED_DATA,
};
use windows::Win32::Foundation::{
    CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE, WAIT_OBJECT_0, ERROR_IO_PENDING,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, WriteFile, FILE_FLAG_OVERLAPPED, FILE_SHARE_READ, FILE_SHARE_WRITE,
    OPEN_EXISTING,
};
use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

const GUID_DEVINTERFACE_HID: GUID = GUID::from_u128(0x4d1e55b2_f16f_11cf_88cb_001111000030);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Mouse,
    Headset,
}

#[derive(Clone, Debug)]
pub struct Battery {
    pub name: String,
    pub kind: Kind,
    pub pct: Option<u8>,
    pub charging: bool,
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
    let mut ov = OVERLAPPED::default();
    ov.hEvent = ev;
    let mut written = 0u32;
    let r = match WriteFile(h, Some(buf), Some(&mut written), Some(&mut ov)) {
        Ok(()) => Ok(()),
        Err(e) if e.code() == ERROR_IO_PENDING.to_hresult() => {
            WaitForSingleObject(ev, timeout_ms);
            Ok(())
        }
        Err(e) => Err(e),
    };
    let _ = CloseHandle(ev);
    r
}

unsafe fn recv_report(h: HANDLE, len: usize, timeout_ms: u32) -> std::result::Result<Vec<u8>, Error> {
    let ev = CreateEventW(None, true, false, PCWSTR::null())?;
    let mut ov = OVERLAPPED::default();
    ov.hEvent = ev;
    let mut buf = vec![0u8; len.max(64)];
    let mut read = 0u32;
    let r = match ReadFile(h, Some(&mut buf), Some(&mut read), Some(&mut ov)) {
        Ok(()) => Ok(buf[..read as usize].to_vec()),
        Err(e) if e.code() == ERROR_IO_PENDING.to_hresult() => {
            if WaitForSingleObject(ev, timeout_ms) == WAIT_OBJECT_0 {
                let mut got = 0u32;
                let _ = GetOverlappedResult(h, &ov, &mut got, false);
                Ok(buf[..got as usize].to_vec())
            } else {
                let _ = CancelIoEx(h, Some(&ov));
                Err(Error::from_hresult(HRESULT(0x800704C7u32 as i32))) // WAIT_TIMEOUT
            }
        }
        Err(e) => Err(e),
    };
    let _ = CloseHandle(ev);
    r
}

unsafe fn transact(h: HANDLE, out: &[u8], in_len: usize, timeout_ms: u32) -> std::result::Result<Vec<u8>, Error> {
    send_report(h, out, timeout_ms)?;
    recv_report(h, in_len, timeout_ms)
}

/// Send a HID++ long (0x11, 20B) message and read the response.
unsafe fn hidpp_send(h: HANDLE, in_len: usize, dev_idx: u8, feat_idx: u8, func: u8, params: &[u8]) -> std::result::Result<Vec<u8>, Error> {
    let mut buf = [0u8; 20];
    buf[0] = 0x11;
    buf[1] = dev_idx;
    buf[2] = feat_idx;
    buf[3] = (func << 4) & 0xF0;
    for (i, p) in params.iter().enumerate() {
        if 4 + i < buf.len() {
            buf[4 + i] = *p;
        }
    }
    transact(h, &buf, in_len, 500)
}

// ------------------------------------------------------- Logitech HID++ ---

/// Returns (level %, charging).
unsafe fn logitech_read(h: HANDLE, in_len: usize) -> Option<(u8, bool)> {
    for dev_idx in 1u8..=4u8 {
        // ping; the mouse may be asleep, so retry once after a short delay
        let pong = 'ping: {
            for _ in 0..3 {
                if let Ok(r) = hidpp_send(h, in_len, dev_idx, 0x00, 0x00, &[]) {
                    if r.len() >= 4 && r[1] == dev_idx {
                        break 'ping Some(r);
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
            None
        };
        let pong = match pong {
            Some(r) => r,
            None => continue,
        };
        let _ = pong;
        // feature set (index 1): func 0 = getCount
        let count = match hidpp_send(h, in_len, dev_idx, 0x01, 0x00, &[]) {
            Ok(r) if r.len() >= 5 => r[4],
            _ => continue,
        };
        // func 1 = getFeature(i) -> id = (resp[4] << 8) | resp[5]
        let mut feats: Vec<(u8, u16)> = Vec::new();
        for i in 0..count.min(40) {
            if let Ok(r) = hidpp_send(h, in_len, dev_idx, 0x01, 0x01, &[i]) {
                if r.len() >= 6 {
                    feats.push((i, ((r[4] as u16) << 8) | r[5] as u16));
                }
            }
        }
        // 0x1004 unified battery: func 0 -> resp[4] = level %
        if let Some(&(idx, _)) = feats.iter().find(|(_, id)| *id == 0x1004) {
            if let Ok(r) = hidpp_send(h, in_len, dev_idx, idx, 0x00, &[]) {
                if r.len() >= 5 {
                    return Some((r[4].min(100), false));
                }
            }
        }
        // 0x1000 legacy battery: func 0 -> resp[4] = level %, resp[5] bit0 = charging
        if let Some(&(idx, _)) = feats.iter().find(|(_, id)| *id == 0x1000) {
            if let Ok(r) = hidpp_send(h, in_len, dev_idx, idx, 0x00, &[]) {
                if r.len() >= 6 {
                    return Some((r[4].min(100), r[5] & 0x01 != 0));
                }
            }
        }
        // 0x1001 voltage: func 0 -> resp[4..6] = mV (big endian)
        if let Some(&(idx, _)) = feats.iter().find(|(_, id)| *id == 0x1001) {
            if let Ok(r) = hidpp_send(h, in_len, dev_idx, idx, 0x00, &[]) {
                if r.len() >= 6 {
                    let mv = ((r[4] as u16) << 8) | r[5] as u16;
                    return Some((voltage_to_pct(mv), false));
                }
            }
        }
    }
    None
}

/// Li-ion curve used by most wireless mice: 4.20 V = 100 %, 3.50 V = 0 %.
fn voltage_to_pct(mv: u16) -> u8 {
    let pct = (mv as i32 - 3500) * 100 / 700;
    pct.clamp(0, 100) as u8
}

// -------------------------------------------------------------- HyperX ---

/// Returns (level %, charging).
unsafe fn hyperx_read(h: HANDLE, out_len: usize, in_len: usize) -> Option<(u8, bool)> {
    let mut level: Option<u8> = None;
    let mut charging = false;

    let cmd = |c: u8| {
        let mut out = vec![0u8; out_len];
        out[0] = 0x66; // report id
        out[1] = c;
        out
    };

    // cmd 0x89 = battery: resp[4] = level, valid if resp[2] || resp[3]
    let out = cmd(0x89);
    let resp = match send_report(h, &out, 400) {
        Ok(()) => recv_report(h, in_len, 600).ok(),
        Err(_) => {
            // some dongles only accept the packet as a feature report
            if HidD_SetFeature(h, out.as_ptr() as *mut c_void, out.len() as u32) {
                recv_report(h, in_len, 600).ok()
            } else {
                None
            }
        }
    };
    if let Some(r) = resp {
        if r.len() >= 5 && (r[2] != 0 || r[3] != 0) {
            level = Some(r[4].min(100));
        }
    }

    // cmd 0x8A = charging state: resp[2] != 0 -> on AC / charging
    let out = cmd(0x8A);
    if send_report(h, &out, 400).is_ok() {
        if let Ok(r) = recv_report(h, in_len, 600) {
            if r.len() >= 3 {
                charging = r[2] != 0;
            }
        }
    }

    level.map(|pct| (pct, charging))
}

// ------------------------------------------------------------- enumerate ---

/// Scan all present HID interfaces and read the battery of every supported device.
pub fn read_all() -> Vec<Battery> {
    let mut result: Vec<Battery> = Vec::new();
    unsafe {
        let hdev = match SetupDiGetClassDevsW(
            Some(&GUID_DEVINTERFACE_HID as *const GUID),
            PCWSTR::null(),
            None,
            DIGCF_DEVICEINTERFACE | DIGCF_PRESENT,
        ) {
            Ok(h) => h,
            Err(_) => return result,
        };
        let mut index: u32 = 0;
        loop {
            let mut ifdata = SP_DEVICE_INTERFACE_DATA {
                cbSize: std::mem::size_of::<SP_DEVICE_INTERFACE_DATA>() as u32,
                ..Default::default()
            };
            if SetupDiEnumDeviceInterfaces(hdev, None, &GUID_DEVINTERFACE_HID as *const GUID, index, &mut ifdata).is_err() {
                break;
            }
            index += 1;

            let mut required: u32 = 0;
            let _ = SetupDiGetDeviceInterfaceDetailW(hdev, &ifdata, None, 0, Some(&mut required), None);
            if required == 0 {
                continue;
            }
            let mut buf = vec![0u8; required as usize];
            let detail = buf.as_mut_ptr() as *mut SP_DEVICE_INTERFACE_DETAIL_DATA_W;
            (*detail).cbSize = std::mem::size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;
            if SetupDiGetDeviceInterfaceDetailW(hdev, &ifdata, Some(detail), required, Some(&mut required), None).is_err() {
                continue;
            }
            let path = PWSTR((*detail).DevicePath.as_ptr() as *mut u16).to_string().unwrap_or_default();

            if let Some(b) = probe_one(&path) {
                // one entry per device kind is enough
                if !result.iter().any(|e| e.kind == b.kind) {
                    #[cfg(debug_assertions)]
                    eprintln!("[hid] {} -> {:?}", b.name, b);
                    result.push(b);
                }
            }
        }
        let _ = SetupDiDestroyDeviceInfoList(hdev);
    }
    result
}

unsafe fn probe_one(path: &str) -> Option<Battery> {
    let handle = open_hid(path)?;

    let mut attr = HIDD_ATTRIBUTES {
        Size: std::mem::size_of::<HIDD_ATTRIBUTES>() as u32,
        ..Default::default()
    };
    HidD_GetAttributes(handle, &mut attr);
    let mut product = [0u16; 256];
    HidD_GetProductString(handle, product.as_mut_ptr() as *mut c_void, (product.len() * 2) as u32);
    let product = PWSTR(product.as_mut_ptr()).to_string().unwrap_or_default();

    let mut caps = HIDP_CAPS::default();
    let mut preparsed = PHIDP_PREPARSED_DATA::default();
    if HidD_GetPreparsedData(handle, &mut preparsed) {
        let _ = HidP_GetCaps(preparsed, &mut caps);
        HidD_FreePreparsedData(preparsed);
    }

    let vid = attr.VendorID;
    let pid = attr.ProductID;

    let b = if vid == 0x046D && caps.UsagePage == 0xFF00 && caps.OutputReportByteLength >= 20 {
        let in_len = caps.InputReportByteLength as usize;
        logitech_read(handle, in_len).map(|(pct, charging)| Battery {
            name: if product.is_empty() || product == "USB Receiver" {
                "Souris Logitech".to_string()
            } else {
                product.clone()
            },
            kind: Kind::Mouse,
            pct: Some(pct),
            charging,
        })
    } else if vid == 0x03F0 && pid == 0x05B7 && caps.UsagePage == 0xFF13 {
        let out_len = if caps.OutputReportByteLength > 0 { caps.OutputReportByteLength as usize } else { 62 };
        let in_len = if caps.InputReportByteLength > 0 { caps.InputReportByteLength as usize } else { 62 };
        hyperx_read(handle, out_len, in_len).map(|(pct, charging)| Battery {
            name: if product.is_empty() { "Casque HyperX".to_string() } else { product.clone() },
            kind: Kind::Headset,
            pct: Some(pct),
            charging,
        })
    } else {
        None
    };

    let _ = CloseHandle(handle);
    b
}
