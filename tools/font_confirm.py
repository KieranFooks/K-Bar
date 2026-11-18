#!/usr/bin/env python3
import os
from PIL import Image, ImageDraw, ImageFont

OUT = os.path.join(os.path.dirname(__file__), "..", "assets", "system-icons")
CANDS = list(range(0xE8B9, 0xE8C7)) + [0xE962, 0xE963, 0xE7F6, 0xE7F7, 0xE8C6, 0xE8C7]

for fname in ["segmdl2.ttf", "SegoeIcons.ttf"]:
    path = os.path.join("C:/Windows/Fonts", fname)
    if not os.path.exists(path):
        continue
    font = ImageFont.truetype(path, 40)
    cols = 8
    rows = (len(CANDS) + cols - 1) // cols
    sheet = Image.new("RGB", (cols * 64 + 8, rows * 72 + 8), (24, 24, 24))
    d = ImageDraw.Draw(sheet)
    for i, cp in enumerate(CANDS):
        x = 8 + (i % cols) * 64
        y = 8 + (i // cols) * 72
        img = Image.new("RGBA", (56, 56), (50, 50, 50, 255))
        dd = ImageDraw.Draw(img)
        dd.text((6, 4), chr(cp), font=font, fill=(255, 255, 255, 255))
        sheet.paste(img.convert("RGB"), (x, y))
        d.text((x + 4, y + 58), f"U+{cp:04X}", fill=(220, 220, 220))
    out = os.path.join(OUT, f"font_confirm_{fname}.png")
    sheet.save(out)
    print(out)
