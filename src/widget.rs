//! Unified 2-row × N-column Windows 11 taskbar block (`#![no_std]`, zero heap allocations, single-DIB renderer).
//!
//! Renders as a single `WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_NOACTIVATE |
//! WS_EX_TOOLWINDOW` popup owned by `Shell_TrayWnd`, so `win32k` maintains atomic
//! Z-order above the taskbar with zero focus flicker.

use core::ffi::c_void;
use core::ptr::{null, null_mut};

use crate::hid::{Battery, BatteryList, MAX_DEVICES};
use crate::icon::{self, scale_dip, FontCache};
use crate::w;
use crate::win32::{
    CreateCompatibleDC, CreateDIBSection, CreateWindowExW, DefWindowProcW, DeleteDC,
    DeleteObject, DestroyWindow, DrawTextW, FindWindowExW, FindWindowW, GetClassNameW, GetDC,
    GetDpiForWindow, GetForegroundWindow, GetLayeredWindowAttributes, GetModuleHandleW,
    GetMonitorInfoW, GetWindow, GetWindowLongW, GetWindowRect, IsIconic, IsWindow,
    IsWindowVisible, IsZoomed, MonitorFromWindow, RegisterClassW, ReleaseDC, SelectObject,
    SetBkMode, SetTextColor, SetWindowPos, SingleThreadCell, UpdateLayeredWindow, AC_SRC_ALPHA,
    AC_SRC_OVER, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, BLENDFUNCTION, DIB_RGB_COLORS, DT_CENTER,
    DT_NOCLIP, DT_NOPREFIX, DT_RIGHT, DT_SINGLELINE, DT_VCENTER, GWL_EXSTYLE, GWL_STYLE,
    GW_HWNDPREV, HBITMAP, HDC, HGDIOBJ, HTTRANSPARENT, HWND, HWND_TOPMOST, LWA_ALPHA,
    MA_NOACTIVATE, MONITORINFO, MONITOR_DEFAULTTONEAREST, POINT, RECT, SIZE, SWP_HIDEWINDOW,
    SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOOWNERZORDER, SWP_NOSIZE, SWP_NOZORDER, SWP_SHOWWINDOW,
    TRANSPARENT, ULW_ALPHA, WM_ERASEBKGND, WM_MOUSEACTIVATE, WM_NCHITTEST, WNDCLASSW, WS_CAPTION,
    WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT,
    WS_POPUP, WS_THICKFRAME,
};

