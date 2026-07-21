#!/usr/bin/env bash
# Regenerate bundle/AppIcon.icns from a square source image.
#
#   ./make-icon.sh [source.png] [output.icns]
#
# Defaults: tabt.png -> bundle/AppIcon.icns
#
# Requires: magick (brew install imagemagick), iconutil + sips (system).
set -euo pipefail

SRC="${1:-tabt.png}"
OUT="${2:-bundle/AppIcon.icns}"

command -v magick >/dev/null || { echo "error: magick not found (brew install imagemagick)" >&2; exit 1; }
[ -f "$SRC" ] || { echo "error: source '$SRC' not found" >&2; exit 1; }

W=$(sips -g pixelWidth  "$SRC" | awk '/pixelWidth/{print $2}')
H=$(sips -g pixelHeight "$SRC" | awk '/pixelHeight/{print $2}')
[ "$W" = "$H" ] || { echo "error: source must be square, got ${W}x${H}" >&2; exit 1; }
[ "$W" -ge 128 ] || { echo "error: source must be at least 128x128, got ${W}x${H}" >&2; exit 1; }
[ "$W" -ge 512 ] || echo "warning: source is ${W}px; the 512 slot is upscaled from it" >&2

# One image per distinct pixel size. No @2x aliases: AppKit picks a
# representation by its pixel dimensions, not by the slot's name, so
# icon_256x256@2x.png would just be a byte-identical copy of the 512.
SIZES=(16 32 128 256 512)

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
ICONSET="$WORK/AppIcon.iconset"
mkdir -p "$ICONSET"

# macOS 26 does NOT round a legacy .icns for you. Hand it an opaque square and
# it shrinks that square onto its own light plate -- the icon then reads as
# floating inside a container instead of being the container. The artwork has
# to arrive already carrying the rounded-rect alpha, bleeding to all four edges.
# 0.2237 is the standard corner-radius ratio. Drawn at 1024 and downsampled per
# size so the corners stay smoothly antialiased even at 16px.
RADIUS=$(awk 'BEGIN { printf "%d", 1024 * 0.2237 }')
magick -size 1024x1024 xc:none -fill white \
  -draw "roundrectangle 0,0 1023,1023 ${RADIUS},${RADIUS}" "$WORK/mask.png"

render() {  # render <pixels> <destination>
  local s="$1" dst="$2" filter
  if [ "$s" -gt "$W" ] && [ $((s % W)) -eq 0 ]; then
    # integer upscale of pixel art: nearest-neighbour adds no blur and no
    # ringing, and downscaling it back by the same factor is a bit-exact
    # round trip. Lanczos here would soften every hard pixel edge.
    filter=Point
  else
    filter=Lanczos
  fi
  # Quantize the RGB *before* attaching alpha: -colors on an RGBA image also
  # quantizes the alpha channel and visibly jags the rounded corners.
  magick "$SRC" -filter "$filter" -resize "${s}x${s}!" -alpha off \
    -colors 256 "$WORK/art.png"
  magick "$WORK/art.png" \
    \( "$WORK/mask.png" -filter Lanczos -resize "${s}x${s}!" -alpha extract \) \
    -compose CopyOpacity -composite \
    -strip -define png:compression-level=9 "$dst"
}

for s in "${SIZES[@]}"; do
  render "$s" "$ICONSET/icon_${s}x${s}.png"
done

# A 1024px master feeds the 512pt @2x slot macOS uses for large Retina
# previews. Only emitted when the source really has the pixels; never upscale.
if [ "$W" -ge 1024 ]; then
  render 1024 "$ICONSET/icon_512x512@2x.png"
fi

mkdir -p "$(dirname "$OUT")"
iconutil -c icns "$ICONSET" -o "$OUT"

# Unpack what we just wrote and prove the icon fills its container: the edge
# midpoints must be opaque (art bleeds to the edge) while the corners must be
# transparent (the rounded-rect mask survived the icns round trip). Get this
# wrong and macOS silently falls back to the plate treatment.
iconutil -c iconset "$OUT" -o "$WORK/back.iconset"
CHECK="$WORK/back.iconset/icon_512x512.png"
probe() { magick "$CHECK" -alpha extract -format "%[fx:floor(p{$1,$2}*255)]" info:; }
MID_TOP=$(probe 256 0); MID_LEFT=$(probe 0 256); CORNER=$(probe 0 0)
[ "$MID_TOP" -ge 250 ] && [ "$MID_LEFT" -ge 250 ] || {
  echo "error: artwork does not reach the canvas edge (top=$MID_TOP left=$MID_LEFT); macOS will inset it on a plate" >&2; exit 1; }
[ "$CORNER" -le 5 ] || { echo "error: corners are opaque (alpha=$CORNER); the rounded-rect mask was lost" >&2; exit 1; }

echo "==> $OUT ($(stat -f%z "$OUT") bytes, sizes: ${SIZES[*]}$([ "$W" -ge 1024 ] && echo " 1024"), edges opaque, corners masked)"
