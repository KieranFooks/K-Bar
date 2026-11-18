#!/usr/bin/env python3
"""Generate visual proposals for the battery-tray icon set (PNG mockups).

Design: Windows 11 Fluent style, drawn supersampled (4x) then downscaled.
- Tray icon: rounded battery outline + cap, fill proportional to level,
  color by threshold (green >50, orange 20-50, red <20), lightning bolt when charging.
- Dark variant = white outline (dark taskbar), light variant = black outline.
- Device glyphs for the popup: mouse, headset.
- App icon: rounded-square Fluent tile with battery glyph.
"""
import os
from PIL import Image, ImageDraw

OUT = os.path.join(os.path.dirname(__file__), "..", "assets", "proposals")
SS = 4  # supersampling

# Fluent-ish palette
GREEN = (16, 124, 16, 255)      # #107C10
ORANGE = (255, 140, 0, 255)     # #FF8C00
RED = (196, 43, 28, 255)        # #C42B1C
GRAY = (140, 140, 140, 255)
WHITE = (255, 255, 255, 255)
BLACK = (20, 20, 20, 255)
BOLT = (255, 215, 94, 255)      # #FFD75E
TILE = (0, 120, 212, 255)       # Fluent accent blue #0078D4


def color_for(level, charging):
    if charging:
        return BOLT
    if level is None:
        return GRAY
    if level > 50:
        return GREEN
    if level >= 20:
        return ORANGE
    return RED


def draw_battery(size, level, charging, outline):
    """Draw a battery glyph. Returns RGBA image of `size` px."""
    W = H = size * SS
    img = Image.new("RGBA", (W, H), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)
    stroke = max(1, round(size * 0.09)) * SS
    pad = stroke + SS
    cap_w = max(1, round(size * 0.08)) * SS
    # body rect (leave room for cap on the right)
    x0, y0 = pad, pad
    x1, y1 = W - pad - cap_w - stroke, H - pad
    # cap (the little nub on the right)
    cap_h = (y1 - y0) * 0.4
    cy0 = (y0 + y1) / 2 - cap_h / 2
    cy1 = (y0 + y1) / 2 + cap_h / 2
    d.rounded_rectangle([x0, y0, x1, y1], radius=(y1 - y0) * 0.28, outline=outline, width=stroke)
    d.rounded_rectangle([x1 + stroke, cy0, x1 + stroke + cap_w, cy1], radius=cap_w * 0.4, fill=outline)
    # fill
    if level is not None and level > 0:
        inset = stroke + SS
        fx0, fy0 = x0 + inset, y0 + inset
        fx1, fy1 = x1 - inset, y1 - inset
        fw = (fx1 - fx0) * max(0, min(100, level)) / 100.0
        col = color_for(level, charging)
        if fw > 0:
            d.rounded_rectangle([fx0, fy0, fx0 + fw, fy1], radius=(fy1 - fy0) * 0.3, fill=col)
    # charging bolt overlay
    if charging:
        cx, cy = (x0 + x1) / 2, (y0 + y1) / 2
        s = (x1 - x0) * 0.16
        pts = [
            (cx + s * 0.25, cy - s), (cx - s * 0.55, cy + s * 0.15),
            (cx - s * 0.05, cy + s * 0.15), (cx - s * 0.25, cy + s),
            (cx + s * 0.55, cy - s * 0.15), (cx + s * 0.05, cy - s * 0.15),
        ]
        d.polygon(pts, fill=BOLT)
    return img.resize((size, size), Image.LANCZOS)


def draw_mouse(size, outline):
    W = H = size * SS
    img = Image.new("RGBA", (W, H), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)
    stroke = max(1, round(size * 0.08)) * SS
    pad = stroke + SS
    x0, y0, x1, y1 = pad, pad, W - pad, H - pad
    d.rounded_rectangle([x0, y0, x1, y1], radius=(x1 - x0) * 0.45, outline=outline, width=stroke)
    # scroll wheel line
    cx = (x0 + x1) / 2
    d.line([cx, y0 + (y1 - y0) * 0.18, cx, y0 + (y1 - y0) * 0.42], fill=outline, width=stroke)
    return img.resize((size, size), Image.LANCZOS)


