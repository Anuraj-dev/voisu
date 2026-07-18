# Map — Voisu post-audit hardening

**Label:** `wayfinder:map`

## Destination

Every finding from the 2026-07-18 codebase audit
([full report](assets/audit-2026-07-18.md)) triaged — fix now / fix on trigger /
accepted — and landed as ordered, sized tickets worked one at a time. The map is
done when all five tickets are closed and the trigger rules for the deferred
refactors are recorded where future efforts will see them.

## Notes

- **This effort carries execution into the map** (Raja's override, same as the
  accuracy and latency maps): tickets are implementation tickets. Routing per
  repo rules — criticals touch daemon concurrency, so **GPT-5.6 Sol medium** for
  01/02; **Luna/Terra** for 03/04/05; **Sonnet 5** bulk reading; review **Sol**
  (first high, re-reviews medium).
- **Sequencing (grilling, 2026-07-18):**
  1. Criticals (tickets 01, 02) land **immediately after
     `feature/transcription-accuracy` integrates to main and BEFORE any latency
     ticket** — they touch the same files as latency tickets 01 & 04; small
     diffs land first. STATE.md carries this priority line.
  2. Tickets 03, 04 are **parallel-safe anytime** (packaging/ + CI only, zero
     overlap with latency files).
  3. Ticket 05 (hygiene sweep) waits **behind the latency effort**.
- **Trigger rules for deferred refactors** (decided: no queue slots — see fog).
- CI-only constraint: any future e2e harness tests must run **in CI only, never
  in the default local test run** (Raja: local runs eat his machine).

## Decisions so far

<!-- one line per closed ticket -->

## Not yet specified

- **Split `system.rs` into submodules** (capture / providers / delivery /
  shortcuts / ei). TRIGGER: graduate to a ticket right before overlay-milestone
  or any second-desktop-target work begins. Audit: major, `system.rs` 5,659
  lines, no internal `mod`.
- **Collectionize the provider fan-out** so a third STT provider is a trait
  implementation, not a core-crate edit. TRIGGER: graduate right before any
  third provider is added. Audit: major, hardcoded `Provider` pair across
  ~10 sites.
- **Nested-compositor e2e harness** (real daemon + real portal + PipeWire null
  source + existing fake provider servers; assert clipboard via `wl-paste` and
  the overlay layer-shell surface). TRIGGER: build as part of / immediately
  after the overlay visual milestone. MUST be CI-only — gated so it never runs
  in default local `cargo test`.

## Out of scope

<!-- work ruled beyond this destination -->
