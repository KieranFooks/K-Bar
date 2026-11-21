//! Pure `raw-dylib` Win32 FFI bindings and Win64 ABI structs (`#![no_std]`, zero dependencies).
//!
//! Imports ONLY from `kernel32`, `kernelbase`, `cfgmgr32`, `imm32`, `user32`, and `gdi32`,
//! completely avoiding `advapi32`, `sechost`, `hid`, `oleaut32`, `shell32`,
//! `shcore`, `dwmapi`, `setupapi`, and `MSCTF.dll`.

use core::cell::UnsafeCell;
use core::ffi::c_void;

pub type BOOL = i32;
pub type HANDLE = *mut c_void;
pub type HWND = *mut c_void;
pub type HDC = *mut c_void;
pub type HGDIOBJ = *mut c_void;
pub type HBITMAP = *mut c_void;
pub type HFONT = *mut c_void;
pub type HINSTANCE = *mut c_void;
pub type HMONITOR = *mut c_void;
pub type HWINEVENTHOOK = *mut c_void;
pub type COLORREF = u32;

pub type WndProc = Option<unsafe extern "system" fn(HWND, u32, usize, isize) -> isize>;
pub type WinEventProc =
    Option<unsafe extern "system" fn(HWINEVENTHOOK, u32, HWND, i32, i32, u32, u32)>;

/// Single-threaded static container without `RefCell` / `Mutex` / `OnceLock` runtime overhead.
pub struct SingleThreadCell<T>(UnsafeCell<T>);
unsafe impl<T> Sync for SingleThreadCell<T> {}

impl<T> SingleThreadCell<T> {
    pub const fn new(val: T) -> Self {
        Self(UnsafeCell::new(val))
    }

    /// Return raw mutable pointer to inner value.
    ///
    /// # Safety
    /// Caller must ensure single-threaded access with no aliasing `&mut` references across callbacks.
    pub unsafe fn get(&self) -> *mut T {
        self.0.get()
    }
}

/// Compile-time null-terminated UTF-16 literal helper (ASCII inputs).
pub const fn utf16_z<const N: usize>(s: &str) -> [u16; N] {
    let bytes = s.as_bytes();
    assert!(bytes.len() + 1 == N);
    let mut out = [0u16; N];
    let mut i = 0;
    while i < bytes.len() {
        assert!(bytes[i] < 0x80);
        out[i] = bytes[i] as u16;
        i += 1;
    }
    out
}

