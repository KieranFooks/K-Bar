//! Small flyout window shown on tray click: one row per device
//! (Fluent glyph + name + battery glyph + %), plus a "Quitter" row.

use std::ffi::c_void;
use std::sync::Once;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateFontW, CreatePen, CreateRoundRectRgn, CreateSolidBrush, DeleteObject,
    DrawTextW, EndPaint, FillRect, GetStockObject, RoundRect, SelectObject, SetBkMode, SetTextColor,
    SetWindowRgn, ANTIALIASED_QUALITY, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, DT_END_ELLIPSIS,
    DT_LEFT, DT_NOPREFIX, DT_RIGHT, DT_SINGLELINE, DT_TOP, DT_VCENTER, HDC, NULL_BRUSH,
    OUT_DEFAULT_PRECIS, PAINTSTRUCT, PS_SOLID, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetClientRect, PostMessageW, RegisterClassW,
    SetTimer, SetWindowPos, ShowWindow, SystemParametersInfoW, CS_HREDRAW, CS_VREDRAW,
    HWND_TOPMOST, SPI_GETWORKAREA, SW_SHOWNA, SWP_NOACTIVATE, SWP_NOSIZE, SWP_SHOWWINDOW,
    SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, WM_ACTIVATE, WM_CLOSE, WM_DESTROY, WM_KEYDOWN,
    WM_LBUTTONDOWN, WM_PAINT, WM_TIMER, WNDCLASSW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    WS_EX_TOPMOST, WS_POPUP,
};

use crate::{hid::Kind, icon, STATE};

const ROWH: i32 = 36;
const PAD: i32 = 10;
const VK_ESCAPE: u32 = 0x1B;

fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
    COLORREF(r as u32 | (g as u32) << 8 | (b as u32) << 16)
}

fn font(height: i32, face: &str) -> windows::Win32::Graphics::Gdi::HFONT {
    let face_w: Vec<u16> = face.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        CreateFontW(
            height, 0, 0, 0, 400, 0, 0, 0,
            DEFAULT_CHARSET, OUT_DEFAULT_PRECIS, CLIP_DEFAULT_PRECIS, ANTIALIASED_QUALITY, 0,
            PCWSTR(face_w.as_ptr()),
        )
    }
}

/// Show the popup, or close it if already open.
pub unsafe fn toggle(_main: HWND) {
    {
        let mut g = STATE.0.lock().unwrap();
        if let Some(p) = g.as_ref().and_then(|a| a.popup) {
            close(p);
            return;
        }
    }

    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let hinst = GetModuleHandleW(PCWSTR::null()).map(|m| HINSTANCE(m.0)).unwrap_or_default();
        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(popup_proc),
            hInstance: hinst,
            lpszClassName: w!("BatteryTrayPopupClass"),
            ..Default::default()
        };
        RegisterClassW(&wc);
    });

    let n = STATE.0.lock().unwrap().as_ref().map(|a| a.devices.len()).unwrap_or(0);
    let wpx = 280i32;
    let hpx = PAD + (n as i32 + 1) * ROWH + 6;

    if let Ok(p) = CreateWindowExW(
        WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE,
        w!("BatteryTrayPopupClass"),
        w!("Battery Tray"),
        WS_POPUP,
        0, 0, wpx, hpx,
        None, None, None, None,
    ) {
        let mut wa = RECT::default();
        let _ = SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            Some(&mut wa as *mut RECT as *mut c_void),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        );
        let x = wa.right - wpx - 10;
        let y = wa.bottom - hpx - 10;
        let rgn = CreateRoundRectRgn(0, 0, wpx + 1, hpx + 1, 14, 14);
        SetWindowRgn(p, Some(rgn), true);
        let _ = SetWindowPos(p, Some(HWND_TOPMOST), x, y, 0, 0, SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW);
        SetTimer(Some(p), 1, 8000, None);
        let _ = ShowWindow(p, SW_SHOWNA);
        let mut g = STATE.0.lock().unwrap();
        if let Some(a) = g.as_mut() {
            a.popup = Some(p);
        }
    }
}

unsafe fn close(hwnd: HWND) {
    let _ = DestroyWindow(hwnd);
    let mut g = STATE.0.lock().unwrap();
    if let Some(a) = g.as_mut() {
        if a.popup == Some(hwnd) {
            a.popup = None;
        }
    }
}

unsafe fn draw_glyph(hdc: HDC, cp: u32, x: i32, y: i32, size: i32) {
    if let Some(c) = char::from_u32(cp) {
        let mut t: Vec<u16> = c.to_string().encode_utf16().collect();
        let mut rc = RECT { left: x, top: y, right: x + size, bottom: y + size };
        DrawTextW(hdc, &mut t, &mut rc, DT_LEFT | DT_TOP | DT_SINGLELINE | DT_NOPREFIX);
    }
}

