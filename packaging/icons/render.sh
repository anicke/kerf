#!/bin/sh
# Renders kerf.svg into the icon files the app and the packages use. Run it after editing the
# SVG and commit the results. Needs resvg (`cargo install resvg`) and Python with Pillow.
set -eu
cd "$(dirname "$0")"
for size in 16 24 32 48 64 128 256 512; do
    resvg -w "$size" -h "$size" kerf.svg "kerf-$size.png"
done
python3 - <<'PY'
from PIL import Image
sizes = [16, 24, 32, 48, 64, 128, 256]
images = {s: Image.open(f"kerf-{s}.png") for s in sizes + [512]}
# Windows: one .ico with every size Explorer and the taskbar ask for.
images[256].save("kerf.ico", sizes=[(s, s) for s in sizes],
                 append_images=[images[s] for s in sizes[:-1]])
# macOS: Pillow builds the .icns from the largest image.
images[512].save("kerf.icns")
PY
