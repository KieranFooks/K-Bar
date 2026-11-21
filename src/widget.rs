//! Unified 2-line Windows 11 taskbar block (`#![no_std]`, zero heap allocations, single-DIB renderer).
//!
//! Renders as a single `WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_NOACTIVATE |
//! WS_EX_TOOLWINDOW` popup owned by `Shell_TrayWnd`, so `win32k` maintains atomic
//! Z-order above the taskbar with zero focus flicker.

use core::ffi::c_void;
use core::ptr::{null, null_mut};

use crate::hid::{BatteryList, Kind};
use crate::icon::{self, scale_dip, FontCache};
use crate::w;
use crate::win32::{
    CreateCompatibleDC, CreateDIBSection, CreateWindowExW, DefWindowProcW, DeleteDC,
    DeleteObject, DestroyWindow, DrawTextW, FindWindowExW, FindWindowW, GetClassNameW, GetDC,
    GetDpiForSystem, GetDpiForWindow, GetForegroundWindow, GetModuleHandleW, GetMonitorInfoW,
    GetWindow, GetWindowRect, IsWindow, IsWindowVisible, IsZoomed, MonitorFromWindow,
    RegisterClassW, ReleaseDC, SelectObject, SetBkMode, SetTextColor, SetWindowPos, ShowWindow,
    SingleThreadCell, UpdateLayeredWindow, AC_SRC_ALPHA, AC_SRC_OVER, BITMAPINFO,
    BITMAPINFOHEADER, BI_RGB, BLENDFUNCTION, DIB_RGB_COLORS, DT_CENTER, DT_NOPREFIX, DT_RIGHT,
    DT_SINGLELINE, DT_VCENTER, GW_HWNDPREV, HBITMAP, HDC, HGDIOBJ, HWND, HWND_TOPMOST, MONITORINFO,
    MONITOR_DEFAULTTONEAREST, POINT, RECT, SIZE, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SW_HIDE,
    SW_SHOWNOACTIVATE, TRANSPARENT, ULW_ALPHA, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};

