//! Library part of Battery Tray: HID readers, glyph/mask primitives, unified taskbar widget.

pub mod hid;
pub mod icon;
pub mod widget;

use std::ffi::c_void;

use windows::core::w;
use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::System::Registry::{
    RegGetValueW, HKEY_CURRENT_USER, REG_VALUE_TYPE, RRF_RT_ANY,
};

use hid::Battery;

/// Custom window messages handled by the main hidden window.
pub const WM_POLL_NOW: u32 = 0x8000 + 2;
pub const WM_SYNC_WIDGET: u32 = 0x8000 + 3;

/// HKCU\...\Personalize\SystemUsesLightTheme — light taskbar => dark glyphs.
pub fn light_theme() -> bool {
    unsafe {
        let mut data = 0u32;
        let mut cb = std::mem::size_of::<u32>() as u32;
        let mut ty = REG_VALUE_TYPE(0);
        let r = RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"),
            w!("SystemUsesLightTheme"),
            RRF_RT_ANY,
            Some(&mut ty as *mut REG_VALUE_TYPE),
            Some(&mut data as *mut u32 as *mut c_void),
            Some(&mut cb),
        );
        r == ERROR_SUCCESS && data != 0
    }
}

/// Merge fresh poll readings with previously seen devices in deterministic order
/// (`Mouse`, then `Headset`), retaining the last known percentage if a sleeping
/// device skips a poll.
pub fn merge_devices(previous: &[Battery], fresh: Vec<Battery>) -> Vec<Battery> {
    let mut out = fresh;
    for prev in previous {
        if !out.iter().any(|d| d.kind == prev.kind) {
            out.push(Battery {
                name: prev.name.clone(),
                kind: prev.kind,
                pct: prev.pct,
                charging: false,
                online: false,
            });
        }
    }
    out.sort_by_key(|d| d.kind);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hid::Kind;

    #[test]
    fn merge_devices_preserves_order_and_last_reading_on_missed_poll() {
        let prev = vec![
            Battery {
                name: "G502 LIGHTSPEED".into(),
                kind: Kind::Mouse,
                pct: Some(86),
                charging: true,
                online: true,
            },
            Battery {
                name: "HyperX Cloud III Wireless".into(),
                kind: Kind::Headset,
                pct: Some(28),
                charging: false,
                online: true,
            },
        ];

        let fresh = vec![Battery {
            name: "HyperX Cloud III Wireless".into(),
            kind: Kind::Headset,
            pct: Some(27),
            charging: false,
            online: true,
        }];

        let merged = merge_devices(&prev, fresh);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].kind, Kind::Mouse);
        assert_eq!(merged[0].pct, Some(86));
        assert!(!merged[0].online);
        assert!(!merged[0].charging);
        assert_eq!(merged[1].kind, Kind::Headset);
        assert_eq!(merged[1].pct, Some(27));
        assert!(merged[1].online);
    }
}
