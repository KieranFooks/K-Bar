//! Unified 2-line Windows 11 taskbar block (passive click-through overlay).
//!
//! Renders as a single `WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_NOACTIVATE |
//! WS_EX_TOOLWINDOW | WS_EX_TOPMOST` per-pixel alpha window positioned directly to
//! the left of `TrayNotifyWnd` on `Shell_TrayWnd`, matching the Windows 11 Clock
//! typography. Auto-hides in fullscreen apps/games without loading `shell32.dll`
//! or `shcore.dll`.

use std::cell::RefCell;
use std::ffi::c_void;
use std::sync::Once;

use windows::core::{w, PCWSTR, PWSTR};
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, GetMonitorInfoW,
    MonitorFromWindow, ReleaseDC, SelectObject, AC_SRC_ALPHA, AC_SRC_OVER, BI_RGB, BITMAPINFO,
    BITMAPINFOHEADER, BLENDFUNCTION, DIB_RGB_COLORS, DT_NOPREFIX, DT_RIGHT, DT_SINGLELINE,
    DT_VCENTER, MONITORINFO, MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{GetDpiForSystem, GetDpiForWindow};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, FindWindowExW, FindWindowW, GetClassNameW,
    GetForegroundWindow, GetWindow, GetWindowRect, IsWindowVisible, RegisterClassW, SetWindowPos,
    ShowWindow, UpdateLayeredWindow, CS_HREDRAW, CS_VREDRAW, GW_HWNDPREV, HWND_TOPMOST,
    SWP_NOACTIVATE, SWP_SHOWWINDOW, SW_HIDE, SW_SHOWNOACTIVATE, ULW_ALPHA, WNDCLASSW,
    WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT,
    WS_POPUP,
};

use crate::{hid::Battery, icon};

