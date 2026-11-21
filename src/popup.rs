//! Native Windows 11 flyout shown on taskbar block click:
//! - DWM rounded corners, immersive dark/light mode, subtle border
//! - Per-Monitor DPI scaling (GetDpiForWindow) & double-buffered GDI paint
//! - Anchored directly above the unified taskbar block
//! - Dismisses on outside click (WA_INACTIVE) or Escape
//! - Zero deadlock: STATE.0 lock is never held across DestroyWindow/PostMessageW

use std::ffi::c_void;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::Once;
use std::time::{Duration, Instant};

use windows::core::{w, BOOL, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWCP_ROUND, DWMWA_BORDER_COLOR, DWMWA_USE_IMMERSIVE_DARK_MODE,
    DWMWA_WINDOW_CORNER_PREFERENCE, DWM_WINDOW_CORNER_PREFERENCE,
};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreateFontW, CreatePen,
    CreateSolidBrush, DeleteDC, DeleteObject, DrawTextW, EndPaint, FillRect, GetMonitorInfoW,
    GetStockObject, InvalidateRect, MonitorFromPoint, RoundRect, SelectObject, SetBkMode,
    SetTextColor, ANTIALIASED_QUALITY, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, DRAW_TEXT_FORMAT,
    DT_CENTER, DT_END_ELLIPSIS, DT_LEFT, DT_NOPREFIX, DT_RIGHT, DT_SINGLELINE, DT_VCENTER, HDC,
    HFONT, MONITORINFO, MONITOR_DEFAULTTONEAREST, NULL_BRUSH, NULL_PEN, OUT_DEFAULT_PRECIS,
    PAINTSTRUCT, PS_SOLID, SRCCOPY, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetClientRect, GetCursorPos, GetWindowRect,
    PostMessageW, RegisterClassW, SetForegroundWindow, SetWindowPos, ShowWindow,
    SystemParametersInfoW, CS_HREDRAW, CS_VREDRAW, HWND_TOPMOST, SPI_GETWORKAREA, SWP_NOACTIVATE,
    SWP_NOZORDER, SW_SHOW, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, WM_ACTIVATE, WM_CLOSE, WM_DESTROY,
    WM_DPICHANGED, WM_ERASEBKGND, WM_KEYDOWN, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE,
    WM_PAINT, WNDCLASSW, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};

use crate::{
    hid::Battery, icon, is_autostart_enabled, save_offset_x_dip, set_autostart, widget, STATE,
    WM_REFRESH_NOW,
};

const WIDTH_DIP: i32 = 320;
const PAD_DIP: i32 = 10;
const DEV_ROW_DIP: i32 = 52;
const EMPTY_ROW_DIP: i32 = 44;
const SEP_GAP_DIP: i32 = 8;
const ACT_ROW_DIP: i32 = 36;

const VK_ESCAPE: u32 = 0x1B;
const WM_MOUSELEAVE: u32 = 0x02A3;

static HOVER_IDX: AtomicIsize = AtomicIsize::new(-1);

#[derive(Clone, Copy, PartialEq, Eq)]
enum ActionKind {
    Refresh,
    Autostart,
    ResetPos,
    Quit,
}

fn action_list(has_offset: bool) -> Vec<ActionKind> {
    if has_offset {
        vec![
            ActionKind::Refresh,
            ActionKind::Autostart,
            ActionKind::ResetPos,
            ActionKind::Quit,
        ]
    } else {
        vec![ActionKind::Refresh, ActionKind::Autostart, ActionKind::Quit]
    }
}

fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
    COLORREF(r as u32 | ((g as u32) << 8) | ((b as u32) << 16))
}

fn scale(dip: i32, dpi: u32) -> i32 {
    (dip * dpi as i32 + 48) / 96
}

fn total_height_dip(dev_count: usize, action_count: usize) -> i32 {
    let dev_h = if dev_count == 0 {
        EMPTY_ROW_DIP
    } else {
        dev_count as i32 * DEV_ROW_DIP
    };
    PAD_DIP + dev_h + SEP_GAP_DIP + action_count as i32 * ACT_ROW_DIP + PAD_DIP
}

