//! Library part of Battery Tray: HID readers, icon rendering, tray + popup.

pub mod hid;
pub mod icon;
pub mod popup;
pub mod tray;

use std::ffi::c_void;
use std::sync::Mutex;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{HWND, ERROR_SUCCESS};
use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, REG_VALUE_TYPE, RRF_RT_ANY};
use windows::Win32::UI::WindowsAndMessaging::HICON;

pub struct App {
    pub main: Option<HWND>,
    pub devices: Vec<hid::Battery>,
    pub light_theme: bool,
    pub popup: Option<HWND>,
    pub icon: Option<HICON>,
    pub tip: String,
}

// HWND/HICON wrap raw pointers, so they are not Send; the mutex is only ever
// locked from the UI thread and the popup thread, never shared across threads.
pub struct SyncState(pub Mutex<Option<App>>);
unsafe impl Sync for SyncState {}
pub static STATE: SyncState = SyncState(Mutex::new(None));

/// HKCU\...\Personalize\SystemUsesLightTheme — light taskbar => dark glyphs.
pub fn light_theme() -> bool {
    unsafe {
        let sub: Vec<u16> = "Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let val: Vec<u16> = "SystemUsesLightTheme".encode_utf16().chain(std::iter::once(0)).collect();
        let mut data = 0u32;
        let mut cb = std::mem::size_of::<u32>() as u32;
        let mut ty = REG_VALUE_TYPE(0);
        let r = RegGetValueW(
            HKEY_CURRENT_USER,
            PCWSTR(sub.as_ptr()),
            PCWSTR(val.as_ptr()),
            RRF_RT_ANY,
            Some(&mut ty as *mut REG_VALUE_TYPE),
            Some(&mut data as *mut u32 as *mut c_void),
            Some(&mut cb),
        );
        if r == ERROR_SUCCESS {
            data != 0
        } else {
            false
        }
    }
}

pub fn tip_text(devices: &[hid::Battery]) -> String {
    if devices.is_empty() {
        return "Aucune batterie détectée".to_string();
    }
    devices
        .iter()
        .map(|d| match d.pct {
            Some(p) => format!(
                "{} {} %{}",
                d.name,
                p,
                if d.charging { " (en charge)" } else { "" }
            ),
            None => format!("{} : niveau inconnu", d.name),
        })
        .collect::<Vec<_>>()
        .join(" · ")
}
