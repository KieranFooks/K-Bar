//! Thin wrapper around Shell_NotifyIconW (tray icon add/modify/delete).

use windows::Win32::Foundation::HWND;
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY,
    NIM_SETVERSION, NOTIFYICON_VERSION_4, NOTIFYICONDATAW,
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

pub unsafe fn add(hwnd: HWND, id: u32, cb: u32, icon: HICON, tip: &str) -> bool {
    let mut nid = make_nid(hwnd, id, cb, icon, tip);
    let ok = Shell_NotifyIconW(NIM_ADD, &nid).as_bool();
    nid.Anonymous.uVersion = NOTIFYICON_VERSION_4;
    let _ = Shell_NotifyIconW(NIM_SETVERSION, &nid);
    ok
}

pub unsafe fn modify(hwnd: HWND, id: u32, cb: u32, icon: HICON, tip: &str) -> bool {
    let nid = make_nid(hwnd, id, cb, icon, tip);
    Shell_NotifyIconW(NIM_MODIFY, &nid).as_bool()
}

pub unsafe fn delete(hwnd: HWND, id: u32) -> bool {
    let nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: id,
        ..Default::default()
    };
    Shell_NotifyIconW(NIM_DELETE, &nid).as_bool()
}