unsafe fn make_font(height: i32, weight: i32, face: &str) -> HFONT {
    let face_w: Vec<u16> = face.encode_utf16().chain(std::iter::once(0)).collect();
    CreateFontW(
        height,
        0,
        0,
        0,
        weight,
        0,
        0,
        0,
        DEFAULT_CHARSET,
        OUT_DEFAULT_PRECIS,
        CLIP_DEFAULT_PRECIS,
        ANTIALIASED_QUALITY,
        0,
        PCWSTR(face_w.as_ptr()),
    )
}

unsafe fn apply_dwm_style(hwnd: HWND, light: bool) {
    let corner = DWMWCP_ROUND;
    let _ = DwmSetWindowAttribute(
        hwnd,
        DWMWA_WINDOW_CORNER_PREFERENCE,
        &corner as *const _ as *const c_void,
        std::mem::size_of::<DWM_WINDOW_CORNER_PREFERENCE>() as u32,
    );
    let dark = BOOL::from(!light);
    let _ = DwmSetWindowAttribute(
        hwnd,
        DWMWA_USE_IMMERSIVE_DARK_MODE,
        &dark as *const _ as *const c_void,
        std::mem::size_of::<BOOL>() as u32,
    );
    let border = if light {
        rgb(0xDC, 0xDC, 0xDC)
    } else {
        rgb(0x45, 0x45, 0x45)
    };
    let _ = DwmSetWindowAttribute(
        hwnd,
        DWMWA_BORDER_COLOR,
        &border as *const _ as *const c_void,
        std::mem::size_of::<COLORREF>() as u32,
    );
}

unsafe fn anchor_rect() -> (RECT, RECT) {
    let widget_hwnd = {
        let g = STATE.0.lock().unwrap();
        g.as_ref().and_then(|a| a.widget)
    };

    let mut pt = POINT::default();
    let icon_rc = widget_hwnd
        .and_then(|w| {
            let mut rc = RECT::default();
            GetWindowRect(w, &mut rc).ok().map(|()| rc)
        })
        .unwrap_or_else(|| {
            if GetCursorPos(&mut pt).is_ok() {
                RECT {
                    left: pt.x - 16,
                    top: pt.y - 16,
                    right: pt.x + 16,
                    bottom: pt.y + 16,
                }
            } else {
                let mut wa = RECT::default();
                let _ = SystemParametersInfoW(
                    SPI_GETWORKAREA,
                    0,
                    Some(&mut wa as *mut RECT as *mut c_void),
                    SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
                );
                RECT {
                    left: wa.right - 80,
                    top: wa.bottom,
                    right: wa.right,
                    bottom: wa.bottom,
                }
            }
        });

    let center = POINT {
        x: (icon_rc.left + icon_rc.right) / 2,
        y: (icon_rc.top + icon_rc.bottom) / 2,
    };
    let hmon = MonitorFromPoint(center, MONITOR_DEFAULTTONEAREST);
    let mut mi = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    let work = if GetMonitorInfoW(hmon, &mut mi).as_bool() {
        mi.rcWork
    } else {
        let mut wa = RECT::default();
        let _ = SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            Some(&mut wa as *mut RECT as *mut c_void),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        );
        wa
    };
    (icon_rc, work)
}

fn compute_popup_pos(icon_rc: RECT, work: RECT, wpx: i32, hpx: i32, margin: i32) -> (i32, i32) {
    let cx = (icon_rc.left + icon_rc.right) / 2;
    let x = (cx - wpx / 2).clamp(work.left + margin, (work.right - wpx - margin).max(work.left));

    let y = if icon_rc.top >= work.bottom - 4 {
        work.bottom - hpx - margin
    } else if icon_rc.bottom <= work.top + 4 {
        work.top + margin
    } else if icon_rc.top - hpx - margin >= work.top {
        icon_rc.top - hpx - margin
    } else {
        (work.bottom - hpx - margin).max(work.top + margin)
    };
    (x, y)
}