#[derive(Clone, PartialEq, Eq)]
struct RenderKey {
    devices: Vec<Battery>,
    dpi: u32,
    light_theme: bool,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

thread_local! {
    static LAST_RENDER: RefCell<Option<RenderKey>> = const { RefCell::new(None) };
}

fn scale(dip: i32, dpi: u32) -> i32 {
    (dip * dpi as i32 + 48) / 96
}

fn draw_mask(
    canvas: (&mut [u8], usize, usize),
    mask: &[u8],
    size: (i32, i32),
    origin: (i32, i32),
    fg: (u8, u8, u8),
) {
    let (rgba, wu, hu) = canvas;
    let (mw, mh) = size;
    let (ox, oy) = origin;
    for my in 0..mh {
        for mx in 0..mw {
            let cov = mask[(my * mw + mx) as usize];
            icon::composite_px(rgba, wu, hu, ox + mx, oy + my, fg, cov);
        }
    }
}

/// Render the unified 2-line taskbar block into straight RGBA `(w, h, pixels)`.
/// Strictly monochrome (`r == g == b` on every pixel) with uniform shade across rows.
pub fn render_widget_rgba(devices: &[Battery], dpi: u32, light_theme: bool) -> (i32, i32, Vec<u8>) {
    let sc = |dip: i32| scale(dip, dpi);
    let any_charging = devices.iter().any(|d| d.charging);
    let w = if any_charging { sc(82) } else { sc(70) };
    let h = sc(38);

    let wu = w as usize;
    let hu = h as usize;
    let mut rgba = vec![0u8; wu * hu * 4];

    let fg: (u8, u8, u8) = if light_theme {
        (24, 24, 24)
    } else {
        (245, 245, 245)
    };

    let pad_x = sc(8);
    let row_h = sc(16);
    let glyph_w = sc(13);
    let glyph_font = sc(11);
    let text_font = sc(12);

    if devices.is_empty() {
        let ry = (h - row_h) / 2;
        if let Some(gm) =
            unsafe { icon::render_glyph_mask(icon::BATTERY_BASE, glyph_w, row_h, glyph_font) }
        {
            draw_mask((&mut rgba, wu, hu), &gm, (glyph_w, row_h), (pad_x, ry), fg);
        }
        let text_x = pad_x + glyph_w + sc(2);
        let text_w = (w - pad_x - text_x).max(8);
        if let Some(tm) = unsafe {
            icon::render_text_mask_aligned(
                "--",
                &["Segoe UI"],
                text_w,
                row_h,
                -text_font,
                400,
                DT_RIGHT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
            )
        } {
            draw_mask((&mut rgba, wu, hu), &tm, (text_w, row_h), (text_x, ry), fg);
        }
        return (w, h, rgba);
    }

    let visible_rows = devices.len().min(2);
    let start_y = (h - visible_rows as i32 * row_h) / 2;

    for (i, d) in devices.iter().take(2).enumerate() {
        let ry = start_y + i as i32 * row_h;

        // Device glyph (left)
        if let Some(gm) =
            unsafe { icon::render_glyph_mask(icon::kind_glyph(d.kind), glyph_w, row_h, glyph_font) }
        {
            draw_mask((&mut rgba, wu, hu), &gm, (glyph_w, row_h), (pad_x, ry), fg);
        }

        // Optional charging bolt between icon and percentage
        let mut text_x = pad_x + glyph_w + sc(2);
        if d.charging {
            let bolt_w = sc(10);
            if let Some(bm) = unsafe { icon::render_glyph_mask(icon::BOLT, bolt_w, row_h, sc(10)) }
            {
                draw_mask((&mut rgba, wu, hu), &bm, (bolt_w, row_h), (text_x, ry), fg);
            }
            text_x += bolt_w + sc(1);
        }

        // Right-aligned percentage text (uniform full-opacity monochrome across all rows)
        let pct_str = match d.pct {
            Some(p) => format!("{p} %"),
            None => "--".to_string(),
        };
        let text_w = (w - pad_x - text_x).max(8);
        if let Some(tm) = unsafe {
            icon::render_text_mask_aligned(
                &pct_str,
                &["Segoe UI"],
                text_w,
                row_h,
                -text_font,
                400,
                DT_RIGHT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
            )
        } {
            draw_mask((&mut rgba, wu, hu), &tm, (text_w, row_h), (text_x, ry), fg);
        }
    }

    (w, h, rgba)
}

/// Pure positioning helper placing the widget just left of `TrayNotifyWnd`
/// clamped cleanly inside `tray_rc`.
pub fn compute_widget_rect(
    tray_rc: RECT,
    notify_rc: Option<RECT>,
    w: i32,
    h: i32,
    dpi: u32,
) -> (i32, i32) {
    let sc = |dip: i32| scale(dip, dpi);
    let base_x = match notify_rc {
        Some(n)
            if n.left > tray_rc.left + w
                && n.right <= tray_rc.right + 4
                && (n.right - n.left) >= 40 =>
        {
            n.left - w - sc(6)
        }
        _ => tray_rc.right - sc(340) - w,
    };
    let margin = sc(8);
    let min_x = tray_rc.left + margin;
    let max_x = (tray_rc.right - w - margin).max(min_x);
    let x = base_x.clamp(min_x, max_x);
    let y = tray_rc.top + ((tray_rc.bottom - tray_rc.top) - h) / 2;
    (x, y)
}

unsafe fn present_layered(hwnd: HWND, x: i32, y: i32, w: i32, h: i32, rgba: &[u8]) {
    let screen = GetDC(None);
    let memdc = CreateCompatibleDC(Some(screen));

    let bi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };

    let mut bits: *mut c_void = std::ptr::null_mut();
    let Ok(bmp) = CreateDIBSection(Some(memdc), &bi, DIB_RGB_COLORS, &mut bits, None, 0) else {
        let _ = DeleteDC(memdc);
        ReleaseDC(None, screen);
        return;
    };
    if bits.is_null() {
        let _ = DeleteObject(bmp.into());
        let _ = DeleteDC(memdc);
        ReleaseDC(None, screen);
        return;
    }

    let dst = std::slice::from_raw_parts_mut(bits as *mut u8, (w * h * 4) as usize);
    icon::rgba_to_premul_bgra(rgba, dst);
    let old_bmp = SelectObject(memdc, bmp.into());

    let dst_pt = POINT { x, y };
    let src_pt = POINT { x: 0, y: 0 };
    let sz = SIZE { cx: w, cy: h };
    let blend = BLENDFUNCTION {
        BlendOp: AC_SRC_OVER as u8,
        BlendFlags: 0,
        SourceConstantAlpha: 255,
        AlphaFormat: AC_SRC_ALPHA as u8,
    };

    let _ = UpdateLayeredWindow(
        hwnd,
        Some(screen),
        Some(&dst_pt),
        Some(&sz),
        Some(memdc),
        Some(&src_pt),
        COLORREF(0),
        Some(&blend),
        ULW_ALPHA,
    );

    SelectObject(memdc, old_bmp);
    let _ = DeleteObject(bmp.into());
    let _ = DeleteDC(memdc);
    ReleaseDC(None, screen);
}

unsafe fn window_class_name(hwnd: HWND) -> String {
    let mut buf = [0u16; 128];
    let n = GetClassNameW(hwnd, &mut buf);
    if n <= 0 {
        return String::new();
    }
    PWSTR(buf.as_mut_ptr())
        .to_string()
        .unwrap_or_default()
        .chars()
        .take(n as usize)
        .collect()
}

