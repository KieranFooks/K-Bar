#!/usr/bin/env python3
"""Build contact sheets (PIL) from extracted 32px group icons, one per DLL."""
import os, re, sys
from PIL import Image, ImageDraw

BASE = os.path.join(os.path.dirname(__file__), "..", "assets", "system-icons")
COLS = 14
CELL = 44
LABEL = 12

def sheet(dll_dir, out_png):
    files = [f for f in os.listdir(dll_dir) if f.endswith("_32px.png")]
    def key(f):
        m = re.match(r"id(\d+)_ord(\d+)_32px\.png", f)
        return (int(m.group(2)), int(m.group(1))) if m else (9999, 0)
    files.sort(key=key)
    rows = (len(files) + COLS - 1) // COLS
    W = COLS * CELL + 8
    H = rows * (CELL + LABEL) + 24
    img = Image.new("RGB", (W, H), (32, 32, 32))
    d = ImageDraw.Draw(img)
    d.text((8, 4), os.path.basename(dll_dir) + f"  ({len(files)} icons)", fill=(255, 255, 255))
    for i, f in enumerate(files):
        m = re.match(r"id(\d+)_ord(\d+)", f)
        label = f"o{m.group(2)}/i{m.group(1)}" if m else f
        x = 8 + (i % COLS) * CELL
        y = 24 + (i // COLS) * (CELL + LABEL)
        try:
            ic = Image.open(os.path.join(dll_dir, f)).convert("RGBA")
            bg = Image.new("RGBA", (32, 32), (60, 60, 60, 255))
            bg.alpha_composite(ic.resize((32, 32), Image.LANCZOS))
            img.paste(bg.convert("RGB"), (x, y))
        except Exception as e:
            d.rectangle([x, y, x + 32, y + 32], outline=(255, 0, 0))
        d.text((x, y + 33), label, fill=(200, 200, 200))
    img.save(out_png)
    print(out_png, img.size, len(files))

for dll in ["powercpl.dll", "DDORes.dll", "imageres.dll", "shell32.dll", "moricons.dll", "pifmgr.dll"]:
    d = os.path.join(BASE, dll)
    if os.path.isdir(d):
        sheet(d, os.path.join(BASE, dll + "_sheet.png"))
