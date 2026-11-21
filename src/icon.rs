//! Shared Fluent glyph constants, grayscale GDI coverage-mask rendering,
//! and straight-RGBA / premultiplied-BGRA compositing helpers.
//!
//! Uses `ANTIALIASED_QUALITY` (grayscale coverage, zero ClearType RGB fringes)
//! so the unified taskbar block stays strictly monochrome.

use std::ffi::c_void;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{COLORREF, RECT};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, CreateFontW, DeleteDC, DeleteObject, DrawTextW, GetDC,
    ReleaseDC, SelectObject, SetBkMode, SetTextColor, ANTIALIASED_QUALITY, BI_RGB, BITMAPINFO,
    BITMAPINFOHEADER, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, DIB_RGB_COLORS, DRAW_TEXT_FORMAT,
    DT_CENTER, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, OUT_DEFAULT_PRECIS, TRANSPARENT,
};

use crate::hid::Kind;

/// Battery0..Battery10 fallback glyph when no devices are detected.
pub const BATTERY_BASE: u32 = 0xEBA0;
/// Mouse glyph.
pub const MOUSE: u32 = 0xE962;
/// Headphones glyph.
pub const HEADSET: u32 = 0xE7F6;
/// Lightning bolt glyph (charging indicator).
pub const BOLT: u32 = 0xE945;

pub fn kind_glyph(kind: Kind) -> u32 {
    match kind {
        Kind::Mouse => MOUSE,
        Kind::Headset => HEADSET,
    }
}

/// Render white text/glyph onto a black 32bpp DIB using `ANTIALIASED_QUALITY`
/// (grayscale only, zero ClearType RGB fringes) and return a `w * h` 8-bit
/// coverage mask (`0..=255`).
///
/// # Safety
/// Must be called on a thread with GDI access. All temporary GDI objects are
/// destroyed before returning.
pub(crate) unsafe fn render_text_mask_aligned(
    text: &str,
    faces: &[&str],
    w: i32,
    h: i32,
    font_height: i32,
    weight: i32,
    align: DRAW_TEXT_FORMAT,
) -> Option<Vec<u8>> {
    let w = w.max(2);
    let h = h.max(2);
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
    DrawTextW(memdc, &mut wide, &mut rc, align);

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

/// Render a centered Fluent icon glyph into a `w * h` 8-bit grayscale mask.
///
/// # Safety
/// Must be called on a thread with GDI access.
pub(crate) unsafe fn render_glyph_mask(
    codepoint: u32,
    w: i32,
    h: i32,
    font_px: i32,
) -> Option<Vec<u8>> {
    let ch = char::from_u32(codepoint)?;
    let mut s = [0u8; 4];
    let text = ch.encode_utf8(&mut s);
    render_text_mask_aligned(
        text,
        &["Segoe Fluent Icons", "Segoe MDL2 Assets"],
        w,
        h,
        -font_px.abs(),
        400,
        DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
    )
}

/// Composite `src_rgb` with `alpha` onto straight RGBA pixel at `(x, y)` inside `w x h`.
pub(crate) fn composite_px(
    rgba: &mut [u8],
    w: usize,
    h: usize,
    x: i32,
    y: i32,
    rgb: (u8, u8, u8),
    alpha: u8,
) {
    if alpha == 0 || x < 0 || y < 0 || x >= w as i32 || y >= h as i32 {
        return;
    }
    let idx = (y as usize * w + x as usize) * 4;
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

/// Convert straight RGBA pixels into premultiplied BGRA (for `UpdateLayeredWindow`).
pub(crate) fn rgba_to_premul_bgra(rgba: &[u8], dst_bgra: &mut [u8]) {
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_glyph_mapping() {
        assert_eq!(kind_glyph(Kind::Mouse), MOUSE);
        assert_eq!(kind_glyph(Kind::Headset), HEADSET);
    }

    #[test]
    fn grayscale_text_and_glyph_masks_produce_nonzero_coverage() {
        unsafe {
            let m1 = render_glyph_mask(MOUSE, 16, 16, 12).expect("mouse glyph mask");
            assert_eq!(m1.len(), 256);
            assert!(m1.iter().any(|&v| v > 100));

            let m2 = render_text_mask_aligned(
                "86 %",
                &["Segoe UI"],
                44,
                16,
                -12,
                400,
                DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
            )
            .expect("text mask");
            assert_eq!(m2.len(), 44 * 16);
            assert!(m2.iter().any(|&v| v > 100));
        }
    }

    #[test]
    fn premul_bgra_swaps_channels_and_premultiplies_alpha() {
        let rgba = [200u8, 100, 50, 128];
        let mut bgra = [0u8; 4];
        rgba_to_premul_bgra(&rgba, &mut bgra);
        assert_eq!(bgra[3], 128);
        assert_eq!(bgra[2], 100); // R
        assert_eq!(bgra[1], 50); // G
        assert_eq!(bgra[0], 25); // B
    }
}
