# PRD — Transcription accuracy to production level

> Status: READY FOR APPROVAL. Approved by: (pending)
> Wayfinder map: `.scratch/voisu-accuracy/map.md` · Evidence: recordings 11–14, 2026-07-17

## 1. Problem & evidence

Live blind test (2026-07-17): Raja dictated four technical paragraphs; overall
**26.3% WER** (per-paragraph 12.1% / 49.1% / 20.6% / 26.9%). Production bar is
≤10%. Root causes established from `voisu history` + code inspection:

1. **Deepgram integration is architecturally broken.** `DEEPGRAM_CHUNK_BYTES = 16_000 * 2`
   (`system.rs:57`): one-second PCM slices are POSTed independently to the
   batch `/v1/listen` endpoint and concatenated. Context-free 1 s slices
   produce word salad (recording 11's deepgram source). Any single chunk
   failure fails the whole provider (`??` in `complete`) and the absence is
   **invisible in history** (recordings 12–14 show no deepgram source and no
   diagnostic).
2. **Groq path is handicapped.** `whisper-large-v3-turbo` called with **no
   `prompt`, no `language`, no `temperature`** (`system.rs:3598`). Domain terms
   lose to common-word bias: async→"racing", serde→"a shared", Tokio→"Tokyo",
   caching→"casting", grep→"grab", SELinux→"ac linux".
3. **Chunk-seam artifacts.** Groq streams 30 s chunks with only 0.5 s overlap,
   merged by naive word-overlap dedup (`merge_chunk_transcripts`,
   `system.rs:3640`) → duplications ("The The tricky part", "need the need")
   at seams.
4. **Reconciliation is NOT the primary villain** (contrary to the earlier
   STATE.md hypothesis). In the one reconciled recording it slightly improved
   the Groq source. But it merged a catastrophically bad Deepgram source
   instead of discarding it — a policy gap that matters once Deepgram returns.

## 2. Goals & acceptance

- Overall **WER ≤10%** on the same 4-paragraph suite, re-dictated live
  (scorer: `wer.py` methodology — normalized, punctuation-insensitive).
- **No fluent-nonsense substitutions** (grammatical phrases replacing what was
  said, e.g. "shut up the stale value").
- **No silent provider absence**: every history record either contains each
  configured provider's source Transcript or a recorded failure diagnostic.
- Latency does not regress: release-to-text tail (time after capture
  finalize) stays ≤ current ~0.5–1 s for typical dictations.
- Operating cost: effectively free at personal heavy daily use (see §5).

## 3. Design

### 3.1 Groq request strategy (ticket 04)

- **Full-audio-at-finalize for Recordings ≤ 120 s** (the dominant dictation
  case). Evidence: the finalize-tail request already costs ~400–500 ms for
  35–43 s recordings (recordings 12–14, `completed_ms` 402–493); Groq Whisper
  transcribes far faster than realtime, so one full-audio request at finalize
  costs approximately the same latency as today's unavoidable tail request —
  while giving Whisper full context and eliminating all seams.
- **Recordings > 120 s** keep pre-streamed chunking, but: chunk 60 s, overlap
  raised 0.5 s → **4 s**, and the merge uses the existing word-overlap dedup
  with the longer window (up from 24 words if needed). Seam quality at this
  length is secondary; >120 s dictations are rare.
- Request gains: `language=en` (config-overridable), `temperature=0`,
  vocabulary `prompt` (§3.2), model from config (§5 default; env override
  `VOISU_GROQ_MODEL` retained).

### 3.2 Vocabulary prompt system (ticket 04)

- **Built-in developer dictionary + user dictionary**, merged into a Whisper
  `prompt`. Hard constraint: Whisper honors only ~224 tokens of prompt — the
  builder takes user terms first, then built-in categories, and truncates at
  budget.
- Built-in dictionary ships wide for engineer out-of-the-box use, grouped by
  category (AI tooling: Claude, Claude Code, Codex, OpenAI, Anthropic, GPT,
  LLM, Groq, Deepgram, Whisper, token, inference…; Linux/system: systemd,
  systemctl, journalctl, SELinux, Wayland, KDE, PipeWire, xkbcommon, RPM,
  dnf, grep, chmod, kernel…; programming: async, await, serde, Tokio, enum,
  mutex, Rust, cargo, TypeScript, npm…; infra/full-stack: Kubernetes, Docker,
  Postgres, Redis, pub-sub, WebSocket, API, JSON, HTTP, TLS, CI/CD,
  deployment, frontend, backend, full-stack, latency, p99…).
- User dictionary: plain text, one term per line, comments with `#`, at
  `$XDG_CONFIG_HOME/voisu/dictionary.txt` (default `~/.config/voisu/`).
  Missing file = built-ins only. No CLI needed this iteration.
- The same merged term list feeds Deepgram **keyterm boosting** (§3.3) —
  single source of truth.
- Prompt shape: natural comma-separated glossary (Whisper biases toward prompt
  vocabulary/style; avoid instructions — it is not an instruction channel).

### 3.3 Deepgram nova-3 websocket streaming (ticket 05)

Implementation guide (2026-07-17): notes-vault →
`AI Created Stuff/Hyprvox Rebuild/Deepgram Streaming Guide for Voisu.md` —
the implementer follows it, including its pseudocode mapping onto the
unchanged `ProviderStream` trait. Bound decisions:

- Real-time websocket streaming replaces the 1 s batch chunks: connect
  `wss://api.deepgram.com/v1/listen` with `model=nova-3&encoding=linear16&`
  `sample_rate=16000&channels=1&interim_results=true&smart_format=true` plus
  endpointing/utterance-end params per the guide; auth via the existing
  `Authorization: Token` header scheme.
- Audio goes out as raw binary WS frames from `send_audio`; control messages
  (`Finalize`, `CloseStream`, `KeepAlive`) as JSON text frames. The final
  Transcript is assembled from **`is_final: true` segments only** (a
  `TranscriptAccumulator`), never blind concatenation.
- `keyterm` repeated query params (nova-3 replaced `keywords`; no boost
  weights) fed from the shared dictionary (§3.2).
- **Transport: native `tokio-tungstenite` (rustls) + `futures-util`,
  in-process** — a persistent duplex stream doesn't fit the one-shot
  curl-subprocess pattern; a single long-lived websocket I/O task slots into
  the existing `ProviderReaper` adoption contract (one `JoinHandle` instead of
  many curl chunk tasks).
- No built-in resume on the server side: bounded app-level reconnect; a
  mid-Recording Deepgram drop fails the provider **visibly** (§3.5) and the
  parallel Groq stream + existing selection pipeline carries the Recording.

### 3.4 Reconciliation source-quality gating (ticket 06)

- Before requesting an LLM merge, gate on source comparability: if the two
  source Transcripts are catastrophically divergent (length-ratio and
  token-overlap heuristics — exact thresholds set in implementation with
  tests), **select the better source** (existing selection semantics) instead
  of merging garbage in.
- Reconciliation prompt/model tuning itself is out of this iteration (map:
  Not yet specified) — the gate protects quality first.

### 3.5 Provider-failure visibility (ticket 06)

- A provider that fails or is absent produces a recorded entry in the history
  record: provider, stage reached, boundary diagnostic. `voisu history` and
  `voisu export` surface it. No silent absence.

## 4. Implementation split — three parallel subagents on a feature branch

All work lands on a feature branch (e.g. `feature/transcription-accuracy`);
local RPM install + live 4-paragraph suite BEFORE any PR; PR + merge on CI
green only after the suite shows accuracy AND latency wins. Driver is purely
orchestrator. Reviews: GPT-5.6 Sol (first review high, re-reviews medium).

| Ticket | Scope | Agent | Main files |
|---|---|---|---|
| 04 Groq accuracy | §3.1 + §3.2 | Opus 4.8 | `system.rs` (GroqStream, request_groq_chunk), new dictionary module, config |
| 05 Deepgram streaming | §3.3 | **Fable 5, medium effort** (architectural) | `system.rs` (DeepgramStream replacement), Cargo deps per guide |
| 06 Reconciliation + visibility | §3.4 + §3.5 | Opus 4.8 | `voisu-core/src/lib.rs` (decision pipeline), history/diagnostics records |

Coordination contract: 04 owns the shared dictionary module; 05 consumes it
read-only (merge point: driver integrates if landed in parallel). 06 touches
`voisu-core` decision pipeline; 04/05 touch `voisu-app` adapters — disjoint by
design. All: TDD through public seams, workspace tests green, GTK-free default
build preserved.

## 5. Model & cost defaults

Research: `.scratch/voisu-accuracy/assets/02-groq-deepgram-pricing.md` (2026-07-17).

- **Groq Whisper is free forever at Raja's scale** (free tier: 7,200
  audio-sec/hr, 2,000 req/day — covers 2 h/day dictation for both models).
  Since cost does not discriminate, **default = `whisper-large-v3`** — the
  accuracy goal decides (published WER ~8.4–10.3% vs turbo's ~12%, gap widens
  on jargon). `VOISU_GROQ_MODEL` env override retained; live A/B on the suite
  at acceptance (ticket 07) confirms or flips the default.
