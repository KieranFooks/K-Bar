//! katt, an ultra-minimal `#![no_std]` + `#![no_main]` Windows 11 taskbar utility.
//!
//! Displays wireless peripheral battery levels in a passive click-through taskbar block
//! using only `kernel32`, `kernelbase`, `cfgmgr32`, `imm32`, `user32`, and `gdi32` (`-nostdlib`).
//! A single background worker thread runs HID/BT polling so timeouts never block UI sync.

#![cfg_attr(not(test), no_std)]
#![cfg_attr(not(test), no_main)]
#![windows_subsystem = "windows"]

use core::ffi::c_void;
use core::mem::MaybeUninit;
#[cfg(not(test))]
use core::panic::PanicInfo;
use core::ptr::{addr_of_mut, null, null_mut};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use katt::hid::{BatteryList, HidScanner, PollOutput};
use katt::widget::WidgetWindow;
use katt::win32::{
    CloseHandle, CreateEventW, CreateMutexW, CreateThread, CreateWindowExW, DefWindowProcW,
    DestroyWindow, DispatchMessageW, ExitProcess, GetDesktopWindow, GetLastError, GetMessageW,
    GetModuleHandleW, GetWindowThreadProcessId, ImmDisableIME, KillTimer, PostMessageW,
    PostQuitMessage, RegisterClassW, RegisterWindowMessageW, SetEvent,
    SetProcessDpiAwarenessContext, SetTimer, SetWinEventHook, SingleThreadCell, UnhookWinEvent,
    WaitForSingleObject, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, ERROR_ALREADY_EXISTS,
    EVENT_OBJECT_LOCATIONCHANGE, EVENT_OBJECT_REORDER, EVENT_SYSTEM_FOREGROUND,
    EVENT_SYSTEM_MINIMIZEEND, EVENT_SYSTEM_MINIMIZESTART, EVENT_SYSTEM_MOVESIZEEND, HANDLE,
    HWND, HWINEVENTHOOK, INFINITE, MSG, OBJID_CLIENT, OBJID_WINDOW, PBT_APMRESUMEAUTOMATIC,
    PBT_APMRESUMESUSPEND, WINEVENT_OUTOFCONTEXT, WINEVENT_SKIPOWNPROCESS, WM_CLOSE, WM_DESTROY,
    WM_DEVICECHANGE, WM_DISPLAYCHANGE, WM_DPICHANGED, WM_POWERBROADCAST, WM_SETTINGCHANGE,
    WM_TIMER, WNDCLASSW, WS_EX_TOOLWINDOW, WS_OVERLAPPED,
};
use katt::{
    light_theme, merge_devices, w, widget, WM_POLL_DONE, WM_POLL_NOW, WM_SYNC_WIDGET,
};

const POLL_TIMER_ID: usize = 100;
const DEVICECHANGE_DEBOUNCE_TIMER_ID: usize = 101;
const WIDGET_SYNC_TIMER_ID: usize = 102;
const POLL_INTERVAL_MS: u32 = 45_000;
const WIDGET_SYNC_INTERVAL_MS: u32 = 1_000;

static TASKBAR_CREATED: AtomicUsize = AtomicUsize::new(0);
static MAIN_HWND_RAW: AtomicUsize = AtomicUsize::new(0);
static DESKTOP_HWND_RAW: AtomicUsize = AtomicUsize::new(0);
static TRAY_HWND_RAW: AtomicUsize = AtomicUsize::new(0);
static NOTIFY_HWND_RAW: AtomicUsize = AtomicUsize::new(0);
static SYNC_PENDING: AtomicBool = AtomicBool::new(false);
static APP_INIT: AtomicBool = AtomicBool::new(false);

static POLL_EVENT_RAW: AtomicUsize = AtomicUsize::new(0);
static POLL_FORCE_RESCAN: AtomicBool = AtomicBool::new(false);
static POLL_STOP: AtomicBool = AtomicBool::new(false);
static POLL_HAS_DATA: AtomicBool = AtomicBool::new(false);
static SLOT_LOCK: AtomicBool = AtomicBool::new(false);

#[cfg(not(test))]
#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    unsafe { ExitProcess(1) }
}

struct AppState {
    active: bool,
    main: HWND,
    widget: Option<WidgetWindow>,
    syncing: bool,
    devices: BatteryList,
    light_theme: bool,
    tray_pid: u32,
    tray_tid: u32,
    tray_loc_hook: HWINEVENTHOOK,
    tray_reorder_hook: HWINEVENTHOOK,
    worker_thread: HANDLE,
    poll_event: HANDLE,
}