unsafe fn should_show_on_taskbar(
    tray_hwnd: HWND,
    tray_rc: RECT,
    mon_rc: RECT,
    main_hwnd: HWND,
    widget_hwnd: HWND,
) -> bool {
    if !IsWindowVisible(tray_hwnd).as_bool() {
        return false;
    }
    // Auto-hidden taskbar parked off-screen
    if tray_rc.top >= mon_rc.bottom - 4
        || tray_rc.bottom <= mon_rc.top + 4
        || tray_rc.left >= mon_rc.right - 4
        || tray_rc.right <= mon_rc.left + 4
    {
        return false;
    }

    let fg = GetForegroundWindow();
    if !fg.0.is_null()
        && IsWindowVisible(fg).as_bool()
        && fg != tray_hwnd
        && fg != widget_hwnd
        && fg != main_hwnd
    {
        let cls = window_class_name(fg);
        if cls != "Progman" && cls != "WorkerW" {
            let tray_mon = MonitorFromWindow(tray_hwnd, MONITOR_DEFAULTTONEAREST);
            let fg_mon = MonitorFromWindow(fg, MONITOR_DEFAULTTONEAREST);
            if tray_mon == fg_mon {
                let mut fg_rc = RECT::default();
                if GetWindowRect(fg, &mut fg_rc).is_ok()
                    && fg_rc.left <= mon_rc.left
                    && fg_rc.top <= mon_rc.top
                    && fg_rc.right >= mon_rc.right
                    && fg_rc.bottom >= mon_rc.bottom
                {
                    return false;
                }
            }
        }
    }

    true
}

unsafe fn is_above_taskbar(widget: HWND, tray: HWND) -> bool {
    let mut cur = GetWindow(tray, GW_HWNDPREV).ok();
    for _ in 0..64 {
        let Some(h) = cur else { break };
        if h == widget {
            return true;
        }
        cur = GetWindow(h, GW_HWNDPREV).ok();
    }
    false
}

/// Create the unified passive layered taskbar block window.
///
/// # Safety
/// Must be called on the UI thread.
pub unsafe fn create() -> Option<HWND> {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let hinst = GetModuleHandleW(PCWSTR::null())
            .map(|m| HINSTANCE(m.0))
            .unwrap_or_default();
        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(widget_proc),
            hInstance: hinst,
            lpszClassName: w!("BatteryTrayWidgetClass"),
            ..Default::default()
        };
        RegisterClassW(&wc);
    });

    let hinst = GetModuleHandleW(PCWSTR::null())
        .map(|m| HINSTANCE(m.0))
        .unwrap_or_default();

    CreateWindowExW(
        WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST,
        w!("BatteryTrayWidgetClass"),
        w!("Battery Tray"),
        WS_POPUP,
        0,
        0,
        70,
        38,
        None,
        None,
        Some(hinst),
        None,
    )
    .ok()
}

/// Synchronize the taskbar block's visibility, z-order, position, and rendered bitmap.
///
/// # Safety
/// Must be called on the UI thread.
pub unsafe fn sync_position(
    main: HWND,
    widget: HWND,
    devices: &[Battery],
    light_theme: bool,
    force_redraw: bool,
) {
    let Ok(tray_hwnd) = FindWindowW(w!("Shell_TrayWnd"), PCWSTR::null()) else {
        if IsWindowVisible(widget).as_bool() {
            let _ = ShowWindow(widget, SW_HIDE);
        }
        return;
    };

    let mut tray_rc = RECT::default();
    if GetWindowRect(tray_hwnd, &mut tray_rc).is_err() {
        return;
    }

    let hmon = MonitorFromWindow(tray_hwnd, MONITOR_DEFAULTTONEAREST);
    let mut mi = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    let mon_rc = if GetMonitorInfoW(hmon, &mut mi).as_bool() {
        mi.rcMonitor
    } else {
        tray_rc
    };

    if !should_show_on_taskbar(tray_hwnd, tray_rc, mon_rc, main, widget) {
        if IsWindowVisible(widget).as_bool() {
            let _ = ShowWindow(widget, SW_HIDE);
        }
        return;
    }

    let d = GetDpiForWindow(tray_hwnd);
    let dpi = if d >= 96 {
        d
    } else {
        GetDpiForSystem().max(96)
    };

    let notify_rc = FindWindowExW(Some(tray_hwnd), None, w!("TrayNotifyWnd"), PCWSTR::null())
        .ok()
        .and_then(|n| {
            let mut r = RECT::default();
            GetWindowRect(n, &mut r).ok().map(|()| r)
        });

    let any_charging = devices.iter().any(|d| d.charging);
    let w = if any_charging {
        scale(82, dpi)
    } else {
        scale(70, dpi)
    };
    let h = scale(38, dpi);
    let (x, y) = compute_widget_rect(tray_rc, notify_rc, w, h, dpi);

    let key = RenderKey {
        devices: devices.to_vec(),
        dpi,
        light_theme,
        x,
        y,
        w,
        h,
    };

    let needs_redraw = LAST_RENDER.with(|cell| {
        let mut last = cell.borrow_mut();
        let changed = force_redraw || last.as_ref() != Some(&key);
        if changed {
            *last = Some(key);
        }
        changed
    });

    if needs_redraw {
        let (rw, rh, rgba) = render_widget_rgba(devices, dpi, light_theme);
        present_layered(widget, x, y, rw, rh, &rgba);
    }

    let visible = IsWindowVisible(widget).as_bool();
    if !visible {
        let _ = ShowWindow(widget, SW_SHOWNOACTIVATE);
    }

    if !visible || needs_redraw || !is_above_taskbar(widget, tray_hwnd) {
        let _ = SetWindowPos(
            widget,
            Some(HWND_TOPMOST),
            x,
            y,
            w,
            h,
            SWP_NOACTIVATE | SWP_SHOWWINDOW,
        );
    }
}

