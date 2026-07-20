# Spec — Overlay audio waveform (live bar meter)

> Status: **approved design, not implemented**. Date: 2026-07-20.
> Supersedes the glyph meter (`▂▆█`) currently rendered in the Overlay capsule.

## 1. Goal

While a Recording is active, the Overlay shows a live bar meter that reacts to the user's
voice in near-real-time — the "HyprVox / assistant" look, not an oscilloscope trace.

Scope is deliberately narrow: **only the Recording phase changes.** Processing, Success and
Failure keep their current text-only presentation, unchanged.

## 2. Product decisions (settled with Raja)

| Decision | Choice | Rationale |
|---|---|---|
| Visual form | **Bar meter** (frequency bands, no time axis) | Reads as "listening"; a scrolling waveform reads as "recording audio" and is too dense for a glanceable pill |
| Behaviour at rest | **Flat line**, motionless | Motion always means audio. Makes the bars a pure function of the mic |
| "Still alive" cue | Gently pulsing record dot on the pill | Keeps honesty in the bars, aliveness in the chrome |
| Processing phase | **No bars.** Current text presentation, untouched | The bar meter lives and dies with the Recording phase |
| Click to stop | **No.** Overlay stays click-through | Existing empty input region (`voisu-overlay.rs:227`) is preserved |
| Transport | Fast **poll** (request/response), not push | Keeps the IPC stateless; see §4 |
| Protocol version | **Stays at 1** | See §8 |
| FFT dependency | **None** — hand-rolled biquad bank | See §6 |

## 3. Research findings that shaped this design

These were verified against the code on 2026-07-20. **The implementing agent must
re-verify them before writing code** (see §11) — they are load-bearing.

### 3.1 Audio arrives every 100 ms, not continuously — THE critical constraint

`system.rs:54` — `const PCM_CHUNK_BYTES: usize = 3_200`. At s16le mono 16 kHz this is
**exactly 100 ms of audio per `AudioChunk`**.

A naive 20 ms poll against this yields a 10 fps meter with 100 ms latency. Any design that
does not address this does not deliver a live waveform.

**Do NOT simply shrink `PCM_CHUNK_BYTES`.** Two traps:
- `MIN_RECORDING_BYTES = PCM_CHUNK_BYTES` (`system.rs:55`) — shrinking it silently
  redefines the silent-Recording validation threshold.
- Provider streaming would fan out 5× more websocket sends per Recording.

### 3.2 Chunks are not fixed size

`stdout.read()` can return short reads (`system.rs:1718-1742`). There is no reassembly
logic forcing exact boundaries. Level windowing must not assume a fixed sample count.

### 3.3 The lifecycle actor is a serial bottleneck

Every `Command` funnels through one `mpsc::channel(64)` into a single `actor_loop`
(`voisu-daemon.rs:135, 424`). A 50 Hz command would queue behind lifecycle transitions and
disk-backed commands (`History`, `Export`).

Precedent for bypassing it: `chunk_counter: Arc<AtomicU32>` and `first_chunk_ms:
Arc<AtomicU64>` (`voisu-daemon.rs:374-375`) already expose capture-pump data to the IPC
layer without the actor.

### 3.4 An unknown Command variant produces silence, not an error

`Command` has no `#[serde(other)]` fallback. On an unknown variant `serve()` returns `Err`,
which the caller **discards** (`let _ = serve(...)`, `voisu-daemon.rs:176`). The socket
closes with no response. The client cannot distinguish this from a dead daemon.

### 3.5 The Overlay polls with blocking I/O on the GTK main thread

`read_status()` (`voisu-overlay.rs:433-446`) does a synchronous `UnixStream` round trip with
a **150 ms** read timeout, called from inside `glib::timeout_add_local`
(`voisu-overlay.rs:304`). There is no worker thread and no async dispatch. At 20 ms cadence
a slow daemon would stall the UI.

### 3.6 There is already a meter widget

`voisu-overlay.rs:239-256` builds `capsule` (a `gtk::Box`) containing `label` and a `meter`
`gtk::Label`. `render_surface` (`voisu-overlay.rs:414-419`) sets its text to `▂▆█` / `▂▅▆` /
`▂▃▂` from `view.activity`. We **replace** this widget, not add alongside it.

