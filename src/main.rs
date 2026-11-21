//! Battery Tray — ultra-minimal `#![no_std]` + `#![no_main]` single-threaded Windows 11 utility.
//!
//! Displays wireless peripheral battery levels in a passive click-through taskbar block
//! using only `kernel32`, `kernelbase`, `imm32`, `user32`, and `gdi32` (`-nostdlib`).

#![cfg_attr(not(test), no_std)]
#![cfg_attr(not(test), no_main)]
#![windows_subsystem = "windows"]

use core::mem::MaybeUninit;
#[cfg(not(test))]
use core::panic::PanicInfo;
use core::ptr::{addr_of_mut, null, null_mut};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

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
    WINEVENT_SKIPOWNPROCESS, WM_CLOSE, WM_DESTROY, WM_DEVICECHANGE, WM_DISPLAYCHANGE,
    WM_DPICHANGED, WM_POWERBROADCAST, WM_SETTINGCHANGE, WM_TIMER, WNDCLASSW, WS_EX_TOOLWINDOW,
    WS_OVERLAPPED,
};
use battery_tray::{light_theme, merge_devices, w, widget, WM_POLL_NOW, WM_SYNC_WIDGET};

const POLL_TIMER_ID: usize = 100;
const DEVICECHANGE_DEBOUNCE_TIMER_ID: usize = 101;
const WIDGET_SYNC_TIMER_ID: usize = 102;
const POLL_INTERVAL_MS: u32 = 45_000;
const WIDGET_SYNC_INTERVAL_MS: u32 = 5_000;

static TASKBAR_CREATED: AtomicUsize = AtomicUsize::new(0);
static MAIN_HWND_RAW: AtomicUsize = AtomicUsize::new(0);
static TRAY_HWND_RAW: AtomicUsize = AtomicUsize::new(0);
static NOTIFY_HWND_RAW: AtomicUsize = AtomicUsize::new(0);
static SYNC_PENDING: AtomicBool = AtomicBool::new(false);
static APP_INIT: AtomicBool = AtomicBool::new(false);

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
    syncing: bool,
    devices: BatteryList,
    light_theme: bool,
    tray_pid: u32,
    tray_tid: u32,
    tray_loc_hook: HWINEVENTHOOK,
}

impl AppState {
    /// Initialize `AppState` directly in `.bss` (`MaybeUninit::zeroed()`) without a large stack copy.
    unsafe fn init_in_place(dst: *mut Self, main: HWND, light: bool) {
        addr_of_mut!((*dst).active).write(true);
        addr_of_mut!((*dst).main).write(main);
        addr_of_mut!((*dst).widget).write(None);
        HidScanner::init_in_place(addr_of_mut!((*dst).scanner));
        addr_of_mut!((*dst).polling).write(false);
        addr_of_mut!((*dst).syncing).write(false);
        addr_of_mut!((*dst).devices).write(BatteryList::new());
        addr_of_mut!((*dst).light_theme).write(light);
        addr_of_mut!((*dst).tray_pid).write(0);
        addr_of_mut!((*dst).tray_tid).write(0);
        addr_of_mut!((*dst).tray_loc_hook).write(null_mut());
    }
}

// Stored in `.bss` via `MaybeUninit::zeroed()` so `.data` stays 0 bytes on disk.
static APP: SingleThreadCell<MaybeUninit<AppState>> = SingleThreadCell::new(MaybeUninit::zeroed());

#[inline]
unsafe fn app_mut() -> Option<&'static mut AppState> {
    if APP_INIT.load(Ordering::Acquire) {
        Some((*APP.get()).assume_init_mut())
    } else {
        None
    }
}

unsafe fn update_tray_tracking(tray_hwnd: Option<HWND>, notify_hwnd: Option<HWND>) {
    TRAY_HWND_RAW.store(tray_hwnd.map_or(0, |h| h as usize), Ordering::Relaxed);
    NOTIFY_HWND_RAW.store(notify_hwnd.map_or(0, |h| h as usize), Ordering::Relaxed);

    let Some(tray) = tray_hwnd else { return };
    let mut pid = 0u32;
    let tid = GetWindowThreadProcessId(tray, &mut pid);
    if pid == 0 || tid == 0 {
        return;
    }
    let Some(state) = app_mut() else { return };
    if !state.active {
        return;
    }
    if state.tray_pid == pid && state.tray_tid == tid && !state.tray_loc_hook.is_null() {
        return;
    }
    if !state.tray_loc_hook.is_null() {
        let old_hook = state.tray_loc_hook;
        state.tray_loc_hook = null_mut();
        let _ = UnhookWinEvent(old_hook);
    }
    state.tray_pid = pid;
    state.tray_tid = tid;
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
    let (main, mut slot, devices, light) = {
        let Some(state) = app_mut() else { return };
        if !state.active || state.syncing {
            return;
        }
        state.syncing = true;
        (
            state.main,
            state.widget.take(),
            state.devices,
            state.light_theme,
        )
    };

    let (tray_hwnd, notify_hwnd) =
        widget::sync_position(main, &mut slot, &devices, light, force_redraw);

    let mut leftover = None;
    if let Some(state) = app_mut() {
        state.syncing = false;
        if state.active {
            if let Some(orphan) = state.widget.take() {
                leftover = Some(orphan);
            }
            state.widget = slot;
            update_tray_tracking(tray_hwnd, notify_hwnd);
        } else {
            leftover = slot;
        }
    } else {
        leftover = slot;
    }
    if let Some(w) = leftover {
        widget::destroy(w);
    }
}

