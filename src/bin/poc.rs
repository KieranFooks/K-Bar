//! POC — discover battery sources for wireless peripherals.
//!
//! 1. WinRT `Windows.Devices.Power.Battery` (turned out to report 0 devices here).
//! 2. Raw HID probe via SetupAPI + HidD_*: enumerate HID interfaces, then
//!    - Logitech (VID 046D) vendor page 0xFF00: HID++ long reports (0x11),
//!      feature-set enumeration + battery features 0x1004 / 0x1000 / 0x1001.
//!    - HyperX Cloud III Wireless (VID 03F0 PID 05B7) vendor page 0xFF13:
//!      62-byte packets with report id 0x66, commands 0x89 (battery) / 0x8A (charging).

use std::ffi::c_void;

use windows::core::{Error, GUID, HRESULT, PCWSTR, PWSTR, Result};
use windows::Devices::Enumeration::DeviceInformation;
use windows::Devices::Power::Battery;
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
    CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE, WAIT_OBJECT_0, ERROR_INVALID_FUNCTION,
    ERROR_IO_PENDING,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, WriteFile, FILE_FLAG_OVERLAPPED, FILE_SHARE_READ, FILE_SHARE_WRITE,
    OPEN_EXISTING,
};
use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

const GUID_DEVINTERFACE_HID: GUID = GUID::from_u128(0x4d1e55b2_f16f_11cf_88cb_001111000030);

fn hex(buf: &[u8]) -> String {
    buf.iter().map(|b| format!("{:02X}", b)).collect::<Vec<_>>().join(" ")
}

// ---------------------------------------------------------------- WinRT ---

fn winrt_batteries() -> Result<()> {
    println!("=== WinRT Windows.Devices.Power.Battery ===");
    let selector = Battery::GetDeviceSelector()?;
    let devices = DeviceInformation::FindAllAsyncAqsFilter(&selector)?.join()?;
    let n = devices.Size()?;
    println!("battery devices found: {}", n);
    for i in 0..n {
        let dev = devices.GetAt(i)?;
        let name = dev.Name()?;
        let id = dev.Id()?;
        match Battery::FromIdAsync(&id)?.join() {
            Ok(batt) => {
                let r = batt.GetReport()?;
                let rem = r.RemainingCapacityInMilliwattHours()?.Value()?;
                let full = r.FullChargeCapacityInMilliwattHours()?.Value()?;
                let status = r.Status()?;
                let pct = if full > 0 {
                    format!("{:.0}%", 100.0 * rem as f64 / full as f64)
                } else {
                    "n/a".to_string()
                };
                println!("  [battery] {} | status={:?} | {}/{} mWh = {} | id={}", name, status, rem, full, pct, id);
            }
            Err(e) => println!("  [battery] {} | FromIdAsync error: {} | id={}", name, e, id),
        }
    }
    Ok(())
}

// ------------------------------------------------------------ HID helpers ---

struct Hid {
    handle: HANDLE,
    caps: HIDP_CAPS,
    vid: u16,
    pid: u16,
    product: String,
    path: String,
}

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

/// Overlapped write of an output report, wait up to `timeout_ms`.
unsafe fn send_report(h: HANDLE, buf: &[u8], timeout_ms: u32) -> std::result::Result<(), Error> {
    let ev = CreateEventW(None, true, false, PCWSTR::null())?;
    let mut ov = OVERLAPPED::default();
    ov.hEvent = ev;
    let mut written = 0u32;
    match WriteFile(h, Some(buf), Some(&mut written), Some(&mut ov)) {
        Ok(()) => Ok(()),
        Err(e) if e.code() == ERROR_IO_PENDING.to_hresult() => {
            WaitForSingleObject(ev, timeout_ms);
            Ok(())
        }
        Err(e) => Err(e),
    }
}

/// Overlapped read of an input report, wait up to `timeout_ms`.
unsafe fn recv_report(h: HANDLE, len: usize, timeout_ms: u32) -> std::result::Result<Vec<u8>, Error> {
    let ev = CreateEventW(None, true, false, PCWSTR::null())?;
    let mut ov = OVERLAPPED::default();
    ov.hEvent = ev;
    let mut buf = vec![0u8; len.max(64)];
    let mut read = 0u32;
    let immediate = match ReadFile(h, Some(&mut buf), Some(&mut read), Some(&mut ov)) {
        Ok(()) => Some(read as usize),
        Err(e) if e.code() == ERROR_IO_PENDING.to_hresult() => None,
        Err(e) => return Err(e),
    };
    if let Some(n) = immediate {
        return Ok(buf[..n].to_vec());
    }
    if WaitForSingleObject(ev, timeout_ms) == WAIT_OBJECT_0 {
        let mut got = 0u32;
        let _ = GetOverlappedResult(h, &ov, &mut got, false);
        Ok(buf[..got as usize].to_vec())
    } else {
        let _ = CancelIoEx(h, Some(&ov));
        Err(Error::from_hresult(HRESULT(0x800704C7u32 as i32))) // WAIT_TIMEOUT
    }
}

