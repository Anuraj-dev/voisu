# Research Groq Whisper pricing and model choice for near-free operation

**Label:** `wayfinder:research` (AFK)  
**Status:** closed  
**Blocks:** 03-write-prd

## Question

Which Groq Whisper model should be Voisu's default — `whisper-large-v3` or
`whisper-large-v3-turbo` — given Raja wants to run effectively free forever?
Needed: current Groq pricing per audio-hour for both models, free-tier /
rate-limit reality, cost projection for heavy daily dictation (e.g. 2 h
audio/day), accuracy difference on technical speech (published evidence; a live
A/B on the 4-paragraph suite happens at acceptance), and whether the vocabulary
`prompt` parameter changes billing. Also: Deepgram nova-3 streaming pricing and
free-credit terms, for the same near-free constraint.

## Resolution

Groq Whisper is free forever at personal heavy use (free tier: 7,200
audio-sec/hr, 2,000 req/day). Cost does not discriminate between models, so
accuracy decides: default `whisper-large-v3` (published WER ~8.4–10.3% vs
turbo ~12%), confirmed or flipped by the live A/B at acceptance.
Reconciliation on `llama-3.1-8b-instant` is effectively free. Deepgram nova-3
streaming has NO recurring free tier — the $200 signup credit lasts ~7 months
at 2 h/day; keep it as a credit-funded second opinion and cleanly disableable.
Full asset: [assets/02-groq-deepgram-pricing.md](../assets/02-groq-deepgram-pricing.md)
