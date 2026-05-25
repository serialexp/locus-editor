#!/usr/bin/env bash
# Regenerate every derived icon asset from the source SVGs.
#
# Inputs:
#   assets/icon.svg        — primary mark, used for sizes >= 64 px
#   assets/icon-small.svg  — simplified silhouette, used for sizes <= 48 px
#
# Outputs (under assets/generated/):
#   png/icon-{16,32,48,64,128,256,512}.png   — individual sizes
#   png/locus-editor.png                    — copy of icon-256.png, named for
#                                              Linux hicolor / .desktop usage
#   icon.ico        — Windows multi-resolution
#   icon.icns       — macOS multi-resolution
#   icon-256.rgba   — raw RGBA8 pixels (256x256), embedded by the runtime
#
# Dependencies (any rasterizer + ImageMagick):
#   - rsvg-convert  (preferred — cleanest SVG output)
#   - magick / convert  (ImageMagick; falls back to its SVG renderer)
#   ImageMagick is required regardless for ICO/ICNS packing.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
SRC_MAIN="$ROOT/assets/icon.svg"
SRC_SMALL="$ROOT/assets/icon-small.svg"
OUT="$ROOT/assets/generated"
PNG_OUT="$OUT/png"

if [[ ! -f "$SRC_MAIN" || ! -f "$SRC_SMALL" ]]; then
    echo "error: missing source SVGs ($SRC_MAIN, $SRC_SMALL)" >&2
    exit 1
fi

# Pick a rasterizer.
if command -v rsvg-convert >/dev/null 2>&1; then
    RASTERIZER=rsvg
elif command -v magick >/dev/null 2>&1; then
    RASTERIZER=magick
elif command -v convert >/dev/null 2>&1; then
    RASTERIZER=convert
else
    echo "error: need rsvg-convert, magick, or convert (ImageMagick) on PATH" >&2
    exit 1
fi

# We always need ImageMagick for ICO / ICNS / RGBA packing.
if command -v magick >/dev/null 2>&1; then
    IM=magick
elif command -v convert >/dev/null 2>&1; then
    IM=convert
else
    echo "error: ImageMagick (magick or convert) is required for ICO/ICNS output" >&2
    exit 1
fi

mkdir -p "$PNG_OUT"

rasterize() {
    local src="$1" size="$2" dst="$3"
    case "$RASTERIZER" in
        rsvg)
            rsvg-convert -w "$size" -h "$size" "$src" -o "$dst"
            ;;
        magick|convert)
            # density × viewBox-units / 72 ≈ output pixels; 768 dpi gives a
            # crisp render at 256 px and downsamples nicely for smaller sizes.
            "$RASTERIZER" -background none -density 768 "$src" -resize "${size}x${size}" "$dst"
            ;;
    esac
}

echo "==> rasterizing PNGs (rasterizer: $RASTERIZER)"
for size in 16 32 48; do
    rasterize "$SRC_SMALL" "$size" "$PNG_OUT/icon-${size}.png"
done
for size in 64 128 256 512; do
    rasterize "$SRC_MAIN" "$size" "$PNG_OUT/icon-${size}.png"
done
cp "$PNG_OUT/icon-256.png" "$PNG_OUT/locus-editor.png"

echo "==> packing icon.ico (Windows)"
# Windows .ico: include the standard set. Explorer picks the right one per DPI.
"$IM" \
    "$PNG_OUT/icon-16.png" \
    "$PNG_OUT/icon-32.png" \
    "$PNG_OUT/icon-48.png" \
    "$PNG_OUT/icon-64.png" \
    "$PNG_OUT/icon-128.png" \
    "$PNG_OUT/icon-256.png" \
    "$OUT/icon.ico"

echo "==> packing icon.icns (macOS)"
# ImageMagick's .icns writer is broken (writes a single PNG with the wrong
# extension), so we use a small Python helper that emits a proper multi-
# resolution ICNS file from the per-size PNGs.
"$SCRIPT_DIR/make-icns.py" "$OUT/icon.icns" "$PNG_OUT"

echo "==> writing icon-256.rgba (runtime embed)"
# Raw RGBA8 (premultiplied off, sRGB) so the runtime can pass it straight to
# winit::Icon::from_rgba without a PNG decoder dependency.
"$IM" "$PNG_OUT/icon-256.png" -depth 8 RGBA:"$OUT/icon-256.rgba"

echo "done — outputs in $OUT"