/// Write an output report then read the matching input report.
unsafe fn transact(h: HANDLE, out: &[u8], in_len: usize, timeout_ms: u32) -> std::result::Result<Vec<u8>, Error> {
    send_report(h, out, timeout_ms)?;
    recv_report(h, in_len, timeout_ms)
}

// ------------------------------------------------------- Logitech HID++ ---

/// Send a HID++ long (0x11, 20B) or short (0x10, 7B) message.
unsafe fn hidpp_send(h: HANDLE, in_len: usize, dev_idx: u8, feat_idx: u8, func: u8, params: &[u8], long: bool) -> std::result::Result<Vec<u8>, Error> {
    let buf = if long {
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
        buf.to_vec()
    } else {
        let mut buf = [0u8; 7];
        buf[0] = 0x10;
        buf[1] = dev_idx;
        buf[2] = feat_idx;
        buf[3] = (func << 4) & 0xF0;
        for (i, p) in params.iter().enumerate() {
            if 4 + i < buf.len() {
                buf[4 + i] = *p;
            }
        }
        buf.to_vec()
    };
    transact(h, &buf, in_len, 1000)
}

unsafe fn hidpp_probe(h: &Hid) {
    let in_len = h.caps.InputReportByteLength as usize;
    let long = h.caps.OutputReportByteLength >= 20;
    println!("  [hidpp] probing {} (VID={:04X} PID={:04X} out={} {})", h.product, h.vid, h.pid, h.caps.OutputReportByteLength, if long { "long" } else { "short" });

    // HID++ 1.0 short-report register probing (battery regs 0x07/0x08) on short interfaces
    if !long {
        for dev_idx in 1u8..=4 {
            for (sub, addr, p0) in [(0x00u8, 0x00u8, 0x00u8), (0x00, 0xFF, 0x07), (0x00, 0xFF, 0x08), (0x07, 0x00, 0x00), (0x08, 0x00, 0x00)] {
                if let Ok(r) = transact(h.handle, &[0x10, dev_idx, sub, addr, p0, 0, 0], in_len, 800) {
                    println!("    short dev={} sub={:02X} addr={:02X} p0={:02X} -> {}", dev_idx, sub, addr, p0, hex(&r[..r.len().min(7)]));
                }
            }
        }
        return;
    }

    for dev_idx in 1u8..=4 {
        // root (feature index 0), function 0 = ping
        let pong = match hidpp_send(h.handle, in_len, dev_idx, 0x00, 0x00, &[], true) {
            Ok(r) if r.len() >= 4 && r[1] == dev_idx => r,
            _ => continue,
        };
        println!("    dev_idx={} alive, ping resp: {}", dev_idx, hex(&pong[..pong.len().min(20)]));

        // feature set (index 1): func 0 = getCount
        let count = match hidpp_send(h.handle, in_len, dev_idx, 0x01, 0x00, &[], true) {
            Ok(r) if r.len() >= 5 => r[4],
            _ => {
                println!("    dev_idx={} no feature-set response", dev_idx);
                continue;
            }
        };
        println!("    dev_idx={} feature count={}", dev_idx, count);

        // enumerate feature ids: func 1 = getFeature(index)
        let mut feats: Vec<(u8, u16)> = Vec::new();
        for i in 0..count.min(40) {
            if let Ok(r) = hidpp_send(h.handle, in_len, dev_idx, 0x01, 0x01, &[i], true) {
                if r.len() >= 6 {
                    let id = ((r[4] as u16) << 8) | r[5] as u16;
                    feats.push((i, id));
                }
            }
        }
        println!("    features: {:04X?}", feats.iter().map(|(_, id)| format!("{:04X}", id)).collect::<Vec<_>>());

        // unified battery 0x1004: try functions 0..=3, print raw
        if let Some(&(idx, _)) = feats.iter().find(|(_, id)| *id == 0x1004) {
            for func in 0u8..=3 {
                if let Ok(r) = hidpp_send(h.handle, in_len, dev_idx, idx, func, &[], true) {
                    println!("    [0x1004] dev{} func{}: {}", dev_idx, func, hex(&r[..r.len().min(20)]));
                }
            }
        }
        // legacy battery status 0x1000 func 0: resp[4]=level%, resp[5]=status
        if let Some(&(idx, _)) = feats.iter().find(|(_, id)| *id == 0x1000) {
            if let Ok(r) = hidpp_send(h.handle, in_len, dev_idx, idx, 0x00, &[], true) {
                println!("    [0x1000] dev{} level={}% status={} raw: {}", dev_idx, r.get(4).copied().unwrap_or(0), r.get(5).copied().unwrap_or(0), hex(&r[..r.len().min(20)]));
            }
        }
        // battery voltage 0x1001 func 0: resp[4..6]=mV
        if let Some(&(idx, _)) = feats.iter().find(|(_, id)| *id == 0x1001) {
            if let Ok(r) = hidpp_send(h.handle, in_len, dev_idx, idx, 0x00, &[], true) {
                let mv = ((r.get(4).copied().unwrap_or(0) as u16) << 8) | r.get(5).copied().unwrap_or(0) as u16;
                println!("    [0x1001] dev{} voltage={}mV raw: {}", dev_idx, mv, hex(&r[..r.len().min(20)]));
            }
        }
    }
}

