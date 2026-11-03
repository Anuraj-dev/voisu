# Site assets

Every logo on the site comes from **`logo.svg`**. The pages reference it
directly (header, footer, docs). The favicons, touch icon and share card
are PNGs rendered from it.

## Swap the logo

1. Replace `logo.svg` with the new mark. A square viewBox works best; the
   site shows it clipped to a rounded square (radius 18% of the side).
2. Rebuild the PNGs:

   ```sh
   sh assets/build-icons.sh
   ```

   This needs `rsvg-convert` (`librsvg2-tools` on Fedora, `librsvg` on
   Arch). It rewrites `favicon-16.png`, `favicon-32.png`,
   `apple-touch-icon.png` (180 px), `icon-512.png` and `og.png` (the
   1200×630 share card, drawn from `og.svg`, which embeds `logo.svg`).
3. Commit `logo.svg` and the regenerated PNGs together.

`logo-wordmark.svg` is the mark with the "voisu" wordmark under it, for
places where a stacked lockup helps (it is not used in the page header, which
pairs `logo.svg` with HTML text). If the mark changes, update it by hand to
match.

## Other files

- `site.css`: the one stylesheet for every page.
- `site.js`: install tabs, copy buttons, docs menu. Pages work without it.
- `hero.js`: the landing-page capsule animation.
