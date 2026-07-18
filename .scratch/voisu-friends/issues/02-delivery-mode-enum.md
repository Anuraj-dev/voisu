# Implement delivery_mode enum (type | clipboard) + CLI toggle

**Label:** `wayfinder:task` (AFK, implementation)
**Status:** resolved (2026-07-18, PR #55 merged)
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

## Resolution (2026-07-18)

Implemented by gpt-5.6-terra (high), reviewed by Sol (high, 1 round: single-quoted
TOML literals + legacy header migration + usage text; full TOML-lexer scope declined
as over-engineering for a CLI-managed file — rationale on PR #55). Merged as PR #55.

- `DeliveryMode { Type (default), Clipboard, Guarded }` persisted as quoted string
  root key `delivery_mode`; unknown/missing degrades to Type; single-quoted values
  accepted.
- Both-key-preserving writer: `set_deepgram_enabled`/`set_delivery_mode` share
  `write_config`, never discard the other managed root key; legacy managed headers
  stripped on migration.
- `voisu delivery [type|clipboard|guarded]` — no arg prints mode; guarded persists
  with a not-yet-available notice (exit 0) so it activates when ticket 04 lands.
- Daemon resolves mode once at startup; `build_delivery_adapter` used at startup and
  the supervised rebuild path; Guarded degrades to clipboard-only pending ticket 04;
  `VOISU_DISABLE_DIRECT_DELIVERY` wins over the persisted mode.
- Suite: 359 passed, 0 failed; all three CI gates green.
