//! Runtime monochrome icon rendering using Windows system Fluent/UI fonts
//! and a crisp 3x5 pixel bitmap font for 16-20px taskbars.
//!
//! Uses grayscale coverage masks (never ClearType RGB fringes, never luminance-
//! derived alpha) composited into straight RGBA, then converted to premultiplied
//! 32bpp BGRA + 1bpp AND mask `HICON`s so both dark and light Windows 11 taskbars
//! look razor sharp and strictly monochrome.

use std::ffi::c_void;

use windows::core::{BOOL, PCWSTR};
use windows::Win32::Foundation::{COLORREF, RECT};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, CreateFontW, DeleteDC, DeleteObject, DrawTextW, GetDC,
    ReleaseDC, SelectObject, SetBkMode, SetTextColor, ANTIALIASED_QUALITY, BI_RGB, BITMAPINFO,
    BITMAPINFOHEADER, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, DIB_RGB_COLORS, DT_CENTER,
    DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, OUT_DEFAULT_PRECIS, TRANSPARENT,
};
use windows::Win32::UI::WindowsAndMessaging::{CreateIconIndirect, HICON, ICONINFO};

use crate::hid::Kind;

/// Battery0..Battery10 (empty .. full).
pub const BATTERY_BASE: u32 = 0xEBA0;
/// BatteryCharging0..10 (bolt overlay, empty .. full).
pub const BATTERY_CHARGING_BASE: u32 = 0xEBAB;
/// Mouse glyph.
pub const MOUSE: u32 = 0xE962;
/// Headphones glyph.
pub const HEADSET: u32 = 0xE7F6;
/// Refresh / Sync glyph.
pub const REFRESH: u32 = 0xE72C;
/// Checkmark glyph.
pub const CHECK: u32 = 0xE73E;
/// Power glyph.
pub const POWER: u32 = 0xE7E8;

/// Map a battery level (0-100 %) + charging flag to a Fluent glyph codepoint.
pub fn battery_glyph(pct: Option<u8>, charging: bool) -> u32 {
    let lvl = match pct {
        Some(p) => ((p as u32 + 5) / 10).min(10),
        None => 0,
    };
    if charging {
        BATTERY_CHARGING_BASE + lvl
    } else {
        BATTERY_BASE + lvl
    }
}

pub fn kind_glyph(kind: Kind) -> u32 {
    match kind {
        Kind::Mouse => MOUSE,
        Kind::Headset => HEADSET,
    }
}

/// Render white text/glyph onto a black 32bpp DIB using `ANTIALIASED_QUALITY`
/// (grayscale only, no ClearType RGB subpixel fringes) and return a `w * h`
/// 8-bit coverage mask (`0..=255`).
unsafe fn render_text_mask(
    text: &str,
    faces: &[&str],
    w: i32,
    h: i32,
    font_height: i32,
    weight: i32,
) -> Option<Vec<u8>> {
    let w = w.max(4);
    let h = h.max(4);
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
    let bmp = match CreateDIBSection(Some(memdc), &bi, DIB_RGB_COLORS, &mut bits, None, 0) {
        Ok(b) if !bits.is_null() => b,
        _ => {
            let _ = DeleteDC(memdc);
            ReleaseDC(None, screen);
            return None;
        }
    };
    std::ptr::write_bytes(bits as *mut u8, 0, (w * h * 4) as usize);
    let old_bmp = SelectObject(memdc, bmp.into());

    let mut font = None;
    for face in faces {
        let face_w: Vec<u16> = face.encode_utf16().chain(std::iter::once(0)).collect();
        let f = CreateFontW(
            font_height,
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
        );
        if !f.0.is_null() {
            font = Some(f);
            break;
        }
    }

    let Some(font) = font else {
        SelectObject(memdc, old_bmp);
        let _ = DeleteObject(bmp.into());
        let _ = DeleteDC(memdc);
        ReleaseDC(None, screen);
        return None;
    };

    let old_font = SelectObject(memdc, font.into());
    SetBkMode(memdc, TRANSPARENT);
    SetTextColor(memdc, COLORREF(0x00FF_FFFF));
    let mut wide: Vec<u16> = text.encode_utf16().collect();
    let mut rc = RECT {
        left: 0,
        top: 0,
        right: w,
        bottom: h,
    };
    DrawTextW(
        memdc,
        &mut wide,
        &mut rc,
        DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
    );

    let slice = std::slice::from_raw_parts(bits as *const u8, (w * h * 4) as usize);
    let mut mask = Vec::with_capacity((w * h) as usize);
    for px in slice.as_chunks::<4>().0 {
        let avg = ((px[0] as u16 + px[1] as u16 + px[2] as u16) / 3) as u8;
        mask.push(avg);
    }

    SelectObject(memdc, old_font);
    let _ = DeleteObject(font.into());
    SelectObject(memdc, old_bmp);
    let _ = DeleteObject(bmp.into());
    let _ = DeleteDC(memdc);
    ReleaseDC(None, screen);
    Some(mask)
}