/// Open the flyout if closed, or close it if already open (no deadlock).
///
/// # Safety
/// Must be called on the UI thread with a valid main `HWND`.
pub unsafe fn toggle(main: HWND) {
    let (open_popup, closed_at) = {
        let g = STATE.0.lock().unwrap();
        (
            g.as_ref().and_then(|a| a.popup),
            g.as_ref().and_then(|a| a.popup_closed_at),
        )
    };

    if let Some(p) = open_popup {
        let _ = DestroyWindow(p);
        return;
    }

    if closed_at.is_some_and(|t| t.elapsed() < Duration::from_millis(250)) {
        return;
    }

    open(main);
}

/// Create and display the flyout anchored right above the taskbar block.
///
/// # Safety
/// Must be called on the UI thread with a valid main `HWND`.
pub unsafe fn open(_main: HWND) {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let hinst = GetModuleHandleW(PCWSTR::null())
            .map(|m| HINSTANCE(m.0))
            .unwrap_or_default();
        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(popup_proc),
            hInstance: hinst,
            lpszClassName: w!("BatteryTrayPopupClass"),
            ..Default::default()
        };
        RegisterClassW(&wc);
    });

    let (dev_count, light, has_offset) = {
        let g = STATE.0.lock().unwrap();
        match g.as_ref() {
            Some(a) => (a.devices.len(), a.light_theme, a.offset_x_dip != 0),
            None => (0, false, false),
        }
    };
    let act_count = action_list(has_offset).len();
    let (icon_rc, work) = anchor_rect();

    let hinst = GetModuleHandleW(PCWSTR::null())
        .map(|m| HINSTANCE(m.0))
        .unwrap_or_default();
    let Ok(p) = CreateWindowExW(
        WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
        w!("BatteryTrayPopupClass"),
        w!("Battery Tray"),
        WS_POPUP,
        icon_rc.left,
        work.bottom - 100,
        100,
        100,
        None,
        None,
        Some(hinst),
        None,
    ) else {
        return;
    };

    let dpi = GetDpiForWindow(p).max(96);
    let wpx = scale(WIDTH_DIP, dpi);
    let hpx = scale(total_height_dip(dev_count, act_count), dpi);
    let margin = scale(12, dpi);
    let (x, y) = compute_popup_pos(icon_rc, work, wpx, hpx, margin);

    apply_dwm_style(p, light);
    let _ = SetWindowPos(p, Some(HWND_TOPMOST), x, y, wpx, hpx, SWP_NOACTIVATE);
    HOVER_IDX.store(-1, Ordering::Relaxed);

    {
        let mut g = STATE.0.lock().unwrap();
        if let Some(a) = g.as_mut() {
            a.popup = Some(p);
        }
    }

    let _ = ShowWindow(p, SW_SHOW);
    let _ = SetForegroundWindow(p);
    widget::redraw();
}

/// If the popup is open when devices, offset, or theme update, refresh size, DWM style, and paint.
///
/// # Safety
/// Must be called on the UI thread.
pub unsafe fn refresh_if_open() {
    let (p, dev_count, light, has_offset) = {
        let g = STATE.0.lock().unwrap();
        match g.as_ref() {
            Some(a) if a.popup.is_some() => (
                a.popup.unwrap(),
                a.devices.len(),
                a.light_theme,
                a.offset_x_dip != 0,
            ),
            _ => return,
        }
    };

    let act_count = action_list(has_offset).len();
    apply_dwm_style(p, light);
    let dpi = GetDpiForWindow(p).max(96);
    let wpx = scale(WIDTH_DIP, dpi);
    let hpx = scale(total_height_dip(dev_count, act_count), dpi);
    let margin = scale(12, dpi);
    let (icon_rc, work) = anchor_rect();
    let (x, y) = compute_popup_pos(icon_rc, work, wpx, hpx, margin);
    let _ = SetWindowPos(p, None, x, y, wpx, hpx, SWP_NOZORDER | SWP_NOACTIVATE);
    let _ = InvalidateRect(Some(p), None, false);
}