// -------------------------------------------------------------- HyperX ---

unsafe fn hyperx_probe(h: &Hid) {
    let out_len = if h.caps.OutputReportByteLength > 0 { h.caps.OutputReportByteLength as usize } else { 62 };
    let in_len = if h.caps.InputReportByteLength > 0 { h.caps.InputReportByteLength as usize } else { 62 };
    println!("  [hyperx] probing {} (out={} in={})", h.product, out_len, in_len);
    for cmd in [0x89u8, 0x8au8] {
        let mut out = vec![0u8; out_len];
        out[0] = 0x66; // report id
        out[1] = cmd;  // 0x89 = battery, 0x8A = charging
        match send_report(h.handle, &out, 1000) {
            Ok(()) => match recv_report(h.handle, in_len, 1500) {
                Ok(r) => println!("    cmd 0x{:02X} resp: {}", cmd, hex(&r[..r.len().min(24)])),
                Err(e) => println!("    cmd 0x{:02X} read error: {}", cmd, e),
            },
            Err(e) if e.code() == ERROR_INVALID_FUNCTION.to_hresult() => {
                // Windows quirk: some dongles only accept the packet as a feature report
                println!("    cmd 0x{:02X} write -> INVALID_FUNCTION, retrying as feature report", cmd);
                if HidD_SetFeature(h.handle, out.as_ptr() as *mut c_void, out.len() as u32) {
                    match recv_report(h.handle, in_len, 1500) {
                        Ok(r) => println!("    cmd 0x{:02X} (feature) resp: {}", cmd, hex(&r[..r.len().min(24)])),
                        Err(e2) => println!("    cmd 0x{:02X} read error: {}", cmd, e2),
                    }
                } else {
                    println!("    cmd 0x{:02X} HidD_SetFeature failed", cmd);
                }
            }
            Err(e) => println!("    cmd 0x{:02X} write error: {}", cmd, e),
        }
    }
}

// ------------------------------------------------------------- enumerate ---

unsafe fn hid_probe() {
    println!("\n=== HID interfaces (SetupAPI) ===");
    let hdev = match SetupDiGetClassDevsW(
        Some(&GUID_DEVINTERFACE_HID as *const GUID),
        PCWSTR::null(),
        None,
        DIGCF_DEVICEINTERFACE | DIGCF_PRESENT,
    ) {
        Ok(h) => h,
        Err(e) => {
            println!("SetupDiGetClassDevsW failed: {}", e);
            return;
        }
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

        probe_one(&path);
    }
    let _ = SetupDiDestroyDeviceInfoList(hdev);
}

unsafe fn probe_one(path: &str) {
    let handle = match open_hid(path) {
        Some(h) => h,
        None => {
            println!("  [hid] cannot open: {}", path);
            return;
        }
    };

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

    println!(
        "  [hid] VID={:04X} PID={:04X} page={:04X} usage={:04X} in={} out={} feat={} | {} | {}",
        attr.VendorID, attr.ProductID, caps.UsagePage, caps.Usage,
        caps.InputReportByteLength, caps.OutputReportByteLength, caps.FeatureReportByteLength,
        product, path
    );

    let hid = Hid { handle, caps, vid: attr.VendorID, pid: attr.ProductID, product, path: path.to_string() };

    if hid.vid == 0x046D && hid.caps.UsagePage == 0xFF00 {
        hidpp_probe(&hid);
    }
    if hid.vid == 0x03F0 && hid.pid == 0x05B7 && hid.caps.UsagePage == 0xFF13 {
        hyperx_probe(&hid);
    }

    let _ = CloseHandle(handle);
}

fn main() {
    if let Err(e) = winrt_batteries() {
        println!("[WinRT] error: {}", e);
    }
    unsafe { hid_probe() };
    println!("\ndone.");
}