def draw_headset(size, outline):
    W = H = size * SS
    img = Image.new("RGBA", (W, H), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)
    stroke = max(1, round(size * 0.09)) * SS
    pad = stroke + SS
    x0, y0, x1, y1 = pad, pad, W - pad, H - pad
    # headband arc
    d.arc([x0, y0, x1, y0 + (y1 - y0) * 1.4], start=180, end=360, fill=outline, width=stroke)
    # ear pads
    ph = (y1 - y0) * 0.34
    pw = (x1 - x0) * 0.22
    d.rounded_rectangle([x0, y1 - ph, x0 + pw, y1], radius=pw * 0.4, fill=outline)
    d.rounded_rectangle([x1 - pw, y1 - ph, x1, y1], radius=pw * 0.4, fill=outline)
    return img.resize((size, size), Image.LANCZOS)


def draw_app_icon(size):
    W = H = size * SS
    img = Image.new("RGBA", (W, H), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)
    r = W * 0.22
    d.rounded_rectangle([0, 0, W - 1, H - 1], radius=r, fill=TILE)
    # white battery glyph centered
    bat = draw_battery(int(size * 0.62), 65, False, WHITE)
    img.alpha_composite(bat, (int(W * 0.19), int(H * 0.22)))
    return img.resize((size, size), Image.LANCZOS)


def main():
    sizes = [16, 20, 24, 32, 48]
    levels = [100, 75, 50, 25, 10, 0, None]
    for theme, outline in (("dark", WHITE), ("light", BLACK)):
        for lvl in levels:
            for charging in (False, True) if lvl in (75, 25) else (False,):
                name = f"tray_{theme}_{'unk' if lvl is None else lvl}{'_chg' if charging else ''}.png"
                d = os.path.join(OUT, "tray", theme)
                os.makedirs(d, exist_ok=True)
                for s in sizes:
                    draw_battery(s, lvl, charging, outline).save(os.path.join(d, f"{s}_{name}"))
    # device glyphs
    for theme, outline in (("dark", WHITE), ("light", BLACK)):
        d = os.path.join(OUT, "devices", theme)
        os.makedirs(d, exist_ok=True)
        for s in (16, 20, 24, 32):
            draw_mouse(s, outline).save(os.path.join(d, f"{s}_mouse.png"))
            draw_headset(s, outline).save(os.path.join(d, f"{s}_headset.png"))
    # app icon
    d = os.path.join(OUT, "app")
    os.makedirs(d, exist_ok=True)
    for s in (32, 48, 64, 128, 256):
        draw_app_icon(s).save(os.path.join(d, f"app_{s}.png"))

    # contact sheet
    cols = []
    sheet_rows = []
    def cell(img, label):
        s = 48
        im = img.resize((s, s), Image.NEAREST) if img.size != (s, s) else img
        c = Image.new("RGBA", (s + 4, s + 18), (40, 40, 40, 255))
        c.paste(im, (2, 2), im)
        dd = ImageDraw.Draw(c)
        dd.text((2, s + 4), label, fill=(255, 255, 255, 255))
        return c
    # tray dark row: levels
    row = []
    for lvl in [100, 75, 50, 25, 10, 0, None]:
        row.append(cell(draw_battery(48, lvl, False, WHITE), str(lvl)))
    row.append(cell(draw_battery(48, 60, True, WHITE), "chg"))
    sheet_rows.append(("tray dark (niveaux + charge)", row))
    row = [cell(draw_battery(48, l, False, BLACK), str(l)) for l in [100, 50, 10]]
    sheet_rows.append(("tray light", row))
    row = [cell(draw_mouse(48, WHITE), "mouse"), cell(draw_headset(48, WHITE), "headset"), cell(draw_app_icon(48), "app")]
    sheet_rows.append(("glyphs + app icon", row))
    cw = max(sum(c.width for c in row) + 8 * len(row) for _, row in sheet_rows)
    ch = sum(row[0].height + 24 for _, row in sheet_rows) + 8 * len(sheet_rows)
    sheet = Image.new("RGBA", (cw + 16, ch + 16), (24, 24, 24, 255))
    y = 8
    dd = ImageDraw.Draw(sheet)
    for label, row in sheet_rows:
        dd.text((8, y), label, fill=(255, 255, 255, 255))
        y += 18
        x = 8
        for c in row:
            sheet.paste(c, (x, y), c)
            x += c.width + 8
        y += row[0].height + 8
    sheet = sheet.convert("RGB")
    sheet.save(os.path.join(OUT, "contact_sheet.png"))
    print("saved to", os.path.abspath(OUT))


if __name__ == "__main__":
    main()
