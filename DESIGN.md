# Voisu Overlay design

The current contract for the Linux GTK Overlay. The shipped values live in
`crates/voisu-app/src/overlay.rs` and
`crates/voisu-app/src/bin/voisu-overlay.rs`; change this file with them. The
Windows and macOS pill (`apps/windows/ui/overlay/overlay.css`) follows the same
colours.

## Surface

- GTK4 Layer Shell surface with a 320×64 px placement box around a 120×40 px
  pill. A pill carrying text widens to 320 px at most, on up to two lines.
- The default position is `(0.5, 0.964)`, as fractions of the screen across and
  down (bottom centre). The user can move it (`voisu overlay-position`).
- The status pill is click-through and keyboard-unfocusable, and never changes
  the focused application. Move mode is the exception: it takes pointer input
  and, on demand, the keyboard.
- Hidden at `Idle`. Animation and level sampling stop; daemon status polling
  continues every 200 ms.
- The daemon remains the only owner of Recording and Delivery. The Overlay reads
  daemon status, accepts move-mode requests, and may disappear at any time.

## Tokens

| Token | Value | Use |
|---|---|---|
| Surface | `#17191D` at 96% | capsule background |
| Primary | `#F4F5F7` | Recording bars, text |
| Processing | `#8FB4FF` | Processing, loading |
| Success | `#65D6A0` | completed Delivery |
| Failure | `#FF8A8A` | failure text and border; final-seconds limit border |
| Caution | `#FFB454` | no speech, unavailable, clipboard-only Delivery, approaching limit |
| Radius | 20 px | capsule corners |
| Text size | 10 pt, weight 600 | system font; the success glyph is 13 pt |

The text contrast target is 4.5:1 for normal text and 3:1 for large state text.
Colour is never the only state signal: every state differs in shape or text and
carries an accessible description.

## State treatment

Recording, Processing, Success and no speech are graphic-first, with no visible
text apart from the limit warning below.

- Recording: 26 live level bars, 2 px wide with 2 px gaps. In the last minute
  before the Recording Deadline the pill turns into a text warning. In the final
  ten seconds the text goes away: amber bars with a red border.
- Processing: an animated light sweep over the bars.
- Success: check mark when the Transcript is pasted; hidden after 600 ms.
- No speech: amber dotted bar floor, shown briefly.
- Copied: `Copied · Ctrl+V` in amber when Delivery fell back to the clipboard.
  Hidden after 2 s.
- Failure: a short text such as `Recording failed`,
  `Voisu service isn't running` or `Held back text that looked wrong`.
- Idle: fully hidden.

## Accessibility and motion

When GTK animations are disabled (`gtk-enable-animations`, which carries the
desktop reduced-motion preference), the Processing sweep stays still. The live
Recording meter keeps moving, since it shows that audio is arriving. No state
depends on hue alone.

## Screenshot record

**2026-07-16, Fedora KDE Plasma / Wayland, first release design** (since
replaced by the pill above). Round 1 found one mismatch: the GTK window
background rendered as a square-cornered rectangle behind the capsule. A
transparent window background fixed it, and Recording, Processing, Success,
Failure, Daemon unavailable and Idle-hidden were recaptured and passed. Captures
are retained in the maintainer's release archive.
