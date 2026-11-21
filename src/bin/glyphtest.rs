//! Debug helper: render tray icons on dark and light backgrounds at 1x and 4x
//! zoom into a single BMP contact sheet for visual verification.
//! Usage: glyphtest.exe [output.bmp]

use battery_tray::{
    hid::Kind,
    icon::{self, BATTERY_BASE},
};

fn blend_bg(bg: (u8, u8, u8), rgba: &[u8], i: usize) -> (u8, u8, u8) {
    let r = rgba[i * 4] as u16;
    let g = rgba[i * 4 + 1] as u16;
    let b = rgba[i * 4 + 2] as u16;
    let a = rgba[i * 4 + 3] as u16;
    let inv = 255 - a;
    (
        ((r * a + bg.0 as u16 * inv + 127) / 255) as u8,
        ((g * a + bg.1 as u16 * inv + 127) / 255) as u8,
        ((b * a + bg.2 as u16 * inv + 127) / 255) as u8,
    )
}

fn write_bmp(path: &str, width: usize, height: usize, rgb: &[u8]) {
    let row_bytes = (width * 3 + 3) & !3;
    let img_bytes = row_bytes * height;
    let file_size = 54 + img_bytes;

    let mut out = Vec::with_capacity(file_size);
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&(file_size as u32).to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&54u32.to_le_bytes());

    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&(width as i32).to_le_bytes());
    out.extend_from_slice(&(height as i32).to_le_bytes()); // bottom-up
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&24u16.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&(img_bytes as u32).to_le_bytes());
    out.extend_from_slice(&2835u32.to_le_bytes());
    out.extend_from_slice(&2835u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());

    let pad = vec![0u8; row_bytes - width * 3];
    for y in (0..height).rev() {
        for x in 0..width {
            let idx = (y * width + x) * 3;
            out.push(rgb[idx + 2]);
            out.push(rgb[idx + 1]);
            out.push(rgb[idx]);
        }
        out.extend_from_slice(&pad);
    }
    std::fs::write(path, out).expect("write bmp");
}

struct VariantSpec {
    kind: Option<Kind>,
    pct: Option<u8>,
    charging: bool,
    online: bool,
}

fn main() {
    let out_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "glyph_preview.bmp".to_string());

    let variants: &[VariantSpec] = &[
        VariantSpec { kind: Some(Kind::Mouse), pct: Some(86), charging: false, online: true },
        VariantSpec { kind: Some(Kind::Mouse), pct: Some(100), charging: true, online: true },
        VariantSpec { kind: Some(Kind::Mouse), pct: Some(86), charging: false, online: false },
        VariantSpec { kind: Some(Kind::Headset), pct: Some(28), charging: false, online: true },
        VariantSpec { kind: Some(Kind::Headset), pct: Some(9), charging: true, online: true },
        VariantSpec { kind: Some(Kind::Headset), pct: None, charging: false, online: false },
        VariantSpec { kind: None, pct: None, charging: false, online: false },
    ];
    let sizes = [16i32, 24, 32];

    let cell_w = 180usize;
    let cell_h = 152usize;
    let cols = variants.len();
    let rows = sizes.len() * 2; // dark + light per size
    let width = cols * cell_w;
    let height = rows * cell_h;
    let mut canvas = vec![0x24u8; width * height * 3];

    for (si, &sz) in sizes.iter().enumerate() {
        for (ti, &light) in [false, true].iter().enumerate() {
            let row = si * 2 + ti;
            let bg = if light { (0xEF, 0xEF, 0xEF) } else { (0x1F, 0x1F, 0x2E) };

            for (col, v) in variants.iter().enumerate() {
                let x0 = col * cell_w;
                let y0 = row * cell_h;
                for y in y0..(y0 + cell_h) {
                    for x in x0..(x0 + cell_w) {
                        let idx = (y * width + x) * 3;
                        canvas[idx] = bg.0;
                        canvas[idx + 1] = bg.1;
                        canvas[idx + 2] = bg.2;
                    }
                }

                let rgba = match v.kind {
                    Some(k) => icon::render_tray_rgba(
                        icon::kind_glyph(k),
                        v.pct,
                        v.charging,
                        v.online,
                        sz,
                        light,
                    ),
                    None => icon::render_centered_glyph_rgba(BATTERY_BASE, sz, light),
                }
                .expect("render icon");

                let s = sz as usize;
                // 1x native render at top-left of cell
                for py in 0..s {
                    for px in 0..s {
                        let c = blend_bg(bg, &rgba, py * s + px);
                        let dx = x0 + 8 + px;
                        let dy = y0 + 8 + py;
                        let idx = (dy * width + dx) * 3;
                        canvas[idx] = c.0;
                        canvas[idx + 1] = c.1;
                        canvas[idx + 2] = c.2;
                    }
                }

                // 4x zoom (or 3x at 32px) right beside it
                let zoom = if sz >= 32 { 3 } else { 4 };
                let zx0 = x0 + 16 + s;
                let zy0 = y0 + 8;
                for py in 0..s {
                    for px in 0..s {
                        let c = blend_bg(bg, &rgba, py * s + px);
                        for zy in 0..zoom {
                            for zx in 0..zoom {
                                let dx = zx0 + px * zoom + zx;
                                let dy = zy0 + py * zoom + zy;
                                if dx < x0 + cell_w && dy < y0 + cell_h {
                                    let idx = (dy * width + dx) * 3;
                                    canvas[idx] = c.0;
                                    canvas[idx + 1] = c.1;
                                    canvas[idx + 2] = c.2;
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    write_bmp(&out_path, width, height, &canvas);
    println!("Wrote contact sheet {} ({}x{})", out_path, width, height);
}
