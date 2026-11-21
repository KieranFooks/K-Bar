//! Unified 2-line Windows 11 taskbar block (Option 1A).
//!
//! Renders as a single `WS_EX_LAYERED | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW | WS_EX_TOPMOST`
//! per-pixel alpha window positioned directly to the left of `TrayNotifyWnd` on
//! `Shell_TrayWnd`, matching the Windows 11 Clock block style. Auto-hides in
//! fullscreen apps/games, supports horizontal drag-to-reposition (`OffsetX` in
//! registry), and opens the detailed flyout on click.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, Once};

use windows::core::{w, PCWSTR, PWSTR};
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, GetMonitorInfoW,
    MonitorFromWindow, ReleaseDC, SelectObject, AC_SRC_ALPHA, AC_SRC_OVER, BI_RGB, BITMAPINFO,
    BITMAPINFOHEADER, BLENDFUNCTION, DIB_RGB_COLORS, DT_NOPREFIX, DT_RIGHT, DT_SINGLELINE,
    DT_VCENTER, MONITORINFO, MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    ReleaseCapture, SetCapture, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT,
};
use windows::Win32::UI::Shell::{SHQueryUserNotificationState, QUNS_RUNNING_D3D_FULL_SCREEN};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, FindWindowExW, FindWindowW, GetClassNameW,
    GetCursorPos, GetForegroundWindow, GetWindow, GetWindowRect, IsWindowVisible, LoadCursorW,
    RegisterClassW, SetWindowPos, ShowWindow, UpdateLayeredWindow, CS_HREDRAW, CS_VREDRAW,
    GW_HWNDPREV, HWND_TOPMOST, IDC_ARROW, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
    SWP_SHOWWINDOW, SW_HIDE, SW_SHOWNOACTIVATE, ULW_ALPHA, WM_CAPTURECHANGED, WM_LBUTTONDOWN,
    WM_LBUTTONUP, WM_MOUSEMOVE, WM_RBUTTONUP, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};

use crate::{
    hid::Battery, icon, popup, save_offset_x_dip, STATE,
};

const WM_MOUSELEAVE: u32 = 0x02A3;

static HOVERED: AtomicBool = AtomicBool::new(false);

#[derive(Default)]
struct DragState {
    active: bool,
    moved: bool,
    down_x: i32,
    start_offset_dip: i32,
    dpi: u32,
}

static DRAG: Mutex<DragState> = Mutex::new(DragState {
    active: false,
    moved: false,
    down_x: 0,
    start_offset_dip: 0,
    dpi: 96,
});

