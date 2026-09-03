#!/bin/bash
# Rasterise assets/lynxrdp.svg into every icon form the packages need.
#
# The results are committed, so a normal build needs none of these tools.
# Run this only after changing the SVG.
#
#   librsvg2-bin  rsvg-convert   (SVG -> PNG)
#   icoutils      icotool        (PNGs -> .ico for Windows)
#   icnsutils     png2icns       (PNGs -> .icns for macOS)
set -euo pipefail
cd "$(dirname "$0")"
SVG=lynxrdp.svg

for tool in rsvg-convert icotool png2icns; do
    command -v "$tool" >/dev/null || { echo "missing $tool; see the header of this script" >&2; exit 1; }
done

# Freedesktop icon theme sizes, laid out as they will be installed.
for size in 16 22 24 32 48 64 128 256 512; do
    dir="icons/hicolor/${size}x${size}/apps"
    mkdir -p "$dir"
    rsvg-convert -w "$size" -h "$size" "$SVG" -o "$dir/lynxrdp.png"
done
mkdir -p icons/hicolor/scalable/apps
cp "$SVG" icons/hicolor/scalable/apps/lynxrdp.svg

# The window icon eframe loads at runtime.
cp icons/hicolor/256x256/apps/lynxrdp.png lynxrdp-256.png

# Windows. 256 is the largest an .ico holds as PNG; Explorer picks per view.
icotool -c -o lynxrdp.ico \
    icons/hicolor/16x16/apps/lynxrdp.png \
    icons/hicolor/24x24/apps/lynxrdp.png \
    icons/hicolor/32x32/apps/lynxrdp.png \
    icons/hicolor/48x48/apps/lynxrdp.png \
    icons/hicolor/64x64/apps/lynxrdp.png \
    icons/hicolor/128x128/apps/lynxrdp.png \
    icons/hicolor/256x256/apps/lynxrdp.png

# macOS. png2icns takes only the sizes an .icns can hold.
png2icns lynxrdp.icns \
    icons/hicolor/16x16/apps/lynxrdp.png \
    icons/hicolor/32x32/apps/lynxrdp.png \
    icons/hicolor/48x48/apps/lynxrdp.png \
    icons/hicolor/128x128/apps/lynxrdp.png \
    icons/hicolor/256x256/apps/lynxrdp.png \
    icons/hicolor/512x512/apps/lynxrdp.png

echo "regenerated:"
find icons -type f | sort
ls -l lynxrdp.ico lynxrdp.icns lynxrdp-256.png