impl AppState {
    unsafe fn init_in_place(
        dst: *mut Self,
        main: HWND,
        light: bool,
        worker_thread: HANDLE,
        poll_event: HANDLE,
    ) {
        addr_of_mut!((*dst).active).write(true);
        addr_of_mut!((*dst).main).write(main);
        addr_of_mut!((*dst).widget).write(None);
        addr_of_mut!((*dst).syncing).write(false);
        addr_of_mut!((*dst).devices).write(BatteryList::new());
        addr_of_mut!((*dst).light_theme).write(light);
        addr_of_mut!((*dst).tray_pid).write(0);
        addr_of_mut!((*dst).tray_tid).write(0);
        addr_of_mut!((*dst).tray_loc_hook).write(null_mut());
        addr_of_mut!((*dst).tray_reorder_hook).write(null_mut());
        addr_of_mut!((*dst).worker_thread).write(worker_thread);
        addr_of_mut!((*dst).poll_event).write(poll_event);
    }
}

// Stored in `.bss` via `MaybeUninit::zeroed()` so `.data` stays 0 bytes on disk.
static APP: SingleThreadCell<MaybeUninit<AppState>> = SingleThreadCell::new(MaybeUninit::zeroed());
static SCANNER: SingleThreadCell<MaybeUninit<HidScanner>> =
    SingleThreadCell::new(MaybeUninit::zeroed());
static POLL_SLOT: SingleThreadCell<MaybeUninit<PollOutput>> =
    SingleThreadCell::new(MaybeUninit::zeroed());

#[inline]
unsafe fn app_mut() -> Option<&'static mut AppState> {
    if APP_INIT.load(Ordering::Acquire) {
        Some((*APP.get()).assume_init_mut())
    } else {
        None
    }
}

#[inline]
fn lock_slot() {
    while SLOT_LOCK
        .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        core::hint::spin_loop();
    }
}

#[inline]
fn unlock_slot() {
    SLOT_LOCK.store(false, Ordering::Release);
}

unsafe extern "system" fn poll_worker_proc(_param: *mut c_void) -> u32 {
    let scanner_ptr = (*SCANNER.get()).as_mut_ptr();
    HidScanner::init_in_place(scanner_ptr);
    let scanner = &mut *scanner_ptr;

    loop {
        let ev_raw = POLL_EVENT_RAW.load(Ordering::Acquire);
        if ev_raw == 0 || POLL_STOP.load(Ordering::Acquire) {
            break;
        }
        let _ = WaitForSingleObject(ev_raw as HANDLE, INFINITE);
        if POLL_STOP.load(Ordering::Acquire) {
            break;
        }

        let force = POLL_FORCE_RESCAN.swap(false, Ordering::AcqRel);
        let out = scanner.poll(force);
        if POLL_STOP.load(Ordering::Acquire) {
            break;
        }

        lock_slot();
        (*POLL_SLOT.get()).write(out);
        POLL_HAS_DATA.store(true, Ordering::Release);
        unlock_slot();

        let main_raw = MAIN_HWND_RAW.load(Ordering::Acquire);
        if main_raw != 0 {
            let _ = PostMessageW(main_raw as HWND, WM_POLL_DONE, 0, 0);
        }
    }

    scanner.close();
    0
}