unsafe fn render_glyph_mask(codepoint: u32, w: i32, h: i32, font_px: i32) -> Option<Vec<u8>> {
    let ch = char::from_u32(codepoint)?;
    let mut s = [0u8; 4];
    let text = ch.encode_utf8(&mut s);
    render_text_mask(
        text,
        &["Segoe Fluent Icons", "Segoe MDL2 Assets"],
        w,
        h,
        -font_px.abs(),
        400,
    )
}

// ------------------------------------------------ Crisp 3x5 pixel digits ----

fn glyph_3x5(c: char) -> [u8; 5] {
    match c {
        '0' => [0b111, 0b101, 0b101, 0b101, 0b111],
        '1' => [0b010, 0b110, 0b010, 0b010, 0b111],
        '2' => [0b111, 0b001, 0b111, 0b100, 0b111],
        '3' => [0b111, 0b001, 0b111, 0b001, 0b111],
        '4' => [0b101, 0b101, 0b111, 0b001, 0b001],
        '5' => [0b111, 0b100, 0b111, 0b001, 0b111],
        '6' => [0b111, 0b100, 0b111, 0b101, 0b111],
        '7' => [0b111, 0b001, 0b001, 0b010, 0b010],
        '8' => [0b111, 0b101, 0b111, 0b101, 0b111],
        '9' => [0b111, 0b101, 0b111, 0b001, 0b111],
        _ => [0b000, 0b000, 0b111, 0b000, 0b000],
    }
}

/// Crisp 4x6 monochrome lightning bolt pattern (bit 3 = leftmost pixel).
const BOLT_4X6: [u8; 6] = [
    0b0011,
    0b0100,
    0b1111,
    0b0010,
    0b0100,
    0b1000,
];

// -------------------------------------------------- RGBA layer compositing --

fn blend_u8(dst: u8, src: u8, alpha: u8) -> u8 {
    let a = alpha as u16;
    let inv = 255 - a;
    ((src as u16 * a + dst as u16 * inv + 127) / 255) as u8
}

/// Composite `src_rgb` with `alpha` onto straight RGBA pixel at `(x, y)`.
fn composite_px(rgba: &mut [u8], size: usize, x: i32, y: i32, rgb: (u8, u8, u8), alpha: u8) {
    if alpha == 0 || x < 0 || y < 0 || x >= size as i32 || y >= size as i32 {
        return;
    }
    let idx = (y as usize * size + x as usize) * 4;
    let dst_a = rgba[idx + 3] as u16;
    let src_a = alpha as u16;
    let out_a = src_a + (dst_a * (255 - src_a) + 127) / 255;
    if out_a == 0 {
        return;
    }
    let dst_weight = (dst_a * (255 - src_a) + 127) / 255;
    for (c, s) in [rgb.0, rgb.1, rgb.2].into_iter().enumerate() {
        let num = s as u16 * src_a + rgba[idx + c] as u16 * dst_weight;
        rgba[idx + c] = ((num + out_a / 2) / out_a).min(255) as u8;
    }
    rgba[idx + 3] = out_a.min(255) as u8;
}

/// Punch transparency into existing canvas (for crisp 1px halos around overlays).
fn punch_alpha(rgba: &mut [u8], size: usize, x: i32, y: i32, erase: u8) {
    if erase == 0 || x < 0 || y < 0 || x >= size as i32 || y >= size as i32 {
        return;
    }
    let idx = (y as usize * size + x as usize) * 4;
    let a = rgba[idx + 3] as u16;
    rgba[idx + 3] = ((a * (255 - erase as u16) + 127) / 255) as u8;
}

