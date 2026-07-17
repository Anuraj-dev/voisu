# Write and approve the transcription accuracy PRD

**Label:** `wayfinder:task` (HITL)  
**Status:** open · **Assignee:** driver (Fable 5 session 2026-07-17)  
**Blocked by:** 01-deepgram-streaming-research, 02-groq-pricing-benchmark  
**Blocks:** 04, 05, 06

## Question

Produce `docs/specs/2026-07-17-transcription-accuracy.md`: evidence summary,
decided design (Groq request strategy + vocabulary prompt system + dictionary
format/location, Deepgram websocket streaming design, reconciliation
source-quality gating, provider-failure visibility in history), acceptance
criteria (≤10% WER suite), and the implementation split for three parallel
Opus subagents. Raja approves before implementation starts.
