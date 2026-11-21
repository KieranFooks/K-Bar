//! Library part of Battery Tray: HID readers, icon primitives, unified taskbar widget, popup, shared state.

pub mod hid;
pub mod icon;
pub mod popup;
pub mod widget;

use std::ffi::c_void;
use std::sync::Mutex;
use std::time::Instant;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{ERROR_SUCCESS, HWND};
use windows::Win32::System::LibraryLoader::GetModuleFileNameW;
use windows::Win32::System::Registry::{
    RegCloseKey, RegDeleteValueW, RegGetValueW, RegOpenKeyExW, RegSetKeyValueW, RegSetValueExW,
    HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE, REG_DWORD, REG_SZ, REG_VALUE_TYPE, RRF_RT_ANY,
};

use hid::Battery;

/// Custom window messages handled by the main hidden window.
pub const WM_UPDATE: u32 = 0x8000 + 2;
pub const WM_REFRESH_NOW: u32 = 0x8000 + 3;
pub const WM_SHOW_POPUP: u32 = 0x8000 + 4;
pub const WM_SYNC_WIDGET: u32 = 0x8000 + 5;

pub struct App {
    pub main: Option<HWND>,
    pub widget: Option<HWND>,
    pub popup: Option<HWND>,
    pub popup_closed_at: Option<Instant>,
    pub devices: Vec<Battery>,
    pub light_theme: bool,
    pub offset_x_dip: i32,
}

// HWND wraps raw pointers; STATE is only locked briefly to snapshot/update fields
// and never held across Win32 calls that re-enter WndProcs (DestroyWindow, SendMessageW).
pub struct SyncState(pub Mutex<Option<App>>);
unsafe impl Sync for SyncState {}
pub static STATE: SyncState = SyncState(Mutex::new(None));

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
/// (`Mouse`, then `Headset`).
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

/// Human-readable status summary for a single device.
pub fn device_tip(d: &Battery) -> String {
    match (d.online, d.pct, d.charging) {
        (true, Some(p), true) => format!("{} — {} % (en charge)", d.name, p),
        (true, Some(p), false) => format!("{} — {} %", d.name, p),
        (false, Some(p), _) => format!("{} — {} % (en veille)", d.name, p),
        (_, None, _) => format!("{} — Hors ligne", d.name),
    }
}

const APP_SUBKEY: PCWSTR = w!("Software\\BatteryTray");
const OFFSET_VALUE: PCWSTR = w!("OffsetX");

pub fn load_offset_x_dip() -> i32 {
    unsafe {
        let mut data = 0u32;
        let mut cb = std::mem::size_of::<u32>() as u32;
        let mut ty = REG_VALUE_TYPE(0);
        let r = RegGetValueW(
            HKEY_CURRENT_USER,
            APP_SUBKEY,
            OFFSET_VALUE,
            RRF_RT_ANY,
            Some(&mut ty as *mut REG_VALUE_TYPE),
            Some(&mut data as *mut u32 as *mut c_void),
            Some(&mut cb),
        );
        if r == ERROR_SUCCESS {
            (data as i32).clamp(-1500, 1500)
        } else {
            0
        }
    }
}

pub fn save_offset_x_dip(offset: i32) {
    unsafe {
        let raw = offset as u32;
        let _ = RegSetKeyValueW(
            HKEY_CURRENT_USER,
            APP_SUBKEY,
            OFFSET_VALUE,
            REG_DWORD.0,
            Some(&raw as *const u32 as *const c_void),
            std::mem::size_of::<u32>() as u32,
        );
    }
}

const RUN_SUBKEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const RUN_VALUE: PCWSTR = w!("BatteryTray");

pub fn is_autostart_enabled() -> bool {
    unsafe {
        let mut ty = REG_VALUE_TYPE(0);
        let mut cb = 0u32;
        let r = RegGetValueW(
            HKEY_CURRENT_USER,
            RUN_SUBKEY,
            RUN_VALUE,
            RRF_RT_ANY,
            Some(&mut ty as *mut REG_VALUE_TYPE),
            None,
            Some(&mut cb),
        );
        r == ERROR_SUCCESS && cb > 2
    }
}

pub fn set_autostart(enabled: bool) -> bool {
    unsafe {
        let mut hkey = HKEY::default();
        if RegOpenKeyExW(HKEY_CURRENT_USER, RUN_SUBKEY, Some(0), KEY_SET_VALUE, &mut hkey)
            != ERROR_SUCCESS
        {
            return false;
        }
        let ok = if enabled {
            let mut path_buf = [0u16; 1024];
            let n = GetModuleFileNameW(None, &mut path_buf) as usize;
            if n == 0 || n >= path_buf.len() {
                false
            } else {
                let exe = String::from_utf16_lossy(&path_buf[..n]);
                let cmd: Vec<u16> = format!("\"{exe}\"")
                    .encode_utf16()
                    .chain(std::iter::once(0))
                    .collect();
                let bytes = std::slice::from_raw_parts(
                    cmd.as_ptr() as *const u8,
                    cmd.len() * std::mem::size_of::<u16>(),
                );
                RegSetValueExW(hkey, RUN_VALUE, Some(0), REG_SZ, Some(bytes)) == ERROR_SUCCESS
            }
        } else {
            let r = RegDeleteValueW(hkey, RUN_VALUE);
            r == ERROR_SUCCESS || r.0 == 2 // ERROR_FILE_NOT_FOUND
        };
        let _ = RegCloseKey(hkey);
        ok
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hid::Kind;

    #[test]
    fn merge_devices_preserves_order_and_marks_sleeping_mouse_offline() {
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

    #[test]
    fn device_tip_formats_all_states() {
        assert_eq!(
            device_tip(&Battery {
                name: "G502 LIGHTSPEED".into(),
                kind: Kind::Mouse,
                pct: Some(86),
                charging: true,
                online: true,
            }),
            "G502 LIGHTSPEED — 86 % (en charge)"
        );
        assert_eq!(
            device_tip(&Battery {
                name: "G502 LIGHTSPEED".into(),
                kind: Kind::Mouse,
                pct: Some(86),
                charging: false,
                online: true,
            }),
            "G502 LIGHTSPEED — 86 %"
        );
        assert_eq!(
            device_tip(&Battery {
                name: "G502 LIGHTSPEED".into(),
                kind: Kind::Mouse,
                pct: Some(86),
                charging: false,
                online: false,
            }),
            "G502 LIGHTSPEED — 86 % (en veille)"
        );
        assert_eq!(
            device_tip(&Battery {
                name: "HyperX Cloud III Wireless".into(),
                kind: Kind::Headset,
                pct: None,
                charging: false,
                online: false,
            }),
            "HyperX Cloud III Wireless — Hors ligne"
        );
    }
}