/// Paint opaque `rgb` onto an already-opaque pill pixel with coverage `cov` (keeps alpha = 255).
fn paint_over_solid(rgba: &mut [u8], size: usize, x: i32, y: i32, rgb: (u8, u8, u8), cov: u8) {
    if cov == 0 || x < 0 || y < 0 || x >= size as i32 || y >= size as i32 {
        return;
    }
    let idx = (y as usize * size + x as usize) * 4;
    rgba[idx] = blend_u8(rgba[idx], rgb.0, cov);
    rgba[idx + 1] = blend_u8(rgba[idx + 1], rgb.1, cov);
    rgba[idx + 2] = blend_u8(rgba[idx + 2], rgb.2, cov);
}

fn sdf_round_rect(px: f32, py: f32, x0: f32, y0: f32, x1: f32, y1: f32, r: f32) -> f32 {
    let cx = (x0 + x1) * 0.5;
    let cy = (y0 + y1) * 0.5;
    let hx = (x1 - x0) * 0.5 - r;
    let hy = (y1 - y0) * 0.5 - r;
    let dx = (px - cx).abs() - hx;
    let dy = (py - cy).abs() - hy;
    let ox = dx.max(0.0);
    let oy = dy.max(0.0);
    (ox * ox + oy * oy).sqrt() + dx.max(dy).min(0.0) - r
}

fn draw_charging_bolt(rgba: &mut [u8], size: usize, fg: (u8, u8, u8), fg_alpha: u8) {
    let scale = (size / 16).max(1) as i32;
    let bw = 4 * scale;
    let bh = 6 * scale;
    let bx = size as i32 - bw - if size >= 24 { 1 } else { 0 };
    let by = if size >= 24 { 1 } else { 0 };

    let mut bolt_px = vec![false; (bw * bh) as usize];
    for (row_idx, &bits) in BOLT_4X6.iter().enumerate() {
        for col in 0..4 {
            if (bits >> (3 - col)) & 1 != 0 {
                for sy in 0..scale {
                    for sx in 0..scale {
                        let px = col * scale + sx;
                        let py = row_idx as i32 * scale + sy;
                        bolt_px[(py * bw + px) as usize] = true;
                    }
                }
            }
        }
    }

    // 1px transparent halo around bolt so underlying glyph never merges into it
    for py in -1..=bh {
        for px in -1..=bw {
            let mut near = false;
            for oy in -1..=1 {
                for ox in -1..=1 {
                    let nx = px + ox;
                    let ny = py + oy;
                    if nx >= 0 && ny >= 0 && nx < bw && ny < bh && bolt_px[(ny * bw + nx) as usize] {
                        near = true;
                    }
                }
            }
            if near {
                punch_alpha(rgba, size, bx + px, by + py, 220);
            }
        }
    }

    for py in 0..bh {
        for px in 0..bw {
            if bolt_px[(py * bw + px) as usize] {
                composite_px(rgba, size, bx + px, by + py, fg, fg_alpha);
            }
        }
    }
}