`activity: u8` (`overlay.rs:39-45`, derived from `streamed_chunk_count`) is consumed in
exactly that one `match` and asserted by no test. Removal is safe.

### 3.7 The capsule is not theme-aware

CSS is an inline string literal (`voisu-overlay.rs:259-274`) with a hardcoded dark capsule
`rgba(23,25,29,0.96)` and per-phase accent colours. It ignores system light/dark entirely.

### 3.8 The Overlay backend does not affect widget content

`window.set_child(Some(&capsule))` (`voisu-overlay.rs:256`) runs on both the LayerShell and
RegularSurface paths. Backend affects **placement and stacking only**. GNOME/Mutter (no
`zwlr_layer_shell_v1`) therefore gets a pixel-identical waveform with no second code path.

### 3.9 Pure logic is testable without the `overlay` feature

Only the `voisu-overlay` **binary** is gated by `required-features = ["overlay"]`. The
`voisu-app` library — including `overlay.rs` and `feedback.rs` — compiles unconditionally
and contains zero `#[cfg(feature)]` attributes. Pure level/smoothing logic can live in the
library and be tested with a plain `cargo test -p voisu-app`.

## 4. Architecture

```
pw-record ──~20ms reads──> reader thread ──> level frame ring (Arc)  ← NEW
                                        └──> reassemble 3200B ──> AudioChunk ──> provider
                                                                    (path unchanged)

Overlay ──Level{after_seq}──> serve() ──reads ring directly──┘   (bypasses the actor)
        ──OverlayStatus────> serve() ──> actor                    (unchanged, 200ms)
```

Two independent polls with strictly separated concerns:

| Poll | Cadence | Path | Authority over |
|---|---|---|---|
| `OverlayStatus` | 200 ms (unchanged) | through the actor | Phase, daemon liveness |
| `Level` | ~20 ms, Recording only | direct ring read | Bar heights **only** |

### 4.1 Decoupling the read rate from the chunk rate (resolves §3.1)

The reader thread (`system.rs:1718-1742`) reads in ~20 ms pieces (640 bytes) instead of
3200. For each piece it computes a level frame and pushes it to the ring. It accumulates
pieces into a 3200-byte buffer and pushes an `AudioChunk` **only when full** — so
`PCM_CHUNK_BYTES`, `MIN_RECORDING_BYTES`, and the provider streaming path are all
byte-identical to today.

Because reads may be short (§3.2), the accumulator is a running buffer, not an assumption.
The final partial chunk at end-of-Recording flushes as it does today.

Actual granularity is bounded below by `pw-record`'s own quantum (typically 10–40 ms), so
~20–40 ms is the realistic floor. That is comfortably inside the ~50 ms perceptual
threshold.

### 4.2 The level ring

A fixed-capacity ring of the last 8 frames, each `{ seq: u64, bands: [u8; 20] }`, stored as
`Arc<Mutex<...>>` on `ActiveRecording` (`voisu-daemon.rs:370-378`), created alongside
`chunk_counter`/`first_chunk_ms` and written from the reader thread.

The mutex is held only for a `push`/`copy` — **never across an `.await`**.

### 4.3 The stateless cursor

Request: `Level { after_seq: u64 }`. Response: the frames newer than that sequence.

The cursor lives in the **request**, so the daemon keeps no per-client state — consistent
with the existing request/response IPC. The Overlay never misses a peak between polls, and
never re-renders a duplicate frame.

`Level` is answered **directly inside `serve()`** from the `Arc`, never entering the actor
channel (resolves §3.3). Level data is purely observational; it needs no serialization
against lifecycle transitions.

## 5. Level computation (daemon)

Per ~20 ms frame:

1. Decode s16le pairs — `i16::from_le_bytes([lo, hi])` over `chunks_exact(2)`, the idiom
   already used at `system.rs:1836`.