#[derive(Clone, Copy, Debug)]
pub struct WidgetWindow {
    pub hwnd: HWND,
    pub owner_tray: Option<HWND>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct RenderKey {
    rows: [Option<(Kind, Option<u8>, bool)>; 2],
    dpi: u32,
    light_theme: bool,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

static CLASS_REGISTERED: SingleThreadCell<bool> = SingleThreadCell::new(false);
static LAST_RENDER: SingleThreadCell<Option<RenderKey>> = SingleThreadCell::new(None);
static LAST_OWNER_ATTEMPT: SingleThreadCell<usize> = SingleThreadCell::new(0);
static FONTS: SingleThreadCell<FontCache> = SingleThreadCell::new(FontCache::empty());

pub fn widget_dimensions(devices: &BatteryList, dpi: u32) -> (i32, i32) {
    let w = if devices.any_charging() {
        scale_dip(82, dpi)
    } else {
        scale_dip(70, dpi)
    };
    let h = scale_dip(38, dpi);
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
}

/// Single-DIB monochrome GDI render producing premultiplied 32bpp BGRA in `bits`.
unsafe fn draw_widget_to_dib(
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

    let fonts = &mut *FONTS.get();
    fonts.ensure_dpi(dpi);
    let old_font = SelectObject(memdc, fonts.glyph);
    let _ = SetBkMode(memdc, TRANSPARENT);
    let _ = SetTextColor(memdc, 0x00FF_FFFF);

    let sc = |dip: i32| scale_dip(dip, dpi);
    let pad_x = sc(8);
    let row_h = sc(16);
    let glyph_w = sc(13);

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
                DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
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
        let visible_rows = devices.count() as i32;
        let start_y = (h - visible_rows * row_h) / 2;
        let mut row_idx = 0i32;

        for slot in &devices.slots {
            let Some(d) = *slot else { continue };
            let ry = start_y + row_idx * row_h;
            row_idx += 1;

            if !fonts.glyph.is_null() {
                SelectObject(memdc, fonts.glyph);
                let ch = icon::kind_glyph(d.kind);
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
                    DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
                );
            }

            let mut text_x = pad_x + glyph_w + sc(2);
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
                        DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
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
) {
    let Some((screen, memdc, bmp, old_bmp, _bits, w, h)) =
        draw_widget_to_dib(devices, dpi, light_theme)
    else {
        return;
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

    let _ = UpdateLayeredWindow(
        hwnd,
        screen,
        &dst_pt,
        &sz,
        memdc,
        &src_pt,
        0,
        &blend,
        ULW_ALPHA,
    );

    SelectObject(memdc, old_bmp);
    let _ = DeleteObject(bmp);
    let _ = DeleteDC(memdc);
    ReleaseDC(null_mut(), screen);
}

/// Render premultiplied 32bpp BGRA into `out_bgra` (shared with `glyphtest` and tests).
pub fn render_widget_premul_bgra(
    devices: &BatteryList,
    dpi: u32,
    light_theme: bool,
    out_bgra: &mut [u8],
) -> (i32, i32) {
    unsafe {
        let Some((screen, memdc, bmp, old_bmp, bits, w, h)) =
            draw_widget_to_dib(devices, dpi, light_theme)
        else {
            return (0, 0);
        };
        let byte_len = ((w * h * 4) as usize).min(out_bgra.len());
        let src = core::slice::from_raw_parts(bits, byte_len);
        out_bgra[..byte_len].copy_from_slice(src);
        SelectObject(memdc, old_bmp);
        let _ = DeleteObject(bmp);
        let _ = DeleteDC(memdc);
        ReleaseDC(null_mut(), screen);
        (w, h)
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

    let fg = GetForegroundWindow();
    if !fg.is_null()
        && IsWindowVisible(fg) != 0
        && fg != tray_hwnd
        && fg != widget_hwnd
        && fg != main_hwnd
        && IsZoomed(fg) == 0
    {
        let mut cls_buf = [0u16; 64];
        let cls_len = GetClassNameW(fg, cls_buf.as_mut_ptr(), cls_buf.len() as i32);
        let cls_slice = if cls_len > 0 {
            &cls_buf[..(cls_len as usize).min(cls_buf.len())]
        } else {
            &[]
        };
        if !is_shell_overlay_class_utf16(cls_slice) {
            let tray_mon = MonitorFromWindow(tray_hwnd, MONITOR_DEFAULTTONEAREST);
            let fg_mon = MonitorFromWindow(fg, MONITOR_DEFAULTTONEAREST);
            if tray_mon == fg_mon {
                let mut fg_rc = RECT::default();
                if GetWindowRect(fg, &mut fg_rc) != 0
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
    let mut cur = GetWindow(tray, GW_HWNDPREV);
    let mut i = 0;
    while i < 64 && !cur.is_null() {
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
    let reg = &mut *CLASS_REGISTERED.get();
    if !*reg {
        let wc = WNDCLASSW {
            lpfn_wnd_proc: Some(widget_proc),
            h_instance: hinst,
            lpsz_class_name: w!("BatteryTrayWidgetClass"),
            ..Default::default()
        };
        let _ = RegisterClassW(&wc);
        *reg = true;
    }

    if let Some(owner) = owner_tray {
        let hwnd = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            w!("BatteryTrayWidgetClass"),
            w!("Battery Tray"),
            WS_POPUP,
            0,
            0,
            70,
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
        w!("BatteryTrayWidgetClass"),
        w!("Battery Tray"),
        WS_POPUP,
        0,
        0,
        70,
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

/// Synchronize the taskbar block's ownership, visibility, position, and rendered bitmap.
/// Returns the currently tracked `Shell_TrayWnd` handle if found.
///
/// # Safety
/// Must be called on the UI thread.
pub unsafe fn sync_position(
    main: HWND,
    slot: &mut Option<WidgetWindow>,
    devices: &BatteryList,
    light_theme: bool,
    force_redraw: bool,
) -> Option<HWND> {
    let tray_hwnd = FindWindowW(w!("Shell_TrayWnd"), null());
    if tray_hwnd.is_null() {
        if let Some(w_info) = *slot {
            if IsWindow(w_info.hwnd) != 0 && IsWindowVisible(w_info.hwnd) != 0 {
                let _ = ShowWindow(w_info.hwnd, SW_HIDE);
            }
        }
        return None;
    }

    let tray_raw = tray_hwnd as usize;
    let last_attempt = &mut *LAST_OWNER_ATTEMPT.get();
    let needs_create = match *slot {
        None => true,
        Some(w) if IsWindow(w.hwnd) == 0 => true,
        Some(w) => w.owner_tray != Some(tray_hwnd) && *last_attempt != tray_raw,
    };

    let mut recreated = false;
    if needs_create {
        if let Some(old) = slot.take() {
            if IsWindow(old.hwnd) != 0 {
                let _ = DestroyWindow(old.hwnd);
            }
        }
        *LAST_RENDER.get() = None;
        *last_attempt = tray_raw;
        *slot = create(Some(tray_hwnd));
        recreated = true;
    }

    let Some(w_info) = *slot else {
        return Some(tray_hwnd);
    };
    let widget = w_info.hwnd;

    let mut tray_rc = RECT::default();
    if GetWindowRect(tray_hwnd, &mut tray_rc) == 0 {
        return Some(tray_hwnd);
    }

    let hmon = MonitorFromWindow(tray_hwnd, MONITOR_DEFAULTTONEAREST);
    let mut mi = MONITORINFO::default();
    let mon_rc = if GetMonitorInfoW(hmon, &mut mi) != 0 {
        mi.rc_monitor
    } else {
        tray_rc
    };

    if !should_show_on_taskbar(tray_hwnd, tray_rc, mon_rc, main, widget) {
        if IsWindowVisible(widget) != 0 {
            let _ = ShowWindow(widget, SW_HIDE);
        }
        return Some(tray_hwnd);
    }

    let d = GetDpiForWindow(tray_hwnd);
    let dpi = if d >= 96 {
        d
    } else {
        GetDpiForSystem().max(96)
    };

    let notify_hwnd = FindWindowExW(tray_hwnd, null_mut(), w!("TrayNotifyWnd"), null());
    let notify_rc = if !notify_hwnd.is_null() {
        let mut r = RECT::default();
        if GetWindowRect(notify_hwnd, &mut r) != 0 {
            Some(r)
        } else {
            None
        }
    } else {
        None
    };

    let (w, h) = widget_dimensions(devices, dpi);
    let (x, y) = compute_widget_rect(tray_rc, notify_rc, w, h, dpi);

    let key = RenderKey {
        rows: [
            devices.slots[0].map(|b| (b.kind, b.pct, b.charging)),
            devices.slots[1].map(|b| (b.kind, b.pct, b.charging)),
        ],
        dpi,
        light_theme,
        x,
        y,
        w,
        h,
    };

    let last = &mut *LAST_RENDER.get();
    let needs_redraw = recreated || force_redraw || *last != Some(key);
    if needs_redraw {
        *last = Some(key);
        present_widget(widget, x, y, devices, dpi, light_theme);
    }

    if IsWindowVisible(widget) == 0 {
        let _ = ShowWindow(widget, SW_SHOWNOACTIVATE);
    }

    if w_info.owner_tray.is_none() && !is_above_taskbar(widget, tray_hwnd) {
        let _ = SetWindowPos(
            widget,
            HWND_TOPMOST,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
    }

    Some(tray_hwnd)
}

/// Destroy the taskbar block window and cached GDI fonts cleanly.
///
/// # Safety
/// Must be called on the UI thread.
pub unsafe fn destroy(w: WidgetWindow) {
    *LAST_RENDER.get() = None;
    *LAST_OWNER_ATTEMPT.get() = 0;
    (*FONTS.get()).clear();
    if IsWindow(w.hwnd) != 0 {
        let _ = DestroyWindow(w.hwnd);
    }
}

unsafe extern "system" fn widget_proc(hwnd: HWND, msg: u32, wp: usize, lp: isize) -> isize {
    DefWindowProcW(hwnd, msg, wp, lp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hid::Battery;

    #[test]
    fn widget_render_is_uniform_and_monochrome_on_dark_and_light_themes() {
        let mut online_devs = BatteryList::new();
        online_devs.set(Battery::with_name("G502 LIGHTSPEED", Kind::Mouse, Some(86), false, true));
        online_devs.set(Battery::with_name(
            "HyperX Cloud III Wireless",
            Kind::Headset,
            Some(100),
            true,
            true,
        ));

        let mut offline_mouse_devs = BatteryList::new();
        offline_mouse_devs.set(Battery::with_name(
            "G502 LIGHTSPEED",
            Kind::Mouse,
            Some(86),
            false,
            false,
        ));
        offline_mouse_devs.set(Battery::with_name(
            "HyperX Cloud III Wireless",
            Kind::Headset,
            Some(100),
            true,
            true,
        ));

        for dpi in [96u32, 144] {
            for light in [false, true] {
                let (ew, eh) = widget_dimensions(&online_devs, dpi);
                let byte_len = (ew * eh * 4) as usize;
                let mut bgra_on = vec![0u8; byte_len];
                let mut bgra_off = vec![0u8; byte_len];

                let (w, h) = render_widget_premul_bgra(&online_devs, dpi, light, &mut bgra_on);
                let (w2, h2) =
                    render_widget_premul_bgra(&offline_mouse_devs, dpi, light, &mut bgra_off);

                assert_eq!((w, h), (ew, eh));
                assert_eq!((w2, h2), (ew, eh));
                assert_eq!(bgra_on, bgra_off);

                let visible_px = bgra_on
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .filter(|p| p[3] > 60)
                    .count();
                assert!(
                    visible_px > 40,
                    "expected visible glyph/text pixels, got {visible_px} (dpi={dpi}, light={light})"
                );

                for (i, px) in bgra_on.as_chunks::<4>().0.iter().enumerate() {
                    assert!(
                        px[0] == px[1] && px[1] == px[2] && px[0] <= px[3],
                        "invalid premul monochrome pixel at {i}: {:?}",
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

        let (xf, _) = compute_widget_rect(tray, None, 70, 38, 96);
        assert_eq!(xf, 2560 - 340 - 70);
    }

    #[test]
    fn shell_overlay_classes_are_ignored_for_fullscreen_hiding() {
        let to_u16 = |s: &str| -> Vec<u16> { s.encode_utf16().collect() };
        assert!(is_shell_overlay_class_utf16(&to_u16("Progman")));
        assert!(is_shell_overlay_class_utf16(&to_u16("WorkerW")));
        assert!(is_shell_overlay_class_utf16(&to_u16("Shell_TrayWnd")));
        assert!(is_shell_overlay_class_utf16(&to_u16("Windows.UI.Core.CoreWindow")));
        assert!(is_shell_overlay_class_utf16(&to_u16("XamlExplorerHostIslandWindow")));
        assert!(is_shell_overlay_class_utf16(&to_u16("MultitaskingViewFrame")));
        assert!(!is_shell_overlay_class_utf16(&to_u16("Chrome_WidgetWin_1")));
        assert!(!is_shell_overlay_class_utf16(&to_u16("UnrealWindow")));
    }
}
