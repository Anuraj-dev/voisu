# Research digest — distribution & roadmap decisions (2026-07-18)

Twelve Sonnet 5 scouts (benchmark rows 122–133), adversarially fact-checked (row 133: 6/8
CONFIRMED, 2 PARTLY TRUE, 0 WRONG). This file is the durable synthesis; decisions themselves are
made by Raja in the grilling session and recorded in docs/decisions.md when finalized.

## 1. Toolkit: GTK4 stays, Electron rejected (evidence-settled)
- Chromium/Ozone Wayland has NO layer-shell surface path, no flag; overlay via Electron would need
  XWayland hacks or a native helper (fact-check softened "impossible" to "no native path").
- Every comparable tool (Handy, whisper-overlay, hyprwhspr, wanderlay) uses GTK+layer-shell.
- Electron adds 150–250 MB Chromium + CVE re-shipping treadmill per package.
- If a web-tech dashboard is ever wanted: Tauri, never Electron.

## 2. Compositor support tiers (drives friends' packaging)
- Tier 1: Hyprland (Omarchy) + KDE Plasma — full overlay, EIS input, wlr clipboard.
  CAVEAT: Hyprland's RemoteDesktop/EIS portal path is under-documented; needs a live smoke test
  on Omarchy before promising auto-type there (third-party portal add-on exists because mainline
  was shaky).
