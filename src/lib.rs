//! Library part of katt (`#![no_std]` in release): pure Win32 bindings, HID + BT readers, widget.

#![cfg_attr(not(test), no_std)]

pub mod drivers;
pub mod hid;
pub mod icon;
pub mod updater;
pub mod widget;
pub mod win32;

use core::ffi::c_void;
use core::mem::transmute;
use core::ptr::{null, null_mut};

use hid::{BatteryList, Source};
use win32::{
    FreeLibrary, GetProcAddress, LoadLibraryW, RegGetValueW, ERROR_SUCCESS, HKEY_CURRENT_USER,
    RRF_RT_REG_DWORD,
};

/// Custom window messages handled by the main hidden window.
pub const WM_POLL_NOW: u32 = 0x8000 + 2;
pub const WM_SYNC_WIDGET: u32 = 0x8000 + 3;
pub const WM_POLL_DONE: u32 = 0x8000 + 4;
pub const WM_UPDATE_AVAILABLE: u32 = 0x8000 + 5;
pub const WM_UPDATE_READY: u32 = 0x8000 + 6;
pub const WM_TOGGLE_DISPLAY_MODE: u32 = 0x8000 + 7;

unsafe fn read_hkcu_dword_flag(subkey: *const u16, value: *const u16) -> bool {
    let mut data = 0u32;
    let mut cb = core::mem::size_of::<u32>() as u32;
    let mut ty = 0u32;
    let r = RegGetValueW(
        HKEY_CURRENT_USER,
        subkey,
        value,
        RRF_RT_REG_DWORD,
        &mut ty,
        &mut data as *mut u32 as *mut c_void,
        &mut cb,
    );
    r == ERROR_SUCCESS && data != 0
}

/// `HKCU\...\Personalize\SystemUsesLightTheme` via `kernelbase.dll` (avoids `advapi32.dll`).
pub fn light_theme() -> bool {
    unsafe {
        read_hkcu_dword_flag(
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"),
            w!("SystemUsesLightTheme"),
        )
    }
}

/// Read persisted `HKCU\Software\katt\BatteryIcons` preference (`false` = percentages).
pub fn load_battery_icons_pref() -> bool {
    unsafe { read_hkcu_dword_flag(w!("Software\\katt"), w!("BatteryIcons")) }
}

/// Persist a value under `HKCU\Software\katt` via ephemeral `advapi32.dll` (zero static import).
///
/// # Safety
/// `value_name` must be a valid NUL-terminated UTF-16 string and `data` must be readable for `byte_len` bytes.
#[inline(never)]
pub unsafe fn save_hkcu_katt_value(
    value_name: *const u16,
    value_type: u32,
    data: *const u8,
    byte_len: u32,
) {
    const REG_OPTION_NON_VOLATILE: u32 = 0;
    const KEY_SET_VALUE: u32 = 0x0002;

    type FnRegCreateKeyExW = unsafe extern "system" fn(
        isize,
        *const u16,
        u32,
        *const u16,
        u32,
        u32,
        *const c_void,
        *mut isize,
        *mut u32,
    ) -> i32;
    type FnRegSetValueExW =
        unsafe extern "system" fn(isize, *const u16, u32, u32, *const u8, u32) -> i32;
    type FnRegCloseKey = unsafe extern "system" fn(isize) -> i32;

    let adv = LoadLibraryW(w!("advapi32.dll"));
    if adv.is_null() {
        return;
    }
    let p_create = GetProcAddress(adv, c"RegCreateKeyExW".as_ptr().cast::<u8>());
    let p_set = GetProcAddress(adv, c"RegSetValueExW".as_ptr().cast::<u8>());
    let p_close = GetProcAddress(adv, c"RegCloseKey".as_ptr().cast::<u8>());
    if !p_create.is_null() && !p_set.is_null() && !p_close.is_null() {
        let reg_create: FnRegCreateKeyExW = transmute(p_create);
        let reg_set: FnRegSetValueExW = transmute(p_set);
        let reg_close: FnRegCloseKey = transmute(p_close);

        let mut hkey: isize = 0;
        if reg_create(
            HKEY_CURRENT_USER,
            w!("Software\\katt"),
            0,
            null(),
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            null(),
            &mut hkey,
            null_mut(),
        ) == ERROR_SUCCESS
        {
            let _ = reg_set(hkey, value_name, 0, value_type, data, byte_len);
            let _ = reg_close(hkey);
        }
    }
    let _ = FreeLibrary(adv);
}

/// Persist `HKCU\Software\katt\BatteryIcons` (`REG_DWORD`) across reboots and updates.
pub fn save_battery_icons_pref(enabled: bool) {
    const REG_DWORD: u32 = 4;
    let val: u32 = u32::from(enabled);
    unsafe {
        save_hkcu_katt_value(
            w!("BatteryIcons"),
            REG_DWORD,
            &val as *const u32 as *const u8,
            core::mem::size_of::<u32>() as u32,
        );
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
