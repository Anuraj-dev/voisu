# Map — Transcription accuracy to production level

**Label:** `wayfinder:map`

## Destination

An approved PRD (`docs/specs/2026-07-17-transcription-accuracy.md`) for the
transcription accuracy overhaul, implemented and accepted: overall WER ≤10% on
the live 4-paragraph technical dictation suite, with no fluent-nonsense
substitutions and no silent provider absence.

## Notes

- This effort carries execution into the map (Raja's override): after the PRD
  ticket closes, implementation tickets are worked in parallel (up to 3) on a
  feature branch. Routing (Raja, 2026-07-17): the driver is **purely
  orchestrator**; hard/architectural tickets (05 Deepgram streaming) go to
  **Fable 5 subagents at medium effort**; regular tickets (04, 06) to
  **Opus 4.8 subagents**; Sonnet 5 for research/bulk reading; **code review
  stays GPT-5.6 Sol** (first review high, re-reviews medium, per repo
  routing). Local test (RPM from committed feature branch, no push) against
  the 4-paragraph suite BEFORE any PR; PR + merge only on CI green after the
  suite shows accuracy and latency wins.
- Deepgram stays even past the $200 credit (Raja will rotate to a new
  account/API key); the disableable switch remains required.
- Evidence base: 2026-07-17 blind dictation test (4 technical paragraphs,
  26.3% WER overall) + `voisu history` recordings 11–14. Key findings in
  `docs/sessions/2026-07-17.md` and the PRD.
- Decided in grilling (2026-07-17): Deepgram is rebuilt as real nova-3
  websocket streaming (batch-on-finalize rejected for latency; dropping it
  rejected); vocabulary prompt = built-in developer/engineer dictionary +
  user-editable dictionary, rich out of the box (AI tooling, Linux, deployment,
  full-stack terms); Groq model choice via benchmark + pricing research with a
  near-free-forever operating constraint; acceptance bar = ≤10% WER on the
  4-paragraph suite re-dictated live.
- The local Markdown tracker is authoritative.

## Decisions so far

<!-- one line per closed ticket -->

- [Research Deepgram nova-3 websocket streaming integration](issues/01-deepgram-streaming-research.md) — native tokio-tungstenite websocket, is_final-only assembly, keyterm boosting; guide in notes-vault.
- [Research Groq Whisper pricing and model choice for near-free operation](issues/02-groq-pricing-benchmark.md) — Groq free tier covers heavy personal use; default `whisper-large-v3` on accuracy; Deepgram is credit-funded, must stay disableable.

## Not yet specified

- Whether reconciliation prompt/model itself needs tuning once both providers
  produce comparable-quality sources (depends on post-fix evidence).
- Mic/audio-quality improvements (input gain, noise, sample-rate path) if the
  suite still misses 10% after provider fixes.
- Delivery-side `xkbcommon`/direct-Delivery failure (tracked in STATE.md, a
  separate effort — only in this map if it blocks acceptance evidence).
- Future (post-acceptance): benchmark whether a single provider/model alone
  meets the bar — Raja wants a Groq-only vs dual-provider comparison
  eventually; explicitly not this iteration.

## Out of scope

- Local speech inference.
- Overlay redesign/polish (STATE.md priority 2, separate effort).
- Replacing Groq or Deepgram with other providers.