unsafe fn request_poll(force_rescan: bool) {
    if force_rescan {
        POLL_FORCE_RESCAN.store(true, Ordering::Release);
    }
    let ev_raw = POLL_EVENT_RAW.load(Ordering::Acquire);
    if ev_raw != 0 {
        let _ = SetEvent(ev_raw as HANDLE);
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
    if state.tray_pid == pid
        && state.tray_tid == tid
        && !state.tray_loc_hook.is_null()
        && !state.tray_reorder_hook.is_null()
    {
        return;
    }
    if !state.tray_loc_hook.is_null() {
        let old_hook = state.tray_loc_hook;
        state.tray_loc_hook = null_mut();
        let _ = UnhookWinEvent(old_hook);
    }
    if !state.tray_reorder_hook.is_null() {
        let old_hook = state.tray_reorder_hook;
        state.tray_reorder_hook = null_mut();
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
    state.tray_reorder_hook = SetWinEventHook(
        EVENT_OBJECT_REORDER,
        EVENT_OBJECT_REORDER,
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

unsafe fn handle_poll_done() {
    lock_slot();
    let out = if POLL_HAS_DATA.swap(false, Ordering::AcqRel) {
        Some((*POLL_SLOT.get()).assume_init_read())
    } else {
        None
    };
    unlock_slot();

    let Some(out) = out else { return };
    let active = {
        let Some(state) = app_mut() else { return };
        if !state.active {
            false
        } else {
            state.devices = merge_devices(
                &state.devices,
                out.devices,
                &out.present_hid_vp[..out.present_hid_vp_len],
            );
            true
        }
    };
    if active {
        sync_widget(false);
    }
}

unsafe fn cleanup_app_state(hwnd: HWND) {
    let _ = KillTimer(hwnd, POLL_TIMER_ID);
    let _ = KillTimer(hwnd, DEVICECHANGE_DEBOUNCE_TIMER_ID);
    let _ = KillTimer(hwnd, WIDGET_SYNC_TIMER_ID);

    MAIN_HWND_RAW.store(0, Ordering::Release);
    DESKTOP_HWND_RAW.store(0, Ordering::Relaxed);
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
    let reorder_hook = state.tray_reorder_hook;
    state.tray_reorder_hook = null_mut();
    let worker = state.worker_thread;
    state.worker_thread = null_mut();
    let poll_ev = state.poll_event;
    state.poll_event = null_mut();

    POLL_STOP.store(true, Ordering::Release);
    if !poll_ev.is_null() {
        let _ = SetEvent(poll_ev);
    }
    if !worker.is_null() {
        let _ = WaitForSingleObject(worker, 1500);
        let _ = CloseHandle(worker);
    }
    if !poll_ev.is_null() {
        POLL_EVENT_RAW.store(0, Ordering::Release);
        let _ = CloseHandle(poll_ev);
    }

    for hook in [loc_hook, reorder_hook] {
        if !hook.is_null() {
            let _ = UnhookWinEvent(hook);
        }
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
    if event == EVENT_OBJECT_REORDER {
        if id_object != OBJID_CLIENT && id_object != OBJID_WINDOW {
            return;
        }
        let desk = DESKTOP_HWND_RAW.load(Ordering::Relaxed);
        if desk == 0 || (hwnd as usize) != desk {
            return;
        }
    } else {
        if id_object != OBJID_WINDOW {
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
    }
    let raw = MAIN_HWND_RAW.load(Ordering::Relaxed);
    if raw != 0
        && !SYNC_PENDING.swap(true, Ordering::AcqRel)
        && PostMessageW(raw as HWND, WM_SYNC_WIDGET, 0, 0) == 0
    {
        SYNC_PENDING.store(false, Ordering::Release);
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: usize, lp: isize) -> isize {
    match msg {
        WM_POLL_NOW => {
            request_poll(true);
            0
        }
        WM_POLL_DONE => {
            handle_poll_done();
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
            request_poll(true);
            0
        }
        WM_TIMER if wp == POLL_TIMER_ID => {
            request_poll(false);
            0
        }
        WM_TIMER if wp == WIDGET_SYNC_TIMER_ID => {
            sync_widget(false);
            0
        }
        WM_POWERBROADCAST => {
            if wp == PBT_APMRESUMEAUTOMATIC || wp == PBT_APMRESUMESUSPEND {
                request_poll(true);
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

        let instance_mutex = CreateMutexW(null(), 0, w!("Local\\katt.SingleInstance"));
        if instance_mutex.is_null() {
            ExitProcess(1);
        }
        if GetLastError() == ERROR_ALREADY_EXISTS {
            let _ = CloseHandle(instance_mutex);
            ExitProcess(0);
        }

        let hinst = GetModuleHandleW(null());
        let class = w!("KattMainClass");
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
            w!("katt"),
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

        MAIN_HWND_RAW.store(hwnd as usize, Ordering::Release);
        DESKTOP_HWND_RAW.store(GetDesktopWindow() as usize, Ordering::Relaxed);
        TASKBAR_CREATED.store(
            RegisterWindowMessageW(w!("TaskbarCreated")) as usize,
            Ordering::Relaxed,
        );

        let poll_event = CreateEventW(null(), 0, 0, null());
        let worker_thread = if !poll_event.is_null() {
            POLL_EVENT_RAW.store(poll_event as usize, Ordering::Release);
            CreateThread(null(), 64 * 1024, poll_worker_proc, null_mut(), 0, null_mut())
        } else {
            null_mut()
        };

        AppState::init_in_place(
            (*APP.get()).as_mut_ptr(),
            hwnd,
            light_theme(),
            worker_thread,
            poll_event,
        );
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
