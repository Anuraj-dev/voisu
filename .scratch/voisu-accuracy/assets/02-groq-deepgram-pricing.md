# Groq & Deepgram pricing research (mid-2026) — for near-free-forever personal dictation

Research date: 2026-07-17. Prices/limits change; re-verify against official pages before committing to an ADR.

## 1. Groq Whisper (audio transcription)

| Model | Price / audio-hour | WER (short-form) | Speed |
|---|---|---|---|
| `whisper-large-v3` | **$0.111/hr** | ~8.4–10.3% (sources vary) | ~164–228x real-time |
| `whisper-large-v3-turbo` | **$0.04/hr** (~2.8x cheaper) | ~12% | ~216–228x real-time |

- Minimum billing: **10 seconds per request**, regardless of actual clip length — short dictation utterances still get billed as a 10s floor. This matters for chunking strategy: fewer, longer requests are cheaper than many tiny ones.
- **Free tier** (no credit card needed): **2,000 Whisper requests/day**, **7,200 audio-seconds/hour** (~2 hours of audio per rolling clock-hour), plus the general account caps of **30 RPM** and up to ~1,000–14,400 RPD depending on model tier. Usage under these caps is not billed at all — you stay on the free plan.
- **`prompt` parameter**: capped at 224 tokens, used only to bias transcription style/vocabulary (e.g., steer spelling of jargon/proper nouns). It does **not** add token-based billing — Whisper is billed strictly per audio-hour processed, not per token, so the prompt is billing-neutral.
- **Accuracy delta, large-v3 vs turbo**: sources disagree on the exact numbers (8.4% vs 12%, or 10.3% vs 12%), but all agree turbo trades a few points of WER for large real-time-factor and cost gains. No source found a dedicated technical-jargon/domain-vocabulary WER breakdown for Groq's hosted versions specifically — general guidance from Groq's own docs is: use `large-v3` for "error-sensitive" use cases, `large-v3-turbo` when price/latency matters and multilingual support is enough. Given the `prompt` biasing option exists precisely to help with jargon/proper nouns, it partially compensates for turbo's higher base WER without cost.

Sources:
- https://console.groq.com/docs/speech-to-text
- https://console.groq.com/docs/model/whisper-large-v3
- https://console.groq.com/docs/model/whisper-large-v3-turbo
- https://console.groq.com/docs/rate-limits
- https://groq.com/blog/whisper-large-v3-turbo-now-available-on-groq-combining-speed-quality-for-speech-recognition
- https://groq.com/blog/groq-runs-whisper-large-v3-at-a-164x-speed-factor-according-to-new-artificial-analysis-benchmark
- https://www.cloudzero.com/blog/groq-pricing/
- https://tokenmix.ai/blog/groq-free-tier-limits-2026

## 2. Groq chat completion (reconciliation calls)

