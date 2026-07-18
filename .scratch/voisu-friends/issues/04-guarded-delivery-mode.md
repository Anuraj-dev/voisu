# Implement guarded delivery mode (focus-guard)

**Label:** `wayfinder:task` (AFK implementation + HITL live validation)
**Status:** open
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
