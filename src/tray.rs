//! Thin wrapper around Shell_NotifyIconW and Shell_NotifyIconGetRect.

use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::UI::Shell::{
    Shell_NotifyIconGetRect, Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD,
    NIM_DELETE, NIM_MODIFY, NIM_SETVERSION, NOTIFYICONDATAW, NOTIFYICONIDENTIFIER,
    NOTIFYICON_VERSION_4,
};
use windows::Win32::UI::WindowsAndMessaging::HICON;

fn make_nid(hwnd: HWND, id: u32, cb: u32, icon: HICON, tip: &str) -> NOTIFYICONDATAW {
    let mut nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: id,
        uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
        uCallbackMessage: cb,
        hIcon: icon,
        ..Default::default()
    };
    for (i, c) in tip.encode_utf16().take(127).enumerate() {
        nid.szTip[i] = c;
    }
    nid
}

/// Register a taskbar notification icon (`NIM_ADD` + `NOTIFYICON_VERSION_4`).
///
/// # Safety
/// `hwnd` must be a valid window handle owned by the current thread and `icon` a valid `HICON`.
pub unsafe fn add(hwnd: HWND, id: u32, cb: u32, icon: HICON, tip: &str) -> bool {
    let mut nid = make_nid(hwnd, id, cb, icon, tip);
    let ok = Shell_NotifyIconW(NIM_ADD, &nid).as_bool();
    nid.Anonymous.uVersion = NOTIFYICON_VERSION_4;
    let _ = Shell_NotifyIconW(NIM_SETVERSION, &nid);
    ok
}

/// Update an existing taskbar notification icon (`NIM_MODIFY`).
///
/// # Safety
/// `hwnd` must be a valid window handle and `icon` a valid `HICON`.
pub unsafe fn modify(hwnd: HWND, id: u32, cb: u32, icon: HICON, tip: &str) -> bool {
    let nid = make_nid(hwnd, id, cb, icon, tip);
    Shell_NotifyIconW(NIM_MODIFY, &nid).as_bool()
}

/// Remove a taskbar notification icon (`NIM_DELETE`).
///
/// # Safety
/// `hwnd` must be a valid window handle previously passed to [`add`].
pub unsafe fn delete(hwnd: HWND, id: u32) -> bool {
    let nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: id,
        ..Default::default()
    };
    Shell_NotifyIconW(NIM_DELETE, &nid).as_bool()
}

/// Query the screen-space bounding rectangle of a tray icon (useful for anchoring flyouts).
///
/// # Safety
/// `hwnd` must be a valid window handle previously passed to [`add`].
pub unsafe fn get_rect(hwnd: HWND, id: u32) -> Option<RECT> {
    let ident = NOTIFYICONIDENTIFIER {
        cbSize: std::mem::size_of::<NOTIFYICONIDENTIFIER>() as u32,
        hWnd: hwnd,
        uID: id,
        ..Default::default()
    };
    Shell_NotifyIconGetRect(&ident).ok()
}
