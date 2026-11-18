#!/usr/bin/env python3
"""Render glyph ranges from Segoe Fluent Icons / Segoe MDL2 Assets to find
battery / mouse / headset codepoints."""
import os
from PIL import Image, ImageDraw, ImageFont

FONTS = "C:/Windows/Fonts"
OUT = os.path.join(os.path.dirname(__file__), "..", "assets", "system-icons")

RANGES = {
    "battery_guess_E8B0-E8D0": range(0xE8B0, 0xE8D1),
    "battery_guess_EB80-EBB0": range(0xEB80, 0xEBB1),
    "devices_E960-E9A0": range(0xE960, 0xE9A1),
    "devices_E7C0-E800": range(0xE7C0, 0xE801),
    "devices_EA00-EA40": range(0xEA00, 0xEA41),
}

def probe(font_path, ranges, out_png):
    font = ImageFont.truetype(font_path, 28)
    cells = []
    for name, rng in ranges.items():
        for cp in rng:
            ch = chr(cp)
            img = Image.new("RGBA", (36, 44), (32, 32, 32, 255))
            d = ImageDraw.Draw(img)
            try:
                d.text((4, 2), ch, font=font, fill=(255, 255, 255, 255))
            except Exception:
                pass
            d.text((2, 32), f"U+{cp:04X}", fill=(180, 180, 180))
            cells.append((f"U+{cp:04X}", img))
    cols = 16
    rows = (len(cells) + cols - 1) // cols
    sheet = Image.new("RGB", (cols * 40 + 8, rows * 48 + 8), (24, 24, 24))
    d = ImageDraw.Draw(sheet)
    for i, (label, im) in enumerate(cells):
        x = 8 + (i % cols) * 40
        y = 8 + (i // cols) * 48
        sheet.paste(im.convert("RGB"), (x, y))
    sheet.save(out_png)
    print(out_png, sheet.size, len(cells), "glyphs from", os.path.basename(font_path))

fluent = os.path.join(FONTS, "SegoeIcons.ttf")
mdl2 = os.path.join(FONTS, "segmdl2.ttf")
if os.path.exists(fluent):
    probe(fluent, RANGES, os.path.join(OUT, "font_fluent.png"))
if os.path.exists(mdl2):
    probe(mdl2, RANGES, os.path.join(OUT, "font_mdl2.png"))