/// Destroy the taskbar block window cleanly.
///
/// # Safety
/// Must be called on the UI thread.
pub unsafe fn destroy(widget: HWND) {
    LAST_RENDER.with(|cell| *cell.borrow_mut() = None);
    let _ = DestroyWindow(widget);
}

unsafe extern "system" fn widget_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    DefWindowProcW(hwnd, msg, wp, lp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hid::Kind;

    #[test]
    fn widget_render_is_uniform_and_monochrome_on_dark_and_light_themes() {
        let online_devs = vec![
            Battery {
                name: "G502 LIGHTSPEED".into(),
                kind: Kind::Mouse,
                pct: Some(86),
                charging: false,
                online: true,
            },
            Battery {
                name: "HyperX Cloud III Wireless".into(),
                kind: Kind::Headset,
                pct: Some(100),
                charging: true,
                online: true,
            },
        ];
        let offline_mouse_devs = vec![
            Battery {
                name: "G502 LIGHTSPEED".into(),
                kind: Kind::Mouse,
                pct: Some(86),
                charging: false,
                online: false,
            },
            Battery {
                name: "HyperX Cloud III Wireless".into(),
                kind: Kind::Headset,
                pct: Some(100),
                charging: true,
                online: true,
            },
        ];

        for dpi in [96u32, 144] {
            for light in [false, true] {
                let (w, h, rgba_on) = render_widget_rgba(&online_devs, dpi, light);
                let (w2, h2, rgba_off) = render_widget_rgba(&offline_mouse_devs, dpi, light);

                assert_eq!((w, h), (w2, h2));
                assert_eq!(rgba_on.len(), (w * h * 4) as usize);
                // Uniform shade: sleeping/offline row must render identically to online row
                assert_eq!(rgba_on, rgba_off);

                let visible_text_px = rgba_on.as_chunks::<4>().0.iter().filter(|p| p[3] > 60).count();
                assert!(
                    visible_text_px > 40,
                    "expected visible glyph/text pixels, got {visible_text_px} (dpi={dpi}, light={light})"
                );

                // Strict monochrome invariant across every pixel
                for (i, px) in rgba_on.as_chunks::<4>().0.iter().enumerate() {
                    assert!(
                        px[0] == px[1] && px[1] == px[2],
                        "non-monochrome pixel at {i}: {:?}",
                        px
                    );
                }
            }
        }
    }

    #[test]
    fn compute_widget_rect_places_left_of_notify_and_clamps() {
        let tray = RECT {
            left: 0,
            top: 1392,
            right: 2560,
            bottom: 1440,
        };
        let notify = RECT {
            left: 2210,
            top: 1392,
            right: 2560,
            bottom: 1440,
        };

        let (x, y) = compute_widget_rect(tray, Some(notify), 70, 38, 96);
        assert_eq!(x, 2210 - 70 - 6);
        assert_eq!(y, 1392 + (48 - 38) / 2);

        // Fallback when notify_rc is unavailable
        let (xf, _) = compute_widget_rect(tray, None, 70, 38, 96);
        assert_eq!(xf, 2560 - 340 - 70);
    }
}
