# Voisu Overlay design

This file is the approval gate for Ticket 11. The GTK implementation must not
introduce visual tokens outside this list.

## Surface

- Bottom-centred GTK4 Layer Shell surface, 280 px wide by 64 px high.
- No input region: the surface is click-through, keyboard-unfocusable, and
  never activates or changes the focused application.
- Hidden at `Idle`; no timer, pulse, redraw, or animation is scheduled while
  hidden.
- The daemon remains the only owner of Recording and Delivery. The Overlay
  only reads versioned IPC status and may disappear at any time.

## Tokens

| Token | Value | Use |
|---|---|---|
| Surface | `#17191D` at 96% | capsule background |
| Recording | `#65D6A0` | active Recording accent |
| Processing | `#8FB4FF` | processing accent |
| Success | `#B8E986` | completed Delivery |
| Failure | `#FF8A8A` | Quality Failure or unavailable daemon |
| Primary text | `#F4F5F7` | state label |
| Secondary text | `#B5BAC5` | supporting label |
| Radius | 32 px | capsule corners |
| Outer margin | 24 px | bottom and screen edge spacing |
| Text size | system default, 11 pt | no custom font dependency |

The minimum text contrast target is 4.5:1 for normal text and 3:1 for large
state text. The accent is never the only state signal: every state has a text
label and an accessible description.

## State treatment

- Recording: `Recording` label plus a static three-bar voice activity meter.
  The bars are updated only from observed status and are not a decorative
  continuous animation.
- Processing: `Processing` label and a static progress glyph; no spinner is
  required.
- Success: `Delivered` label, shown briefly, then hidden.
- Failure: `Quality Failure` label (or `Daemon unavailable`) with a static
  warning glyph, shown briefly, then hidden.
- Idle: fully hidden.

## Accessibility and motion review

The GTK window must set `can_focus=false`, `focusable=false`, and an empty input
region. GTK `Settings:gtk-enable-animations` and the desktop reduced-motion
preference are treated as a reduced-motion request: all transitions become
immediate and the activity meter stays static. High-contrast themes retain the
same labels, spacing, and contrast targets; no state depends on hue alone.

## Screenshot gate

Before Ticket 11 is closed, capture the Overlay on Fedora KDE Plasma / Wayland
for Recording, Processing, Success, Failure, and Idle. Critique each capture
against the tokens above, correct any mismatch, and recapture. This sandbox
cannot open a real desktop surface, so the Fedora screenshot gate is **PENDING
for the orchestrator/host**; no screenshots are claimed here.