#[derive(Clone, PartialEq, Eq)]
struct RenderKey {
    devices: Vec<Battery>,
    dpi: u32,
    light_theme: bool,
    hovered: bool,
    active: bool,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

static LAST_RENDER: Mutex<Option<RenderKey>> = Mutex::new(None);

fn scale(dip: i32, dpi: u32) -> i32 {
    (dip * dpi as i32 + 48) / 96
}

/// Render the unified 2-line taskbar pill into straight RGBA `(w, h, pixels)`.
/// Strictly monochrome (`r == g == b` on every pixel).
pub fn render_widget_rgba(
    devices: &[Battery],
    dpi: u32,
    light_theme: bool,
    hovered: bool,
    active: bool,
) -> (i32, i32, Vec<u8>) {
    let sc = |dip: i32| scale(dip, dpi);
    let any_charging = devices.iter().any(|d| d.online && d.charging);
    let w = if any_charging { sc(82) } else { sc(70) };
    let h = sc(38);

    let wu = w as usize;
    let hu = h as usize;
    let mut rgba = vec![0u8; wu * hu * 4];

    type LayerTint = Option<((u8, u8, u8), u8)>;
    let fg: (u8, u8, u8) = if light_theme { (24, 24, 24) } else { (245, 245, 245) };
    let (pill_fill, pill_border): (LayerTint, LayerTint) =
        match (light_theme, active, hovered) {
            (false, true, _) => (Some(((255, 255, 255), 26)), Some(((255, 255, 255), 38))),
            (false, false, true) => (Some(((255, 255, 255), 18)), Some(((255, 255, 255), 28))),
            (true, true, _) => (Some(((0, 0, 0), 20)), Some(((0, 0, 0), 30))),
            (true, false, true) => (Some(((0, 0, 0), 14)), Some(((0, 0, 0), 22))),
            (_, false, false) => (None, None),
        };

    // Rounded pill backdrop (when idle, keep alpha = 1 inside pill so hit-testing always succeeds)
    let rad = sc(5) as f32;
    let x0 = 0.5f32;
    let y0 = 0.5f32;
    let x1 = w as f32 - 0.5;
    let y1 = h as f32 - 0.5;

    for y in 0..h {
        for x in 0..w {
            let d = icon::sdf_round_rect(x as f32 + 0.5, y as f32 + 0.5, x0, y0, x1, y1, rad);
            if d < 0.6 {
                let edge_cov = (0.6 - d).clamp(0.0, 1.0);
                if let Some((fill_rgb, fill_a)) = pill_fill {
                    let a = ((fill_a as f32 * edge_cov).round() as i32).clamp(0, 255) as u8;
                    icon::composite_px(&mut rgba, wu, hu, x, y, fill_rgb, a);
                    if d > -1.0 {
                        if let Some((b_rgb, b_a)) = pill_border {
                            let border_cov = edge_cov * (1.0 - (-d).clamp(0.0, 1.0));
                            let ba =
                                ((b_a as f32 * border_cov).round() as i32).clamp(0, 255) as u8;
                            icon::composite_px(&mut rgba, wu, hu, x, y, b_rgb, ba);
                        }
                    }
                } else {
                    // alpha = 1 makes the rounded box transparent to the eye yet responsive to hover/click
                    let idx = (y as usize * wu + x as usize) * 4;
                    rgba[idx] = fg.0;
                    rgba[idx + 1] = fg.1;
                    rgba[idx + 2] = fg.2;
                    rgba[idx + 3] = 1;
                }
            }
        }
    }

    let pad_x = sc(8);
    let row_h = sc(16);
    let glyph_w = sc(13);
    let glyph_font = sc(11);
    let text_font = sc(12);

    if devices.is_empty() {
        let ry = (h - row_h) / 2;
        if let Some(gm) = unsafe { icon::render_glyph_mask(icon::BATTERY_BASE, glyph_w, row_h, glyph_font) } {
            for gy in 0..row_h {
                for gx in 0..glyph_w {
                    let cov = gm[(gy * glyph_w + gx) as usize];
                    let a = ((cov as u16 * 160 + 127) / 255) as u8;
                    icon::composite_px(&mut rgba, wu, hu, pad_x + gx, ry + gy, fg, a);
                }
            }
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
            for ty in 0..row_h {
                for tx in 0..text_w {
                    let cov = tm[(ty * text_w + tx) as usize];
                    let a = ((cov as u16 * 160 + 127) / 255) as u8;
                    icon::composite_px(&mut rgba, wu, hu, text_x + tx, ry + ty, fg, a);
                }
            }
        }
        return (w, h, rgba);
    }

    let visible_rows = devices.len().min(2);
    let start_y = (h - visible_rows as i32 * row_h) / 2;

    for (i, d) in devices.iter().take(2).enumerate() {
        let ry = start_y + i as i32 * row_h;
        let alpha_scale: u16 = if d.online { 255 } else { 145 };

        // Device glyph (left)
        if let Some(gm) =
            unsafe { icon::render_glyph_mask(icon::kind_glyph(d.kind), glyph_w, row_h, glyph_font) }
        {
            for gy in 0..row_h {
                for gx in 0..glyph_w {
                    let cov = gm[(gy * glyph_w + gx) as usize];
                    let a = ((cov as u16 * alpha_scale + 127) / 255) as u8;
                    icon::composite_px(&mut rgba, wu, hu, pad_x + gx, ry + gy, fg, a);
                }
            }
        }

        // Optional charging bolt between icon and percentage
        let mut text_x = pad_x + glyph_w + sc(2);
        if d.online && d.charging {
            let bolt_w = sc(10);
            if let Some(bm) = unsafe { icon::render_glyph_mask(icon::BOLT, bolt_w, row_h, sc(10)) }
            {
                for by in 0..row_h {
                    for bx in 0..bolt_w {
                        let cov = bm[(by * bolt_w + bx) as usize];
                        icon::composite_px(&mut rgba, wu, hu, text_x + bx, ry + by, fg, cov);
                    }
                }
            }
            text_x += bolt_w + sc(1);
        }

        // Right-aligned percentage text
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
            for ty in 0..row_h {
                for tx in 0..text_w {
                    let cov = tm[(ty * text_w + tx) as usize];
                    let a = ((cov as u16 * alpha_scale + 127) / 255) as u8;
                    icon::composite_px(&mut rgba, wu, hu, text_x + tx, ry + ty, fg, a);
                }
            }
        }
    }

