//! Battery Tray — lightweight native Windows 11 taskbar utility showing
//! battery levels of wireless peripherals (Logitech mouse, HyperX headset).
//!
//! Renders as a unified 2-line monochrome block directly on the Windows 11
//! taskbar (Option 1A) using per-pixel alpha (`UpdateLayeredWindow`), auto-hides
//! in fullscreen apps, supports horizontal drag-to-adjust, and opens a native
//! Windows 11 flyout on click.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::ffi::c_void;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::Duration;

use battery_tray::{
    hid, light_theme, load_offset_x_dip, merge_devices, popup, widget, App, STATE, WM_REFRESH_NOW,
    WM_SHOW_POPUP, WM_SYNC_WIDGET, WM_UPDATE,
};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, HINSTANCE, HWND, LPARAM, LRESULT, WPARAM, ERROR_ALREADY_EXISTS,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, FindWindowW, GetMessageW,
    KillTimer, PostMessageW, PostQuitMessage, RegisterClassW, RegisterWindowMessageW, SetTimer,
    TranslateMessage, CS_HREDRAW, CS_VREDRAW, EVENT_SYSTEM_FOREGROUND, MSG,
    PBT_APMRESUMEAUTOMATIC, PBT_APMRESUMESUSPEND, WINDOW_EX_STYLE, WINEVENT_OUTOFCONTEXT,
    WINEVENT_SKIPOWNPROCESS, WM_CLOSE, WM_DEVICECHANGE, WM_DISPLAYCHANGE, WM_POWERBROADCAST,
    WM_SETTINGCHANGE, WM_TIMER, WNDCLASSW, WS_EX_TOOLWINDOW, WS_OVERLAPPED,
};

const DEVICECHANGE_DEBOUNCE_TIMER_ID: usize = 101;
const WIDGET_SYNC_TIMER_ID: usize = 102;
const POLL_INTERVAL: Duration = Duration::from_secs(45);

static TASKBAR_CREATED: OnceLock<u32> = OnceLock::new();
static WORKER_SIGNAL: OnceLock<Arc<(Mutex<WorkerSignal>, Condvar)>> = OnceLock::new();
static MAIN_HWND_RAW: AtomicUsize = AtomicUsize::new(0);

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

unsafe extern "system" fn win_event_proc(
    _hook: HWINEVENTHOOK,
    _event: u32,
    _hwnd: HWND,
    _id_object: i32,
    _id_child: i32,
    _id_event_thread: u32,
    _dwms_event_time: u32,
) {
    let raw = MAIN_HWND_RAW.load(Ordering::Relaxed);
    if raw != 0 {
        let _ = PostMessageW(
            Some(HWND(raw as *mut c_void)),
            WM_SYNC_WIDGET,
            WPARAM(0),
            LPARAM(0),
        );
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
            widget::sync_position(true);
            popup::refresh_if_open();
            LRESULT(0)
        }
        WM_SYNC_WIDGET | WM_DISPLAYCHANGE => {
            widget::sync_position(msg == WM_DISPLAYCHANGE);
            LRESULT(0)
        }
        WM_SHOW_POPUP => {
            popup::open(hwnd);
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
                widget::sync_position(true);
                popup::refresh_if_open();
            }
            LRESULT(0)
        }
        WM_DEVICECHANGE => {
            SetTimer(Some(hwnd), DEVICECHANGE_DEBOUNCE_TIMER_ID, 1500, None);
            LRESULT(0)
        }
        WM_TIMER if wp.0 == DEVICECHANGE_DEBOUNCE_TIMER_ID => {
            let _ = KillTimer(Some(hwnd), DEVICECHANGE_DEBOUNCE_TIMER_ID);
            wake_worker(true);
            LRESULT(0)
        }
        WM_TIMER if wp.0 == WIDGET_SYNC_TIMER_ID => {
            widget::sync_position(false);
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
            let _ = KillTimer(Some(hwnd), DEVICECHANGE_DEBOUNCE_TIMER_ID);
            let _ = KillTimer(Some(hwnd), WIDGET_SYNC_TIMER_ID);

            let popup_hwnd = {
                let mut g = STATE.0.lock().unwrap();
                g.as_mut().and_then(|a| a.popup.take())
            };
            if let Some(p) = popup_hwnd {
                let _ = DestroyWindow(p);
            }

            widget::destroy();

            if let Some(pair) = WORKER_SIGNAL.get() {
                let mut s = pair.0.lock().unwrap();
                s.shutdown = true;
                s.wake = true;
                pair.1.notify_one();
            }

            MAIN_HWND_RAW.store(0, Ordering::Relaxed);
            let _ = DestroyWindow(hwnd);
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => {
            let tbc = *TASKBAR_CREATED.get().unwrap_or(&0);
            if tbc != 0 && msg == tbc {
                widget::sync_position(true);
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

        // Hidden top-level window (NOT HWND_MESSAGE) so broadcast messages
        // (TaskbarCreated, WM_SETTINGCHANGE, WM_DEVICECHANGE, WM_POWERBROADCAST) arrive.
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

        MAIN_HWND_RAW.store(hwnd.0 as usize, Ordering::Relaxed);
        let light = light_theme();
        let offset_x_dip = load_offset_x_dip();

        *STATE.0.lock().unwrap() = Some(App {
            main: Some(hwnd),
            widget: None,
            popup: None,
            popup_closed_at: None,
            devices: Vec::new(),
            light_theme: light,
            offset_x_dip,
        });
        let _ = TASKBAR_CREATED.set(RegisterWindowMessageW(w!("TaskbarCreated")));

        let _ = widget::create(hwnd);
        widget::sync_position(true);

        let fg_hook = SetWinEventHook(
            EVENT_SYSTEM_FOREGROUND,
            EVENT_SYSTEM_FOREGROUND,
            None,
            Some(win_event_proc),
            0,
            0,
            WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
        );
        SetTimer(Some(hwnd), WIDGET_SYNC_TIMER_ID, 1000, None);

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

        if !fg_hook.is_invalid() {
            let _ = UnhookWinEvent(fg_hook);
        }
        let _ = worker_handle.join();
        let _ = CloseHandle(instance_mutex);
    }
}
