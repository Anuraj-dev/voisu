# Voisu — State
> Cloud-first Linux desktop dictation app (Fedora KDE Plasma / Wayland) · Last checkpoint: 2026-07-18

## 🚧 In progress / next
- **LATENCY EFFORT COMPLETE. All hardening criticals + 03/04 CLOSED.** Queued next:
  1. **Flip Deepgram default to ON** (Raja, 2026-07-18: keep the jargon accuracy
     as the everyday default; the toggle stays for the fast path). First code
     change of the next session — do NOT do standalone.
  2. **Hardening 05 hygiene sweep** — BoundaryError boxing + the justified
     lint-allow debts the CI gate documented; also bump the 3 s `wait_for_marker`
     bound (flaked once under RPM-build contention).
  3. On the **next RPM build/install**: packaged units now carry the sandbox
     directives; then DELETE the validation drop-ins at
     `~/.config/systemd/user/voisu{,-overlay}.service.d/sandbox-validation.conf`
     (they duplicate the merged unit and keep the live install sandboxed until then).
- Priority 2 unchanged: Overlay visual polish. Future idea: packaging beyond RPM.

## Status
- **Latency effort CLOSED (all 5 tickets).** L-01 toggle (PR #27), L-04 FLAC
  (PR #28), L-05 already-shipped (PR #24). L-02 live eval done on RPM
  `git58a607f`: Groq-only tails 474–1075 ms vs reconciled 727–1670 ms (−25–36%),
  but Groq-only mangled 8/9 jargon probes that reconciliation repaired —
  evidence in `.scratch/voisu-latency/assets/02-groq-only-evidence.md`.
  L-03 DECISION: Deepgram KEPT (default flips to ON next session).
- **Hardening 03 VALIDATED + merged (PR #31):** sandboxed units proven on the
  live install via drop-ins — doctor 6/6 PASS, full reconciled Recording
  delivered, overlay layer-shell with zero degradation, `MemoryDenyWriteExecute`
  fine on the daemon.
- **History-pretty (PR #30):** `voisu history` renders a human-first paged view
  (transcript + tail latency headline, top-20 newest, Enter-to-page on TTY);
  `voisu history --json` keeps the old byte-identical JSON. Terminal-escape
  injection from network strings sanitized at the `truncate_inline` choke point.
- **Hardening 04 (PR #29):** CI now has parallel `clippy -D warnings` (all
  targets) and `cargo audit --deny warnings` gates. Took 8 CI rounds: lib.rs
  `#![allow]`s do NOT reach bin/test crate roots; real fixes included explicit
  `truncate(false)` on the flock lock file.
- **Hardening 03 (PR #31, OPEN/HELD):** sandboxing for both user units
  (`ProtectSystem=strict` + explicit ReadWritePaths, address-family restrictions,
  `MemoryDenyWriteExecute` daemon-only); `systemd-analyze verify` clean.
- Test baseline: **346** (330 + 16 history_view/CLI).
- `docs/model-benchmark.md` rows 103–121: Sol/Opus head-to-head verdict +
  post-latency ride-alongs + routing recommendation.

## Architecture map
- Domain, IPC, Transcript decision, diagnostics -> `crates/voisu-core/src/lib.rs`, `diagnostics.rs`
- Fedora capture/provider/clipboard/portal/libei adapters + ProviderReaper + FLAC encode -> `crates/voisu-app/src/system.rs`
- Recording/replay supervision + DisabledProvider -> `crates/voisu-app/src/bin/voisu-daemon.rs`
- Persisted config (deepgram toggle) -> `crates/voisu-app/src/config.rs`
- History pretty-rendering (pure) -> `crates/voisu-app/src/history_view.rs`
- Dictionary / Whisper prompt builder -> `crates/voisu-app/src/dictionary.rs`
- Daemon + Overlay user-service lifecycle -> `crates/voisu-app/src/service.rs`
- Public CLI (`voisu deepgram`, `voisu history [--json]`) -> `crates/voisu-app/src/bin/voisu.rs`
- GTK Overlay -> `crates/voisu-app/src/bin/voisu-overlay.rs`, `overlay.rs`, `feedback.rs`
- RPM units/spec/build/smoke (now sandboxed units on PR #31) -> `packaging/`
- CI gates (test/clippy/audit) -> `.github/workflows/ci.yml`
- Latency effort -> `docs/specs/2026-07-17-latency-optimization.md`, `.scratch/voisu-latency/`
- Accuracy effort -> `docs/specs/2026-07-17-transcription-accuracy.md`, `.scratch/voisu-accuracy/`
- Hardening map + audit -> `.scratch/voisu-hardening/` (01, 02, 04 CLOSED; 03 drafted/held)
- Fedora procedure/evidence -> `docs/packaging-fedora.md`, `docs/release-evidence.md`

## Stack & run
- Stack: Rust 2024 + Tokio + serde + zbus 5 + GTK4 (opt-in) + runtime libei + flacenc · Run: `cargo run -p voisu-app --bin voisu-daemon` · Test: `cargo test --workspace`
- Robust test count: `cargo test --workspace 2>&1 | grep -oE "[0-9]+ passed; [0-9]+ failed" | awk '{p+=$1; f+=$3} END {print p, f}'`
- Test baseline: **346**.

## Key decisions (top 3–5)
- Deepgram = default-OFF runtime toggle (delete-or-keep finalized by ticket 03
  after live eval); disabled Provider is an adapter stand-in, not coordinator
  surgery — supervision/reaper/barrier untouched.
- FLAC (lossless) upload, no duration gate; Opus codec rejected for WER risk.
- CI clippy gate ships with justified `#![allow]`s (result_large_err,
  large_enum_variant, too_many_arguments) — shrinking those types is the
  hardening-05 sweep, not the gate's job.
- Network-sourced strings are sanitized (all C0/C1 controls) at ONE choke point
  before terminal rendering; `--json` stays byte-identical for scripts.
- Test assertions pin only deterministic pre-stop capture — post-signal bytes
  are best-effort by design.

## Gotchas
- **Disk critically tight (~11–14 GB).** `cargo clean` before RPM builds;
  `TMPDIR=/var/tmp RUST_TEST_THREADS=4`; build script refuses a dirty tree.
- **No local clippy/rustfmt on this machine (no rustup)** — the CI lint gate is
  the only clippy run; expect CI-iteration for new lint errors, and remember
  clippy stops at the first failing target (errors surface serially).
- **Pin cwd (`git -C …`) in every compound git command when a worktree is
  active** — a cwd slip pushed a (benign) lint-allow commit directly to main
  this session.
- cladex gotchas: prompt via **stdin** when combining `-p` with other flags;
  headless needs `--permission-mode acceptEdits` + explicit `--allowedTools`;
  costs in JSON are nominal (real billing = Codex Plus quota).
- **Subagent doc fence:** every dispatch prompt needs an explicit "do not touch
  docs/STATE/checkpoint/benchmark" line (held perfectly once adopted).
- The accuracy WER suite assumes Deepgram ON — run it with `voisu deepgram on`.
- `packaging/build-rpm.sh` requires a clean COMMITTED checkout; embeds commit.
- Whisper `prompt` ~224 tokens; Groq free tier: 7,200 audio-sec/hr, 2,000 req/day.
- Use `CONTEXT.md` terms exactly; default builds GTK-free (`--features overlay`).
- Leftover diagnostics (optional cleanup): `/var/tmp/pwtest.raw`,
  `/var/tmp/pwpipe.err`, fixture `pwtest.raw` under
  `/run/user/1000/voisu/v1/diagnostics/fixtures/`.
