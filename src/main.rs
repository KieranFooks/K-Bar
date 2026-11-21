//! Battery Tray — ultra-lightweight single-threaded Windows 11 taskbar utility
//! displaying battery levels of wireless peripherals (Logitech mouse, HyperX headset).
//!
//! Renders as a passive, click-through unified 2-line monochrome block directly on the
//! Windows 11 taskbar using per-pixel alpha (`UpdateLayeredWindow`), auto-hides in
//! fullscreen apps, and polls HID hardware every 45 s plus on USB plug/resume events.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::cell::RefCell;
use std::ffi::c_void;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::OnceLock;

use battery_tray::{
    hid, light_theme, merge_devices, widget, WM_POLL_NOW, WM_SYNC_WIDGET,
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
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW, KillTimer,
    PostMessageW, PostQuitMessage, RegisterClassW, RegisterWindowMessageW, SetTimer,
    TranslateMessage, CS_HREDRAW, CS_VREDRAW, EVENT_SYSTEM_FOREGROUND, MSG,
    PBT_APMRESUMEAUTOMATIC, PBT_APMRESUMESUSPEND, WINDOW_EX_STYLE, WINEVENT_OUTOFCONTEXT,
    WINEVENT_SKIPOWNPROCESS, WM_CLOSE, WM_DEVICECHANGE, WM_DISPLAYCHANGE, WM_POWERBROADCAST,
    WM_SETTINGCHANGE, WM_TIMER, WNDCLASSW, WS_EX_TOOLWINDOW, WS_OVERLAPPED,
};

const POLL_TIMER_ID: usize = 100;
const DEVICECHANGE_DEBOUNCE_TIMER_ID: usize = 101;
const WIDGET_SYNC_TIMER_ID: usize = 102;
const POLL_INTERVAL_MS: u32 = 45_000;

static TASKBAR_CREATED: OnceLock<u32> = OnceLock::new();
static MAIN_HWND_RAW: AtomicUsize = AtomicUsize::new(0);

struct AppState {
    main: HWND,
    widget: Option<widget::WidgetWindow>,
    scanner: Option<hid::HidScanner>,
    devices: Vec<hid::Battery>,
    light_theme: bool,
}

thread_local! {
    static APP: RefCell<Option<AppState>> = const { RefCell::new(None) };
}

unsafe fn sync_widget(force_redraw: bool) {
    let taken = APP.with(|cell| {
        let mut borrow = cell.borrow_mut();
        let state = borrow.as_mut()?;
        let slot = state.widget.take();
        Some((slot, state.main, state.devices.clone(), state.light_theme))
    });
    let Some((mut slot, main, devices, light)) = taken else {
        return;
    };

    widget::sync_position(main, &mut slot, &devices, light, force_redraw);

    let leftover = APP.with(|cell| {
        let mut borrow = cell.borrow_mut();
        if let Some(state) = borrow.as_mut() {
            state.widget = slot;
            None
        } else {
            slot
        }
    });
    if let Some(w) = leftover {
        widget::destroy(w);
    }
}