/// Render a monochrome tray icon as straight RGBA (`size * size * 4` bytes).
/// Strictly monochrome (`r == g == b` on every non-transparent pixel) for both
/// dark and light taskbars.
pub fn render_tray_rgba(
    codepoint: u32,
    pct: Option<u8>,
    charging: bool,
    online: bool,
    size: i32,
    light_theme: bool,
) -> Option<Vec<u8>> {
    let size = size.max(16) as usize;
    let mut rgba = vec![0u8; size * size * 4];

    // Strict monochrome neutral palette matching Windows 11 taskbar glyphs
    let (fg, inv): ((u8, u8, u8), (u8, u8, u8)) = if light_theme {
        ((27, 27, 27), (255, 255, 255))
    } else {
        ((255, 255, 255), (24, 24, 24))
    };

    if let Some(p) = pct {
        let p = p.min(100);
        let badge_text = p.to_string();
        let glyph_alpha_scale: u16 = if online { 255 } else { 155 };
        let pill_alpha: u8 = if online { 255 } else { 190 };

        // 1. Device glyph in top-left (~81% box)
        let gbox = ((size as f32) * 0.81).round() as i32;
        let gmask = unsafe { render_glyph_mask(codepoint, gbox, gbox, gbox) }?;
        for y in 0..gbox {
            for x in 0..gbox {
                let cov = gmask[(y * gbox + x) as usize];
                let a = ((cov as u16 * glyph_alpha_scale + 127) / 255) as u8;
                composite_px(&mut rgba, size, x, y, fg, a);
            }
        }

        // 2. Optional charging bolt in top-right
        if charging {
            draw_charging_bolt(&mut rgba, size, fg, glyph_alpha_scale as u8);
        }

        // 3. Bottom-right percentage pill
        if size <= 22 {
            // Crisp pixel-exact badge for 16x16 and 20x20 taskbars
            let is_100 = badge_text.len() >= 3;
            let text_w: i32 = if is_100 {
                9 // 1px '1' + 1 + 3px '0' + 1 + 3px '0'
            } else {
                badge_text.len() as i32 * 3 + (badge_text.len() as i32 - 1)
            };
            let pad_x = if badge_text.len() == 1 { 2 } else { 1 };
            let pad_y = 1;
            let pill_w = text_w + pad_x * 2;
            let pill_h = 5 + pad_y * 2;
            let px0 = size as i32 - pill_w;
            let py0 = size as i32 - pill_h;

            let is_pill = |rx: i32, ry: i32| -> bool {
                if rx < 0 || ry < 0 || rx >= pill_w || ry >= pill_h {
                    return false;
                }
                let corner = (rx == 0 || rx == pill_w - 1) && (ry == 0 || ry == pill_h - 1);
                !corner
            };

            // 1px transparent halo around pill so glyph lines don't touch pill edge
            for ry in -1..=pill_h {
                for rx in -1..=pill_w {
                    if is_pill(rx, ry) {
                        continue;
                    }
                    let adj = is_pill(rx - 1, ry)
                        || is_pill(rx + 1, ry)
                        || is_pill(rx, ry - 1)
                        || is_pill(rx, ry + 1);
                    if adj {
                        punch_alpha(&mut rgba, size, px0 + rx, py0 + ry, 235);
                    }
                }
            }

            // Solid pill
            for ry in 0..pill_h {
                for rx in 0..pill_w {
                    if is_pill(rx, ry) {
                        composite_px(&mut rgba, size, px0 + rx, py0 + ry, fg, pill_alpha);
                    }
                }
            }

            // Crisp inverse digits inside pill (no transparent holes!)
            let mut cx = px0 + pad_x;
            let cy = py0 + pad_y;
            for (di, ch) in badge_text.chars().enumerate() {
                if is_100 && di == 0 && ch == '1' {
                    for row in 0..5 {
                        paint_over_solid(&mut rgba, size, cx, cy + row, inv, 255);
                    }
                    cx += 2;
                } else {
                    let rows = glyph_3x5(ch);
                    for (row, &bits) in rows.iter().enumerate() {
                        for col in 0..3 {
                            if (bits >> (2 - col)) & 1 != 0 {
                                paint_over_solid(
                                    &mut rgba,
                                    size,
                                    cx + col,
                                    cy + row as i32,
                                    inv,
                                    255,
                                );
                            }
                        }
                    }
                    cx += 4;
                }
            }
        } else {
            // HiDPI (24px, 32px, 48px+): SDF rounded pill + Segoe UI Bold mask
            let s = size as f32;
            let pill_h = (s * 0.44).round();
            let pill_w = (s * match badge_text.len() {
                1 => 0.42,
                2 => 0.56,
                _ => 0.70,
            })
            .round();
            let x0 = s - pill_w - 0.5;
            let y0 = s - pill_h - 0.5;
            let x1 = s - 0.5;
            let y1 = s - 0.5;
            let rad = pill_h * 0.34;

            for y in (y0 as i32 - 2).max(0)..size as i32 {
                for x in (x0 as i32 - 2).max(0)..size as i32 {
                    let d = sdf_round_rect(x as f32 + 0.5, y as f32 + 0.5, x0, y0, x1, y1, rad);
                    if d < 1.3 {
                        let halo = (1.3 - d).clamp(0.0, 1.0);
                        punch_alpha(&mut rgba, size, x, y, (halo * 230.0) as u8);
                    }
                    if d < 0.6 {
                        let cov = (0.6 - d).clamp(0.0, 1.0);
                        let a = ((cov * pill_alpha as f32).round() as i32).clamp(0, 255) as u8;
                        composite_px(&mut rgba, size, x, y, fg, a);
                    }
                }
            }

            let pw = pill_w.round() as i32;
            let ph = pill_h.round() as i32;
            let font_px = -(ph - 2).max(8);
            if let Some(tmask) =
                unsafe { render_text_mask(&badge_text, &["Segoe UI"], pw, ph, font_px, 700) }
            {
                let bx = x0.round() as i32;
                let by = y0.round() as i32;
                for ty in 0..ph {
                    for tx in 0..pw {
                        let cov = tmask[(ty * pw + tx) as usize];
                        if cov > 0 {
                            paint_over_solid(&mut rgba, size, bx + tx, by + ty, inv, cov);
                        }
                    }
                }
            }
        }
    } else {
        // Offline / unknown level: centered dimmed monochrome device glyph
        let sz = size as i32;
        let gmask = unsafe { render_glyph_mask(codepoint, sz, sz, sz) }?;
        for y in 0..sz {
            for x in 0..sz {
                let cov = gmask[(y * sz + x) as usize];
                let a = ((cov as u16 * 120 + 127) / 255) as u8;
                composite_px(&mut rgba, size, x, y, fg, a);
            }
        }
    }

    Some(rgba)
}

