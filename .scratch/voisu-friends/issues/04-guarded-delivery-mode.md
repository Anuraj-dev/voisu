# Implement guarded delivery mode (focus-guard)

**Label:** `wayfinder:task` (AFK implementation + HITL live validation)
**Status:** resolved (2026-07-19, PR #56 merged)
**Blocked by:** 02-delivery-mode-enum, 03-research-focus-tracking
**Blocks:** 09-packaging-accounts-setup (phase gate)

## Question

Implement `delivery_mode = "guarded"`: capture focused-window identity at
Recording start (per ticket 03's recommended mechanism), re-check at delivery;
on match → auto-type as normal, on mismatch/unknown → clipboard-only + desktop
notification ("focus changed — transcript on clipboard"). KDE first; Hyprland
if ticket 03 shows it's cheap; other compositors degrade to plain "type"
behavior with a doctor note. No competitor ships this — differentiator ticket.
RED→GREEN with a fake FocusProbe; live validation on the Fedora KDE host before
merge. Routing: Sol medium (touches delivery supervision + new IPC/adapters =
architectural), first review Sol high is self-review — so review goes to Opus
high per cross-model rule.

## Resolution (2026-07-19)

Implemented by gpt-5.6-sol (medium, cladex), reviewed by Opus 4.8 high (cross-model
exception), 1 fix round. Merged as PR #56 after rebase onto tickets 05/08 (391/0 both
suites post-rebase; driver resolved the one conflict: per-Recording adapter
construction from 05 kept, delivery_mode/probe as actor params from 04 kept).

- FocusProbe seam in voisu-core (None = fail closed, never "unchanged").
- KDE: runtime-materialized KWin script (unlinked after load) pushes activation via
  callDBus to daemon-owned org.voisu.Focus1 (all-string wire — Opus caught callDBus
  marshaling JS Numbers as INT32, which zbus would reject against u32, silently
  defeating the feature); sender-authenticated against KWin's unique owner; 10-min
  staleness + owner check fail closed (long-dwell clipboard fallback is the accepted
  tradeoff — bounds the fail-open window if the push-only script dies silently).
- Hyprland: bounded hyprctl activewindow -j, address identity. Elsewhere: Null probe.
- Strict stable_id comparison (same-app different-window = mismatch); guard trip →
  clipboard + notification, delivery_fallback_reason "focus changed during Recording".
- VOISU_DISABLE_DIRECT_DELIVERY wins; type/clipboard construct no probe;
  `voisu delivery guarded` now confirms normally; doctor reports the focus backend.
- HITL: Raja waived pre-merge live KDE validation ("why waiting, merge it"); live
  guarded-mode check remains a post-merge nice-to-have.
