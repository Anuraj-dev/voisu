# Research Deepgram nova-3 websocket streaming integration

**Label:** `wayfinder:research` (AFK)  
**Status:** closed  
**Blocks:** 03-write-prd

## Question

How exactly does Voisu integrate Deepgram's real-time websocket streaming API
(nova-3): connection lifecycle, auth, audio framing from the existing 16 kHz
s16le mono PCM stream, interim vs final results, keyterm boosting,
smart_format, endpointing/finalize semantics, error/reconnect handling, and how
this maps onto the existing `ProviderStream` trait (`send_audio` /
`complete` / `abort`) and the restricted-process model (current code shells out
to curl; a websocket needs a different transport). Deliverable: an
implementation guide written to Raja's notes-vault and linked here.

## Resolution

Guide written to the notes-vault:
`AI Created Stuff/Hyprvox Rebuild/Deepgram Streaming Guide for Voisu.md`.
Key bindings: `wss://api.deepgram.com/v1/listen` (nova-3, linear16 16 kHz
mono, interim_results, smart_format, endpointing/utterance_end); raw binary WS
frames for audio, JSON text frames for Finalize/CloseStream/KeepAlive; final
Transcript assembled from `is_final: true` segments only; `keyterm` repeated
params (replaces `keywords`); transport = native `tokio-tungstenite` (rustls)
+ `futures-util` in-process, one long-lived io-task adopted by the existing
`ProviderReaper`; bounded app-level reconnect, visible provider failure, Groq
carries the Recording on a mid-Recording drop. PRD §3.3 binds these.
