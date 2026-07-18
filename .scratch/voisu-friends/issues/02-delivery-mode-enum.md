# Implement delivery_mode enum (type | clipboard) + CLI toggle

**Label:** `wayfinder:task` (AFK, implementation)
**Status:** open
**Blocked by:** EXTERNAL — fix batch merging first (touches config.rs, same as
the Deepgram default flip)
**Blocks:** 04-guarded-delivery-mode, 08-gnome-plain-window-fallback

## Question

Add persisted `delivery_mode` to `crates/voisu-app/src/config.rs` as a string
enum: `"type"` (default — current behavior: clipboard + auto-type) |
`"clipboard"` (clipboard only, no emulated input). Reserve `"guarded"` in the
enum (parse + persist, but reject with "not yet available" until ticket 04).
CLI: `voisu delivery type|clipboard` following the `voisu deepgram on|off`
pattern in `voisu.rs`; daemon honors the mode in the delivery path
(voisu-daemon.rs / system.rs delivery adapters). RED→GREEN; tests for enum
round-trip, CLI, and clipboard-only skipping the libei path. Routing: Terra
high (regular feature), Sol review.
