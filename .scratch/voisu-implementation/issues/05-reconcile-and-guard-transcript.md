# 05 — Reconcile and guard the final Transcript

**Parent:** [Build reliable cloud-first dictation for Fedora Wayland](../../voisu-spec/issues/01-fedora-cloud-dictation.md)

**What to build:** A bounded decision pipeline that selects agreeing Source
Transcripts cheaply, reconciles material differences, blocks unsafe candidates,
and delivers exactly one clean final Transcript.

**Blocked by:** 04 — Stream concurrently to Deepgram with a Provider Deadline.

**Status:** ready-for-agent

- [ ] Near-identical Source Transcripts select deterministically without a reconciliation request.
- [ ] Material disagreements invoke the configured cloud reconciliation model within a bounded deadline.
- [ ] Quality guardrails reject prompt artifacts, meta-reasoning, obvious hallucinated suffixes, mixed-script garbage, and suspicious expansion.
- [ ] One bounded recovery attempt can repair a candidate without exposing intermediate text.
- [ ] Failed recovery selects a clean Source Transcript or reports failure when neither source is safe.
- [ ] Every path produces at most one Delivery and records its selection, validation, and fallback reasons.

