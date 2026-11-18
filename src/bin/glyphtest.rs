//! Debug helper: render Fluent glyphs to raw 32bpp BGRA files for visual check.
//! Usage: glyphtest.exe <outdir>

use battery_tray::icon;

fn main() {
    let outdir = std::env::args().nth(1).unwrap_or_else(|| ".".to_string());
    unsafe {
        for (cp, name) in [
            (0xEBA0u32, "battery0"),
            (0xEBA3, "battery3"),
            (0xEBA8, "battery8"),
            (0xEBAA, "battery10"),
            (0xEBAB, "charging0"),
            (0xEBB0, "charging5"),
            (0xEBB5, "charging10"),
            (0xE962, "mouse"),
            (0xE7F6, "headset"),
            (0xE7E9, "power"),
        ] {
            if let Some(px) = icon::glyph_bitmap(cp, 32, 0x00FF_FFFF) {
                let path = format!("{}/glyph_{}_{:04X}.raw", outdir, name, cp);
                std::fs::write(&path, &px).unwrap();
                let nonzero = px.chunks(4).filter(|c| c[3] > 0).count();
                println!("{} -> {} bytes, {} opaque px", name, px.len(), nonzero);
            } else {
                println!("{} -> RENDER FAILED", name);
            }
        }
    }
}
