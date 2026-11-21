//! Battery Tray — lightweight native Windows 11 tray app showing battery levels
//! of wireless peripherals (Logitech mouse, HyperX headset) in the taskbar.
//!
//! One tray icon per device with stable uIDs (mouse=1, headset=2), monochrome
//! Segoe Fluent Icons + crisp percentage badge, hidden top-level window (so
//! TaskbarCreated, WM_SETTINGCHANGE, WM_DEVICECHANGE, and WM_POWERBROADCAST
//! broadcasts arrive), and an event-driven Condvar HID polling worker.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::collections::BTreeMap;
use std::ffi::c_void;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::Duration;

use battery_tray::{
    device_tip, hid, icon, light_theme, merge_devices, popup, tray, tray_id_for_kind, App,
    TrayRenderKey, TraySlot, STATE, TRAY_ID_PLACEHOLDER, WM_REFRESH_NOW, WM_SHOW_POPUP,
    WM_TRAYICON, WM_UPDATE,
};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, HINSTANCE, HWND, LPARAM, LRESULT, WPARAM, ERROR_ALREADY_EXISTS,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Shell::{NIN_SELECT, Shell_NotifyIconW, NIM_ADD};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyIcon, DestroyWindow, DispatchMessageW, FindWindowW,
    GetMessageW, GetSystemMetrics, PostMessageW, PostQuitMessage, RegisterClassW,
    RegisterWindowMessageW, SetTimer, KillTimer, TranslateMessage, CS_HREDRAW, CS_VREDRAW,
    MSG, PBT_APMRESUMEAUTOMATIC, PBT_APMRESUMESUSPEND, SM_CXSMICON, WINDOW_EX_STYLE,
    WM_CLOSE, WM_CONTEXTMENU, WM_DEVICECHANGE, WM_POWERBROADCAST, WM_SETTINGCHANGE, WM_TIMER,
    WNDCLASSW, WS_EX_TOOLWINDOW, WS_OVERLAPPED,
};

const NIN_KEYSELECT: u32 = NIN_SELECT + 1;
const DEVICECHANGE_DEBOUNCE_TIMER_ID: usize = 101;
const POLL_INTERVAL: Duration = Duration::from_secs(45);

static TASKBAR_CREATED: OnceLock<u32> = OnceLock::new();
static WORKER_SIGNAL: OnceLock<Arc<(Mutex<WorkerSignal>, Condvar)>> = OnceLock::new();

#[derive(Default)]
struct WorkerSignal {
    wake: bool,
    force_rescan: bool,
    shutdown: bool,
}

fn wake_worker(force_rescan: bool) {
    if let Some(pair) = WORKER_SIGNAL.get() {
        let mut s = pair.0.lock().unwrap();
        s.wake = true;
        s.force_rescan |= force_rescan;
        pair.1.notify_one();
    }
}

/// Synchronize taskbar notification icons with the current `devices` list and theme.
/// Skips GDI rendering + IPC when nothing changed, and deletes stale icon slots.
unsafe fn update_tray(hwnd: HWND) {
    let size = GetSystemMetrics(SM_CXSMICON).max(16);

    let desired: Vec<(u32, TrayRenderKey)> = {
        let g = STATE.0.lock().unwrap();
        let Some(a) = g.as_ref() else { return };
        if a.devices.is_empty() {
            vec![(
                TRAY_ID_PLACEHOLDER,
                TrayRenderKey {
                    kind: None,
                    pct: None,
                    charging: false,
                    online: false,
                    light_theme: a.light_theme,
                    size,
                    tip: "Aucune batterie détectée".to_string(),
                },
            )]
        } else {
            a.devices
                .iter()
                .map(|d| {
                    (
                        tray_id_for_kind(d.kind),
                        TrayRenderKey {
                            kind: Some(d.kind),
                            pct: d.pct,
                            charging: d.charging,
                            online: d.online,
                            light_theme: a.light_theme,
                            size,
                            tip: device_tip(d),
                        },
                    )
                })
                .collect()
        }
    };

    // Check which icons actually changed vs unchanged
    let (stale_ids, to_render): (Vec<u32>, Vec<(u32, TrayRenderKey, bool)>) = {
        let g = STATE.0.lock().unwrap();
        let Some(a) = g.as_ref() else { return };
        let stale = a
            .icons
            .keys()
            .copied()
            .filter(|id| !desired.iter().any(|(d_id, _)| d_id == id))
            .collect();
        let updates = desired
            .into_iter()
            .filter_map(|(id, key)| match a.icons.get(&id) {
                Some(slot) if slot.key == key => None,
                Some(_) => Some((id, key, true)),
                None => Some((id, key, false)),
            })
            .collect();
        (stale, updates)
    };

    // Delete stale slots (e.g. placeholder once devices appear) outside mutex lock
    for id in stale_ids {
        tray::delete(hwnd, id);
        let removed = {
            let mut g = STATE.0.lock().unwrap();
            g.as_mut().and_then(|a| a.icons.remove(&id))
        };
        if let Some(slot) = removed {
            let _ = DestroyIcon(slot.icon);
        }
    }

    // Render and add/modify changed slots
    for (id, key, was_added) in to_render {
        let new_icon = match key.kind {
            Some(k) => icon::device_icon(k, key.pct, key.charging, key.online, key.size, key.light_theme),
            None => icon::placeholder_icon(key.size, key.light_theme),
        };
        let Some(ic) = new_icon else { continue };

        if was_added {
            let _ = tray::modify(hwnd, id, WM_TRAYICON, ic, &key.tip);
        } else {
            let ok = tray::add(hwnd, id, WM_TRAYICON, ic, &key.tip);
            #[cfg(debug_assertions)]
            eprintln!("[tray] NIM_ADD id={id} -> {ok}");
            if !ok {
                let _ = DestroyIcon(ic);
                continue;
            }
        }

        let old_icon = {
            let mut g = STATE.0.lock().unwrap();
            g.as_mut().and_then(|a| {
                a.icons
                    .insert(id, TraySlot { icon: ic, key })
                    .map(|s| s.icon)
            })
        };
        if let Some(old) = old_icon {
            let _ = DestroyIcon(old);
        }
    }
}