2. Feed a bank of **20 second-order IIR bandpass (biquad) filters**, log-spaced from
   **80 Hz to 8 kHz**. Log spacing is essential: voice energy concentrates below 1 kHz, so
   linear bands would leave the upper half of the meter permanently dead. 8 kHz is Nyquist
   for a 16 kHz stream.
3. Envelope-follow each band (single-pole low-pass over squared amplitude).
4. Convert to dB, clamp at a −60 dB floor, normalise to `u8`.

Yielding a 20-byte frame. This is a pure function — `fn bands(pcm: &[i16], state: &mut
BandState) -> [u8; 20]` — testable headlessly.

## 6. No new dependency

A hand-rolled biquad bank (~60 lines, RBJ cookbook coefficients) rather than an FFT crate.

At 320 samples the CPU difference between an FFT and a biquad bank is **not measurable** —
both are far below the daemon's websocket and capture costs. So this is purely a packaging
decision, and it follows the precedent set by the `keyring` ADR (2026-07-20, PR #62), which
rejected a dependency specifically to avoid COPR vendored-build bloat where a zero-dep path
existed with equivalent capability. `rustfft` would add 7 crates to the vendor tarball for
no capability gain.

If genuine frequency-domain analysis is ever needed beyond a bar meter, `rustfft` is the
correct minimal choice — not `spectrum-analyzer` (heavier, opinionated) and not `realfft`
(no dependency saving here).

## 7. Rendering (Overlay)

`overlay.rs` documents itself as *"presentation-only state derived from the daemon's public
observer response"*. All display logic lands there; the daemon stays the source of truth.

- **Widget:** a `gtk::DrawingArea` replacing the `meter` `gtk::Label` in `capsule`
  (§3.6). 20 rounded bars mirrored about the centre line.
- **Colour:** the existing `.recording` accent `#65D6A0`. No new theming infrastructure —
  the capsule is hardcoded dark (§3.7) and introducing theme-awareness for one widget would
  be inconsistent.
- **Smoothing:** a pure `BarSmoother` — **fast attack, slow release** per bar. Fast attack
  so consonants snap; slow release so the meter falls gracefully instead of strobing. This
  single choice decides whether the result looks premium or looks like a 1995 equaliser.
- **Coalescing:** when a poll returns several frames, each is fed through the smoother in
  order, then **one** draw occurs. Peaks survive without drawing faster than the screen.
- **At rest:** all bars at floor, motionless.
- **Non-Recording phases:** the DrawingArea draws nothing. Processing/Success/Failure render
  exactly as today.

The record dot on the pill pulses gently so a long pause does not read as a crash.

## 8. Compatibility

`PROTOCOL_VERSION` **stays at 1**. Adding a `Level` variant does not bump it.

Rationale: the socket path is namespaced by version (`.../voisu/v1/daemon.sock`), so bumping
would make a running old daemon invisible to the new CLI **and** the new Overlay until
restart — breaking `voisu status` and every other command to defend against a window that
barely exists. Daemon and Overlay are user systemd units that keep running old code in
memory until restarted, at which point both flip together.

The residual skew case (new Overlay, old daemon) is handled by §9 rather than by versioning.

## 9. Failure handling — the isolation rule

**A failed `Level` poll must never influence daemon-liveness state.**

Per §3.4, an old daemon answers an unknown `Level` variant with silence, which is
indistinguishable from death. Without isolation, a skewed Overlay would drive
`PresentationController::observe_unreachable` 50 times a second and strobe the
"daemon unavailable" flash while dictation works perfectly.

Therefore:

- The `Level` poll has its own failure path. It never calls `observe_unreachable` and never
  touches the flash latch (`overlay.rs:155-166`).
- The 200 ms `OverlayStatus` poll remains the **sole** authority on daemon liveness.
- On level failure: bars decay to floor. Nothing else changes.
- The `Level` read timeout is **~5 ms**, not 150 ms (§3.5). A skipped frame is invisible; a
  stalled UI is not. The poll must skip rather than block.

This rule is required regardless of the versioning decision — a level poll can also fail
from a timeout or a busy daemon.

## 10. Timer lifecycle

The fast timer is armed on **entering** the Recording phase and disarmed on leaving it.

Arm/disarm must key off `ObservedSignal` (`overlay.rs:202-209`), reusing the semantics of
the existing `RecordingNotifyLatch` (`overlay.rs:218-238`) — **not** off `view.phase`
directly. `RecordingNotifyLatch` deliberately distinguishes `Reachable(Recording)` from
`Unreachable` so that a transient status-read failure mid-Recording does not reset the latch.
A second, inconsistent edge definition would be a bug.

`glib::timeout_add_local` has no pause/resume; a source runs until it returns
`ControlFlow::Break`. Either tear the source down on phase exit and reinstall on entry, or
install permanently and early-return when not Recording. Prefer teardown so the idle cost is
genuinely zero.

**Arm on phase, not on backend.** If the window is buried on GNOME the poll still runs — the
user *is* recording and the window may resurface at any moment; a frozen waveform on
resurface is worse than a few seconds of 2–4% CPU.

Daemon-unreachable also disarms, so a dead daemon never means 50 reconnect attempts/second.

## 11. Mandatory verification before implementation

This change touches the **audio capture path**, which is critical and easy to break subtly.
The implementing agent must NOT begin editing code on the strength of this document alone.

Before writing any code, the agent must verify in the current source and report findings:

1. `PCM_CHUNK_BYTES` and `MIN_RECORDING_BYTES` (`system.rs:54-55`) — confirm values and
   every consumer of both.
2. The reader thread's ownership and locking (`system.rs:1677-1742`) — confirm that adding
   a per-piece computation there cannot block, deadlock, or delay the provider path, and
   confirm what `PROCESS_POLL` actually is (it was not resolved during research).
3. That no resampling exists anywhere on the audio path, i.e. every `AudioChunk` really is
   s16le mono 16 kHz regardless of provider (`system.rs:2331, 2698`;
   `voisu-core/src/lib.rs:472`).
4. The actor bypass is genuinely safe — that reading the ring inside `serve()` cannot race
   Recording teardown or observe a freed `Arc`.
5. `MAX_CONNECTIONS = 32` (`voisu-daemon.rs:41`) — confirm a 50 Hz connection-per-request
   poll cannot starve the shared connection budget used by the CLI and other clients. **If
   it can, raise it in review before proceeding** — this may require a persistent connection
   for `Level` instead, which would be a design change, not an implementation detail.
6. That removing `activity` breaks no test.

Any finding that contradicts this spec must be raised **before** implementing, not worked
around silently.

## 12. Testing

All substantive logic is pure and runs headlessly without GTK, following the existing
convention in `overlay.rs` (small `Default` struct + `observe(...)`-style method + injected
`Instant`; sentence-like test names; `red_` prefix for RED proofs).

| Unit | Tests |
|---|---|
| `bands()` | 1 kHz sine lands in the expected band; silence → all floor; full-scale → all ceiling; short/partial buffers do not panic |
| `BarSmoother` | attack rises within N frames; release decays monotonically; coalesced frames preserve a peak that a single-frame path would drop |
| Timer arming | `Reachable(Recording)` arms; every other reachable phase disarms; an `Unreachable` blip mid-Recording does **not** disarm (mirrors `RecordingNotifyLatch`) |
| Seq cursor | no duplicate frames across polls; no dropped frames; wraparound of the 8-slot ring when the Overlay stalls |
| Isolation rule | a failing `Level` poll leaves `PresentationController` state untouched and never arms the unavailable flash |
| Chunk reassembly | short reads reassemble to byte-identical 3200-byte `AudioChunk`s; the final partial chunk still flushes |

The reassembly test is the most important regression guard in this list: it proves the
provider path is unchanged.

## 13. Explicit non-goals

- No scrolling/oscilloscope waveform.
- No bars during Processing, Success, or Failure.
- No click-to-stop; the Overlay stays click-through.
- No theme-awareness for the capsule (out of scope; would require revisiting all of it).
- No change to `PROTOCOL_VERSION`, provider streaming, or the 200 ms status poll.
- No second capture stream. The Overlay never touches the microphone.