unsafe fn paint(hwnd: HWND) {
    let mut ps = PAINTSTRUCT::default();
    let hdc = BeginPaint(hwnd, &mut ps);
    let mut rc = RECT::default();
    let _ = GetClientRect(hwnd, &mut rc);
    let w = rc.right - rc.left;
    let h = rc.bottom - rc.top;

    let (devices, light) = {
        let g = STATE.0.lock().unwrap();
        match g.as_ref() {
            Some(a) => (a.devices.clone(), a.light_theme),
            None => {
                let _ = EndPaint(hwnd, &ps);
                return;
            }
        }
    };

    let (bg, border, fg, sub) = if light {
        (rgb(0xFB, 0xFB, 0xFB), rgb(0xE0, 0xE0, 0xE0), rgb(0x1B, 0x1B, 0x1B), rgb(0x61, 0x61, 0x61))
    } else {
        (rgb(0x20, 0x20, 0x20), rgb(0x3F, 0x3F, 0x3F), rgb(0xFF, 0xFF, 0xFF), rgb(0xB8, 0xB8, 0xB8))
    };

    let b = CreateSolidBrush(bg);
    FillRect(hdc, &rc, b);
    let _ = DeleteObject(b.into());

    let pen = CreatePen(PS_SOLID, 1, border);
    let oldp = SelectObject(hdc, pen.into());
    let oldb = SelectObject(hdc, GetStockObject(NULL_BRUSH));
    let _ = RoundRect(hdc, 0, 0, w, h, 12, 12);
    SelectObject(hdc, oldb);
    SelectObject(hdc, oldp);
    let _ = DeleteObject(pen.into());

    let fglyph = font(-20, "Segoe Fluent Icons");
    let ftext = font(-13, "Segoe UI");
    SetBkMode(hdc, TRANSPARENT);

    for (i, d) in devices.iter().enumerate() {
        let y = PAD + i as i32 * ROWH;
        SelectObject(hdc, fglyph.into());
        SetTextColor(hdc, fg);
        let g = match d.kind {
            Kind::Mouse => icon::MOUSE,
            Kind::Headset => icon::HEADSET,
        };
        draw_glyph(hdc, g, 12, y + 8, 20);

        SelectObject(hdc, ftext.into());
        SetTextColor(hdc, fg);
        let mut name_w: Vec<u16> = d.name.encode_utf16().collect();
        let mut nrc = RECT { left: 42, top: y, right: w - 104, bottom: y + ROWH };
        DrawTextW(hdc, &mut name_w, &mut nrc, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS);

        SetTextColor(hdc, sub);
        let pct_s = match d.pct {
            Some(p) => format!("{} %", p),
            None => "-- %".to_string(),
        };
        let mut pct_w: Vec<u16> = pct_s.encode_utf16().collect();
        let mut prc = RECT { left: w - 100, top: y, right: w - 38, bottom: y + ROWH };
        DrawTextW(hdc, &mut pct_w, &mut prc, DT_RIGHT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);

        SelectObject(hdc, fglyph.into());
        SetTextColor(hdc, fg);
        draw_glyph(hdc, icon::battery_glyph(d.pct, d.charging), w - 32, y + 9, 18);
    }

    // separator + quit row
    let qy = PAD + devices.len() as i32 * ROWH;
    let lb = CreateSolidBrush(border);
    let lrc = RECT { left: 8, top: qy, right: w - 8, bottom: qy + 1 };
    FillRect(hdc, &lrc, lb);
    let _ = DeleteObject(lb.into());

    SelectObject(hdc, fglyph.into());
    SetTextColor(hdc, sub);
    draw_glyph(hdc, icon::CLOSE, 12, qy + 8, 20);
    SelectObject(hdc, ftext.into());
    let mut qw: Vec<u16> = "Quitter".encode_utf16().collect();
    let mut qrc = RECT { left: 42, top: qy, right: w - 12, bottom: qy + ROWH };
    DrawTextW(hdc, &mut qw, &mut qrc, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);

    let _ = DeleteObject(fglyph.into());
    let _ = DeleteObject(ftext.into());
    let _ = EndPaint(hwnd, &ps);
}

unsafe extern "system" fn popup_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            paint(hwnd);
            LRESULT(0)
        }
        WM_TIMER => {
            close(hwnd);
            LRESULT(0)
        }
        WM_ACTIVATE => {
            if (wp.0 as u32) & 0xFFFF == 0 {
                // WA_INACTIVE
                close(hwnd);
            }
            LRESULT(0)
        }
        WM_KEYDOWN if wp.0 as u32 == VK_ESCAPE => {
            close(hwnd);
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            let y = ((lp.0 >> 16) & 0xFFFF) as i32;
            let n = STATE.0.lock().unwrap().as_ref().map(|a| a.devices.len()).unwrap_or(0);
            if y >= PAD + n as i32 * ROWH {
                let main = STATE.0.lock().unwrap().as_ref().and_then(|a| a.main).unwrap_or_default();
                let _ = PostMessageW(Some(main), WM_CLOSE, WPARAM(0), LPARAM(0));
                close(hwnd);
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            let mut g = STATE.0.lock().unwrap();
            if let Some(a) = g.as_mut() {
                if a.popup == Some(hwnd) {
                    a.popup = None;
                }
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}