#[macro_export]
macro_rules! w {
    ($s:literal) => {{
        const LEN: usize = $s.len() + 1;
        const ARR: [u16; LEN] = $crate::win32::utf16_z::<LEN>($s);
        ARR.as_ptr()
    }};
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct POINT {
    pub x: i32,
    pub y: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct SIZE {
    pub cx: i32,
    pub cy: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct RECT {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct MONITORINFO {
    pub cb_size: u32,
    pub rc_monitor: RECT,
    pub rc_work: RECT,
    pub dw_flags: u32,
}

impl Default for MONITORINFO {
    fn default() -> Self {
        Self {
            cb_size: core::mem::size_of::<Self>() as u32,
            rc_monitor: RECT::default(),
            rc_work: RECT::default(),
            dw_flags: 0,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct WNDCLASSW {
    pub style: u32,
    pub lpfn_wnd_proc: WndProc,
    pub cb_cls_extra: i32,
    pub cb_wnd_extra: i32,
    pub h_instance: HINSTANCE,
    pub h_icon: HANDLE,
    pub h_cursor: HANDLE,
    pub hbr_background: HANDLE,
    pub lpsz_menu_name: *const u16,
    pub lpsz_class_name: *const u16,
}

impl Default for WNDCLASSW {
    fn default() -> Self {
        Self {
            style: 0,
            lpfn_wnd_proc: None,
            cb_cls_extra: 0,
            cb_wnd_extra: 0,
            h_instance: core::ptr::null_mut(),
            h_icon: core::ptr::null_mut(),
            h_cursor: core::ptr::null_mut(),
            hbr_background: core::ptr::null_mut(),
            lpsz_menu_name: core::ptr::null(),
            lpsz_class_name: core::ptr::null(),
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct MSG {
    pub hwnd: HWND,
    pub message: u32,
    pub w_param: usize,
    pub l_param: isize,
    pub time: u32,
    pub pt: POINT,
}

impl Default for MSG {
    fn default() -> Self {
        Self {
            hwnd: core::ptr::null_mut(),
            message: 0,
            w_param: 0,
            l_param: 0,
            time: 0,
            pt: POINT::default(),
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct OVERLAPPED {
    pub internal: usize,
    pub internal_high: usize,
    pub offset: u32,
    pub offset_high: u32,
    pub h_event: HANDLE,
}

impl OVERLAPPED {
    pub const fn with_event(ev: HANDLE) -> Self {
        Self {
            internal: 0,
            internal_high: 0,
            offset: 0,
            offset_high: 0,
            h_event: ev,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct RAWINPUTDEVICELIST {
    pub h_device: HANDLE,
    pub dw_type: u32,
}

impl Default for RAWINPUTDEVICELIST {
    fn default() -> Self {
        Self {
            h_device: core::ptr::null_mut(),
            dw_type: 0,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct RID_DEVICE_INFO_HID {
    pub dw_vendor_id: u32,
    pub dw_product_id: u32,
    pub dw_version_number: u32,
    pub us_usage_page: u16,
    pub us_usage: u16,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub union RID_DEVICE_INFO_UNION {
    pub hid: RID_DEVICE_INFO_HID,
    pub _keyboard_pad: [u32; 6],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct RID_DEVICE_INFO {
    pub cb_size: u32,
    pub dw_type: u32,
    pub u: RID_DEVICE_INFO_UNION,
}

impl Default for RID_DEVICE_INFO {
    fn default() -> Self {
        Self {
            cb_size: core::mem::size_of::<Self>() as u32,
            dw_type: 0,
            u: RID_DEVICE_INFO_UNION { _keyboard_pad: [0; 6] },
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct BITMAPINFOHEADER {
    pub bi_size: u32,
    pub bi_width: i32,
    pub bi_height: i32,
    pub bi_planes: u16,
    pub bi_bit_count: u16,
    pub bi_compression: u32,
    pub bi_size_image: u32,
    pub bi_x_pels_per_meter: i32,
    pub bi_y_pels_per_meter: i32,
    pub bi_clr_used: u32,
    pub bi_clr_important: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct BITMAPINFO {
    pub bmi_header: BITMAPINFOHEADER,
    pub bmi_colors: [u32; 1],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct BLENDFUNCTION {
    pub blend_op: u8,
    pub blend_flags: u8,
    pub source_constant_alpha: u8,
    pub alpha_format: u8,
}

#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct GUID {
    pub data1: u32,
    pub data2: u16,
    pub data3: u16,
    pub data4: [u8; 8],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct DEVPROPKEY {
    pub fmtid: GUID,
    pub pid: u32,
}

// Compile-time Win64 C ABI verification
const _: () = assert!(core::mem::size_of::<POINT>() == 8);
const _: () = assert!(core::mem::size_of::<SIZE>() == 8);
const _: () = assert!(core::mem::size_of::<RECT>() == 16);
const _: () = assert!(core::mem::size_of::<MONITORINFO>() == 40);
const _: () = assert!(core::mem::size_of::<WNDCLASSW>() == 72);
const _: () = assert!(core::mem::size_of::<MSG>() == 48);
const _: () = assert!(core::mem::size_of::<OVERLAPPED>() == 32);
const _: () = assert!(core::mem::size_of::<RAWINPUTDEVICELIST>() == 16);
const _: () = assert!(core::mem::size_of::<RID_DEVICE_INFO_HID>() == 16);
const _: () = assert!(core::mem::size_of::<RID_DEVICE_INFO>() == 32);
const _: () = assert!(core::mem::size_of::<BITMAPINFOHEADER>() == 40);
const _: () = assert!(core::mem::size_of::<BITMAPINFO>() == 44);
const _: () = assert!(core::mem::size_of::<BLENDFUNCTION>() == 4);
const _: () = assert!(core::mem::size_of::<GUID>() == 16);
const _: () = assert!(core::mem::size_of::<DEVPROPKEY>() == 20);

// Win32 Constants
pub const INVALID_HANDLE_VALUE: HANDLE = (-1isize) as HANDLE;
pub const ERROR_SUCCESS: i32 = 0;
pub const ERROR_INVALID_PARAMETER: u32 = 87;
pub const ERROR_ALREADY_EXISTS: u32 = 183;
pub const ERROR_IO_PENDING: u32 = 997;
pub const ERROR_INVALID_USER_BUFFER: u32 = 1784;
pub const WAIT_OBJECT_0: u32 = 0;
pub const INFINITE: u32 = 0xFFFF_FFFF;

pub const GENERIC_READ: u32 = 0x8000_0000;
pub const GENERIC_WRITE: u32 = 0x4000_0000;
pub const FILE_SHARE_READ: u32 = 1;
pub const FILE_SHARE_WRITE: u32 = 2;
pub const OPEN_EXISTING: u32 = 3;
pub const FILE_FLAG_OVERLAPPED: u32 = 0x4000_0000;

pub const IOCTL_HID_SET_FEATURE: u32 = 0x000B_0191;
pub const IOCTL_HID_GET_FEATURE: u32 = 0x000B_0192;
pub const IOCTL_HID_GET_PRODUCT_STRING: u32 = 0x000B_01BE;

pub const CR_SUCCESS: u32 = 0;
pub const CM_GETIDLIST_FILTER_ENUMERATOR: u32 = 0x0000_0001;
pub const CM_GETIDLIST_FILTER_PRESENT: u32 = 0x0000_0100;
pub const CM_LOCATE_DEVNODE_NORMAL: u32 = 0;
pub const CM_LOCATE_DEVNODE_PHANTOM: u32 = 1;
pub const DEVPROP_TYPE_BYTE: u32 = 3;
pub const DEVPROP_TYPE_UINT16: u32 = 5;
pub const DEVPROP_TYPE_UINT32: u32 = 7;
pub const DEVPROP_TYPE_BOOLEAN: u32 = 17;
pub const DEVPROP_TYPE_STRING: u32 = 18;
pub const DEVPROP_TYPE_STRING_LIST: u32 = 0x2012;

pub const HKEY_CURRENT_USER: isize = (-2147483647i32) as isize;
pub const RRF_RT_REG_DWORD: u32 = 0x0000_0010;

pub const DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2: isize = -4;

pub const GWL_STYLE: i32 = -16;
pub const GWL_EXSTYLE: i32 = -20;

pub const WS_OVERLAPPED: u32 = 0x0000_0000;
pub const WS_POPUP: u32 = 0x8000_0000;
pub const WS_CAPTION: u32 = 0x00C0_0000;
pub const WS_THICKFRAME: u32 = 0x0004_0000;
pub const WS_EX_TOPMOST: u32 = 0x0000_0008;
pub const WS_EX_TRANSPARENT: u32 = 0x0000_0020;
pub const WS_EX_TOOLWINDOW: u32 = 0x0000_0080;
pub const WS_EX_LAYERED: u32 = 0x0008_0000;
pub const WS_EX_NOACTIVATE: u32 = 0x0800_0000;

pub const HWND_TOPMOST: HWND = (-1isize) as HWND;
pub const GW_HWNDPREV: u32 = 3;
pub const SW_HIDE: i32 = 0;
pub const SW_SHOWNOACTIVATE: i32 = 4;
pub const SWP_NOSIZE: u32 = 0x0001;
pub const SWP_NOMOVE: u32 = 0x0002;
pub const SWP_NOZORDER: u32 = 0x0004;
pub const SWP_NOACTIVATE: u32 = 0x0010;
pub const SWP_SHOWWINDOW: u32 = 0x0040;
pub const SWP_HIDEWINDOW: u32 = 0x0080;
pub const SWP_NOOWNERZORDER: u32 = 0x0200;
pub const LWA_ALPHA: u32 = 0x0000_0002;
pub const ULW_ALPHA: u32 = 0x0000_0002;

pub const MONITOR_DEFAULTTONEAREST: u32 = 2;

pub const WM_DESTROY: u32 = 0x0002;
pub const WM_CLOSE: u32 = 0x0010;
pub const WM_ERASEBKGND: u32 = 0x0014;
pub const WM_SETTINGCHANGE: u32 = 0x001A;
pub const WM_MOUSEACTIVATE: u32 = 0x0021;
pub const WM_DISPLAYCHANGE: u32 = 0x007E;
pub const WM_NCHITTEST: u32 = 0x0084;
pub const WM_TIMER: u32 = 0x0113;
pub const WM_POWERBROADCAST: u32 = 0x0218;
pub const WM_DEVICECHANGE: u32 = 0x0219;
pub const WM_DPICHANGED: u32 = 0x02E0;
pub const MA_NOACTIVATE: isize = 3;
pub const HTTRANSPARENT: isize = -1;
pub const PBT_APMRESUMESUSPEND: usize = 0x0007;
pub const PBT_APMRESUMEAUTOMATIC: usize = 0x0012;

pub const EVENT_SYSTEM_FOREGROUND: u32 = 0x0003;
pub const EVENT_SYSTEM_MOVESIZEEND: u32 = 0x000B;
pub const EVENT_SYSTEM_MINIMIZESTART: u32 = 0x0016;
pub const EVENT_SYSTEM_MINIMIZEEND: u32 = 0x0017;
pub const EVENT_OBJECT_REORDER: u32 = 0x8004;
pub const EVENT_OBJECT_LOCATIONCHANGE: u32 = 0x800B;
pub const OBJID_WINDOW: i32 = 0;
pub const OBJID_CLIENT: i32 = -4;
pub const WINEVENT_OUTOFCONTEXT: u32 = 0x0000;
pub const WINEVENT_SKIPOWNPROCESS: u32 = 0x0002;

pub const RIM_TYPEHID: u32 = 2;
pub const RIDI_DEVICENAME: u32 = 0x2000_0007;
pub const RIDI_DEVICEINFO: u32 = 0x2000_000B;

pub const BI_RGB: u32 = 0;
pub const DIB_RGB_COLORS: u32 = 0;
pub const AC_SRC_OVER: u8 = 0;
pub const AC_SRC_ALPHA: u8 = 1;
pub const TRANSPARENT: i32 = 1;
pub const DEFAULT_CHARSET: u32 = 1;
pub const OUT_DEFAULT_PRECIS: u32 = 0;
pub const CLIP_DEFAULT_PRECIS: u32 = 0;
pub const ANTIALIASED_QUALITY: u32 = 4;

pub const DT_RIGHT: u32 = 0x0002;
pub const DT_CENTER: u32 = 0x0001;
pub const DT_VCENTER: u32 = 0x0004;
pub const DT_SINGLELINE: u32 = 0x0020;
pub const DT_NOCLIP: u32 = 0x0100;
pub const DT_NOPREFIX: u32 = 0x0800;

#[link(name = "kernel32", kind = "raw-dylib")]
extern "system" {
    pub fn CreateMutexW(sec: *const c_void, initial_owner: BOOL, name: *const u16) -> HANDLE;
    pub fn GetLastError() -> u32;
    pub fn CloseHandle(h: HANDLE) -> BOOL;
    pub fn ExitProcess(code: u32) -> !;
    pub fn GetModuleHandleW(name: *const u16) -> HINSTANCE;
    pub fn CreateFileW(
        name: *const u16,
        access: u32,
        share: u32,
        sec: *const c_void,
        disp: u32,
        flags: u32,
        tmpl: HANDLE,
    ) -> HANDLE;
    pub fn ReadFile(
        h: HANDLE,
        buf: *mut u8,
        len: u32,
        read: *mut u32,
        ov: *mut OVERLAPPED,
    ) -> BOOL;
    pub fn WriteFile(
        h: HANDLE,
        buf: *const u8,
        len: u32,
        written: *mut u32,
        ov: *mut OVERLAPPED,
    ) -> BOOL;
    pub fn DeviceIoControl(
        h: HANDLE,
        code: u32,
        in_buf: *const c_void,
        in_len: u32,
        out_buf: *mut c_void,
        out_len: u32,
        bytes_ret: *mut u32,
        ov: *mut OVERLAPPED,
    ) -> BOOL;
    pub fn CreateEventW(
        sec: *const c_void,
        manual_reset: BOOL,
        initial_state: BOOL,
        name: *const u16,
    ) -> HANDLE;
    pub fn SetEvent(h: HANDLE) -> BOOL;
    pub fn ResetEvent(h: HANDLE) -> BOOL;
    pub fn WaitForSingleObject(h: HANDLE, ms: u32) -> u32;
    pub fn CancelIoEx(h: HANDLE, ov: *mut OVERLAPPED) -> BOOL;
    pub fn GetOverlappedResult(
        h: HANDLE,
        ov: *mut OVERLAPPED,
        transferred: *mut u32,
        wait: BOOL,
    ) -> BOOL;
    pub fn GetTickCount64() -> u64;
    pub fn Sleep(ms: u32);
    pub fn CreateThread(
        sec: *const c_void,
        stack_size: usize,
        start: unsafe extern "system" fn(*mut c_void) -> u32,
        param: *mut c_void,
        flags: u32,
        tid: *mut u32,
    ) -> HANDLE;
}

#[link(name = "cfgmgr32", kind = "raw-dylib")]
extern "system" {
    pub fn CM_Get_Device_ID_List_SizeW(
        pul_len: *mut u32,
        psz_filter: *const u16,
        ul_flags: u32,
    ) -> u32;
    pub fn CM_Get_Device_ID_ListW(
        psz_filter: *const u16,
        buffer: *mut u16,
        buffer_len: u32,
        ul_flags: u32,
    ) -> u32;
    pub fn CM_Locate_DevNodeW(
        pdn_dev_inst: *mut u32,
        p_device_id: *const u16,
        ul_flags: u32,
    ) -> u32;
    pub fn CM_Get_DevNode_PropertyW(
        dn_dev_inst: u32,
        property_key: *const DEVPROPKEY,
        property_type: *mut u32,
        property_buffer: *mut u8,
        property_buffer_size: *mut u32,
        ul_flags: u32,
    ) -> u32;
    pub fn CM_Get_Parent(pdn_dev_inst: *mut u32, dn_dev_inst: u32, ul_flags: u32) -> u32;
}

#[link(name = "kernelbase", kind = "raw-dylib")]
extern "system" {
    pub fn RegGetValueW(
        hkey: isize,
        subkey: *const u16,
        value: *const u16,
        flags: u32,
        rtype: *mut u32,
        data: *mut c_void,
        cb: *mut u32,
    ) -> i32;
}

#[link(name = "imm32", kind = "raw-dylib")]
extern "system" {
    pub fn ImmDisableIME(thread_id: u32) -> BOOL;
}

#[link(name = "user32", kind = "raw-dylib")]
extern "system" {
    pub fn SetProcessDpiAwarenessContext(value: isize) -> BOOL;
    pub fn RegisterClassW(wc: *const WNDCLASSW) -> u16;
    pub fn CreateWindowExW(
        ex_style: u32,
        class_name: *const u16,
        window_name: *const u16,
        style: u32,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        parent: HWND,
        menu: HANDLE,
        inst: HINSTANCE,
        param: *const c_void,
    ) -> HWND;
    pub fn DestroyWindow(hwnd: HWND) -> BOOL;
    pub fn DefWindowProcW(hwnd: HWND, msg: u32, wp: usize, lp: isize) -> isize;
    pub fn GetMessageW(msg: *mut MSG, hwnd: HWND, min: u32, max: u32) -> BOOL;
    pub fn DispatchMessageW(msg: *const MSG) -> isize;
    pub fn PostMessageW(hwnd: HWND, msg: u32, wp: usize, lp: isize) -> BOOL;
    pub fn PostQuitMessage(exit_code: i32);
    pub fn RegisterWindowMessageW(name: *const u16) -> u32;
    pub fn SetTimer(hwnd: HWND, id: usize, elapse_ms: u32, proc: *const c_void) -> usize;
    pub fn KillTimer(hwnd: HWND, id: usize) -> BOOL;
    pub fn SetWinEventHook(
        min: u32,
        max: u32,
        hmod: HINSTANCE,
        proc: WinEventProc,
        pid: u32,
        tid: u32,
        flags: u32,
    ) -> HWINEVENTHOOK;
    pub fn UnhookWinEvent(hook: HWINEVENTHOOK) -> BOOL;
    pub fn FindWindowW(class_name: *const u16, window_name: *const u16) -> HWND;
    pub fn FindWindowExW(
        parent: HWND,
        child_after: HWND,
        class_name: *const u16,
        window_name: *const u16,
    ) -> HWND;
    pub fn GetWindowRect(hwnd: HWND, rc: *mut RECT) -> BOOL;
    pub fn GetDesktopWindow() -> HWND;
    pub fn IsWindow(hwnd: HWND) -> BOOL;
    pub fn IsWindowVisible(hwnd: HWND) -> BOOL;
    pub fn IsIconic(hwnd: HWND) -> BOOL;
    pub fn IsZoomed(hwnd: HWND) -> BOOL;
    pub fn GetWindowLongW(hwnd: HWND, index: i32) -> i32;
    pub fn ShowWindow(hwnd: HWND, cmd: i32) -> BOOL;
    pub fn SetWindowPos(
        hwnd: HWND,
        after: HWND,
        x: i32,
        y: i32,
        cx: i32,
        cy: i32,
        flags: u32,
    ) -> BOOL;
    pub fn GetForegroundWindow() -> HWND;
    pub fn GetWindow(hwnd: HWND, cmd: u32) -> HWND;
    pub fn GetWindowThreadProcessId(hwnd: HWND, pid: *mut u32) -> u32;
    pub fn GetClassNameW(hwnd: HWND, buf: *mut u16, max_count: i32) -> i32;
    pub fn GetDC(hwnd: HWND) -> HDC;
    pub fn ReleaseDC(hwnd: HWND, hdc: HDC) -> i32;
    pub fn GetLayeredWindowAttributes(
        hwnd: HWND,
        pcr_key: *mut COLORREF,
        pb_alpha: *mut u8,
        pdw_flags: *mut u32,
    ) -> BOOL;
    pub fn UpdateLayeredWindow(
        hwnd: HWND,
        hdc_dst: HDC,
        ppt_dst: *const POINT,
        psize: *const SIZE,
        hdc_src: HDC,
        ppt_src: *const POINT,
        cr_key: COLORREF,
        pblend: *const BLENDFUNCTION,
        flags: u32,
    ) -> BOOL;
    pub fn GetDpiForWindow(hwnd: HWND) -> u32;
    pub fn MonitorFromWindow(hwnd: HWND, flags: u32) -> HMONITOR;
    pub fn GetMonitorInfoW(hmon: HMONITOR, lpmi: *mut MONITORINFO) -> BOOL;
    pub fn GetRawInputDeviceList(
        list: *mut RAWINPUTDEVICELIST,
        num_devices: *mut u32,
        cb_size: u32,
    ) -> u32;
    pub fn GetRawInputDeviceInfoW(
        device: HANDLE,
        command: u32,
        data: *mut c_void,
        pcb_size: *mut u32,
    ) -> u32;
    pub fn DrawTextW(
        hdc: HDC,
        text: *const u16,
        cch_text: i32,
        lprc: *mut RECT,
        format: u32,
    ) -> i32;
}

#[link(name = "gdi32", kind = "raw-dylib")]
extern "system" {
    pub fn CreateCompatibleDC(hdc: HDC) -> HDC;
    pub fn CreateDIBSection(
        hdc: HDC,
        pbmi: *const BITMAPINFO,
        usage: u32,
        ppv_bits: *mut *mut c_void,
        h_section: HANDLE,
        offset: u32,
    ) -> HBITMAP;
    pub fn CreateFontW(
        height: i32,
        width: i32,
        esc: i32,
        orient: i32,
        weight: i32,
        italic: u32,
        underline: u32,
        strike: u32,
        charset: u32,
        out_precis: u32,
        clip_precis: u32,
        quality: u32,
        pitch: u32,
        face: *const u16,
    ) -> HFONT;
    pub fn SelectObject(hdc: HDC, h: HGDIOBJ) -> HGDIOBJ;
    pub fn DeleteObject(ho: HGDIOBJ) -> BOOL;
    pub fn DeleteDC(hdc: HDC) -> BOOL;
    pub fn SetBkMode(hdc: HDC, mode: i32) -> i32;
    pub fn SetTextColor(hdc: HDC, color: COLORREF) -> COLORREF;
}
