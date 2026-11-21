//! Debug helper: render the unified 2-line taskbar block (Option 1A) over dark
//! and light Windows 11 taskbar backgrounds at 96 DPI and 144 DPI (1x and 3x).
//! Usage: glyphtest.exe [output.bmp]

use battery_tray::{
    hid::{Battery, Kind},
    widget,
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
    out.extend_from_slice(&(height as i32).to_le_bytes());
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

struct Case {
    devices: Vec<Battery>,
    hovered: bool,
    active: bool,
}

fn dev(kind: Kind, pct: Option<u8>, charging: bool, online: bool) -> Battery {
    Battery {
        name: "Device".to_string(),
        kind,
        pct,
        charging,
        online,
    }
}

fn main() {
    let out_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "widget_preview.bmp".to_string());

    let cases = [
        Case {
            devices: vec![dev(Kind::Mouse, Some(86), false, true), dev(Kind::Headset, Some(93), false, true)],
            hovered: false,
            active: false,
        },
        Case {
            devices: vec![dev(Kind::Mouse, Some(86), false, true), dev(Kind::Headset, Some(93), false, true)],
            hovered: true,
            active: false,
        },
        Case {
            devices: vec![dev(Kind::Mouse, Some(86), false, true), dev(Kind::Headset, Some(93), false, true)],
            hovered: false,
            active: true,
        },
        Case {
            devices: vec![dev(Kind::Mouse, Some(100), true, true), dev(Kind::Headset, Some(28), false, true)],
            hovered: true,
            active: false,
        },
        Case {
            devices: vec![dev(Kind::Mouse, Some(86), false, false), dev(Kind::Headset, None, false, false)],
            hovered: false,
            active: false,
        },
        Case {
            devices: vec![],
            hovered: true,
            active: false,
        },
    ];

    let dpis = [96u32, 144];
    let cell_w = 270usize;
    let cell_h = 200usize;
    let cols = cases.len();
    let rows = dpis.len() * 2;
    let width = cols * cell_w;
    let height = rows * cell_h;
    let mut canvas = vec![0x20u8; width * height * 3];

    for (di, &dpi) in dpis.iter().enumerate() {
        for (ti, &light) in [false, true].iter().enumerate() {
            let row = di * 2 + ti;
            let bg = if light { (0xEF, 0xEF, 0xEF) } else { (0x1F, 0x1B, 0x33) };

            for (col, case) in cases.iter().enumerate() {
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

                let (w, h, rgba) =
                    widget::render_widget_rgba(&case.devices, dpi, light, case.hovered, case.active);
                let wu = w as usize;
                let hu = h as usize;

                // 1x native render
                for py in 0..hu {
                    for px in 0..wu {
                        let c = blend_bg(bg, &rgba, py * wu + px);
                        let dx = x0 + 10 + px;
                        let dy = y0 + 10 + py;
                        let idx = (dy * width + dx) * 3;
                        canvas[idx] = c.0;
                        canvas[idx + 1] = c.1;
                        canvas[idx + 2] = c.2;
                    }
                }

                // 2x zoom below it
                let zoom = 2usize;
                let zx0 = x0 + 10;
                let zy0 = y0 + 16 + hu;
                for py in 0..hu {
                    for px in 0..wu {
                        let c = blend_bg(bg, &rgba, py * wu + px);
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
    println!("Wrote widget contact sheet {} ({}x{})", out_path, width, height);
}