unsafe fn re_add_all_icons(hwnd: HWND) {
    let entries: Vec<(u32, windows::Win32::UI::WindowsAndMessaging::HICON, String)> = {
        let g = STATE.0.lock().unwrap();
        let Some(a) = g.as_ref() else { return };
        a.icons
            .iter()
            .map(|(&id, slot)| (id, slot.icon, slot.key.tip.clone()))
            .collect()
    };
    for (id, ic, tip) in entries {
        let _ = tray::add(hwnd, id, WM_TRAYICON, ic, &tip);
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_UPDATE => {
            let fresh = Box::from_raw(lp.0 as *mut Vec<hid::Battery>);
            {
                let mut g = STATE.0.lock().unwrap();
                if let Some(a) = g.as_mut() {
                    a.devices = merge_devices(&a.devices, *fresh);
                }
            }
            update_tray(hwnd);
            popup::refresh_if_open();
            LRESULT(0)
        }
        WM_TRAYICON => {
            let event = (lp.0 as u32) & 0xFFFF;
            let tray_id = ((lp.0 as u32) >> 16) & 0xFFFF;
            if matches!(event, NIN_SELECT | NIN_KEYSELECT | WM_CONTEXTMENU) {
                let id = if tray_id == 0 {
                    TRAY_ID_PLACEHOLDER
                } else {
                    tray_id
                };
                popup::toggle(hwnd, id);
            }
            LRESULT(0)
        }
        WM_SHOW_POPUP => {
            let id = {
                let g = STATE.0.lock().unwrap();
                g.as_ref()
                    .and_then(|a| a.icons.keys().next().copied())
                    .unwrap_or(TRAY_ID_PLACEHOLDER)
            };
            popup::open(hwnd, id);
            LRESULT(0)
        }
        WM_REFRESH_NOW => {
            wake_worker(true);
            LRESULT(0)
        }
        WM_SETTINGCHANGE => {
            let light = light_theme();
            let changed = {
                let mut g = STATE.0.lock().unwrap();
                if let Some(a) = g.as_mut() {
                    if a.light_theme != light {
                        a.light_theme = light;
                        true
                    } else {
                        false
                    }
                } else {
                    false
                }
            };
            if changed {
                update_tray(hwnd);
                popup::refresh_if_open();
            }
            LRESULT(0)
        }
        WM_DEVICECHANGE => {
            // Debounce rapid USB/HID plug events by 1.5 s before forced rescan
            SetTimer(Some(hwnd), DEVICECHANGE_DEBOUNCE_TIMER_ID, 1500, None);
            LRESULT(0)
        }
        WM_TIMER if wp.0 == DEVICECHANGE_DEBOUNCE_TIMER_ID => {
            let _ = KillTimer(Some(hwnd), DEVICECHANGE_DEBOUNCE_TIMER_ID);
            wake_worker(true);
            LRESULT(0)
        }
        WM_POWERBROADCAST => {
            let ev = wp.0 as u32;
            if ev == PBT_APMRESUMEAUTOMATIC || ev == PBT_APMRESUMESUSPEND {
                wake_worker(true);
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            let popup_hwnd = {
                let mut g = STATE.0.lock().unwrap();
                g.as_mut().and_then(|a| a.popup.take())
            };
            if let Some(p) = popup_hwnd {
                let _ = DestroyWindow(p);
            }

            let old_icons: Vec<(u32, windows::Win32::UI::WindowsAndMessaging::HICON)> = {
                let mut g = STATE.0.lock().unwrap();
                match g.as_mut() {
                    Some(a) => std::mem::take(&mut a.icons)
                        .into_iter()
                        .map(|(id, s)| (id, s.icon))
                        .collect(),
                    None => Vec::new(),
                }
            };
            for (id, ic) in old_icons {
                tray::delete(hwnd, id);
                let _ = DestroyIcon(ic);
            }

            if let Some(pair) = WORKER_SIGNAL.get() {
                let mut s = pair.0.lock().unwrap();
                s.shutdown = true;
                s.wake = true;
                pair.1.notify_one();
            }

            let _ = DestroyWindow(hwnd);
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => {
            let tbc = *TASKBAR_CREATED.get().unwrap_or(&0);
            if tbc != 0 && msg == tbc {
                re_add_all_icons(hwnd);
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

    let show_popup_flag = std::env::args().any(|a| a == "--show-popup");

    // Keep single-instance mutex open for the entire lifetime of main()
    let instance_mutex = unsafe {
        match CreateMutexW(None, false, w!("Local\\BatteryTray.SingleInstance")) {
            Ok(m) => m,
            Err(_) => return,
        }
    };
    unsafe {
        if GetLastError() == ERROR_ALREADY_EXISTS {
            if let Ok(existing) = FindWindowW(w!("BatteryTrayMainClass"), PCWSTR::null()) {
                let _ = PostMessageW(Some(existing), WM_SHOW_POPUP, WPARAM(0), LPARAM(0));
            }
            let _ = CloseHandle(instance_mutex);
            return;
        }
    }

    let class = w!("BatteryTrayMainClass");
    let hinst = unsafe {
        GetModuleHandleW(PCWSTR::null())
            .map(|m| HINSTANCE(m.0))
            .unwrap_or_default()
    };

    unsafe {
        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            hInstance: hinst,
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassW(&wc);

        // Hidden top-level window (NOT HWND_MESSAGE) so Windows delivers broadcast
        // messages: TaskbarCreated, WM_SETTINGCHANGE, WM_DEVICECHANGE, WM_POWERBROADCAST.
        let hwnd = match CreateWindowExW(
            WINDOW_EX_STYLE(WS_EX_TOOLWINDOW.0),
            class,
            w!("Battery Tray"),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(hinst),
            None,
        ) {
            Ok(h) => h,
            Err(e) => {
                eprintln!("CreateWindowExW failed: {e}");
                let _ = CloseHandle(instance_mutex);
                return;
            }
        };

        let light = light_theme();
        *STATE.0.lock().unwrap() = Some(App {
            main: Some(hwnd),
            devices: Vec::new(),
            light_theme: light,
            popup: None,
            popup_closed_at: None,
            last_clicked_tray_id: TRAY_ID_PLACEHOLDER,
            icons: BTreeMap::new(),
        });
        let _ = TASKBAR_CREATED.set(RegisterWindowMessageW(w!("TaskbarCreated")));
        update_tray(hwnd);

        let signal = Arc::new((Mutex::new(WorkerSignal::default()), Condvar::new()));
        let _ = WORKER_SIGNAL.set(signal.clone());

        let worker_hwnd = hwnd.0 as usize;
        let worker_handle = std::thread::spawn(move || {
            let hwnd = HWND(worker_hwnd as *mut c_void);
            let mut scanner = hid::HidScanner::new();
            let mut force_rescan = true;

            loop {
                let devs = scanner.poll(force_rescan);
                #[cfg(debug_assertions)]
                eprintln!("[poll] {devs:?}");

                let boxed = Box::into_raw(Box::new(devs));
                if PostMessageW(Some(hwnd), WM_UPDATE, WPARAM(0), LPARAM(boxed as isize)).is_err()
                {
                    // UI window closed: free payload to avoid leak
                    drop(Box::from_raw(boxed));
                    break;
                }

                let mut guard = signal.0.lock().unwrap();
                if !guard.wake && !guard.shutdown {
                    let (g, _) = signal.1.wait_timeout(guard, POLL_INTERVAL).unwrap();
                    guard = g;
                }
                if guard.shutdown {
                    break;
                }
                force_rescan = guard.force_rescan;
                guard.wake = false;
                guard.force_rescan = false;
            }
        });

        if show_popup_flag {
            let _ = PostMessageW(Some(hwnd), WM_SHOW_POPUP, WPARAM(0), LPARAM(0));
        }

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        let _ = worker_handle.join();
        let _ = CloseHandle(instance_mutex);
    }

    // Keep unused import warning silent if Shell_NotifyIconW/NIM_ADD not referenced directly here
    let _ = (Shell_NotifyIconW, NIM_ADD);
}