- Tier 2: GNOME (stock Ubuntu) — NO layer-shell ever (mutter#973 declined), NO wlr-data-control
  (mutter#524 declined), auto-type works via Mutter's libei path BUT user must manually enable
  Settings → Remote Desktop first. Pragmatic path: daemon+CLI without overlay on GNOME
  (hyprwhspr does the same; notifications as feedback).
- Ubuntu floor: 24.10+ realistic for auto-type parity — primary blocker is Kubuntu 24.04 shipping
  Plasma 5.27 (pre-InputCapture); libei 1.2.1 vs 1.3.0 is secondary. GNOME 45+ carries libei OK.

## 3. Packaging & channels (recommended architecture)
- .deb: cargo-deb; units shipped as assets to /usr/lib/systemd/user/; custom postinst prints
  enable instructions (mirror RPM UX). Debian-archive inclusion would need debhelper+dh-cargo — not now.
- Arch: AUR source PKGBUILD + cargo-aur-generated voisu-bin (friends install -bin, no toolchain);
  auto-push on tag via KSXGitHub/github-actions-deploy-aur. AUR account needs 2FA (Atomic Arch
  takeover campaign 2026-06).
- Fedora: keep RPM; add COPR with webhook auto-rebuild. GOTCHA: COPR builders have no network —
  crates must be vendored into the SRPM (cargo vendor or rust2rpm).
- Ubuntu channel: self-hosted apt repo (GitHub Pages + aptly, or Cloudsmith/packagecloud free
  tier), GPG-signed. Skip Launchpad PPA (offline builders force vendoring pain for marginal gain).
- One on-tag GitHub Actions workflow: build → cargo-deb → apt-repo push → AUR deploy action;
  COPR self-triggers via webhook.
- Flatpak: LATER (architecture is already portal-shaped and sandbox-ready — unusual asset — but
  no manifest mechanism for systemd user units, open flatpak#2787; clipboard must be native
  Wayland calls, never shell-out to wl-copy). AppImage: NEVER.
- Precedent: popular Rust CLIs (ripgrep/zellij/atuin) don't self-run all channels; AUR is the one
  small projects self-maintain.

## 4. STT latency — friend-debate settled
- Both claims right, different metrics. Deepgram's famous speed = streaming/interim + EOT
  detection (200–300 ms class). Dictation cares about time-to-final after speech stops, where
  Deepgram pays an endpointing tax and Groq's short-clip round trip (~200 ms LPU inference +
  upload) genuinely wins. Our numbers (Groq-only 474–1075 ms; reconciled 727–1670 ms) are
  consistent with public data; reconciled = wait-for-slower-of-two, by construction.
- Deepgram-only mode with endpointing=10 + client-sent Finalize on our own VAD ≈ 300–500 ms
  plausible — competitive with, not clearly faster than, Groq tails. Would need live measurement.
- No rigorous independent Deepgram-vs-Groq time-to-final benchmark exists; measuring on our own
  audio (what we did) is the industry-recommended method.

## 5. Dual-provider reconciliation — precedent & cost
- Cost: non-issue. Worst case ~$17.56/mo (120 min/day, 22 days, Deepgram streaming $0.0048/min +
  Groq large-v3 $0.111/hr); realistically $0/mo for friends (Groq free tier: 7200 audio-sec/hr,
  28,800/day, 2000 req/day; Deepgram $200 no-card credit ≈ a year at 1–2 h/day).
- Precedent: NIST ROVER literature supports diverse two-system fusion (~9–12% relative WER gain;
  diversity precondition met by our streaming-contextual + batch-Whisper pair). But NO shipped
  voice product (Vapi/Retell/LiveKit/Pipecat, Wispr/Superwhisper) runs dual-vendor concurrent
  STT — industry standard is single STT + LLM cleanup layer. Voisu is research-grade-unusual here,
  defensible on our own 8/9 jargon-repair evidence.

## 6. Custom vocabulary (feature EXISTS; needs hardening + UX)
- BUG (must-fix): merged_terms() → Deepgram keyterms are UNCAPPED (voisu-daemon.rs:443,
  system.rs:2074–2132). Deepgram hard-caps at 500 tokens across keyterms (docs recommend 20–50
  terms) and returns 400 "Keyterm limit exceeded" — an oversized user dictionary kills the whole
  streaming connection. Cap by priority (user terms first), mirroring whisper_prompt truncation.
- Keyterm ≠ legacy keywords: plain strings, no intensifiers, casing preserved; only lever for a
  bad term is removal.
- UX gaps vs every competitor: `voisu dictionary add/remove/list/import` CLI; hot-reload (re-read
  per session start or SIGHUP — fits rebuild_replay_adapters pattern; today requires daemon
  restart); split boost-tier (small, ASR-time) vs replacements-tier (unlimited, deterministic
  post-transcription find/replace — handles homophones boosting can't); optional spoken\written
  syntax (Dragon-style, e.g. sequel\SQL).
- Whisper prompt: last-224-tokens honored (confirmed); our comma-glossary shape is right; static
  identical list each request is a (low-risk) hallucination-continuation shape — shuffle only if
  WER suite ever shows artifacts.

## 7. Delivery flag (auto-type opt-out)
- Market: every commercial tool defaults auto-insert ON, clipboard as opt-out (Wispr, Superwhisper);
  Aqua Voice is press-to-paste by design; macOS/Windows have no clipboard-only mode at all.
- Recommended: config enum delivery_mode = "type" (default, current behavior) | "clipboard";
  reserve "guarded" (focus-guard: capture focused window at start, compare at delivery, demote to
  clipboard+notify on mismatch) — NO competitor ships a clean focus-guard; genuine differentiator.
  Per-invocation CLI override fits nerd-dictation precedent. Consider clipboard-restore-after-type
  later (Superwhisper).
- Wayland failure modes justifying the opt-out: portal permission re-prompts (persistent grants
  only Plasma 6.5+), libei device teardown on screen blank, apps rejecting synthetic input.

## 8. Product landscape / roadmap signals
- "Wispr Flow for Linux" gap is real and user-validated (forum threads hunting alternatives).
- Top missing features by evidence: (1) AI reformatting/cleanup layer — #1 praised feature
  everywhere, the thing people pay $12–15/mo for; (2) context-aware per-app formatting;
  (3) auto-learned vocabulary from corrections (✨ Wispr pattern); (4) per-app modes;
  (5) local/offline fallback (privacy-conscious Linux segment; 2026 local models are
  accuracy-competitive).
- Cloud-first stays defensible for jargon-accurate technical dictation (our reconciliation
  evidence), but cloud-ONLY is an increasing liability for the Linux audience specifically.

## 9. BYOK onboarding (friends)
- Pure BYOK, no relay. Free tiers genuinely cover daily dictation (see §5).
- Design: `voisu setup` wizard with live per-key preflight validation; storage via Rust keyring
  crate (Secret Service/KWallet) with LOUD 0600-file fallback (never silent — gh's silent
  fallback is actively hated); env-var override wins at runtime; `voisu doctor` does live
  per-provider round-trips; classify provider errors (401/403 invalid key vs 429 rate-limit vs
  quota) instead of raw HTTP.
- CAVEAT to test on Fedora KDE: keyring may be locked when a systemd user service starts early;
  never block daemon startup on it, retry lazily.

## 10. GNOME overlay deep dive (row 134, requested mid-grilling)
- A GNOME-native overlay IS possible: a companion GNOME Shell extension draws St/Clutter widgets
  on the Shell's own compositor stage (how GNOME's volume OSD works) — true always-on-top,
  click-through, positionable. External app drives it via D-Bus (precedents: GSConnect,
  Custom OSD ext 6142, gnome-osd-notifier).
- Cost: second codebase in GJS, extensions.gnome.org manual review on EVERY update, Shell
  internals churn each 6-month release. Mitigation: keep the extension tiny (D-Bus listener +
  one St widget, zero monkey-patching) — small surface breaks rarely.
- Plain GTK4 window fallback: keep-above is a NO-OP on Wayland by design (GNOME stance:
  clients never control stacking); window can be alt-tabbed behind. Best-effort re-present()
  on events; mediocre but zero cost.
- XWayland override-redirect stays on top today but is an unsupported side-effect with open
  focus/stacking bugs — do not build on it.
- Recommended shape: tiny Shell extension = rich tier (detect at runtime, GSConnect-style
  onboarding), plain window = always-available default tier.

---

# DECISIONS (Raja, grilling session 2026-07-18)

1. **Toolkit**: GTK4 locked in; Electron rejected — record as ADR. Tauri is the pre-agreed
   web-tech fallback if a dashboard is ever wanted.
2. **GNOME overlay**: plain-window fallback ships now (best-effort float, re-present on events);
   companion Shell extension (tiny St widget + D-Bus) is a later roadmap polish item.
3. **Delivery flag**: `delivery_mode` enum = type (default) | clipboard | **guarded** — guarded
   (focus-guard) is IN SCOPE now, KDE/Wayland first. CLI toggle like `voisu deepgram`.
4. **STT modes**: keep two modes as-is (reconciled default + Groq-only fast path). No
   Deepgram-only third mode.
5. **Vocabulary**: keyterm cap fix (bug) + `voisu dictionary add/remove/list` + hot-reload per
   session. Replacements tier + spoken\written syntax deferred to a later ticket.
6. **Packaging**: full plan accepted — cargo-deb, AUR source + -bin auto-push, COPR (vendored
   crates), self-hosted GPG apt repo, one on-tag GH Actions workflow. Flatpak later, AppImage
   never. Raja's first packaging rodeo: tickets must include step-by-step HITL guidance for
   one-time account/key setups (GPG, COPR, AUR+2FA).
7. **Onboarding**: `voisu setup` wizard + keyring (loud 0600 fallback) + env override +
   `voisu doctor` error classification. Test keyring-at-service-start on Fedora KDE first.
8. **Sequencing**: (1) fix batch — Deepgram default flip + hardening-05 + keyterm cap;
   (2) feature effort (wayfinder): delivery_mode+guarded, dictionary CLI+hot-reload, setup
   wizard+keyring; (3) packaging effort (wayfinder): pipeline + tiers + GNOME fallback +
   Hyprland EIS smoke test.

Raja invokes wayfinder himself for efforts 2 and 3. ADR for decision 1 to be written when work
starts (not yet enacted).