- Recommended cheap model: **`llama-3.1-8b-instant`** — **$0.05/M input tokens, $0.08/M output tokens** (one of the cheapest hosted chat models available anywhere, and Groq's fastest small model).
- Free tier for chat models: **30 RPM**, **6,000 TPM**, and **1,000–14,400 RPD** depending on model. A short reconciliation call (correcting/normalizing a transcript snippet, a few hundred tokens in/out) costs a fraction of a cent even off the free tier, and realistically never leaves the free tier at personal-use volumes (a handful of calls per dictation session, not per second).
- Batch API + prompt caching stack for up to ~75% off on-demand rate if ever needed, but at personal-assistant volumes this is moot — the free tier alone covers it.

Sources:
- https://groq.com/pricing
- https://console.groq.com/docs/model/llama-3.1-8b-instant
- https://www.eesel.ai/blog/groq-pricing
- https://tokenmix.ai/blog/groq-free-tier-limits-2026

## 3. Deepgram nova-3 (real-time streaming)

| Tier | Streaming price | Batch price |
|---|---|---|
| Pay-As-You-Go | **$0.0077/min** (~$0.462/hr) | $0.0043/min (~$0.26/hr) |
| Growth plan | $0.0065/min (~16% cheaper) | — |

- **Signup credit: $200**, stated on the official pricing page as **"No expiration. No minimums. No credit card required."** This is a one-time credit, not a recurring free allowance — once spent, usage is billed at the pay-as-you-go rate above. There is no ongoing free-forever tier for streaming beyond this credit.
- **Concurrent connections** on Pay-As-You-Go: up to **50** concurrent REST requests, up to **150** concurrent WebSocket (streaming) connections — far more than a single personal user needs, so not a practical constraint.
- Billing is per-second, so short utterances aren't penalized the way Groq's 10s floor penalizes them.

Sources:
- https://deepgram.com/pricing
- https://brasstranscripts.com/blog/deepgram-pricing-per-minute-2025-real-time-vs-batch
- https://convertaudiototext.com/blog/deepgram-nova-3-explained
- https://deepgram.com/learn/introducing-nova-3-speech-to-text-api

## 4. Cost projection — personal heavy daily dictation (monthly, 30 days/month)

| Daily audio | Groq `large-v3-turbo` | Groq `large-v3` | Deepgram nova-3 streaming (post-credit) |
|---|---|---|---|
| 30 min/day | $0.60/mo *(→ $0 if under free-tier caps)* | $1.67/mo *(→ $0 if under free-tier caps)* | $6.93/mo |
| 1 h/day | $1.20/mo *(→ $0)* | $3.33/mo *(→ $0)* | $13.86/mo |
| 2 h/day | $2.40/mo *(→ $0)* | $6.66/mo *(→ $0)* | $27.72/mo |

Groq reconciliation chat calls (`llama-3.1-8b-instant`): effectively **$0.00/mo** at any of these volumes — token cost per call is a fraction of a cent and call counts stay far under the free-tier RPD/TPM caps for personal use.

**How long does Deepgram's $200 credit last** (streaming, $0.462/hr effective):
- 30 min/day → ~433 hours of credit / 0.5 h/day ≈ **866 days (~2.4 years)**
- 1 h/day → **~433 days (~14 months)**
- 2 h/day → **~216 days (~7 months)**

After the credit is exhausted, Deepgram streaming becomes a real recurring cost (see table above) — there is no way to keep it at $0 indefinitely at these volumes.

## 5. Free-forever verdict per combination

| Combination | Verdict |
|---|---|
| Groq Whisper `large-v3-turbo` | **Effectively free forever** for personal dictation — daily usage patterns (even 2h/day) stay inside the free-tier 7,200 audio-sec/hour and 2,000 req/day caps; cost only appears if those caps are exceeded, and even then it's ~$0.60–2.40/mo at the volumes above. |
| Groq Whisper `large-v3` | **Effectively free forever** too, same free-tier caps apply, at ~2.8x the paid cost if ever pushed past the free tier ($1.67–6.66/mo). |
| Groq chat completion (`llama-3.1-8b-instant`) reconciliation | **Free forever** — negligible token volume for reconciliation calls, well inside free tier. |
| Deepgram nova-3 streaming | **Credit-limited, not free forever.** Starts "free" via the one-time $200 credit (lasts ~7 months to ~2.4 years depending on daily volume), then converts to a real paid subscription ($6.93–$27.72+/mo at these volumes) with no ongoing free path. |

## 6. Recommendation

- **Default Groq model: `whisper-large-v3-turbo`.** At personal-use volumes both models are free (inside Groq's free tier), so the accuracy delta (turbo's ~12% vs large-v3's ~8.4–10.3% WER) is the only real trade-off, not cost. Given turbo is ~2.8x cheaper *if* the free tier is ever exceeded, and Voisu can use the `prompt` parameter (free, no billing impact) to bias toward technical/jargon vocabulary to partially close the accuracy gap, turbo is the better default. Reserve `large-v3` as a fallback/opt-in for sessions where accuracy matters more than speed (e.g., transcribing dense technical dictation where jargon misses are costly) — the cost difference is trivial at these volumes even if paid ($1–7/mo), so switching per-session is cheap insurance, not a budget concern.
- **Groq reconciliation calls**: use `llama-3.1-8b-instant` — free at this scale, no reason to consider a larger/pricier model for a lightweight correction pass.
- **Deepgram nova-3 streaming**: cannot be made free-forever at heavy daily use. The $200 credit buys roughly 7 months (2h/day) to 2.4 years (30min/day) before real charges start ($7–$28/mo thereafter). If the goal is strictly free-forever, Deepgram should be treated as a bounded trial/quality-comparison path, not the primary provider — Groq Whisper is the sustainable default, with Deepgram optionally kept as a premium/streaming-latency option funded by its credit until exhausted.
