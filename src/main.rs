//! Battery Tray — lightweight native tray app showing the battery level of
//! wireless peripherals (Logitech mouse, HyperX headset) in the taskbar.
//!
//! Message-only window + Shell_NotifyIconW + a worker thread polling HID
//! every 30 s. Icons are Segoe Fluent Icons glyphs rendered at runtime,
//! monochrome white/black depending on the taskbar theme.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::ffi::c_void;
use std::sync::OnceLock;
use std::time::Duration;

use battery_tray::{hid, icon, light_theme, popup, tip_text, tray, App, STATE};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{
    GetLastError, HINSTANCE, HWND, LPARAM, LRESULT, WPARAM, ERROR_ALREADY_EXISTS,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
use windows::Win32::UI::WindowsAndMessaging::*;

const WM_TRAYICON: u32 = WM_APP + 1;
const WM_UPDATE: u32 = WM_APP + 2;
const TRAY_ID: u32 = 1;

static TASKBAR_CREATED: OnceLock<u32> = OnceLock::new();

/// Re-render the tray icon (lowest battery drives it) and tooltip.
fn update_tray(hwnd: HWND) {
    let mut guard = STATE.0.lock().unwrap();
    let a = match guard.as_mut() {
        Some(a) => a,
        None => return,
    };
    let size = unsafe { GetSystemMetrics(SM_CXSMICON) }.max(16);
    let color = if a.light_theme { 0u32 } else { 0x00FF_FFFFu32 };
    let lowest = a
        .devices
        .iter()
        .filter(|d| d.pct.is_some())
        .min_by_key(|d| d.pct.unwrap_or(100));
    let glyph = icon::battery_glyph(
        lowest.and_then(|d| d.pct),
        lowest.map(|d| d.charging).unwrap_or(false),
    );
    let tip = tip_text(&a.devices);
    if let Some(ic) = unsafe { icon::glyph_icon(glyph, size, color) } {
        if let Some(old) = a.icon.replace(ic) {
            unsafe { let _ = DestroyIcon(old); }
        }
        a.tip = tip.clone();
        unsafe { tray::modify(hwnd, TRAY_ID, WM_TRAYICON, ic, &tip) };
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_UPDATE => {
            let mut devs = Box::from_raw(lp.0 as *mut Vec<hid::Battery>);
            {
                let mut g = STATE.0.lock().unwrap();
                if let Some(a) = g.as_mut() {
                    // keep the last known value for devices that did not
                    // answer this poll (e.g. mouse asleep)
                    for old in a.devices.iter() {
                        if !devs.iter().any(|d| d.kind == old.kind) {
                            devs.push(old.clone());
                        }
                    }
                    a.devices = *devs;
                }
            }
            update_tray(hwnd);
            // debug helper: auto-open the popup once so it can be screenshotted
            #[cfg(debug_assertions)]
            {
                static ONCE: std::sync::Once = std::sync::Once::new();
                ONCE.call_once(|| popup::toggle(hwnd));
            }
            LRESULT(0)
        }
        WM_TRAYICON => {
            let m = (lp.0 as u32) & 0xFFFF;
            if m == WM_LBUTTONUP || m == WM_RBUTTONUP {
                popup::toggle(hwnd);
            }
            LRESULT(0)
        }
        WM_SETTINGCHANGE => {
            let light = light_theme();
            {
                let mut g = STATE.0.lock().unwrap();
                if let Some(a) = g.as_mut() {
                    a.light_theme = light;
                }
            }
            update_tray(hwnd);
            LRESULT(0)
        }
        WM_CLOSE => {
            tray::delete(hwnd, TRAY_ID);
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => {
            let tbc = *TASKBAR_CREATED.get().unwrap_or(&0);
            if tbc != 0 && msg == tbc {
                // explorer restarted: re-add the icon
                let mut g = STATE.0.lock().unwrap();
                if let Some(a) = g.as_mut() {
                    if let Some(ic) = a.icon {
                        let tip = a.tip.clone();
                        tray::add(hwnd, TRAY_ID, WM_TRAYICON, ic, &tip);
                    }
                }
                return LRESULT(0);
            }
            DefWindowProcW(hwnd, msg, wp, lp)
        }
    }
}

fn main() {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }

    // single instance
    unsafe {
        let _mutex = match CreateMutexW(None, false, w!("Global\\BatteryTray.SingleInstance")) {
            Ok(m) => m,
            Err(_) => return,
        };
        if GetLastError() == ERROR_ALREADY_EXISTS {
            let _ = MessageBoxW(
                None,
                w!("Battery Tray est déjà en cours d'exécution."),
                w!("Battery Tray"),
                MB_OK | MB_ICONINFORMATION,
            );
            return;
        }
    }

    let class = w!("BatteryTrayMainClass");
    let hinst = unsafe { GetModuleHandleW(PCWSTR::null()).map(|m| HINSTANCE(m.0)).unwrap_or_default() };

    unsafe {
        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            hInstance: hinst,
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassW(&wc);

        // message-only window (HWND_MESSAGE parent)
        let hwnd = match CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            w!("Battery Tray"),
            WINDOW_STYLE(0),
            0, 0, 0, 0,
            Some(HWND_MESSAGE),
            None,
            Some(hinst),
            None,
        ) {
            Ok(h) => h,
            Err(e) => {
                eprintln!("CreateWindowExW failed: {}", e);
                return;
            }
        };

        let light = light_theme();
        let size = GetSystemMetrics(SM_CXSMICON).max(16);
        let color = if light { 0u32 } else { 0x00FF_FFFFu32 };
        let ic = icon::glyph_icon(icon::battery_glyph(None, false), size, color).unwrap_or_default();
        let tip = "Battery Tray".to_string();
        let added = tray::add(hwnd, TRAY_ID, WM_TRAYICON, ic, &tip);
        #[cfg(debug_assertions)]
        eprintln!("[tray] NIM_ADD -> {} (icon null: {})", added, ic.0.is_null());
        *STATE.0.lock().unwrap() = Some(App {
            main: Some(hwnd),
            devices: Vec::new(),
            light_theme: light,
            popup: None,
            icon: Some(ic),
            tip,
        });
        let _ = TASKBAR_CREATED.set(RegisterWindowMessageW(w!("TaskbarCreated")));

        // worker thread: poll HID batteries, post results to the UI thread
        let worker_hwnd = hwnd.0 as usize;
        std::thread::spawn(move || {
            let hwnd = HWND(worker_hwnd as *mut c_void);
            loop {
                let devs = hid::read_all();
                #[cfg(debug_assertions)]
                eprintln!("[poll] {:?}", devs);
                let _ = PostMessageW(
                    Some(hwnd),
                    WM_UPDATE,
                    WPARAM(0),
                    LPARAM(Box::into_raw(Box::new(devs)) as isize),
                );
                std::thread::sleep(Duration::from_secs(30));
            }
        });

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}