unsafe fn draw_text_line(hdc: HDC, text: &str, rc: RECT, flags: DRAW_TEXT_FORMAT) {
    let mut wide: Vec<u16> = text.encode_utf16().collect();
    let mut r = rc;
    DrawTextW(hdc, &mut wide, &mut r, flags);
}

unsafe fn draw_glyph_centered(hdc: HDC, codepoint: u32, rc: RECT) {
    if let Some(c) = char::from_u32(codepoint) {
        let mut buf = [0u8; 4];
        let s: &str = c.encode_utf8(&mut buf);
        draw_text_line(
            hdc,
            s,
            rc,
            DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
        );
    }
}

unsafe fn paint(hwnd: HWND) {
    let mut ps = PAINTSTRUCT::default();
    let hdc = BeginPaint(hwnd, &mut ps);
    let mut rc = RECT::default();
    let _ = GetClientRect(hwnd, &mut rc);
    let w = (rc.right - rc.left).max(1);
    let h = (rc.bottom - rc.top).max(1);

    let (devices, light, has_offset): (Vec<Battery>, bool, bool) = {
        let g = STATE.0.lock().unwrap();
        match g.as_ref() {
            Some(a) => (a.devices.clone(), a.light_theme, a.offset_x_dip != 0),
            None => {
                let _ = EndPaint(hwnd, &ps);
                return;
            }
        }
    };

    let dpi = GetDpiForWindow(hwnd).max(96);
    let sc = |dip: i32| scale(dip, dpi);

    let memdc = CreateCompatibleDC(Some(hdc));
    let membmp = CreateCompatibleBitmap(hdc, w, h);
    let old_bmp = SelectObject(memdc, membmp.into());

    let (bg, border, hover_bg, fg, sub, track_bg, fill_fg) = if light {
        (
            rgb(0xF9, 0xF9, 0xF9),
            rgb(0xE0, 0xE0, 0xE0),
            rgb(0xED, 0xED, 0xED),
            rgb(0x1B, 0x1B, 0x1B),
            rgb(0x61, 0x61, 0x61),
            rgb(0xDC, 0xDC, 0xDC),
            rgb(0x1B, 0x1B, 0x1B),
        )
    } else {
        (
            rgb(0x20, 0x20, 0x20),
            rgb(0x38, 0x38, 0x38),
            rgb(0x2D, 0x2D, 0x2D),
            rgb(0xFF, 0xFF, 0xFF),
            rgb(0xA8, 0xA8, 0xA8),
            rgb(0x3A, 0x3A, 0x3A),
            rgb(0xEE, 0xEE, 0xEE),
        )
    };

    let bg_brush = CreateSolidBrush(bg);
    FillRect(memdc, &rc, bg_brush);
    let _ = DeleteObject(bg_brush.into());

    let fglyph = make_font(-sc(18), 400, "Segoe Fluent Icons");
    let fglyph_sm = make_font(-sc(16), 400, "Segoe Fluent Icons");
    let fname = make_font(-sc(13), 600, "Segoe UI");
    let fsub = make_font(-sc(11), 400, "Segoe UI");
    let fbody = make_font(-sc(13), 400, "Segoe UI");
    SetBkMode(memdc, TRANSPARENT);

    let pad = sc(PAD_DIP);
    let mut y = pad;

    if devices.is_empty() {
        let empty_h = sc(EMPTY_ROW_DIP);
        let old_f = SelectObject(memdc, fbody.into());
        SetTextColor(memdc, sub);
        draw_text_line(
            memdc,
            "Aucun périphérique détecté",
            RECT {
                left: sc(14),
                top: y,
                right: w - sc(14),
                bottom: y + empty_h,
            },
            DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
        );
        SelectObject(memdc, old_f);
        y += empty_h;
    } else {
        let row_h = sc(DEV_ROW_DIP);
        for d in &devices {
            let text_col = if d.online { fg } else { sub };

            let old_f = SelectObject(memdc, fglyph.into());
            SetTextColor(memdc, text_col);
            draw_glyph_centered(
                memdc,
                icon::kind_glyph(d.kind),
                RECT {
                    left: sc(14),
                    top: y,
                    right: sc(40),
                    bottom: y + row_h,
                },
            );

            SelectObject(memdc, fname.into());
            SetTextColor(memdc, text_col);
            draw_text_line(
                memdc,
                &d.name,
                RECT {
                    left: sc(48),
                    top: y + sc(7),
                    right: w - sc(92),
                    bottom: y + sc(26),
                },
                DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS,
            );

            SelectObject(memdc, fbody.into());
            SetTextColor(memdc, text_col);
            let pct_str = match d.pct {
                Some(p) => format!("{p} %"),
                None => "--".to_string(),
            };
            draw_text_line(
                memdc,
                &pct_str,
                RECT {
                    left: w - sc(92),
                    top: y + sc(7),
                    right: w - sc(38),
                    bottom: y + sc(26),
                },
                DT_RIGHT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
            );

            SelectObject(memdc, fglyph_sm.into());
            SetTextColor(memdc, text_col);
            draw_glyph_centered(
                memdc,
                icon::battery_glyph(d.pct, d.charging),
                RECT {
                    left: w - sc(34),
                    top: y + sc(7),
                    right: w - sc(12),
                    bottom: y + sc(26),
                },
            );

            let status_note = match (d.online, d.charging) {
                (true, true) => Some("En charge"),
                (false, _) if d.pct.is_some() => Some("En veille"),
                (false, _) => Some("Hors ligne"),
                _ => None,
            };

            let bar_left = sc(48);
            let bar_right = if status_note.is_some() {
                w - sc(82)
            } else {
                w - sc(16)
            };
            let bar_top = y + sc(34);
            let bar_h = sc(4).max(3);
            let bar_r = bar_h;

            let pen_none = GetStockObject(NULL_PEN);
            let old_pen = SelectObject(memdc, pen_none);

            let tb = CreateSolidBrush(track_bg);
            let old_b = SelectObject(memdc, tb.into());
            let _ = RoundRect(
                memdc,
                bar_left,
                bar_top,
                bar_right,
                bar_top + bar_h,
                bar_r,
                bar_r,
            );
            SelectObject(memdc, old_b);
            let _ = DeleteObject(tb.into());

            if let Some(p) = d.pct {
                let total_bw = (bar_right - bar_left).max(1);
                let fill_w = ((total_bw * p.min(100) as i32) + 50) / 100;
                if fill_w > 0 {
                    let fb = CreateSolidBrush(if d.online { fill_fg } else { sub });
                    let ob = SelectObject(memdc, fb.into());
                    let _ = RoundRect(
                        memdc,
                        bar_left,
                        bar_top,
                        bar_left + fill_w.max(bar_h),
                        bar_top + bar_h,
                        bar_r,
                        bar_r,
                    );
                    SelectObject(memdc, ob);
                    let _ = DeleteObject(fb.into());
                }
            }
            SelectObject(memdc, old_pen);

            if let Some(note) = status_note {
                SelectObject(memdc, fsub.into());
                SetTextColor(memdc, sub);
                draw_text_line(
                    memdc,
                    note,
                    RECT {
                        left: w - sc(78),
                        top: y + sc(26),
                        right: w - sc(14),
                        bottom: y + sc(45),
                    },
                    DT_RIGHT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
                );
            }

            SelectObject(memdc, old_f);
            y += row_h;
        }
    }

    // Separator
    let sep_gap = sc(SEP_GAP_DIP);
    let sep_y = y + sep_gap / 2;
    let sep_pen = CreatePen(PS_SOLID, 1, border);
    let old_pen = SelectObject(memdc, sep_pen.into());
    let sb = CreateSolidBrush(border);
    FillRect(
        memdc,
        &RECT {
            left: sc(10),
            top: sep_y,
            right: w - sc(10),
            bottom: sep_y + 1,
        },
        sb,
    );
    let _ = DeleteObject(sb.into());
    SelectObject(memdc, old_pen);
    let _ = DeleteObject(sep_pen.into());
    y += sep_gap;

    // Footer actions
    let act_h = sc(ACT_ROW_DIP);
    let hover = HOVER_IDX.load(Ordering::Relaxed);
    let autostart = is_autostart_enabled();
    let actions = action_list(has_offset);

    for (idx, kind) in actions.iter().enumerate() {
        let ay = y + idx as i32 * act_h;
        if hover == idx as isize {
            let hb = CreateSolidBrush(hover_bg);
            let old_b = SelectObject(memdc, hb.into());
            let old_p = SelectObject(memdc, GetStockObject(NULL_PEN));
            let _ = RoundRect(
                memdc,
                sc(6),
                ay + sc(2),
                w - sc(6),
                ay + act_h - sc(2),
                sc(6),
                sc(6),
            );
            SelectObject(memdc, old_p);
            SelectObject(memdc, old_b);
            let _ = DeleteObject(hb.into());
        }

        let (cp_opt, label, highlight_icon) = match kind {
            ActionKind::Refresh => (Some(icon::REFRESH), "Actualiser", false),
            ActionKind::Autostart => (
                if autostart { Some(icon::CHECK) } else { None },
                "Lancer au démarrage",
                autostart,
            ),
            ActionKind::ResetPos => (Some(icon::UNDO), "Réinitialiser la position", false),
            ActionKind::Quit => (Some(icon::POWER), "Quitter", false),
        };

        let old_f = SelectObject(memdc, fglyph_sm.into());
        if let Some(cp) = cp_opt {
            SetTextColor(memdc, if highlight_icon { fg } else { sub });
            draw_glyph_centered(
                memdc,
                cp,
                RECT {
                    left: sc(14),
                    top: ay,
                    right: sc(38),
                    bottom: ay + act_h,
                },
            );
        } else {
            let pen = CreatePen(PS_SOLID, 1, sub);
            let op = SelectObject(memdc, pen.into());
            let ob = SelectObject(memdc, GetStockObject(NULL_BRUSH));
            let bx = sc(20);
            let by = ay + (act_h - sc(12)) / 2;
            let _ = RoundRect(memdc, bx, by, bx + sc(12), by + sc(12), sc(3), sc(3));
            SelectObject(memdc, ob);
            SelectObject(memdc, op);
            let _ = DeleteObject(pen.into());
        }

        SelectObject(memdc, fbody.into());
        SetTextColor(memdc, fg);
        draw_text_line(
            memdc,
            label,
            RECT {
                left: sc(48),
                top: ay,
                right: w - sc(14),
                bottom: ay + act_h,
            },
            DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
        );
        SelectObject(memdc, old_f);
    }

    let _ = BitBlt(hdc, 0, 0, w, h, Some(memdc), 0, 0, SRCCOPY);

    SelectObject(memdc, old_bmp);
    let _ = DeleteObject(membmp.into());
    let _ = DeleteDC(memdc);
    let _ = DeleteObject(fglyph.into());
    let _ = DeleteObject(fglyph_sm.into());
    let _ = DeleteObject(fname.into());
    let _ = DeleteObject(fsub.into());
    let _ = DeleteObject(fbody.into());
    let _ = EndPaint(hwnd, &ps);
}

