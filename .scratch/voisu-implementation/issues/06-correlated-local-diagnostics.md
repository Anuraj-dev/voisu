# 06 — Inspect and expire correlated local diagnostics

**Parent:** [Build reliable cloud-first dictation for Fedora Wayland](../../voisu-spec/issues/01-fedora-cloud-dictation.md)

**What to build:** Local, bounded diagnostic evidence that lets a user trace and
export one Recording without retaining raw audio or uploading telemetry by
default.

**Blocked by:** 05 — Reconcile and guard the final Transcript.

**Status:** ready-for-agent

- [ ] One correlation ID joins capture, chunk, provider, reconciliation, validation, Delivery, and error events.
- [ ] History exposes Source Transcripts, final Transcript, timing, and decision reasons according to configured retention.
- [ ] Diagnostic export redacts credentials, authorization headers, secret identifiers, and unrelated environment values.
- [ ] Raw audio is absent unless the user explicitly enables debug capture.
- [ ] Debug audio records its expiry and cleanup removes expired captures safely.
- [ ] A fixed captured fixture can be replayed through provider and validation boundaries without speaking again.
- [ ] No standard command uploads diagnostics automatically.