unsafe fn run_poll(force_rescan: bool) {
    let scanner_ptr = {
        let Some(state) = app_mut() else { return };
        if !state.active || state.polling {
            return;
        }
        state.polling = true;
        &mut state.scanner as *mut HidScanner
    };

    let fresh = (*scanner_ptr).poll(force_rescan);

    let (main, mut slot, devices, light) = {
        let Some(state) = app_mut() else { return };
        state.polling = false;
        if !state.active {
            state.scanner.close();
            return;
        }
        state.devices = merge_devices(&state.devices, fresh);
        (state.main, state.widget.take(), state.devices, state.light_theme)
    };

    let (tray_hwnd, notify_hwnd) = widget::sync_position(main, &mut slot, &devices, light, true);

    let mut leftover = None;
    if let Some(state) = app_mut() {
        if state.active {
            if let Some(orphan) = state.widget.take() {
                leftover = Some(orphan);
            }
            state.widget = slot;
            update_tray_tracking(tray_hwnd, notify_hwnd);
        } else {
            leftover = slot;
        }
    } else {
        leftover = slot;
    }
    if let Some(w) = leftover {
        widget::destroy(w);
    }
}

unsafe fn cleanup_app_state(hwnd: HWND) {
    let _ = KillTimer(hwnd, POLL_TIMER_ID);
    let _ = KillTimer(hwnd, DEVICECHANGE_DEBOUNCE_TIMER_ID);
    let _ = KillTimer(hwnd, WIDGET_SYNC_TIMER_ID);

    MAIN_HWND_RAW.store(0, Ordering::Relaxed);
    TRAY_HWND_RAW.store(0, Ordering::Relaxed);
    NOTIFY_HWND_RAW.store(0, Ordering::Relaxed);

    let Some(state) = app_mut() else { return };
    if !state.active {
        return;
    }
    state.active = false;
    let widget_opt = state.widget.take();
    let loc_hook = state.tray_loc_hook;
    state.tray_loc_hook = null_mut();
    if !state.polling {
        state.scanner.close();
    }

    if !loc_hook.is_null() {
        let _ = UnhookWinEvent(loc_hook);
    }
    if let Some(w) = widget_opt {
        widget::destroy(w);
    }
}

unsafe extern "system" fn win_event_proc(
    _hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    id_object: i32,
    _id_child: i32,
    _id_event_thread: u32,
    _dwms_event_time: u32,
) {
    if id_object != 0 {
        return;
    }
    if event == EVENT_OBJECT_LOCATIONCHANGE {
        let h = hwnd as usize;
        let tray = TRAY_HWND_RAW.load(Ordering::Relaxed);
        let notify = NOTIFY_HWND_RAW.load(Ordering::Relaxed);
        if h != tray && (notify == 0 || h != notify) {
            return;
        }
    }
    let raw = MAIN_HWND_RAW.load(Ordering::Relaxed);
    if raw != 0 && !SYNC_PENDING.swap(true, Ordering::AcqRel) && PostMessageW(raw as HWND, WM_SYNC_WIDGET, 0, 0) == 0 {
        SYNC_PENDING.store(false, Ordering::Release);
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: usize, lp: isize) -> isize {
    match msg {
        WM_POLL_NOW => {
            run_poll(true);
            0
        }
        WM_SYNC_WIDGET => {
            SYNC_PENDING.store(false, Ordering::Release);
            sync_widget(false);
            0
        }
        WM_DISPLAYCHANGE | WM_DPICHANGED => {
            sync_widget(true);
            0
        }
        WM_SETTINGCHANGE => {
            let light = light_theme();
            let changed = if let Some(state) = app_mut() {
                if state.active && state.light_theme != light {
                    state.light_theme = light;
                    true
                } else {
                    false
                }
            } else {
                false
            };
            if changed {
                sync_widget(true);
            }
            0
        }
        WM_DEVICECHANGE => {
            let _ = SetTimer(hwnd, DEVICECHANGE_DEBOUNCE_TIMER_ID, 1500, null());
            1
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
            1
        }
        WM_CLOSE => {
            cleanup_app_state(hwnd);
            let _ = DestroyWindow(hwnd);
            0
        }
        WM_DESTROY => {
            cleanup_app_state(hwnd);
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

        AppState::init_in_place((*APP.get()).as_mut_ptr(), hwnd, light_theme());
        APP_INIT.store(true, Ordering::Release);

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
        cleanup_app_state(hwnd);
        let _ = CloseHandle(instance_mutex);
        ExitProcess(0)
    }
}
