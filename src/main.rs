//! Battery Tray — ultra-minimal `#![no_std]` + `#![no_main]` single-threaded Windows 11 utility.
//!
//! Displays wireless peripheral battery levels in a passive click-through taskbar block
//! using only `kernel32`, `kernelbase`, `imm32`, `user32`, and `gdi32` (`-nostdlib`).

#![cfg_attr(not(test), no_std)]
#![cfg_attr(not(test), no_main)]
#![windows_subsystem = "windows"]

#[cfg(not(test))]
use core::panic::PanicInfo;
use core::ptr::{null, null_mut};
use core::sync::atomic::{AtomicUsize, Ordering};

use battery_tray::hid::{BatteryList, HidScanner};
use battery_tray::widget::WidgetWindow;
use battery_tray::win32::{
    CloseHandle, CreateMutexW, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    ExitProcess, GetLastError, GetMessageW, GetModuleHandleW, GetWindowThreadProcessId,
    ImmDisableIME, KillTimer, PostMessageW, PostQuitMessage, RegisterClassW,
    RegisterWindowMessageW, SetProcessDpiAwarenessContext, SetTimer, SetWinEventHook,
    SingleThreadCell, UnhookWinEvent, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
    ERROR_ALREADY_EXISTS, EVENT_OBJECT_LOCATIONCHANGE, EVENT_SYSTEM_FOREGROUND,
    EVENT_SYSTEM_MINIMIZEEND, EVENT_SYSTEM_MINIMIZESTART, EVENT_SYSTEM_MOVESIZEEND, HWND,
    HWINEVENTHOOK, MSG, PBT_APMRESUMEAUTOMATIC, PBT_APMRESUMESUSPEND, WINEVENT_OUTOFCONTEXT,
    WINEVENT_SKIPOWNPROCESS, WM_CLOSE, WM_DEVICECHANGE, WM_DISPLAYCHANGE, WM_POWERBROADCAST,
    WM_SETTINGCHANGE, WM_TIMER, WNDCLASSW, WS_EX_TOOLWINDOW, WS_OVERLAPPED,
};
use battery_tray::{light_theme, merge_devices, w, widget, WM_POLL_NOW, WM_SYNC_WIDGET};

const POLL_TIMER_ID: usize = 100;
const DEVICECHANGE_DEBOUNCE_TIMER_ID: usize = 101;
const WIDGET_SYNC_TIMER_ID: usize = 102;
const POLL_INTERVAL_MS: u32 = 45_000;
const WIDGET_SYNC_INTERVAL_MS: u32 = 5_000;

static TASKBAR_CREATED: AtomicUsize = AtomicUsize::new(0);
static MAIN_HWND_RAW: AtomicUsize = AtomicUsize::new(0);

#[cfg(not(test))]
#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    unsafe { ExitProcess(1) }
}

struct AppState {
    active: bool,
    main: HWND,
    widget: Option<WidgetWindow>,
    scanner: HidScanner,
    polling: bool,
    devices: BatteryList,
    light_theme: bool,
    tray_pid: u32,
    tray_loc_hook: HWINEVENTHOOK,
}

impl AppState {
    const fn empty() -> Self {
        Self {
            active: false,
            main: null_mut(),
            widget: None,
            scanner: HidScanner::new(),
            polling: false,
            devices: BatteryList::new(),
            light_theme: false,
            tray_pid: 0,
            tray_loc_hook: null_mut(),
        }
    }
}

static APP: SingleThreadCell<AppState> = SingleThreadCell::new(AppState::empty());

unsafe fn ensure_tray_location_hook(tray_hwnd: Option<HWND>) {
    let Some(tray) = tray_hwnd else { return };
    let mut pid = 0u32;
    let tid = GetWindowThreadProcessId(tray, &mut pid);
    if pid == 0 || tid == 0 {
        return;
    }
    let state = &mut *APP.get();
    if !state.active {
        return;
    }
    if state.tray_pid == pid && !state.tray_loc_hook.is_null() {
        return;
    }
    if !state.tray_loc_hook.is_null() {
        let _ = UnhookWinEvent(state.tray_loc_hook);
        state.tray_loc_hook = null_mut();
    }
    state.tray_pid = pid;
    state.tray_loc_hook = SetWinEventHook(
        EVENT_OBJECT_LOCATIONCHANGE,
        EVENT_OBJECT_LOCATIONCHANGE,
        null_mut(),
        Some(win_event_proc),
        pid,
        tid,
        WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
    );
}

