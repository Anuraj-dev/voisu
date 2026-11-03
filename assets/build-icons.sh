#!/bin/sh
# Regenerates every raster derived from assets/logo.svg:
#   favicon-16.png, favicon-32.png, apple-touch-icon.png (180),
#   icon-512.png and og.png (1200x630 share card, from og.svg).
# Run from anywhere:  sh assets/build-icons.sh
# Needs rsvg-convert (librsvg2-tools / librsvg).
set -eu
cd "$(dirname "$0")"

# The mark is drawn clipped to a rounded square (radius 18% of the side), the
# same shape the pages give it, so a logo with filled corners still reads as an
# app icon. The clip lives in a temporary wrapper; logo.svg is never edited.
wrap=$(mktemp ./.icon-wrap.XXXXXX.svg)
trap 'rm -f "$wrap"' EXIT
cat > "$wrap" <<'SVG'
<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 1000 1000" width="1000" height="1000">
  <defs><clipPath id="r"><rect x="1" y="1" width="998" height="998" rx="180"/></clipPath></defs>
  <image href="logo.svg" width="1000" height="1000" clip-path="url(#r)" preserveAspectRatio="xMidYMid meet"/>
</svg>
SVG

for size in 16 32; do
  rsvg-convert -w "$size" -h "$size" "$wrap" -o "favicon-$size.png"
done
rsvg-convert -w 180 -h 180 "$wrap" -o apple-touch-icon.png
rsvg-convert -w 512 -h 512 "$wrap" -o icon-512.png
rsvg-convert -w 1200 -h 630 og.svg -o og.png
echo "icons rebuilt from $(pwd)/logo.svg"
