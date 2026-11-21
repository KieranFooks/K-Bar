//! Library part of Battery Tray (`#![no_std]` in release): pure Win32 bindings, HID readers, widget.

#![cfg_attr(not(test), no_std)]

pub mod hid;
pub mod icon;
pub mod widget;
pub mod win32;

use core::ffi::c_void;

use hid::{BatteryList, Kind};
use win32::{RegGetValueW, ERROR_SUCCESS, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};

/// Custom window messages handled by the main hidden window.
pub const WM_POLL_NOW: u32 = 0x8000 + 2;
pub const WM_SYNC_WIDGET: u32 = 0x8000 + 3;

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

/// Merge fresh poll readings with previously seen devices in deterministic order
/// (`Mouse`, then `Headset`), retaining the last known percentage if a sleeping
/// device skips a poll.
pub fn merge_devices(previous: &BatteryList, mut fresh: BatteryList) -> BatteryList {
    for kind in [Kind::Mouse, Kind::Headset] {
        if fresh.get(kind).is_none() {
            if let Some(prev) = previous.get(kind) {
                let mut retained = *prev;
                retained.charging = false;
                retained.online = false;
                fresh.set(retained);
            }
        }
    }
    fresh
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hid::Battery;

    #[test]
    fn merge_devices_preserves_order_and_last_reading_on_missed_poll() {
        let mut prev = BatteryList::new();
        prev.set(Battery::new(Kind::Mouse, Some(86), true, true));
        prev.set(Battery::new(Kind::Headset, Some(28), false, true));

        let mut fresh = BatteryList::new();
        fresh.set(Battery::new(Kind::Headset, Some(27), false, true));

        let merged = merge_devices(&prev, fresh);
        assert_eq!(merged.count(), 2);

        let mouse = merged.get(Kind::Mouse).expect("mouse retained");
        assert_eq!(mouse.pct, Some(86));
        assert!(!mouse.online);
        assert!(!mouse.charging);

        let headset = merged.get(Kind::Headset).expect("headset updated");
        assert_eq!(headset.pct, Some(27));
        assert!(headset.online);
    }
}
