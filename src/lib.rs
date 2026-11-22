//! Library part of katt (`#![no_std]` in release): pure Win32 bindings, HID + BT readers, widget.

#![cfg_attr(not(test), no_std)]

pub mod drivers;
pub mod hid;
pub mod icon;
pub mod updater;
pub mod widget;
pub mod win32;

use core::ffi::c_void;

use hid::{BatteryList, Source};
use win32::{RegGetValueW, ERROR_SUCCESS, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};

/// Custom window messages handled by the main hidden window.
pub const WM_POLL_NOW: u32 = 0x8000 + 2;
pub const WM_SYNC_WIDGET: u32 = 0x8000 + 3;
pub const WM_POLL_DONE: u32 = 0x8000 + 4;
pub const WM_UPDATE_AVAILABLE: u32 = 0x8000 + 5;
pub const WM_UPDATE_READY: u32 = 0x8000 + 6;

/// `HKCU\...\Personalize\SystemUsesLightTheme` via `kernelbase.dll` (avoids `advapi32.dll`).
pub fn light_theme() -> bool {
    unsafe {
        let mut data = 0u32;
        let mut cb = core::mem::size_of::<u32>() as u32;
        let mut ty = 0u32;
        let r = RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"),
            w!("SystemUsesLightTheme"),
            RRF_RT_REG_DWORD,
            &mut ty,
            &mut data as *mut u32 as *mut c_void,
            &mut cb,
        );
        r == ERROR_SUCCESS && data != 0
    }
}

/// Merge fresh poll readings with previously seen devices in deterministic order,
/// retaining the last known percentage for sleeping `NativeHid` devices while their
/// USB dongle stays plugged in, and dropping disconnected `Bluetooth` or unplugged USB devices.
pub fn merge_devices(
    previous: &BatteryList,
    mut fresh: BatteryList,
    present_hid_vp: &[u32],
) -> BatteryList {
    for prev in previous.iter() {
        if prev.source == Source::NativeHid
            && fresh.get_by_id(prev.id).is_none()
            && (prev.vid_pid == 0 || present_hid_vp.contains(&prev.vid_pid))
        {
            let mut retained = *prev;
            retained.charging = false;
            retained.online = false;
            fresh.upsert(retained);
        }
    }
    fresh
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hid::{Battery, Kind};

    #[test]
    fn merge_devices_retains_sleeping_native_hid_but_drops_unplugged_and_bt() {
        let mut prev = BatteryList::new();
        prev.upsert(Battery::with_id(
            1,
            0x046D_C539,
            Kind::Mouse,
            Some(86),
            true,
            true,
            Source::NativeHid,
        ));
        prev.upsert(Battery::with_id(
            2,
            0x03F0_05B7,
            Kind::Headset,
            Some(28),
            false,
            true,
            Source::NativeHid,
        ));
        prev.upsert(Battery::with_id(
            3,
            0x045E_0B13,
            Kind::Gamepad,
            Some(50),
            false,
            true,
            Source::Bluetooth,
        ));

        let mut fresh = BatteryList::new();
        fresh.upsert(Battery::with_id(
            2,
            0x03F0_05B7,
            Kind::Headset,
            Some(27),
            false,
            true,
            Source::NativeHid,
        ));

        // Mouse dongle (0x046D_C539) still present -> retained offline; Bluetooth gamepad disconnected -> dropped
        let merged = merge_devices(&prev, fresh, &[0x046D_C539, 0x03F0_05B7]);
        assert_eq!(merged.count(), 2);

        let mouse = merged.get(Kind::Mouse).expect("mouse retained");
        assert_eq!(mouse.pct, Some(86));
        assert!(!mouse.online);
        assert!(!mouse.charging);

        let headset = merged.get(Kind::Headset).expect("headset updated");
        assert_eq!(headset.pct, Some(27));
        assert!(headset.online);

        assert!(merged.get(Kind::Gamepad).is_none());

        // Mouse dongle unplugged -> dropped
        let unplugged = merge_devices(&prev, fresh, &[0x03F0_05B7]);
        assert_eq!(unplugged.count(), 1);
        assert!(unplugged.get(Kind::Mouse).is_none());
    }
}