unsafe fn run_poll(force_rescan: bool) {
    let taken = APP.with(|cell| {
        let mut borrow = cell.borrow_mut();
        borrow.as_mut().and_then(|s| s.scanner.take())
    });
    let Some(mut scanner) = taken else { return };

    let fresh = scanner.poll(force_rescan);
    #[cfg(debug_assertions)]
    eprintln!("[poll] {fresh:?}");

    let snapshot = APP.with(|cell| {
        let mut borrow = cell.borrow_mut();
        let state = borrow.as_mut()?;
        state.scanner = Some(scanner);
        state.devices = merge_devices(&state.devices, fresh);
        let slot = state.widget.take();
        Some((slot, state.main, state.devices.clone(), state.light_theme))
    });

    if let Some((mut slot, main, devices, light)) = snapshot {
        widget::sync_position(main, &mut slot, &devices, light, true);
        let leftover = APP.with(|cell| {
            let mut borrow = cell.borrow_mut();
            if let Some(state) = borrow.as_mut() {
                state.widget = slot;
                None
            } else {
                slot
            }
        });
        if let Some(w) = leftover {
            widget::destroy(w);
        }
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
        WM_POLL_NOW => {
            run_poll(true);
            LRESULT(0)
        }
        WM_SYNC_WIDGET | WM_DISPLAYCHANGE => {
            sync_widget(msg == WM_DISPLAYCHANGE);
            LRESULT(0)
        }
        WM_SETTINGCHANGE => {
            let light = light_theme();
            let changed = APP.with(|cell| {
                let mut borrow = cell.borrow_mut();
                if let Some(state) = borrow.as_mut() {
                    if state.light_theme != light {
                        state.light_theme = light;
                        return true;
                    }
                }
                false
            });
            if changed {
                sync_widget(true);
            }
            LRESULT(0)
        }
        WM_DEVICECHANGE => {
            SetTimer(Some(hwnd), DEVICECHANGE_DEBOUNCE_TIMER_ID, 1500, None);
            LRESULT(0)
        }
        WM_TIMER if wp.0 == DEVICECHANGE_DEBOUNCE_TIMER_ID => {
            let _ = KillTimer(Some(hwnd), DEVICECHANGE_DEBOUNCE_TIMER_ID);
            run_poll(true);
            LRESULT(0)
        }
        WM_TIMER if wp.0 == POLL_TIMER_ID => {
            run_poll(false);
            LRESULT(0)
        }
        WM_TIMER if wp.0 == WIDGET_SYNC_TIMER_ID => {
            sync_widget(false);
            LRESULT(0)
        }
        WM_POWERBROADCAST => {
            let ev = wp.0 as u32;
            if ev == PBT_APMRESUMEAUTOMATIC || ev == PBT_APMRESUMESUSPEND {
                run_poll(true);
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            let _ = KillTimer(Some(hwnd), POLL_TIMER_ID);
            let _ = KillTimer(Some(hwnd), DEVICECHANGE_DEBOUNCE_TIMER_ID);
            let _ = KillTimer(Some(hwnd), WIDGET_SYNC_TIMER_ID);

            let widget_hwnd = APP.with(|cell| {
                let state = cell.borrow_mut().take();
                state.and_then(|s| s.widget)
            });
            if let Some(w) = widget_hwnd {
                widget::destroy(w);
            }

            MAIN_HWND_RAW.store(0, Ordering::Relaxed);
            let _ = DestroyWindow(hwnd);
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => {
            let tbc = *TASKBAR_CREATED.get().unwrap_or(&0);
            if tbc != 0 && msg == tbc {
                sync_widget(true);
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

    let instance_mutex = unsafe {
        match CreateMutexW(None, false, w!("Local\\BatteryTray.SingleInstance")) {
            Ok(m) => m,
            Err(_) => return,
        }
    };
    unsafe {
        if GetLastError() == ERROR_ALREADY_EXISTS {
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

        APP.with(|cell| {
            *cell.borrow_mut() = Some(AppState {
                main: hwnd,
                widget: None,
                scanner: Some(hid::HidScanner::new()),
                devices: Vec::new(),
                light_theme: light,
            });
        });

        let _ = TASKBAR_CREATED.set(RegisterWindowMessageW(w!("TaskbarCreated")));
        sync_widget(true);

        let fg_hook = SetWinEventHook(
            EVENT_SYSTEM_FOREGROUND,
            EVENT_SYSTEM_FOREGROUND,
            None,
            Some(win_event_proc),
            0,
            0,
            WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
        );
        SetTimer(Some(hwnd), POLL_TIMER_ID, POLL_INTERVAL_MS, None);
        SetTimer(Some(hwnd), WIDGET_SYNC_TIMER_ID, 1000, None);
        let _ = PostMessageW(Some(hwnd), WM_POLL_NOW, WPARAM(0), LPARAM(0));

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        if !fg_hook.is_invalid() {
            let _ = UnhookWinEvent(fg_hook);
        }
        let _ = CloseHandle(instance_mutex);
    }
}