unsafe fn sync_widget(force_redraw: bool) {
    let (active, main, mut slot, devices, light) = {
        let state = &mut *APP.get();
        if !state.active {
            return;
        }
        (
            true,
            state.main,
            state.widget.take(),
            state.devices,
            state.light_theme,
        )
    };
    if !active {
        return;
    }

    let tray_hwnd = widget::sync_position(main, &mut slot, &devices, light, force_redraw);

    let state = &mut *APP.get();
    if state.active {
        state.widget = slot;
        ensure_tray_location_hook(tray_hwnd);
    } else if let Some(w) = slot {
        widget::destroy(w);
    }
}

unsafe fn run_poll(force_rescan: bool) {
    let state = &mut *APP.get();
    if !state.active || state.polling {
        return;
    }
    state.polling = true;
    let scanner_ptr = &mut state.scanner as *mut HidScanner;
    let fresh = (*scanner_ptr).poll(force_rescan);

    let (main, mut slot, devices, light) = {
        let state = &mut *APP.get();
        state.polling = false;
        if !state.active {
            return;
        }
        state.devices = merge_devices(&state.devices, fresh);
        (state.main, state.widget.take(), state.devices, state.light_theme)
    };

    let tray_hwnd = widget::sync_position(main, &mut slot, &devices, light, true);

    let state = &mut *APP.get();
    if state.active {
        state.widget = slot;
        ensure_tray_location_hook(tray_hwnd);
    } else if let Some(w) = slot {
        widget::destroy(w);
    }
}