#[derive(Clone, Copy, Debug)]
pub struct WidgetWindow {
    pub hwnd: HWND,
    pub owner_tray: Option<HWND>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct RenderKey {
    /// Compact row representation: `(kind_tag: 0=none/1=mouse/2=kbd/3=headset/4=gamepad, pct: 255=none, charging)`.
    rows: [(u8, u8, bool); MAX_DEVICES],
    count: u8,
    dpi: u32,
    light_theme: bool,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

impl RenderKey {
    const fn empty() -> Self {
        Self {
            rows: [(0, 0, false); MAX_DEVICES],
            count: 0,
            dpi: 0,
            light_theme: false,
            x: 0,
            y: 0,
            w: 0,
            h: 0,
        }
    }
}

struct RenderCache {
    valid: bool,
    key: RenderKey,
}

impl RenderCache {
    const fn empty() -> Self {
        Self {
            valid: false,
            key: RenderKey::empty(),
        }
    }
}

// All statics below are all-zero at startup so they reside in `.bss` (0 bytes in `.data`).
static CLASS_REGISTERED: SingleThreadCell<bool> = SingleThreadCell::new(false);
static LAST_RENDER: SingleThreadCell<RenderCache> = SingleThreadCell::new(RenderCache::empty());
static LAST_OWNER_ATTEMPT: SingleThreadCell<usize> = SingleThreadCell::new(0);
static FONTS: SingleThreadCell<FontCache> = SingleThreadCell::new(FontCache::empty());

fn col_has_bolt(devices: &BatteryList, col: usize) -> bool {
    let i0 = col * 2;
    let i1 = i0 + 1;
    let b0 = devices.items.get(i0).copied().flatten().is_some_and(|b| b.charging);
    let b1 = devices.items.get(i1).copied().flatten().is_some_and(|b| b.charging);
    b0 || b1
}

fn col_width(devices: &BatteryList, col: usize, dpi: u32) -> i32 {
    if col_has_bolt(devices, col) {
        scale_dip(68, dpi)
    } else {
        scale_dip(56, dpi)
    }
}

pub fn widget_dimensions(devices: &BatteryList, dpi: u32) -> (i32, i32) {
    let count = devices.count();
    let h = scale_dip(38, dpi);
    if count == 0 {
        return (scale_dip(72, dpi), h);
    }
    let cols = if count <= 2 { 1 } else { count.div_ceil(2) };
    let pad_x = scale_dip(8, dpi);
    let col_gap = scale_dip(10, dpi);
    let mut w = pad_x * 2 + col_gap * (cols as i32 - 1);
    let mut c = 0;
    while c < cols {
        w += col_width(devices, c, dpi);
        c += 1;
    }
    (w, h)
}

fn eq_ascii_utf16(wide: &[u16], ascii: &[u8]) -> bool {
    if wide.len() != ascii.len() {
        return false;
    }
    let mut i = 0;
    while i < ascii.len() {
        if wide[i] != ascii[i] as u16 {
            return false;
        }
        i += 1;
    }
    true
}

/// Desktop and Windows 11 Shell XAML overlay classes ignored for fullscreen hiding.
pub fn is_shell_overlay_class_utf16(wide: &[u16]) -> bool {
    eq_ascii_utf16(wide, b"Progman")
        || eq_ascii_utf16(wide, b"WorkerW")
        || eq_ascii_utf16(wide, b"Shell_TrayWnd")
        || eq_ascii_utf16(wide, b"Shell_SecondaryTrayWnd")
        || eq_ascii_utf16(wide, b"Windows.UI.Core.CoreWindow")
        || eq_ascii_utf16(wide, b"XamlExplorerHostIslandWindow")
        || eq_ascii_utf16(wide, b"TopLevelWindowForOverflowXamlIsland")
        || eq_ascii_utf16(wide, b"NotifyIconOverflowWindow")
        || eq_ascii_utf16(wide, b"TaskListThumbnailWnd")
        || eq_ascii_utf16(wide, b"MultitaskingViewFrame")
        || eq_ascii_utf16(wide, b"ForegroundStaging")
        || eq_ascii_utf16(wide, b"TaskSwitcherWnd")
        || eq_ascii_utf16(wide, b"ApplicationManager_DesktopShellWindow")
        || eq_ascii_utf16(wide, b"Shell_InputSwitchTopLevelWindow")
        || eq_ascii_utf16(wide, b"EdgeUiInputTopWndClass")
        || eq_ascii_utf16(wide, b"Windows.UI.Input.InputSite.WindowClass")
        || eq_ascii_utf16(wide, b"tooltips_class32")
        || eq_ascii_utf16(wide, b"SysShadow")
        || eq_ascii_utf16(wide, b"IME")
        || eq_ascii_utf16(wide, b"MSCTFIME UI")
}

/// Returns `true` when `outer` covers `inner` with a 2 px tolerance.
pub fn rect_covers_rect(outer: RECT, inner: RECT) -> bool {
    outer.left <= inner.left + 2
        && outer.top <= inner.top + 2
        && outer.right >= inner.right - 2
        && outer.bottom >= inner.bottom - 2
}

/// Returns `true` when `rc` covers monitor bounds `mon_rc` with a 2 px tolerance.
pub fn rect_covers_monitor(rc: RECT, mon_rc: RECT) -> bool {
    rect_covers_rect(rc, mon_rc)
}

/// Returns `true` for click-through/non-activating overlay HUDs (GeForce, Discord, Blish HUD, tooltips).
pub fn is_overlay_ex_style(ex_style: u32) -> bool {
    (ex_style & (WS_EX_TRANSPARENT | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE)) != 0
}

/// Returns `true` for normal framed maximized desktop apps when the taskbar is still topmost.
pub fn is_normal_maximized_frame(zoomed: bool, style: u32, tray_is_topmost: bool) -> bool {
    zoomed && tray_is_topmost && (style & WS_CAPTION) == WS_CAPTION && (style & WS_THICKFRAME) != 0
}

/// Single-DIB monochrome GDI render producing premultiplied 32bpp BGRA in `bits`.
unsafe fn draw_widget_to_dib(
    fonts: &mut FontCache,
    devices: &BatteryList,
    dpi: u32,
    light_theme: bool,
) -> Option<(HDC, HDC, HBITMAP, HGDIOBJ, *mut u8, i32, i32)> {
    let (w, h) = widget_dimensions(devices, dpi);
    let screen = GetDC(null_mut());
    if screen.is_null() {
        return None;
    }
    let memdc = CreateCompatibleDC(screen);
    if memdc.is_null() {
        ReleaseDC(null_mut(), screen);
        return None;
    }

    let bi = BITMAPINFO {
        bmi_header: BITMAPINFOHEADER {
            bi_size: core::mem::size_of::<BITMAPINFOHEADER>() as u32,
            bi_width: w,
            bi_height: -h,
            bi_planes: 1,
            bi_bit_count: 32,
            bi_compression: BI_RGB,
            ..Default::default()
        },
        bmi_colors: [0],
    };

    let mut bits: *mut c_void = null_mut();
    let bmp = CreateDIBSection(memdc, &bi, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
    if bmp.is_null() || bits.is_null() {
        if !bmp.is_null() {
            let _ = DeleteObject(bmp);
        }
        let _ = DeleteDC(memdc);
        ReleaseDC(null_mut(), screen);
        return None;
    }

    let byte_len = (w * h * 4) as usize;
    core::ptr::write_bytes(bits as *mut u8, 0, byte_len);
    let old_bmp = SelectObject(memdc, bmp);

    fonts.ensure_dpi(dpi);
    let old_font = SelectObject(memdc, fonts.glyph);
    let _ = SetBkMode(memdc, TRANSPARENT);
    let _ = SetTextColor(memdc, 0x00FF_FFFF);

    let sc = |dip: i32| scale_dip(dip, dpi);
    let pad_x = sc(8);
    let col_gap = sc(10);
    let row_h = sc(16);
    let glyph_w = sc(15);

    if devices.is_empty() {
        let ry = (h - row_h) / 2;
        if !fonts.glyph.is_null() {
            SelectObject(memdc, fonts.glyph);
            let ch = icon::BATTERY_BASE;
            let mut rc = RECT {
                left: pad_x,
                top: ry,
                right: pad_x + glyph_w,
                bottom: ry + row_h,
            };
            let _ = DrawTextW(
                memdc,
                &ch,
                1,
                &mut rc,
                DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_NOCLIP,
            );
        }
        if !fonts.text.is_null() {
            SelectObject(memdc, fonts.text);
            let mut txt = [0u16; 8];
            let tlen = icon::format_pct_utf16(None, &mut txt);
            let mut rc = RECT {
                left: pad_x + glyph_w + sc(2),
                top: ry,
                right: w - pad_x,
                bottom: ry + row_h,
            };
            let _ = DrawTextW(
                memdc,
                txt.as_ptr(),
                tlen as i32,
                &mut rc,
                DT_RIGHT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
            );
        }
    } else {
        let count = devices.count();
        let visible_rows = if count == 1 { 1 } else { 2 };
        let start_y = (h - visible_rows * row_h) / 2;
        let cols = if count <= 2 { 1 } else { count.div_ceil(2) };
        let mut col_x = pad_x;

        for c in 0..cols {
            let cw = col_width(devices, c, dpi);
            for r in 0..2 {
                let idx = c * 2 + r;
                let Some(d) = devices.items.get(idx).copied().flatten() else {
                    continue;
                };
                let ry = start_y + (r as i32) * row_h;

                if !fonts.glyph.is_null() {
                    SelectObject(memdc, fonts.glyph);
                    let ch = icon::kind_glyph(d.kind);
                    let mut rc = RECT {
                        left: col_x,
                        top: ry,
                        right: col_x + glyph_w,
                        bottom: ry + row_h,
                    };
                    let _ = DrawTextW(
                        memdc,
                        &ch,
                        1,
                        &mut rc,
                        DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_NOCLIP,
                    );
                }

                let mut text_x = col_x + glyph_w + sc(2);
                if d.charging {
                    let bolt_w = sc(10);
                    if !fonts.bolt.is_null() {
                        SelectObject(memdc, fonts.bolt);
                        let ch = icon::BOLT;
                        let mut rc = RECT {
                            left: text_x,
                            top: ry,
                            right: text_x + bolt_w,
                            bottom: ry + row_h,
                        };
                        let _ = DrawTextW(
                            memdc,
                            &ch,
                            1,
                            &mut rc,
                            DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_NOCLIP,
                        );
                    }
                    text_x += bolt_w + sc(1);
                }

                if !fonts.text.is_null() {
                    SelectObject(memdc, fonts.text);
                    let mut txt = [0u16; 8];
                    let tlen = icon::format_pct_utf16(d.pct, &mut txt);
                    let mut rc = RECT {
                        left: text_x,
                        top: ry,
                        right: col_x + cw,
                        bottom: ry + row_h,
                    };
                    let _ = DrawTextW(
                        memdc,
                        txt.as_ptr(),
                        tlen as i32,
                        &mut rc,
                        DT_RIGHT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
                    );
                }
            }
            col_x += cw + col_gap;
        }
    }

    SelectObject(memdc, old_font);

    // Convert grayscale anti-aliased GDI coverage into premultiplied monochrome BGRA in-place.
    let fg_val: u16 = if light_theme { 24 } else { 245 };
    let px_bytes = core::slice::from_raw_parts_mut(bits as *mut u8, byte_len);
    for px in px_bytes.as_chunks_mut::<4>().0.iter_mut() {
        let a = (px[0] as u16 + px[1] as u16 + px[2] as u16) / 3;
        let c = ((fg_val * a + 127) / 255) as u8;
        px[0] = c;
        px[1] = c;
        px[2] = c;
        px[3] = a as u8;
    }

    Some((screen, memdc, bmp, old_bmp, bits as *mut u8, w, h))
}

unsafe fn present_widget(
    hwnd: HWND,
    x: i32,
    y: i32,
    devices: &BatteryList,
    dpi: u32,
    light_theme: bool,
) -> bool {
    let fonts = &mut *FONTS.get();
    let Some((screen, memdc, bmp, old_bmp, _bits, w, h)) =
        draw_widget_to_dib(fonts, devices, dpi, light_theme)
    else {
        return false;
    };

    let dst_pt = POINT { x, y };
    let src_pt = POINT { x: 0, y: 0 };
    let sz = SIZE { cx: w, cy: h };
    let blend = BLENDFUNCTION {
        blend_op: AC_SRC_OVER,
        blend_flags: 0,
        source_constant_alpha: 255,
        alpha_format: AC_SRC_ALPHA,
    };

    let ok = UpdateLayeredWindow(
        hwnd,
        screen,
        &dst_pt,
        &sz,
        memdc,
        &src_pt,
        0,
        &blend,
        ULW_ALPHA,
    ) != 0;

    SelectObject(memdc, old_bmp);
    let _ = DeleteObject(bmp);
    let _ = DeleteDC(memdc);
    ReleaseDC(null_mut(), screen);
    ok
}

/// Render premultiplied 32bpp BGRA into `out_bgra` using a local `FontCache`
/// (thread-safe helper for `glyphtest` and parallel unit tests).
pub fn render_widget_premul_bgra(
    devices: &BatteryList,
    dpi: u32,
    light_theme: bool,
    out_bgra: &mut [u8],
) -> (i32, i32) {
    unsafe {
        let mut fonts = FontCache::empty();
        let res = draw_widget_to_dib(&mut fonts, devices, dpi, light_theme);
        let dims = if let Some((screen, memdc, bmp, old_bmp, bits, w, h)) = res {
            let byte_len = ((w * h * 4) as usize).min(out_bgra.len());
            let src = core::slice::from_raw_parts(bits, byte_len);
            out_bgra[..byte_len].copy_from_slice(src);
            SelectObject(memdc, old_bmp);
            let _ = DeleteObject(bmp);
            let _ = DeleteDC(memdc);
            ReleaseDC(null_mut(), screen);
            (w, h)
        } else {
            (0, 0)
        };
        fonts.clear();
        dims
    }
}

/// Pure positioning helper placing the widget just left of `TrayNotifyWnd` inside `tray_rc`.
pub fn compute_widget_rect(
    tray_rc: RECT,
    notify_rc: Option<RECT>,
    w: i32,
    h: i32,
    dpi: u32,
) -> (i32, i32) {
    let sc = |dip: i32| scale_dip(dip, dpi);
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

unsafe fn is_zero_alpha_layered(hwnd: HWND, ex_style: u32) -> bool {
    if (ex_style & WS_EX_LAYERED) == 0 {
        return false;
    }
    let mut alpha = 0u8;
    let mut flags = 0u32;
    GetLayeredWindowAttributes(hwnd, null_mut(), &mut alpha, &mut flags) != 0
        && (flags & LWA_ALPHA) != 0
        && alpha == 0
}

unsafe fn is_shell_overlay_window(hwnd: HWND) -> bool {
    let mut cls_buf = [0u16; 64];
    let cls_len = GetClassNameW(hwnd, cls_buf.as_mut_ptr(), cls_buf.len() as i32);
    let cls_slice = if cls_len > 0 {
        &cls_buf[..(cls_len as usize).min(cls_buf.len())]
    } else {
        &[]
    };
    is_shell_overlay_class_utf16(cls_slice)
}

unsafe fn hide_widget(hwnd: HWND) {
    if IsWindow(hwnd) != 0 && IsWindowVisible(hwnd) != 0 {
        let _ = SetWindowPos(
            hwnd,
            null_mut(),
            0,
            0,
            0,
            0,
            SWP_NOMOVE
                | SWP_NOSIZE
                | SWP_NOZORDER
                | SWP_NOACTIVATE
                | SWP_NOOWNERZORDER
                | SWP_HIDEWINDOW,
        );
    }
}

unsafe fn should_show_on_taskbar(
    tray_hwnd: HWND,
    tray_rc: RECT,
    mon_rc: RECT,
    main_hwnd: HWND,
    widget_hwnd: HWND,
) -> bool {
    if IsWindowVisible(tray_hwnd) == 0 {
        return false;
    }
    if tray_rc.top >= mon_rc.bottom - 4
        || tray_rc.bottom <= mon_rc.top + 4
        || tray_rc.left >= mon_rc.right - 4
        || tray_rc.right <= mon_rc.left + 4
    {
        return false;
    }

    let tray_is_topmost = (GetWindowLongW(tray_hwnd, GWL_EXSTYLE) as u32 & WS_EX_TOPMOST) != 0;

    let mut cur = GetWindow(tray_hwnd, GW_HWNDPREV);
    let mut steps = 0;
    while steps < 1024 && !cur.is_null() {
        steps += 1;
        if cur != widget_hwnd
            && cur != main_hwnd
            && IsWindowVisible(cur) != 0
            && IsIconic(cur) == 0
        {
            let ex = GetWindowLongW(cur, GWL_EXSTYLE) as u32;
            if !is_overlay_ex_style(ex) && !is_zero_alpha_layered(cur, ex) {
                let mut cur_rc = RECT::default();
                if GetWindowRect(cur, &mut cur_rc) != 0 {
                    let covers = rect_covers_monitor(cur_rc, mon_rc)
                        || (!tray_is_topmost && rect_covers_rect(cur_rc, tray_rc));
                    if covers && !is_shell_overlay_window(cur) {
                        return false;
                    }
                }
            }
        }
        cur = GetWindow(cur, GW_HWNDPREV);
    }

    let fg = GetForegroundWindow();
    if !fg.is_null()
        && fg != tray_hwnd
        && fg != widget_hwnd
        && fg != main_hwnd
        && IsWindowVisible(fg) != 0
        && IsIconic(fg) == 0
    {
        let fg_ex = GetWindowLongW(fg, GWL_EXSTYLE) as u32;
        if !is_overlay_ex_style(fg_ex) && !is_zero_alpha_layered(fg, fg_ex) {
            let tray_mon = MonitorFromWindow(tray_hwnd, MONITOR_DEFAULTTONEAREST);
            let fg_mon = MonitorFromWindow(fg, MONITOR_DEFAULTTONEAREST);
            if tray_mon == fg_mon {
                let mut fg_rc = RECT::default();
                if GetWindowRect(fg, &mut fg_rc) != 0 && rect_covers_monitor(fg_rc, mon_rc) {
                    let fg_style = GetWindowLongW(fg, GWL_STYLE) as u32;
                    if !is_normal_maximized_frame(IsZoomed(fg) != 0, fg_style, tray_is_topmost)
                        && !is_shell_overlay_window(fg)
                    {
                        return false;
                    }
                }
            }
        }
    }

    true
}

unsafe fn is_above_taskbar(widget: HWND, tray: HWND) -> bool {
    let mut cur = GetWindow(tray, GW_HWNDPREV);
    let mut i = 0;
    while i < 256 && !cur.is_null() {
        if cur == widget {
            return true;
        }
        cur = GetWindow(cur, GW_HWNDPREV);
        i += 1;
    }
    false
}

/// Create the unified passive layered taskbar block window, owned by `owner_tray`.
///
/// # Safety
/// Must be called on the UI thread.
pub unsafe fn create(owner_tray: Option<HWND>) -> Option<WidgetWindow> {
    let hinst = GetModuleHandleW(null());
    let reg = *CLASS_REGISTERED.get();
    if !reg {
        let wc = WNDCLASSW {
            lpfn_wnd_proc: Some(widget_proc),
            h_instance: hinst,
            lpsz_class_name: w!("KattWidgetClass"),
            ..Default::default()
        };
        let _ = RegisterClassW(&wc);
        *CLASS_REGISTERED.get() = true;
    }

    if let Some(owner) = owner_tray {
        let hwnd = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            w!("KattWidgetClass"),
            w!("katt"),
            WS_POPUP,
            0,
            0,
            72,
            38,
            owner,
            null_mut(),
            hinst,
            null(),
        );
        if !hwnd.is_null() {
            return Some(WidgetWindow {
                hwnd,
                owner_tray: Some(owner),
            });
        }
    }

    let hwnd = CreateWindowExW(
        WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST,
        w!("KattWidgetClass"),
        w!("katt"),
        WS_POPUP,
        0,
        0,
        72,
        38,
        null_mut(),
        null_mut(),
        hinst,
        null(),
    );
    if !hwnd.is_null() {
        Some(WidgetWindow {
            hwnd,
            owner_tray: None,
        })
    } else {
        None
    }
}

fn encode_row(slot: Option<Battery>) -> (u8, u8, bool) {
    match slot {
        None => (0, 0, false),
        Some(b) => (b.kind as u8, b.pct.unwrap_or(255), b.charging),
    }
}

fn build_render_key(
    devices: &BatteryList,
    dpi: u32,
    light_theme: bool,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
) -> RenderKey {
    let mut rows = [(0, 0, false); MAX_DEVICES];
    for (i, slot) in devices.items.iter().enumerate() {
        rows[i] = encode_row(*slot);
    }
    RenderKey {
        rows,
        count: devices.len as u8,
        dpi,
        light_theme,
        x,
        y,
        w,
        h,
    }
}

/// Synchronize the taskbar block's ownership, visibility, position, and rendered bitmap.
/// Returns `(Shell_TrayWnd, TrayNotifyWnd)` handles when present.
///
/// # Safety
/// Must be called on the UI thread without re-entering.
pub unsafe fn sync_position(
    main: HWND,
    slot: &mut Option<WidgetWindow>,
    devices: &BatteryList,
    light_theme: bool,
    force_redraw: bool,
) -> (Option<HWND>, Option<HWND>) {
    let tray_hwnd = FindWindowW(w!("Shell_TrayWnd"), null());
    if tray_hwnd.is_null() {
        if let Some(w_info) = *slot {
            hide_widget(w_info.hwnd);
        }
        return (None, None);
    }

    let notify_hwnd = FindWindowExW(tray_hwnd, null_mut(), w!("TrayNotifyWnd"), null());
    let notify_opt = if notify_hwnd.is_null() {
        None
    } else {
        Some(notify_hwnd)
    };

    let tray_raw = tray_hwnd as usize;
    let last_attempt = *LAST_OWNER_ATTEMPT.get();
    let needs_create = match *slot {
        None => true,
        Some(w) if IsWindow(w.hwnd) == 0 => true,
        Some(w) => w.owner_tray != Some(tray_hwnd) && last_attempt != tray_raw,
    };

    let mut recreated = false;
    if needs_create {
        if let Some(old) = slot.take() {
            if IsWindow(old.hwnd) != 0 {
                let _ = DestroyWindow(old.hwnd);
            }
        }
        (*LAST_RENDER.get()).valid = false;
        *LAST_OWNER_ATTEMPT.get() = tray_raw;
        *slot = create(Some(tray_hwnd));
        recreated = true;
    }

    let Some(w_info) = *slot else {
        return (Some(tray_hwnd), notify_opt);
    };
    let widget = w_info.hwnd;

    let mut tray_rc = RECT::default();
    if GetWindowRect(tray_hwnd, &mut tray_rc) == 0 {
        return (Some(tray_hwnd), notify_opt);
    }

    let hmon = MonitorFromWindow(tray_hwnd, MONITOR_DEFAULTTONEAREST);
    let mut mi = MONITORINFO::default();
    let mon_rc = if GetMonitorInfoW(hmon, &mut mi) != 0 {
        mi.rc_monitor
    } else {
        tray_rc
    };

    if !should_show_on_taskbar(tray_hwnd, tray_rc, mon_rc, main, widget) {
        hide_widget(widget);
        return (Some(tray_hwnd), notify_opt);
    }

    let d = GetDpiForWindow(tray_hwnd);
    let dpi = if d >= 96 { d } else { 96 };

    let notify_rc = notify_opt.and_then(|n| {
        let mut r = RECT::default();
        if GetWindowRect(n, &mut r) != 0 {
            Some(r)
        } else {
            None
        }
    });

    let (w, h) = widget_dimensions(devices, dpi);
    let (x, y) = compute_widget_rect(tray_rc, notify_rc, w, h, dpi);
    let key = build_render_key(devices, dpi, light_theme, x, y, w, h);

    let (was_valid, prev_key) = {
        let cache = &*LAST_RENDER.get();
        (cache.valid, cache.key)
    };
    let needs_redraw = recreated || force_redraw || !was_valid || prev_key != key;
    if needs_redraw && present_widget(widget, x, y, devices, dpi, light_theme) {
        let cache = &mut *LAST_RENDER.get();
        cache.valid = true;
        cache.key = key;
    }

    let visible = IsWindowVisible(widget) != 0;
    let needs_z = !is_above_taskbar(widget, tray_hwnd);
    if !visible || needs_z {
        let mut flags = SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOOWNERZORDER;
        if !visible {
            flags |= SWP_SHOWWINDOW;
        }
        let _ = SetWindowPos(widget, HWND_TOPMOST, 0, 0, 0, 0, flags);
    }

    (Some(tray_hwnd), notify_opt)
}

/// Destroy the taskbar block window and cached GDI fonts cleanly.
///
/// # Safety
/// Must be called on the UI thread.
pub unsafe fn destroy(w: WidgetWindow) {
    (*LAST_RENDER.get()).valid = false;
    *LAST_OWNER_ATTEMPT.get() = 0;
    (*FONTS.get()).clear();
    if IsWindow(w.hwnd) != 0 {
        let _ = DestroyWindow(w.hwnd);
    }
}

unsafe extern "system" fn widget_proc(hwnd: HWND, msg: u32, wp: usize, lp: isize) -> isize {
    match msg {
        WM_ERASEBKGND => 1,
        WM_MOUSEACTIVATE => MA_NOACTIVATE,
        WM_NCHITTEST => HTTRANSPARENT,
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hid::{Kind, Source};

    #[test]
    fn widget_render_handles_1_to_4_devices_in_monochrome_premul_bgra() {
        let mut four_devs = BatteryList::new();
        four_devs.upsert(Battery::with_id(1, 0, Kind::Mouse, Some(86), false, true, Source::NativeHid));
        four_devs.upsert(Battery::with_id(2, 0, Kind::Keyboard, Some(74), false, true, Source::Bluetooth));
        four_devs.upsert(Battery::with_id(3, 0, Kind::Headset, Some(100), true, true, Source::NativeHid));
        four_devs.upsert(Battery::with_id(4, 0, Kind::Gamepad, Some(50), false, true, Source::Bluetooth));

        let mut two_devs = BatteryList::new();
        two_devs.upsert(Battery::with_id(1, 0, Kind::Mouse, Some(86), false, true, Source::NativeHid));
        two_devs.upsert(Battery::with_id(3, 0, Kind::Headset, Some(100), true, true, Source::NativeHid));

        assert_eq!(widget_dimensions(&BatteryList::new(), 96), (72, 38));
        assert_eq!(widget_dimensions(&two_devs, 96), (84, 38));
        assert_eq!(widget_dimensions(&four_devs, 96), (150, 38));

        for devs in [&two_devs, &four_devs] {
            for dpi in [96u32, 144] {
                for light in [false, true] {
                    let (ew, eh) = widget_dimensions(devs, dpi);
                    let byte_len = (ew * eh * 4) as usize;
                    let mut bgra = vec![0u8; byte_len];
                    let (w, h) = render_widget_premul_bgra(devs, dpi, light, &mut bgra);
                    assert_eq!((w, h), (ew, eh));

                    let visible_px = bgra
                        .as_chunks::<4>()
                        .0
                        .iter()
                        .filter(|p| p[3] > 60)
                        .count();
                    assert!(visible_px > 40, "expected visible pixels, got {visible_px}");

                    for (i, px) in bgra.as_chunks::<4>().0.iter().enumerate() {
                        assert!(
                            px[0] == px[1] && px[1] == px[2] && px[0] <= px[3],
                            "invalid premul monochrome pixel at {i}: {:?}",
                            px
                        );
                    }
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

        let (x, y) = compute_widget_rect(tray, Some(notify), 72, 38, 96);
        assert_eq!(x, 2210 - 72 - 6);
        assert_eq!(y, 1392 + (48 - 38) / 2);

        let (xf, _) = compute_widget_rect(tray, None, 72, 38, 96);
        assert_eq!(xf, 2560 - 340 - 72);
    }

    #[test]
    fn shell_overlay_classes_are_ignored_for_fullscreen_hiding() {
        let to_u16 = |s: &str| -> Vec<u16> { s.encode_utf16().collect() };
        for ignored in [
            "Progman",
            "WorkerW",
            "Shell_TrayWnd",
            "Shell_SecondaryTrayWnd",
            "Windows.UI.Core.CoreWindow",
            "XamlExplorerHostIslandWindow",
            "TopLevelWindowForOverflowXamlIsland",
            "NotifyIconOverflowWindow",
            "TaskListThumbnailWnd",
            "MultitaskingViewFrame",
            "ForegroundStaging",
            "TaskSwitcherWnd",
            "ApplicationManager_DesktopShellWindow",
            "Shell_InputSwitchTopLevelWindow",
            "EdgeUiInputTopWndClass",
            "Windows.UI.Input.InputSite.WindowClass",
            "tooltips_class32",
            "SysShadow",
            "IME",
            "MSCTFIME UI",
        ] {
            assert!(is_shell_overlay_class_utf16(&to_u16(ignored)));
        }
        assert!(!is_shell_overlay_class_utf16(&to_u16("ArenaNet_Dx_Window_Class")));
    }

    #[test]
    fn fullscreen_rect_and_style_helpers_match_expected_cases() {
        let mon = RECT {
            left: 0,
            top: 0,
            right: 2560,
            bottom: 1440,
        };
        assert!(rect_covers_monitor(mon, mon));
        assert!(!rect_covers_monitor(
            RECT {
                left: 0,
                top: 0,
                right: 2560,
                bottom: 1392,
            },
            mon
        ));
        assert!(!is_overlay_ex_style(0));
        assert!(is_overlay_ex_style(WS_EX_TRANSPARENT));
        assert!(is_normal_maximized_frame(true, WS_CAPTION | WS_THICKFRAME, true));
        assert!(!is_normal_maximized_frame(true, WS_CAPTION | WS_THICKFRAME, false));
    }
}
