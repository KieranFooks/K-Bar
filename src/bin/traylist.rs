//! Debug helper: dump the shell tray window tree and list notification-area
//! toolbar buttons (read-only SendMessage calls only).

use windows::core::{w, PCWSTR, PWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::Controls::{TBBUTTON, TB_BUTTONCOUNT, TB_GETBUTTON, TB_GETBUTTONTEXT};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumChildWindows, EnumWindows, FindWindowExW, FindWindowW, GetClassNameW, SendMessageW,
};

fn class_name(hwnd: HWND) -> String {
    let mut buf = [0u16; 128];
    let n = unsafe { GetClassNameW(hwnd, &mut buf) };
    let s = unsafe { PWSTR(buf.as_mut_ptr()).to_string().unwrap_or_default() };
    if (n as usize) < s.len() {
        s[..n as usize].to_string()
    } else {
        s
    }
}

fn child(parent: HWND, class: &str) -> Option<HWND> {
    let c: Vec<u16> = class.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe { FindWindowExW(Some(parent), None, PCWSTR(c.as_ptr()), PCWSTR::null()).ok() }
}

unsafe fn toolbar_tips(tb: HWND, label: &str) {
    let n = SendMessageW(tb, TB_BUTTONCOUNT, None, None).0 as u32;
    println!("{}: {} buttons", label, n);
    for i in 0..n {
        let mut btn = TBBUTTON::default();
        let _ = SendMessageW(tb, TB_GETBUTTON, Some(WPARAM(i as usize)), Some(LPARAM(&mut btn as *mut TBBUTTON as isize)));
        let mut buf = [0u16; 256];
        let len = SendMessageW(tb, TB_GETBUTTONTEXT, Some(WPARAM(i as usize)), Some(LPARAM(buf.as_mut_ptr() as isize))).0 as usize;
        let text = if len > 0 {
            PWSTR(buf.as_mut_ptr()).to_string().unwrap_or_default()
        } else {
            String::new()
        };
        println!("  [{}] id={} text={:?}", i, btn.idCommand, text);
    }
}

unsafe extern "system" fn enum_cb(hwnd: HWND, _lp: LPARAM) -> windows::core::BOOL {
    let cls = class_name(hwnd);
    if cls.to_lowercase().contains("tray")
        || cls.to_lowercase().contains("notify")
        || cls.to_lowercase().contains("overflow")
        || cls.to_lowercase().contains("pager")
        || cls.to_lowercase().contains("toolbar")
    {
        println!("top: {:?} class={}", hwnd.0, cls);
        let mut found: Vec<HWND> = Vec::new();
        extern "system" fn collect(h: HWND, lp: LPARAM) -> windows::core::BOOL {
            unsafe { (*(lp.0 as *mut Vec<HWND>)).push(h) };
            windows::core::BOOL(1)
        }
        let _ = EnumChildWindows(Some(hwnd), Some(collect), LPARAM(&mut found as *mut Vec<HWND> as isize));
        for c in found {
            println!("   child: {:?} class={}", c.0, class_name(c));
        }
    }
    windows::core::BOOL(1)
}

fn main() {
    unsafe {
        println!("--- window tree scan ---");
        let _ = EnumWindows(Some(enum_cb), LPARAM(0));

        println!("--- toolbars ---");
        if let Ok(tray) = FindWindowW(w!("Shell_TrayWnd"), PCWSTR::null()) {
            if let Some(notify) = child(tray, "TrayNotifyWnd") {
                if let Some(pager) = child(notify, "SysPager") {
                    if let Some(tb) = child(pager, "ToolbarWindow32") {
                        toolbar_tips(tb, "main tray");
                    }
                }
            }
        }
        if let Ok(overflow) = FindWindowW(w!("Notify Icon Overflow Window"), PCWSTR::null()) {
            if let Some(tb) = child(overflow, "ToolbarWindow32") {
                toolbar_tips(tb, "overflow");
            }
        } else {
            println!("overflow window not found at top level");
        }
    }
}
