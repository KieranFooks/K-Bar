//! Library part of Battery Tray: HID readers, icon rendering, tray + popup, shared state.

pub mod hid;
pub mod icon;
pub mod popup;
pub mod tray;

use std::collections::BTreeMap;
use std::ffi::c_void;
use std::sync::Mutex;
use std::time::Instant;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{ERROR_SUCCESS, HWND};
use windows::Win32::System::LibraryLoader::GetModuleFileNameW;
use windows::Win32::System::Registry::{
    RegCloseKey, RegDeleteValueW, RegGetValueW, RegOpenKeyExW, RegSetValueExW, HKEY,
    HKEY_CURRENT_USER, KEY_SET_VALUE, REG_SZ, REG_VALUE_TYPE, RRF_RT_ANY,
};
use windows::Win32::UI::WindowsAndMessaging::HICON;

use hid::{Battery, Kind};

/// Stable Shell_NotifyIcon `uID` values so Windows 11 taskbar pinning/order
/// never swaps when a device sleeps or disconnects.
pub const TRAY_ID_MOUSE: u32 = 1;
pub const TRAY_ID_HEADSET: u32 = 2;
pub const TRAY_ID_PLACEHOLDER: u32 = 99;

/// Custom window messages handled by the main hidden window.
pub const WM_TRAYICON: u32 = 0x8000 + 1; // WM_APP + 1
pub const WM_UPDATE: u32 = 0x8000 + 2;
pub const WM_REFRESH_NOW: u32 = 0x8000 + 3;
pub const WM_SHOW_POPUP: u32 = 0x8000 + 4;

pub fn tray_id_for_kind(kind: Kind) -> u32 {
    match kind {
        Kind::Mouse => TRAY_ID_MOUSE,
        Kind::Headset => TRAY_ID_HEADSET,
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TrayRenderKey {
    pub kind: Option<Kind>,
    pub pct: Option<u8>,
    pub charging: bool,
    pub online: bool,
    pub light_theme: bool,
    pub size: i32,
    pub tip: String,
}

pub struct TraySlot {
    pub icon: HICON,
    pub key: TrayRenderKey,
}

pub struct App {
    pub main: Option<HWND>,
    pub devices: Vec<Battery>,
    pub light_theme: bool,
    pub popup: Option<HWND>,
    pub popup_closed_at: Option<Instant>,
    pub last_clicked_tray_id: u32,
    /// Active tray icons keyed by stable `uID` (`TRAY_ID_MOUSE`, `TRAY_ID_HEADSET`, ...).
    pub icons: BTreeMap<u32, TraySlot>,
}

// HWND/HICON wrap raw pointers; STATE is only accessed briefly to snapshot/update
// fields and never held across Win32 calls that re-enter WndProcs (like DestroyWindow).
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
/// - If a `Mouse` did not answer this poll (e.g. asleep), preserve its last known
///   percentage with `online: false` and `charging: false`.
/// - If a `Headset` answered offline (`pct: None`), keep that offline state.
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

/// Tooltip for a single tray icon (max ~127 UTF-16 code units).
pub fn device_tip(d: &Battery) -> String {
    match (d.online, d.pct, d.charging) {
        (true, Some(p), true) => format!("{} — {} % (en charge)", d.name, p),
        (true, Some(p), false) => format!("{} — {} %", d.name, p),
        (false, Some(p), _) => format!("{} — {} % (en veille)", d.name, p),
        (_, None, _) => format!("{} — Hors ligne", d.name),
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

        // Mouse is asleep so fresh only returns Headset
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
