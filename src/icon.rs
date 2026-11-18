//! Runtime icon rendering from the "Segoe Fluent Icons" system font.
//!
//! No embedded assets: glyphs are drawn with DrawTextW into a 32bpp ARGB
//! bitmap and wrapped into an HICON via CreateIconIndirect. Monochrome
//! (white on dark taskbar, black on light taskbar) so the icon blends in.

use std::ffi::c_void;

use windows::core::{BOOL, PCWSTR};
use windows::Win32::Foundation::{COLORREF, RECT};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, CreateFontW, DeleteDC, DeleteObject, DrawTextW, GetDC,
    ReleaseDC, SelectObject, SetBkMode, SetTextColor, BI_RGB, BITMAPINFO, BITMAPINFOHEADER,
    ANTIALIASED_QUALITY, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, DIB_RGB_COLORS, DT_CENTER,
    DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, OUT_DEFAULT_PRECIS, TRANSPARENT,
};
use windows::Win32::UI::WindowsAndMessaging::{CreateIconIndirect, HICON, ICONINFO};

/// Battery0..Battery10 (empty .. full).
pub const BATTERY_BASE: u32 = 0xEBA0;
/// BatteryCharging0..10 (bolt overlay, empty .. full).
pub const BATTERY_CHARGING_BASE: u32 = 0xEBAB;
/// Mouse glyph.
pub const MOUSE: u32 = 0xE962;
/// Headphones glyph.
pub const HEADSET: u32 = 0xE7F6;
/// Power button glyph.
pub const CLOSE: u32 = 0xE711;

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

/// Render a single Fluent glyph into a `size` x `size` top-down 32bpp BGRA
/// buffer (alpha in byte 3). `color` is the glyph color as 0x00BBGGRR.
pub unsafe fn glyph_bitmap(codepoint: u32, size: i32, color: u32) -> Option<Vec<u8>> {
    let size = size.max(8);
    let screen = GetDC(None);
    let memdc = CreateCompatibleDC(Some(screen));

    let bi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: size,
            biHeight: -size, // top-down
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bits: *mut c_void = std::ptr::null_mut();
    let bmp = CreateDIBSection(Some(memdc), &bi, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
    if bits.is_null() {
        return None;
    }
    std::ptr::write_bytes(bits as *mut u8, 0, (size * size * 4) as usize);
    let old_bmp = SelectObject(memdc, bmp.into());

    // font: prefer Segoe Fluent Icons (Win11), fall back to Segoe MDL2 Assets (Win10)
    let glyph = char::from_u32(codepoint)?;
    let mut text: Vec<u16> = glyph.to_string().encode_utf16().collect();
    let mut font = None;
    for face in ["Segoe Fluent Icons", "Segoe MDL2 Assets"] {
        let face_w: Vec<u16> = face.encode_utf16().chain(std::iter::once(0)).collect();
        let f = CreateFontW(
            -size, 0, 0, 0, 400, 0, 0, 0,
            DEFAULT_CHARSET, OUT_DEFAULT_PRECIS, CLIP_DEFAULT_PRECIS, ANTIALIASED_QUALITY, 0,
            PCWSTR(face_w.as_ptr()),
        );
        if f.0 != std::ptr::null_mut() {
            font = Some(f);
            break;
        }
    }
    let font = font?;
    let old_font = SelectObject(memdc, font.into());
    SetBkMode(memdc, TRANSPARENT);
    SetTextColor(memdc, COLORREF(color));
    let mut rc = RECT { left: 0, top: 0, right: size, bottom: size };
    DrawTextW(memdc, &mut text, &mut rc, DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);
    SelectObject(memdc, old_font);
    let _ = DeleteObject(font.into());

    let mut out = vec![0u8; (size * size * 4) as usize];
    std::ptr::copy_nonoverlapping(bits as *const u8, out.as_mut_ptr(), out.len());

    // GDI leaves the alpha byte at 0 in a 32bpp DIB: derive it from the
    // drawn luminance so the glyph is actually opaque where rendered.
    for px in out.chunks_exact_mut(4) {
        let lum = px[0].max(px[1]).max(px[2]);
        px[3] = lum;
    }

    SelectObject(memdc, old_bmp);
    let _ = DeleteObject(bmp.into());
    let _ = DeleteDC(memdc);
    ReleaseDC(None, screen);
    Some(out)
}

/// Render a single Fluent glyph into a `size` x `size` monochrome HICON.
pub unsafe fn glyph_icon(codepoint: u32, size: i32, color: u32) -> Option<HICON> {
    let size = size.max(8);
    let pixels = glyph_bitmap(codepoint, size, color)?;

    let screen = GetDC(None);
    let memdc = CreateCompatibleDC(Some(screen));

    let make_bi = |bits_count: u16, height: i32| BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: size,
            biHeight: height,
            biPlanes: 1,
            biBitCount: bits_count,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };

    // 32bpp color bitmap from the rendered pixels
    let mut color_bits: *mut c_void = std::ptr::null_mut();
    let color_bmp = CreateDIBSection(Some(memdc), &make_bi(32, -size), DIB_RGB_COLORS, &mut color_bits, None, 0).ok()?;
    if color_bits.is_null() {
        return None;
    }
    std::ptr::copy_nonoverlapping(pixels.as_ptr(), color_bits as *mut u8, pixels.len());

    // 1bpp AND mask built from the alpha channel (glyph = 0 = opaque)
    let mut mask_bits: *mut c_void = std::ptr::null_mut();
    let mask_bmp = CreateDIBSection(Some(memdc), &make_bi(1, -size), DIB_RGB_COLORS, &mut mask_bits, None, 0).ok()?;
    if mask_bits.is_null() {
        return None;
    }
    let stride1 = ((size as usize + 31) / 32) * 4;
    let dst = mask_bits as *mut u8;
    std::ptr::write_bytes(dst, 0xFF, stride1 * size as usize);
    for y in 0..size as usize {
        for x in 0..size as usize {
            let alpha = pixels[(y * size as usize + x) * 4 + 3];
            if alpha > 0 {
                let byte = y * stride1 + x / 8;
                let bit = 7 - (x % 8);
                *dst.add(byte) &= !(1u8 << bit);
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
    let icon = CreateIconIndirect(&ii).ok()?;

    let _ = DeleteObject(mask_bmp.into());
    let _ = DeleteObject(color_bmp.into());
    let _ = DeleteDC(memdc);
    ReleaseDC(None, screen);
    Some(icon)
}