    (w, h, rgba)
}

/// Pure positioning helper placing the widget just left of `TrayNotifyWnd`
/// (with user drag offset clamped cleanly inside `tray_rc`).
pub fn compute_widget_rect(
    tray_rc: RECT,
    notify_rc: Option<RECT>,
    w: i32,
    h: i32,
    dpi: u32,
    offset_x_dip: i32,
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
    let x = (base_x + sc(offset_x_dip)).clamp(min_x, max_x);
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
    main_hwnd: Option<HWND>,
    widget_hwnd: HWND,
    popup_hwnd: Option<HWND>,
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

    if let Ok(QUNS_RUNNING_D3D_FULL_SCREEN) = SHQueryUserNotificationState() {
        return false;
    }

    let fg = GetForegroundWindow();
    if !fg.0.is_null()
        && IsWindowVisible(fg).as_bool()
        && fg != tray_hwnd
        && fg != widget_hwnd
        && Some(fg) != main_hwnd
        && Some(fg) != popup_hwnd
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

/// Create the unified layered taskbar block window.
///
/// # Safety
/// Must be called on the UI thread after `STATE.0` is initialized.
pub unsafe fn create(main: HWND) -> Option<HWND> {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let hinst = GetModuleHandleW(PCWSTR::null())
            .map(|m| HINSTANCE(m.0))
            .unwrap_or_default();
        let cursor = LoadCursorW(None, IDC_ARROW).unwrap_or_default();
        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(widget_proc),
            hInstance: hinst,
            hCursor: cursor,
            lpszClassName: w!("BatteryTrayWidgetClass"),
            ..Default::default()
        };
        RegisterClassW(&wc);
    });

    let hinst = GetModuleHandleW(PCWSTR::null())
        .map(|m| HINSTANCE(m.0))
        .unwrap_or_default();

    let hwnd = CreateWindowExW(
        WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST,
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
    .ok()?;

    {
        let mut g = STATE.0.lock().unwrap();
        if let Some(a) = g.as_mut() {
            a.widget = Some(hwnd);
            let _ = main;
        }
    }
    Some(hwnd)
}