- **Reconciliation model: `llama-3.1-8b-instant`** — free-tier token volume is
  negligible.
- **Deepgram is credit-limited, not free forever**: no recurring free tier;
  the one-time $200 signup credit lasts ~7 months at 2 h/day (~2.4 years at
  30 min/day), then ~$7–28/mo. Decision for approval: Deepgram stays as the
  second opinion funded by credit, and MUST be cleanly disableable
  (single-provider mode already exists — recordings 12–14 prove Groq-only
  works) so hitting the credit wall never breaks dictation or forces payment.

## 6. Out of scope

- Local speech inference; provider replacement; Overlay work.
- Delivery-side `xkbcommon`/direct-Delivery failure (separate effort).
- Reconciliation prompt/model tuning (revisit post-fix with evidence).
- Mic/audio-quality path — only revisited if the suite misses 10% after
  provider fixes.

## Appendix A — Acceptance test suite (reference texts)

The exact four paragraphs for the live WER suite (ticket 07). Score against
these verbatim, normalized/punctuation-insensitive (wer.py methodology from
the 2026-07-17 baseline: 26.3% overall — 12.1% / 49.1% / 20.6% / 26.9%).

**Paragraph 1 — Programming vocabulary**

The async function returns a promise that resolves to a JSON payload. We
deserialize it with serde, match on the enum variant, and propagate errors
using the question mark operator. If the mutex is poisoned, the thread panics,
so we wrap the lock in a helper that recovers gracefully. Finally, we spawn a
Tokio task to flush the buffer and await the join handle before shutdown.

**Paragraph 2 — Systems and CLI vocabulary**

First run systemctl daemon-reload, then enable the user unit with systemctl
--user enable voisu-daemon. Check the journal with journalctl -xe and grep for
xkbcommon errors. The RPM spec expands the changelog automatically, but you
still need rpmbuild with the correct dist tag. If SELinux denies the socket,
use audit2allow to generate a local policy module.

**Paragraph 3 — Jargon, numbers, and acronyms mixed**

The API gateway handles roughly 4,500 requests per second with a p99 latency
of 230 milliseconds. We upgraded from HTTP/1.1 to HTTP/2 over TLS 1.3, which
cut TCP handshake overhead by about 40 percent. The Kubernetes cluster runs 12
nodes with 64 gigabytes of RAM each, and the OOM killer triggered twice last
week when the JVM heap exceeded its cgroup limit.

**Paragraph 4 — Natural spoken technical explanation**

So the way the caching layer works is, whenever a request comes in, we first
check Redis for a hit, and if it's stale we fall back to Postgres but serve
the stale value anyway while revalidating in the background. It's basically
stale-while-revalidate, just implemented by hand. The tricky part was cache
invalidation across replicas, because the pub-sub channel would occasionally
drop messages under load.
