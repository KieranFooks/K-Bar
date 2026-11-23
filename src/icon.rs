//! Fluent glyph constants, stack UTF-16 percentage formatting, and DPI-cached GDI fonts (`#![no_std]`).

use core::ptr::null_mut;

use crate::hid::Kind;
use crate::w;
use crate::win32::{
    CreateFontW, DeleteObject, ANTIALIASED_QUALITY, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, HFONT,
    OUT_DEFAULT_PRECIS,
};

/// Battery0 fallback / discharging base glyph (`0xEBA0`..=`0xEBAA`, steps 0..=10).
pub const BATTERY_BASE: u16 = 0xEBA0;
/// BatteryCharging0 base glyph (`0xEBAB`..=`0xEBB5`, steps 0..=10).
pub const BATTERY_CHARGING_BASE: u16 = 0xEBAB;
/// BatteryUnknown glyph (`0xEC02`, battery outline with X).
pub const BATTERY_UNKNOWN: u16 = 0xEC02;
/// Mouse glyph.
pub const MOUSE: u16 = 0xE962;
/// Keyboard glyph (rounded Fluent keyboard outline).
pub const KEYBOARD: u16 = 0xE765;
/// Headphones glyph.
pub const HEADSET: u16 = 0xE7F6;
/// Gamepad / Xbox controller glyph.
pub const GAMEPAD: u16 = 0xE7FC;
/// Lightning bolt glyph (charging indicator).
pub const BOLT: u16 = 0xE945;

pub const fn kind_glyph(kind: Kind) -> u16 {
    match kind {
        Kind::Mouse => MOUSE,
        Kind::Keyboard => KEYBOARD,
        Kind::Headset => HEADSET,
        Kind::Gamepad => GAMEPAD,
    }
}

/// Map `(pct, charging)` to the corresponding `Segoe Fluent Icons` battery glyph (`0..=10` steps).
pub const fn battery_glyph(pct: Option<u8>, charging: bool) -> u16 {
    let Some(raw) = pct else {
        return BATTERY_UNKNOWN;
    };
    let p = if raw > 100 { 100 } else { raw } as u16;
    let step = (p + 5) / 10;
    let base = if charging {
        BATTERY_CHARGING_BASE
    } else {
        BATTERY_BASE
    };
    base + step
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
    make_font(height, w!("Segoe Fluent Icons"))
}

pub struct FontCache {
    pub dpi: u32,
    pub glyph: HFONT,
    pub bolt: HFONT,
    pub battery: HFONT,
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
            battery: null_mut(),
            text: null_mut(),
        }
    }

    /// Ensure cached `HFONT`s match `dpi`, recreating only when DPI changes.
    ///
    /// # Safety
    /// Must be called on the UI thread with GDI access.
    pub unsafe fn ensure_dpi(&mut self, dpi: u32) {
        if self.dpi == dpi
            && !self.glyph.is_null()
            && !self.battery.is_null()
            && !self.text.is_null()
        {
            return;
        }
        self.clear();
        self.dpi = dpi;
        self.glyph = make_glyph_font(-scale_dip(11, dpi));
        self.bolt = make_glyph_font(-scale_dip(10, dpi));
        self.battery = make_glyph_font(-scale_dip(14, dpi));
        self.text = make_font(-scale_dip(12, dpi), w!("Segoe UI"));
    }

    /// Destroy all cached GDI fonts cleanly.
    ///
    /// # Safety
    /// Must be called on the UI thread when fonts are no longer selected into any DC.
    pub unsafe fn clear(&mut self) {
        for f in [self.glyph, self.bolt, self.battery, self.text] {
            if !f.is_null() {
                let _ = DeleteObject(f);
            }
        }
        self.glyph = null_mut();
        self.bolt = null_mut();
        self.battery = null_mut();
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
        assert_eq!(kind_glyph(Kind::Keyboard), KEYBOARD);
        assert_eq!(kind_glyph(Kind::Headset), HEADSET);
        assert_eq!(kind_glyph(Kind::Gamepad), GAMEPAD);

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

    #[test]
    fn battery_glyph_maps_steps_charging_and_unknown() {
        assert_eq!(battery_glyph(None, false), BATTERY_UNKNOWN);
        assert_eq!(battery_glyph(None, true), BATTERY_UNKNOWN);

        assert_eq!(battery_glyph(Some(0), false), BATTERY_BASE);
        assert_eq!(battery_glyph(Some(4), false), BATTERY_BASE);
        assert_eq!(battery_glyph(Some(5), false), BATTERY_BASE + 1);
        assert_eq!(battery_glyph(Some(50), false), BATTERY_BASE + 5);
        assert_eq!(battery_glyph(Some(86), false), BATTERY_BASE + 9);
        assert_eq!(battery_glyph(Some(95), false), BATTERY_BASE + 10);
        assert_eq!(battery_glyph(Some(150), false), BATTERY_BASE + 10);

        assert_eq!(battery_glyph(Some(0), true), BATTERY_CHARGING_BASE);
        assert_eq!(battery_glyph(Some(50), true), BATTERY_CHARGING_BASE + 5);
        assert_eq!(battery_glyph(Some(100), true), BATTERY_CHARGING_BASE + 10);
    }
}
