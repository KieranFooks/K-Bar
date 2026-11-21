//! Live hardware diagnostic probe for battery-tray.
//!
//! Queries:
//! 1. WinRT `Windows.Devices.Power.Battery`
//! 2. Raw HID interfaces using `battery_tray::hid` (with proper SW_ID filtering)

use std::ffi::c_void;

use battery_tray::hid::{
    self, hidpp_request, parse_1000_status, parse_1001_voltage, parse_1004_status,
    parse_hyperx_battery, parse_hyperx_charging,
};
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
    CloseHandle, ERROR_IO_PENDING, GENERIC_READ, GENERIC_WRITE, HANDLE, WAIT_OBJECT_0,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, WriteFile, FILE_FLAG_OVERLAPPED, FILE_SHARE_READ, FILE_SHARE_WRITE,
    OPEN_EXISTING,
};
use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

const GUID_DEVINTERFACE_HID: GUID = GUID::from_u128(0x4d1e55b2_f16f_11cf_88cb_001111000030);

fn hex(buf: &[u8]) -> String {
    buf.iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn winrt_batteries() -> Result<()> {
    println!("=== WinRT Windows.Devices.Power.Battery ===");
    let selector = Battery::GetDeviceSelector()?;
    let devices = DeviceInformation::FindAllAsyncAqsFilter(&selector)?.join()?;
    let n = devices.Size()?;
    println!("battery devices found: {n}");
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
                println!("  [battery] {name} | status={status:?} | {rem}/{full} mWh | id={id}");
            }
            Err(e) => println!("  [battery] {name} | error: {e} | id={id}"),
        }
    }
    Ok(())
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
                Err(Error::from_hresult(HRESULT(0x800704C7u32 as i32)))
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
                Err(Error::from_hresult(HRESULT(0x800704C7u32 as i32)))
            }
        }
        Err(e) => Err(e),
    };
    let _ = CloseHandle(ev);
    res
}