/// Render a centered single glyph (used for placeholder icon when no devices are present).
pub fn render_centered_glyph_rgba(codepoint: u32, size: i32, light_theme: bool) -> Option<Vec<u8>> {
    let size = size.max(16) as usize;
    let fg: (u8, u8, u8) = if light_theme { (27, 27, 27) } else { (255, 255, 255) };
    let mask = unsafe { render_glyph_mask(codepoint, size as i32, size as i32, size as i32) }?;
    let mut rgba = vec![0u8; size * size * 4];
    for (i, &cov) in mask.iter().enumerate() {
        let idx = i * 4;
        rgba[idx] = fg.0;
        rgba[idx + 1] = fg.1;
        rgba[idx + 2] = fg.2;
        rgba[idx + 3] = cov;
    }
    Some(rgba)
}

/// Convert straight RGBA pixels into a premultiplied 32bpp BGRA + 1bpp AND-mask HICON.
///
/// # Safety
/// Must be called on a thread with access to GDI / User32; caller owns the returned `HICON`
/// and must destroy it via `DestroyIcon`.
pub unsafe fn rgba_to_hicon(rgba: &[u8], size: i32) -> Option<HICON> {
    let sz = size as usize;
    if rgba.len() != sz * sz * 4 {
        return None;
    }

    let screen = GetDC(None);
    let memdc = CreateCompatibleDC(Some(screen));

    let make_bi = |bits_count: u16| BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: size,
            biHeight: -size,
            biPlanes: 1,
            biBitCount: bits_count,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };

    let mut color_bits: *mut c_void = std::ptr::null_mut();
    let color_bmp =
        match CreateDIBSection(Some(memdc), &make_bi(32), DIB_RGB_COLORS, &mut color_bits, None, 0) {
            Ok(b) if !color_bits.is_null() => b,
            _ => {
                let _ = DeleteDC(memdc);
                ReleaseDC(None, screen);
                return None;
            }
        };

    let mut mask_bits: *mut c_void = std::ptr::null_mut();
    let mask_bmp =
        match CreateDIBSection(Some(memdc), &make_bi(1), DIB_RGB_COLORS, &mut mask_bits, None, 0) {
            Ok(b) if !mask_bits.is_null() => b,
            _ => {
                let _ = DeleteObject(color_bmp.into());
                let _ = DeleteDC(memdc);
                ReleaseDC(None, screen);
                return None;
            }
        };

    // Write premultiplied BGRA (Windows alpha icons expect premultiplied RGB)
    let dst_bgra = std::slice::from_raw_parts_mut(color_bits as *mut u8, sz * sz * 4);
    for (src, dst) in rgba
        .as_chunks::<4>()
        .0
        .iter()
        .zip(dst_bgra.as_chunks_mut::<4>().0.iter_mut())
    {
        let a = src[3] as u16;
        dst[0] = ((src[2] as u16 * a + 127) / 255) as u8; // B
        dst[1] = ((src[1] as u16 * a + 127) / 255) as u8; // G
        dst[2] = ((src[0] as u16 * a + 127) / 255) as u8; // R
        dst[3] = a as u8;
    }

    // 1bpp AND mask: bit=1 transparent (alpha == 0), bit=0 opaque (alpha > 0)
    let stride1 = sz.div_ceil(32) * 4;
    let dst_mask = mask_bits as *mut u8;
    std::ptr::write_bytes(dst_mask, 0xFF, stride1 * sz);
    for y in 0..sz {
        for x in 0..sz {
            if rgba[(y * sz + x) * 4 + 3] > 0 {
                let byte = y * stride1 + x / 8;
                let bit = 7 - (x % 8);
                *dst_mask.add(byte) &= !(1u8 << bit);
            }
        }
    }

    let ii = ICONINFO {
        fIcon: BOOL(1),
        xHotspot: 0,
        yHotspot: 0,
        hbmMask: mask_bmp,
        hbmColor: color_bmp,
    };
    let icon = CreateIconIndirect(&ii).ok();

    let _ = DeleteObject(mask_bmp.into());
    let _ = DeleteObject(color_bmp.into());
    let _ = DeleteDC(memdc);
    ReleaseDC(None, screen);
    icon
}

