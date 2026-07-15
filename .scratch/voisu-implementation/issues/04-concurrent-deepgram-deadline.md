# 04 — Stream concurrently to Deepgram with a Provider Deadline

**Parent:** [Build reliable cloud-first dictation for Fedora Wayland](../../voisu-spec/issues/01-fedora-cloud-dictation.md)

**What to build:** Concurrent cloud processing where Deepgram receives live
frames, Groq receives bounded overlapping chunks, and stop completes within a
Provider Deadline even when one provider is slow.

**Blocked by:** 03 — Dictate through PipeWire and Groq into the clipboard.

**Status:** ready-for-agent

- [ ] Deepgram begins receiving audio during the Recording rather than only after stop.
- [ ] Groq chunks carry stable order and overlap metadata and include the final tail.
- [ ] Two valid Source Transcripts are available for reconciliation when both finish within the Provider Deadline.
- [ ] One valid Source Transcript proceeds when the other provider fails or misses the deadline.
- [ ] Late, duplicated, missing, and reordered provider events cannot create duplicated Delivery.
- [ ] Structured events expose chunk, finalization, provider, and release-to-text timing without secrets.