unsafe fn probe_logitech(h: HANDLE, in_len: usize, pid: u16, product: &str) {
    println!("  [hidpp] probing {product} (PID={pid:04X})");
    for dev_idx in 1u8..=6u8 {
        if pid == 0xC539 && dev_idx > 1 {
            break;
        }
        let Ok(pong) = hidpp_request(h, in_len, dev_idx, 0x00, 0x01, &[0, 0, 0x5A], 250) else {
            continue;
        };
        if pong.get(6).copied() != Some(0x5A) {
            continue;
        }
        println!(
            "    dev_idx={dev_idx} HID++ {}.{} ping={}",
            pong[4],
            pong[5],
            hex(&pong[..pong.len().min(8)])
        );

        for (feat_id, label) in [
            (0x0005u16, "DeviceName"),
            (0x1004u16, "UnifiedBattery"),
            (0x1000u16, "BatteryStatus"),
            (0x1001u16, "BatteryVoltage"),
        ] {
            if let Ok(fr) = hidpp_request(
                h,
                in_len,
                dev_idx,
                0x00,
                0x00,
                &feat_id.to_be_bytes(),
                250,
            ) {
                if fr.len() >= 5 && fr[4] != 0 {
                    let fidx = fr[4];
                    println!("    feature {feat_id:04X} ({label}) -> idx={fidx}");
                    match feat_id {
                        0x1004 => {
                            if let Ok(r) = hidpp_request(h, in_len, dev_idx, fidx, 0x01, &[], 250) {
                                println!(
                                    "      [0x1004] raw={} -> {:?}",
                                    hex(&r[..r.len().min(12)]),
                                    parse_1004_status(&r)
                                );
                            }
                        }
                        0x1000 => {
                            if let Ok(r) = hidpp_request(h, in_len, dev_idx, fidx, 0x00, &[], 250) {
                                println!(
                                    "      [0x1000] raw={} -> {:?}",
                                    hex(&r[..r.len().min(12)]),
                                    parse_1000_status(&r)
                                );
                            }
                        }
                        0x1001 => {
                            if let Ok(r) = hidpp_request(h, in_len, dev_idx, fidx, 0x00, &[], 250) {
                                let mv = u16::from_be_bytes([r[4], r[5]]);
                                println!(
                                    "      [0x1001] raw={} ({} mV) -> {:?}",
                                    hex(&r[..r.len().min(12)]),
                                    mv,
                                    parse_1001_voltage(&r)
                                );
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }
}

unsafe fn probe_hyperx(h: HANDLE, out_len: usize, in_len: usize, product: &str) {
    println!("  [hyperx] probing {product} (out={out_len} in={in_len})");
    for cmd in [0x89u8, 0x8Au8] {
        let mut out = vec![0u8; out_len];
        out[0] = 0x66;
        out[1] = cmd;
        if send_report(h, &out, 400).is_err() {
            let _ = HidD_SetFeature(h, out.as_ptr() as *mut c_void, out.len() as u32);
        }
        if let Ok(r) = recv_report(h, in_len, 600) {
            let parsed = if cmd == 0x89 {
                format!("{:?}", parse_hyperx_battery(&r))
            } else {
                format!("{:?}", parse_hyperx_charging(&r))
            };
            println!("    cmd 0x{cmd:02X} raw={} -> {parsed}", hex(&r[..r.len().min(12)]));
        }
    }
}

unsafe fn raw_hid_scan() {
    println!("\n=== Raw HID scan ===");
    let Ok(hdev) = SetupDiGetClassDevsW(
        Some(&GUID_DEVINTERFACE_HID as *const GUID),
        PCWSTR::null(),
        None,
        DIGCF_DEVICEINTERFACE | DIGCF_PRESENT,
    ) else {
        return;
    };

    let mut index: u32 = 0;
    loop {
        let mut ifdata = SP_DEVICE_INTERFACE_DATA {
            cbSize: std::mem::size_of::<SP_DEVICE_INTERFACE_DATA>() as u32,
            ..Default::default()
        };
        if SetupDiEnumDeviceInterfaces(
            hdev,
            None,
            &GUID_DEVINTERFACE_HID as *const GUID,
            index,
            &mut ifdata,
        )
        .is_err()
        {
            break;
        }
        index += 1;

        let mut required = 0u32;
        let _ = SetupDiGetDeviceInterfaceDetailW(hdev, &ifdata, None, 0, Some(&mut required), None);
        if required == 0 {
            continue;
        }
        let mut buf = vec![0u8; required as usize];
        let detail = buf.as_mut_ptr() as *mut SP_DEVICE_INTERFACE_DETAIL_DATA_W;
        (*detail).cbSize = std::mem::size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;
        if SetupDiGetDeviceInterfaceDetailW(
            hdev,
            &ifdata,
            Some(detail),
            required,
            Some(&mut required),
            None,
        )
        .is_err()
        {
            continue;
        }
        let path = PWSTR((*detail).DevicePath.as_ptr() as *mut u16)
            .to_string()
            .unwrap_or_default();
        let Some(h) = open_hid(&path) else { continue };

        let mut attr = HIDD_ATTRIBUTES {
            Size: std::mem::size_of::<HIDD_ATTRIBUTES>() as u32,
            ..Default::default()
        };
        HidD_GetAttributes(h, &mut attr);

        let mut product = [0u16; 256];
        let _ = HidD_GetProductString(
            h,
            product.as_mut_ptr() as *mut c_void,
            (product.len() * 2) as u32,
        );
        let product = PWSTR(product.as_mut_ptr()).to_string().unwrap_or_default();

        let mut caps = HIDP_CAPS::default();
        let mut preparsed = PHIDP_PREPARSED_DATA::default();
        if HidD_GetPreparsedData(h, &mut preparsed) {
            let _ = HidP_GetCaps(preparsed, &mut caps);
            HidD_FreePreparsedData(preparsed);
        }

        if attr.VendorID == 0x046D && caps.UsagePage == 0xFF00 && caps.OutputReportByteLength == 20 {
            probe_logitech(h, caps.InputReportByteLength as usize, attr.ProductID, &product);
        } else if attr.VendorID == 0x03F0 && attr.ProductID == 0x05B7 && caps.UsagePage == 0xFF13 {
            let out_len = (caps.OutputReportByteLength as usize).max(62);
            let in_len = (caps.InputReportByteLength as usize).max(62);
            probe_hyperx(h, out_len, in_len, &product);
        }
        let _ = CloseHandle(h);
    }
    let _ = SetupDiDestroyDeviceInfoList(hdev);
}

fn main() {
    if let Err(e) = winrt_batteries() {
        println!("[WinRT] error: {e}");
    }
    unsafe { raw_hid_scan() };
    println!("\n=== HidScanner::read_all() ===");
    for b in hid::read_all() {
        println!("  {b:?}");
    }
}