/// Build an `HICON` for a peripheral tray slot.
///
/// # Safety
/// Must be called on a thread with GDI/User32 access; caller owns the returned `HICON`.
pub unsafe fn device_icon(
    kind: Kind,
    pct: Option<u8>,
    charging: bool,
    online: bool,
    size: i32,
    light_theme: bool,
) -> Option<HICON> {
    let rgba = render_tray_rgba(kind_glyph(kind), pct, charging, online, size, light_theme)?;
    rgba_to_hicon(&rgba, size)
}

/// Build the fallback placeholder `HICON` when no devices are detected yet.
///
/// # Safety
/// Must be called on a thread with GDI/User32 access; caller owns the returned `HICON`.
pub unsafe fn placeholder_icon(size: i32, light_theme: bool) -> Option<HICON> {
    let rgba = render_centered_glyph_rgba(BATTERY_BASE, size, light_theme)?;
    rgba_to_hicon(&rgba, size)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tray_icons_are_visible_and_monochrome_on_dark_and_light_themes() {
        for size in [16, 20, 24, 32] {
            for light in [false, true] {
                for &(cp, pct, charging, online) in &[
                    (MOUSE, Some(86), false, true),
                    (MOUSE, Some(100), true, true),
                    (HEADSET, Some(28), false, true),
                    (HEADSET, Some(9), true, true),
                    (HEADSET, None, false, false),
                ] {
                    let rgba = render_tray_rgba(cp, pct, charging, online, size, light)
                        .expect("render_tray_rgba failed");
                    assert_eq!(rgba.len(), (size * size * 4) as usize);

                    let opaque_count = rgba.as_chunks::<4>().0.iter().filter(|p| p[3] > 20).count();
                    assert!(
                        opaque_count > 10,
                        "too few visible pixels ({opaque_count}) for size={size} light={light} pct={pct:?}"
                    );

                    // Verify strict monochrome (r == g == b) on every pixel
                    for (idx, px) in rgba.as_chunks::<4>().0.iter().enumerate() {
                        assert!(
                            px[0] == px[1] && px[1] == px[2],
                            "non-monochrome pixel at {idx} (size={size} light={light}): {:?}",
                            px
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn pill_digits_do_not_punch_transparent_holes() {
        for light in [false, true] {
            let rgba = render_tray_rgba(MOUSE, Some(88), false, true, 16, light).unwrap();
            // Bottom-right corner inside the pill must remain fully opaque (alpha == 255)
            let mut opaque_pill_pixels = 0usize;
            for y in 10..15 {
                for x in 8..15 {
                    let a = rgba[(y * 16 + x) * 4 + 3];
                    if a == 255 {
                        opaque_pill_pixels += 1;
                    }
                }
            }
            assert!(
                opaque_pill_pixels >= 25,
                "pill region has transparent holes (only {opaque_pill_pixels} opaque px) on light={light}"
            );
        }
    }
}
