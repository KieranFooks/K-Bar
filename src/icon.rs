//! Fluent glyph constants, stack UTF-16 percentage formatting, and DPI-cached GDI fonts (`#![no_std]`).

use core::ptr::null_mut;

use crate::hid::Kind;
use crate::w;
use crate::win32::{
    CreateFontW, DeleteObject, ANTIALIASED_QUALITY, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, HFONT,
    OUT_DEFAULT_PRECIS,
};

/// Battery0 fallback glyph when no devices are detected.
pub const BATTERY_BASE: u16 = 0xEBA0;
/// Mouse glyph.
pub const MOUSE: u16 = 0xE962;
/// Headphones glyph.
pub const HEADSET: u16 = 0xE7F6;
/// Lightning bolt glyph (charging indicator).
pub const BOLT: u16 = 0xE945;

pub const fn kind_glyph(kind: Kind) -> u16 {
    match kind {
        Kind::Mouse => MOUSE,
        Kind::Headset => HEADSET,
    }
}

/// Format `"86 %"`, `"100 %"`, or `"--"` directly into a stack UTF-16 buffer (no `core::fmt`).
pub fn format_pct_utf16(pct: Option<u8>, out: &mut [u16; 8]) -> usize {
    let Some(raw) = pct else {
        out[0] = b'-' as u16;
        out[1] = b'-' as u16;
        return 2;
    };
    let p = if raw > 100 { 100 } else { raw };
    let n = if p == 100 {
        out[0] = b'1' as u16;
        out[1] = b'0' as u16;
        out[2] = b'0' as u16;
        3
    } else if p >= 10 {
        out[0] = (b'0' + p / 10) as u16;
        out[1] = (b'0' + p % 10) as u16;
        2
    } else {
        out[0] = (b'0' + p) as u16;
        1
    };
    out[n] = b' ' as u16;
    out[n + 1] = b'%' as u16;
    n + 2
}

pub fn scale_dip(dip: i32, dpi: u32) -> i32 {
    (dip * dpi as i32 + 48) / 96
}

unsafe fn make_font(height: i32, face: *const u16) -> HFONT {
    CreateFontW(
        height,
        0,
        0,
        0,
        400,
        0,
        0,
        0,
        DEFAULT_CHARSET,
        OUT_DEFAULT_PRECIS,
        CLIP_DEFAULT_PRECIS,
        ANTIALIASED_QUALITY,
        0,
        face,
    )
}

unsafe fn make_glyph_font(height: i32) -> HFONT {
    let f = make_font(height, w!("Segoe Fluent Icons"));
    if !f.is_null() {
        f
    } else {
        make_font(height, w!("Segoe MDL2 Assets"))
    }
}

pub struct FontCache {
    pub dpi: u32,
    pub glyph: HFONT,
    pub bolt: HFONT,
    pub text: HFONT,
}

impl Default for FontCache {
    fn default() -> Self {
        Self::empty()
    }
}

impl FontCache {
    pub const fn empty() -> Self {
        Self {
            dpi: 0,
            glyph: null_mut(),
            bolt: null_mut(),
            text: null_mut(),
        }
    }

    /// Ensure cached `HFONT`s match `dpi`, recreating only when DPI changes.
    ///
    /// # Safety
    /// Must be called on the UI thread with GDI access.
    pub unsafe fn ensure_dpi(&mut self, dpi: u32) {
        if self.dpi == dpi && !self.glyph.is_null() && !self.text.is_null() {
            return;
        }
        self.clear();
        self.dpi = dpi;
        self.glyph = make_glyph_font(-scale_dip(11, dpi));
        self.bolt = make_glyph_font(-scale_dip(10, dpi));
        self.text = make_font(-scale_dip(12, dpi), w!("Segoe UI"));
    }

    /// Destroy all cached GDI fonts cleanly.
    ///
    /// # Safety
    /// Must be called on the UI thread when fonts are no longer selected into any DC.
    pub unsafe fn clear(&mut self) {
        for f in [self.glyph, self.bolt, self.text] {
            if !f.is_null() {
                let _ = DeleteObject(f);
            }
        }
        self.glyph = null_mut();
        self.bolt = null_mut();
        self.text = null_mut();
        self.dpi = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_glyph_and_pct_formatting() {
        assert_eq!(kind_glyph(Kind::Mouse), MOUSE);
        assert_eq!(kind_glyph(Kind::Headset), HEADSET);

        let mut buf = [0u16; 8];
        let to_s = |len: usize, b: &[u16; 8]| String::from_utf16_lossy(&b[..len]);

        let n = format_pct_utf16(None, &mut buf);
        assert_eq!(to_s(n, &buf), "--");

        let n = format_pct_utf16(Some(0), &mut buf);
        assert_eq!(to_s(n, &buf), "0 %");

        let n = format_pct_utf16(Some(86), &mut buf);
        assert_eq!(to_s(n, &buf), "86 %");

        let n = format_pct_utf16(Some(150), &mut buf);
        assert_eq!(to_s(n, &buf), "100 %");
    }
}