unsafe fn hit_action(hwnd: HWND, y_px: i32) -> isize {
    let (dev_count, has_offset) = {
        let g = STATE.0.lock().unwrap();
        match g.as_ref() {
            Some(a) => (a.devices.len(), a.offset_x_dip != 0),
            None => (0, false),
        }
    };
    let count = action_list(has_offset).len() as i32;
    let dpi = GetDpiForWindow(hwnd).max(96);
    let dev_h = if dev_count == 0 {
        scale(EMPTY_ROW_DIP, dpi)
    } else {
        dev_count as i32 * scale(DEV_ROW_DIP, dpi)
    };
    let start_y = scale(PAD_DIP, dpi) + dev_h + scale(SEP_GAP_DIP, dpi);
    if y_px < start_y {
        return -1;
    }
    let idx = (y_px - start_y) / scale(ACT_ROW_DIP, dpi);
    if (0..count).contains(&idx) {
        idx as isize
    } else {
        -1
    }
}

unsafe extern "system" fn popup_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_ERASEBKGND => LRESULT(1),
        WM_PAINT => {
            paint(hwnd);
            LRESULT(0)
        }
        WM_ACTIVATE => {
            if (wp.0 as u32) & 0xFFFF == 0 {
                {
                    let mut g = STATE.0.lock().unwrap();
                    if let Some(a) = g.as_mut() {
                        a.popup_closed_at = Some(Instant::now());
                    }
                }
                let _ = DestroyWindow(hwnd);
            }
            LRESULT(0)
        }
        WM_KEYDOWN if wp.0 as u32 == VK_ESCAPE => {
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let mut tme = TRACKMOUSEEVENT {
                cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                dwFlags: TME_LEAVE,
                hwndTrack: hwnd,
                dwHoverTime: 0,
            };
            let _ = TrackMouseEvent(&mut tme);
            let y = ((lp.0 >> 16) & 0xFFFF) as i16 as i32;
            let idx = hit_action(hwnd, y);
            if HOVER_IDX.swap(idx, Ordering::Relaxed) != idx {
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
            LRESULT(0)
        }
        WM_MOUSELEAVE => {
            if HOVER_IDX.swap(-1, Ordering::Relaxed) != -1 {
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
            LRESULT(0)
        }
        WM_LBUTTONDOWN => LRESULT(0),
        WM_LBUTTONUP => {
            let y = ((lp.0 >> 16) & 0xFFFF) as i16 as i32;
            let idx = hit_action(hwnd, y);
            let (main, has_offset) = {
                let g = STATE.0.lock().unwrap();
                match g.as_ref() {
                    Some(a) => (a.main, a.offset_x_dip != 0),
                    None => (None, false),
                }
            };
            let actions = action_list(has_offset);
            if let Some(kind) = (idx >= 0).then(|| actions.get(idx as usize)).flatten() {
                match kind {
                    ActionKind::Refresh => {
                        if let Some(m) = main {
                            let _ = PostMessageW(Some(m), WM_REFRESH_NOW, WPARAM(0), LPARAM(0));
                        }
                    }
                    ActionKind::Autostart => {
                        let cur = is_autostart_enabled();
                        let _ = set_autostart(!cur);
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    }
                    ActionKind::ResetPos => {
                        {
                            let mut g = STATE.0.lock().unwrap();
                            if let Some(a) = g.as_mut() {
                                a.offset_x_dip = 0;
                            }
                        }
                        save_offset_x_dip(0);
                        widget::sync_position(true);
                        refresh_if_open();
                    }
                    ActionKind::Quit => {
                        if let Some(m) = main {
                            let _ = PostMessageW(Some(m), WM_CLOSE, WPARAM(0), LPARAM(0));
                        }
                        let _ = DestroyWindow(hwnd);
                    }
                }
            }
            LRESULT(0)
        }
        WM_DPICHANGED => {
            refresh_if_open();
            LRESULT(0)
        }
        WM_DESTROY => {
            HOVER_IDX.store(-1, Ordering::Relaxed);
            let changed = {
                let mut g = STATE.0.lock().unwrap();
                if let Some(a) = g.as_mut() {
                    if a.popup == Some(hwnd) {
                        a.popup = None;
                        a.popup_closed_at = Some(Instant::now());
                        true
                    } else {
                        false
                    }
                } else {
                    false
                }
            };
            if changed {
                widget::redraw();
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}