/// Synchronize the taskbar block's visibility, z-order, position, and rendered bitmap.
///
/// # Safety
/// Must be called on the UI thread.
pub unsafe fn sync_position(force_redraw: bool) {
    let (main, widget_opt, popup_opt, devices, light_theme, offset_x_dip) = {
        let g = STATE.0.lock().unwrap();
        let Some(a) = g.as_ref() else { return };
        (
            a.main,
            a.widget,
            a.popup,
            a.devices.clone(),
            a.light_theme,
            a.offset_x_dip,
        )
    };
    let Some(widget) = widget_opt else { return };

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

    if !should_show_on_taskbar(tray_hwnd, tray_rc, mon_rc, main, widget, popup_opt) {
        if IsWindowVisible(widget).as_bool() {
            let _ = ShowWindow(widget, SW_HIDE);
        }
        return;
    }

    let mut dpix = 96u32;
    let mut dpiy = 96u32;
    let _ = GetDpiForMonitor(hmon, MDT_EFFECTIVE_DPI, &mut dpix, &mut dpiy);
    let dpi = dpix.max(96);

    let notify_rc = FindWindowExW(Some(tray_hwnd), None, w!("TrayNotifyWnd"), PCWSTR::null())
        .ok()
        .and_then(|n| {
            let mut r = RECT::default();
            GetWindowRect(n, &mut r).ok().map(|()| r)
        });

    let hovered = HOVERED.load(Ordering::Relaxed);
    let active = popup_opt.is_some();
    let any_charging = devices.iter().any(|d| d.online && d.charging);
    let w = if any_charging {
        scale(82, dpi)
    } else {
        scale(70, dpi)
    };
    let h = scale(38, dpi);
    let (x, y) = compute_widget_rect(tray_rc, notify_rc, w, h, dpi, offset_x_dip);

    let key = RenderKey {
        devices: devices.clone(),
        dpi,
        light_theme,
        hovered,
        active,
        x,
        y,
        w,
        h,
    };

    let needs_redraw = {
        let mut last = LAST_RENDER.lock().unwrap();
        let changed = force_redraw || last.as_ref() != Some(&key);
        if changed {
            *last = Some(key);
        }
        changed
    };

    if needs_redraw {
        let (rw, rh, rgba) = render_widget_rgba(&devices, dpi, light_theme, hovered, active);
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
        if let Some(p) = popup_opt {
            let _ = SetWindowPos(
                p,
                Some(HWND_TOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
        }
    }
}

/// Force an immediate re-render & placement check.
///
/// # Safety
/// Must be called on the UI thread.
pub unsafe fn redraw() {
    sync_position(true);
}

/// Destroy the taskbar block window cleanly.
///
/// # Safety
/// Must be called on the UI thread without holding `STATE.0`.
pub unsafe fn destroy() {
    let w = {
        let mut g = STATE.0.lock().unwrap();
        g.as_mut().and_then(|a| a.widget.take())
    };
    if let Some(hwnd) = w {
        let _ = DestroyWindow(hwnd);
    }
}

unsafe extern "system" fn widget_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_MOUSEMOVE => {
            let drag_info = {
                let mut d = DRAG.lock().unwrap();
                if d.active {
                    let mut pt = POINT::default();
                    let _ = GetCursorPos(&mut pt);
                    let dx = pt.x - d.down_x;
                    if dx.abs() >= 5 {
                        d.moved = true;
                    }
                    if d.moved {
                        let new_off = (d.start_offset_dip + (dx * 96) / d.dpi as i32).clamp(-1500, 1500);
                        Some(new_off)
                    } else {
                        None
                    }
                } else {
                    None
                }
            };
            if let Some(new_off) = drag_info {
                {
                    let mut g = STATE.0.lock().unwrap();
                    if let Some(a) = g.as_mut() {
                        a.offset_x_dip = new_off;
                    }
                }
                sync_position(false);
                return LRESULT(0);
            }

            let mut tme = TRACKMOUSEEVENT {
                cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                dwFlags: TME_LEAVE,
                hwndTrack: hwnd,
                dwHoverTime: 0,
            };
            let _ = TrackMouseEvent(&mut tme);
            if !HOVERED.swap(true, Ordering::Relaxed) {
                sync_position(true);
            }
            LRESULT(0)
        }
        WM_MOUSELEAVE => {
            if HOVERED.swap(false, Ordering::Relaxed) {
                sync_position(true);
            }
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            let mut pt = POINT::default();
            let _ = GetCursorPos(&mut pt);
            let hmon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
            let mut dpix = 96u32;
            let mut dpiy = 96u32;
            let _ = GetDpiForMonitor(hmon, MDT_EFFECTIVE_DPI, &mut dpix, &mut dpiy);
            let start_off = {
                let g = STATE.0.lock().unwrap();
                g.as_ref().map(|a| a.offset_x_dip).unwrap_or(0)
            };
            {
                let mut d = DRAG.lock().unwrap();
                d.active = true;
                d.moved = false;
                d.down_x = pt.x;
                d.start_offset_dip = start_off;
                d.dpi = dpix.max(96);
            }
            SetCapture(hwnd);
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let was_moved = {
                let mut d = DRAG.lock().unwrap();
                let m = d.active && d.moved;
                d.active = false;
                d.moved = false;
                m
            };
            let _ = ReleaseCapture();

            if was_moved {
                let off = {
                    let g = STATE.0.lock().unwrap();
                    g.as_ref().map(|a| a.offset_x_dip).unwrap_or(0)
                };
                save_offset_x_dip(off);
                popup::refresh_if_open();
            } else {
                let main = {
                    let g = STATE.0.lock().unwrap();
                    g.as_ref().and_then(|a| a.main)
                };
                if let Some(m) = main {
                    popup::toggle(m);
                }
            }
            LRESULT(0)
        }
        WM_RBUTTONUP => {
            let main = {
                let g = STATE.0.lock().unwrap();
                g.as_ref().and_then(|a| a.main)
            };
            if let Some(m) = main {
                popup::toggle(m);
            }
            LRESULT(0)
        }
        WM_CAPTURECHANGED => {
            let mut d = DRAG.lock().unwrap();
            d.active = false;
            d.moved = false;
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hid::Kind;

    #[test]
    fn widget_render_is_visible_and_monochrome_on_dark_and_light_themes() {
        let devs = vec![
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

        for dpi in [96u32, 144] {
            for light in [false, true] {
                let (w, h, idle) = render_widget_rgba(&devs, dpi, light, false, false);
                let (_, _, hov) = render_widget_rgba(&devs, dpi, light, true, false);
                let (_, _, act) = render_widget_rgba(&devs, dpi, light, false, true);

                assert_eq!(idle.len(), (w * h * 4) as usize);
                assert_eq!(hov.len(), idle.len());
                assert_eq!(act.len(), idle.len());

                let visible_text_px = idle.as_chunks::<4>().0.iter().filter(|p| p[3] > 60).count();
                assert!(
                    visible_text_px > 40,
                    "expected visible glyph/text pixels, got {visible_text_px} (dpi={dpi}, light={light})"
                );

                // Hovered state must have higher background alpha than idle
                let corner_idx = ((h as usize / 2) * w as usize + 3) * 4 + 3;
                assert!(hov[corner_idx] > idle[corner_idx]);
                assert!(act[corner_idx] >= hov[corner_idx]);

                // Strict monochrome invariant across every pixel
                for (i, px) in hov.as_chunks::<4>().0.iter().enumerate() {
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
    fn compute_widget_rect_places_left_of_notify_and_clamps_offset() {
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

        let (x, y) = compute_widget_rect(tray, Some(notify), 70, 38, 96, 0);
        assert_eq!(x, 2210 - 70 - 6);
        assert_eq!(y, 1392 + (48 - 38) / 2);

        // Dragged far right -> clamped cleanly inside tray right edge
        let (xr, _) = compute_widget_rect(tray, Some(notify), 70, 38, 96, 1200);
        assert_eq!(xr, 2560 - 70 - 8);

        // Fallback when notify_rc is unavailable
        let (xf, _) = compute_widget_rect(tray, None, 70, 38, 96, 0);
        assert_eq!(xf, 2560 - 340 - 70);
    }
}
