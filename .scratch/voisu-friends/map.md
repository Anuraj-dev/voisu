# Map — Voisu to friends (three distros)

**Label:** `wayfinder:map`
**GitHub mirror:** [map #32](https://github.com/Anuraj-dev/voisu/issues/32) —
tickets #33–#47 map to `issues/01…15` in order (blocking graph in the map
issue's index comment). Local files are canonical; close/comment both when
resolving.

## Destination

Voisu installed and reliably dictating on the three friends' desktops — Omarchy
(Arch + Hyprland), KDE Plasma, and Ubuntu GNOME — delivered through real update
channels (AUR, COPR, self-hosted apt repo) from one on-tag release workflow,
with the friend-facing features shipped first: `delivery_mode`
(type/clipboard/guarded), `voisu dictionary` CLI + hot-reload, and the
`voisu setup` wizard with keyring storage.

## Notes

- **Execution is carried into the map** (Raja's standing override, same as the
  accuracy/latency/hardening maps): tickets run decision→implementation→review→
  merge. Routing per repo CLAUDE.md pinned table; cross-model review
  (implementer's model never reviews itself; first review Sol high, re-reviews
  Sol medium; Sol via cladex). Every dispatch prompt carries the doc fence
  (no docs/STATE, sessions, model-benchmark). Log dispatches as benchmark rows
  continuing from row 134.
- **Evidence base:** all decisions here were researched by a 13-scout Sonnet
  fleet, adversarially fact-checked — digest at
  [.scratch/voisu-research/2026-07-18-distribution-decisions.md](../voisu-research/2026-07-18-distribution-decisions.md).
  Zoom there before re-litigating anything.
- **Sequencing (Raja, 2026-07-18):** fix batch (Deepgram default flip,
  hardening-05, keyterm cap fix) runs OUTSIDE this map, first. Then phase A
  (features), then phase B (packaging) — packaging tickets are blocked behind
  the phase-A implementation tickets.
- **CI vs live testing split (Raja's question, answered):** container
  install-smoke in CI (fedora/ubuntu/arch containers: install package, run
  binaries, systemd-analyze verify, lintian/namcap) catches packaging bugs;
  anything needing a live Wayland session (mic, portals, overlay, auto-type)
  needs a VM or real desktop — friends test last, on packages that already
  passed the container gate.
- **Escalation rule (Raja, 2026-07-18):** if an implementer fails 2 review
  rounds on the same ticket, discard that agent and either (a) the Fable 5
  driver takes the ticket inline, or (b) respawn at higher model/effort —
  consistent with the standing three-strike memory, tightened to two for this
  effort. Workhorses: Opus 4.8 high and Terra high (Luna medium for
  packaging/config); reviews: always Sol (first high, re-reviews medium, via
  cladex) — except where Sol implemented (ticket 04), where Opus high reviews.
- Branch per ticket, PR to main, merge only on CI green (all three gates), no
  AI credits in commits/PRs.

## Decisions so far

<!-- one line per closed ticket -->
- **01 (2026-07-18):** ADR 0007 records GTK4+layer-shell locked, Electron rejected, Tauri sole web-tech fallback (PR #53).
- **02 (2026-07-18):** `delivery_mode` (type|clipboard|guarded, default type) persisted as a second root config key with a both-key-preserving writer; `voisu delivery` CLI (guarded persists with not-yet-available notice); daemon builds clipboard-only adapter for clipboard/guarded, env override wins (PR #55).
- **04 (2026-07-19):** Guarded delivery live: FocusProbe seam (None fails closed), KWin runtime script → sender-authenticated all-string D-Bus push (10-min staleness + owner check bound the fail-open window), Hyprland hyprctl probe, strict stable_id guard in the delivery path → clipboard + notification on mismatch; env override wins. Sub-decisions: callDBus all-string wire (INT32 marshaling trap), script unlinked after load, same-app-different-window = mismatch. Live KDE check post-merge (PR #56).
- **08 (2026-07-19):** GNOME plain-window fallback: pure poll_tick seam (resurface once per rendered visible transition via present(); Recording notification from OBSERVED daemon states — unreachable blips can't refire), surface-handoff guard regression-tested; clipboard verified (overlay none, daemon wl-copy fine on GNOME, Flatpak-proofing → phase B); live GNOME visual check outstanding, non-gating (PR #59).
- **05 (2026-07-19):** Dictionary CLI (add/remove/list, flock-serialized atomic edits, comment-grammar validation) + per-Recording hot-reload: one snapshot per Recording feeds Deepgram keyterms and the Groq whisper prompt; supervised-tail no-fs-I/O invariant preserved; Unicode casefold + Deepgram reconnect declined (PR #57).
- **03 (2026-07-18):** Focus tracking: KDE via KWin scripting (internalId identity; script+D-Bus, no portal path), Hyprland via bounded `hyprctl activewindow -j` (address identity); FocusProbe trait seam with runtime-detected Kwin/Hyprland/Null adapters, Null fails closed. Asset: assets/03-focus-tracking-research.md. Sub-decisions left to 04: KWin script→daemon channel, script packaging, same-app-different-window policy.

## Not yet specified

- **Friend-facing onboarding docs** — README quickstart per distro (install
  command, `voisu setup`, free-tier key signup walkthrough). Sharpens once the
  channels exist and the wizard's final UX is known.
- **Release/versioning scheme** — tag format, changelog discipline, when to cut
  the first tagged release. Sharpens once the release workflow ticket lands.
- **Supported-version claims for Ubuntu** — exact minimum (24.04 degraded vs
  24.10 full) needs empirical confirmation during packaging validation before
  it's written into docs.
- **GNOME auto-type first-run UX** — GNOME needs the manual Settings → Remote
  Desktop enable; how Voisu detects and surfaces that (doctor check? wizard
  step?) sharpens once the GNOME fallback ticket resolves.

## Out of scope

- **GNOME Shell extension overlay** — decided "later polish"; plain-window
  fallback suffices for this destination. Fresh effort if/when earned.
- **Replacements tier + spoken\written dictionary syntax** — deferred by the
  vocab-scope decision; separate feature effort later.
- **Deepgram-only third STT mode** — rejected; two modes stay
  (reconciled default + Groq-only fast path).
- **Flatpak / AppImage** — Flatpak later (blocked on flatpak#2787 systemd-user
  gap), AppImage never.
- **AI cleanup layer, context awareness, auto-learned vocabulary, local STT
  fallback** — top roadmap signals from the landscape research, but beyond this
  map's destination; future efforts.