unsafe extern "system" fn win_event_proc(
    _hook: HWINEVENTHOOK,
    _event: u32,
    _hwnd: HWND,
    id_object: i32,
    _id_child: i32,
    _id_event_thread: u32,
    _dwms_event_time: u32,
) {
    if id_object != 0 {
        return;
    }
    let raw = MAIN_HWND_RAW.load(Ordering::Relaxed);
    if raw != 0 {
        let _ = PostMessageW(raw as HWND, WM_SYNC_WIDGET, 0, 0);
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: usize, lp: isize) -> isize {
    match msg {
        WM_POLL_NOW => {
            run_poll(true);
            0
        }
        WM_SYNC_WIDGET | WM_DISPLAYCHANGE => {
            sync_widget(msg == WM_DISPLAYCHANGE);
            0
        }
        WM_SETTINGCHANGE => {
            let light = light_theme();
            let changed = {
                let state = &mut *APP.get();
                if state.active && state.light_theme != light {
                    state.light_theme = light;
                    true
                } else {
                    false
                }
            };
            if changed {
                sync_widget(true);
            }
            0
        }
        WM_DEVICECHANGE => {
            let _ = SetTimer(hwnd, DEVICECHANGE_DEBOUNCE_TIMER_ID, 1500, null());
            0
        }
        WM_TIMER if wp == DEVICECHANGE_DEBOUNCE_TIMER_ID => {
            let _ = KillTimer(hwnd, DEVICECHANGE_DEBOUNCE_TIMER_ID);
            run_poll(true);
            0
        }
        WM_TIMER if wp == POLL_TIMER_ID => {
            run_poll(false);
            0
        }
        WM_TIMER if wp == WIDGET_SYNC_TIMER_ID => {
            sync_widget(false);
            0
        }
        WM_POWERBROADCAST => {
            if wp == PBT_APMRESUMEAUTOMATIC || wp == PBT_APMRESUMESUSPEND {
                run_poll(true);
            }
            0
        }
        WM_CLOSE => {
            let _ = KillTimer(hwnd, POLL_TIMER_ID);
            let _ = KillTimer(hwnd, DEVICECHANGE_DEBOUNCE_TIMER_ID);
            let _ = KillTimer(hwnd, WIDGET_SYNC_TIMER_ID);

            let (widget_opt, loc_hook, scanner_dummy) = {
                let state = &mut *APP.get();
                state.active = false;
                let w = state.widget.take();
                let h = state.tray_loc_hook;
                state.tray_loc_hook = null_mut();
                let sc = core::mem::replace(&mut state.scanner, HidScanner::new());
                (w, h, sc)
            };
            if !loc_hook.is_null() {
                let _ = UnhookWinEvent(loc_hook);
            }
            if let Some(w) = widget_opt {
                widget::destroy(w);
            }
            drop(scanner_dummy);

            MAIN_HWND_RAW.store(0, Ordering::Relaxed);
            let _ = DestroyWindow(hwnd);
            PostQuitMessage(0);
            0
        }
        _ => {
            let tbc = TASKBAR_CREATED.load(Ordering::Relaxed) as u32;
            if tbc != 0 && msg == tbc {
                sync_widget(true);
                return 0;
            }
            DefWindowProcW(hwnd, msg, wp, lp)
        }
    }
}

#[no_mangle]
pub extern "system" fn mainCRTStartup() -> ! {
    unsafe {
        // Call before window creation / GetMessageW to prevent MSCTF.dll TSF loading.
        let _ = ImmDisableIME(0xFFFF_FFFF);
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);

        let instance_mutex = CreateMutexW(null(), 0, w!("Local\\BatteryTray.SingleInstance"));
        if instance_mutex.is_null() {
            ExitProcess(1);
        }
        if GetLastError() == ERROR_ALREADY_EXISTS {
            let _ = CloseHandle(instance_mutex);
            ExitProcess(0);
        }

        let hinst = GetModuleHandleW(null());
        let class = w!("BatteryTrayMainClass");
        let wc = WNDCLASSW {
            lpfn_wnd_proc: Some(wndproc),
            h_instance: hinst,
            lpsz_class_name: class,
            ..Default::default()
        };
        let _ = RegisterClassW(&wc);

        // Hidden top-level window (NOT HWND_MESSAGE) so broadcast messages arrive.
        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW,
            class,
            w!("Battery Tray"),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            null_mut(),
            null_mut(),
            hinst,
            null(),
        );
        if hwnd.is_null() {
            let _ = CloseHandle(instance_mutex);
            ExitProcess(1);
        }

        MAIN_HWND_RAW.store(hwnd as usize, Ordering::Relaxed);
        TASKBAR_CREATED.store(
            RegisterWindowMessageW(w!("TaskbarCreated")) as usize,
            Ordering::Relaxed,
        );

        {
            let state = &mut *APP.get();
            state.active = true;
            state.main = hwnd;
            state.light_theme = light_theme();
        }

        sync_widget(true);

        let fg_hook = SetWinEventHook(
            EVENT_SYSTEM_FOREGROUND,
            EVENT_SYSTEM_FOREGROUND,
            null_mut(),
            Some(win_event_proc),
            0,
            0,
            WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
        );
        let move_hook = SetWinEventHook(
            EVENT_SYSTEM_MOVESIZEEND,
            EVENT_SYSTEM_MOVESIZEEND,
            null_mut(),
            Some(win_event_proc),
            0,
            0,
            WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
        );
        let min_hook = SetWinEventHook(
            EVENT_SYSTEM_MINIMIZESTART,
            EVENT_SYSTEM_MINIMIZEEND,
            null_mut(),
            Some(win_event_proc),
            0,
            0,
            WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
        );

        let _ = SetTimer(hwnd, POLL_TIMER_ID, POLL_INTERVAL_MS, null());
        let _ = SetTimer(hwnd, WIDGET_SYNC_TIMER_ID, WIDGET_SYNC_INTERVAL_MS, null());
        let _ = PostMessageW(hwnd, WM_POLL_NOW, 0, 0);

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            let _ = DispatchMessageW(&msg);
        }

        for hook in [fg_hook, move_hook, min_hook] {
            if !hook.is_null() {
                let _ = UnhookWinEvent(hook);
            }
        }
        let loc_hook = {
            let state = &mut *APP.get();
            let h = state.tray_loc_hook;
            state.tray_loc_hook = null_mut();
            h
        };
        if !loc_hook.is_null() {
            let _ = UnhookWinEvent(loc_hook);
        }

        let _ = CloseHandle(instance_mutex);
        ExitProcess(0)
    }
}
