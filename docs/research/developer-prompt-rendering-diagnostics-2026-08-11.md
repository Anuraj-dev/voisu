# Developer Prompt Rendering — Fallback Feedback & Diagnostic Evidence (#142)

**Issue:** [#142](https://github.com/Anuraj-dev/voisu/issues/142) · parent map [#133](https://github.com/Anuraj-dev/voisu/issues/133) · blocked by [#139](https://github.com/Anuraj-dev/voisu/issues/139) · feeds [#144](https://github.com/Anuraj-dev/voisu/issues/144)

**Artifacts:** `developer-prompt-rendering-diagnostics-{schema,corpus,prototype}-2026-08-11.*`

**Status:** research proof package. Docs only — no product Rust. Defines the smallest bounded user-facing feedback and local diagnostic event contract so cloud deadline misses are explainable, temporary late-result evaluation is safe, and production keeps a simpler evidence surface after the evaluation lane is removed.

Governing product lock (#137 / #139 / #141 / #138): English-first organize-only; at most one structured cloud call; hierarchical fallback to local baseline; final-only Delivery ≤1.5s; never auto-send / live-type / replace delivered text; temporary pre-production diagnostics may retain valid late cloud results for offline comparison only.

## Answer (ticket question)

The smallest sufficient contract is:

1. **Silent-or-minimal user feedback** — default silent. When a cloud attempt fails hard (deadline / provider / schema / hard validation) and Delivery uses the local baseline, at most one non-blocking status from a closed message catalog. Never claim cloud success when baseline was used. Never auto-send or re-write already-Delivered text.
2. **Ordered local diagnostic event timeline** — relative milliseconds from `utterance_end` (t0), stable event names, bounded payload, redacted of secrets and unbounded HTTP bodies.
3. **Explicit mode flag** — `evaluation` vs `production`. Evaluation may retain one valid late structured candidate for offline comparison (side channel / log). Production never upgrades Delivery; it may record timing-only evidence that a late response arrived.
4. **Production cleanup boundary** — evaluation-only events/fields/UI are listed and must be compile-gated or removed before production DPR ships. Remaining production evidence: timing, failure class, fallback reason, and late-arrival timing without a second Delivery.

This package deliberately does **not** re-define organize-only semantics, combined-call composition, or intent routing — it inherits #139 / #141 / #138.

## Package files

| File | Role |
| --- | --- |
| `developer-prompt-rendering-diagnostics-2026-08-11.md` | This decision writeup |
| `developer-prompt-rendering-diagnostics-schema-2026-08-11.json` | Structural schema for diagnostic records |
| `developer-prompt-rendering-diagnostics-corpus-2026-08-11.json` | Executable fixtures |
| `developer-prompt-rendering-diagnostics-prototype-2026-08-11.py` | Stdlib-only simulator + bounds gate + mutations |

## A. User-facing fallback feedback (bounded)

### Principles

- Prefer **silent**. The Final Transcript appearing in the target surface is the primary feedback.
- If feedback exists, it is **short**, **non-blocking**, **at most one per utterance**, and drawn from a **closed message catalog**.
- Feedback must **never** claim cloud success when the baseline was Delivered.
- Feedback must **never** imply an automatic upgrade, auto-send, or replacement of delivered text.
- Cloud skipped / not attempted / local fast path → **silent** (normal success path).
- Soft salvage (`accept_preserve_words`, `accept_natural_layout`) → **silent** (user receives usable text).
- Hard fallback after a cloud attempt → optional **`minimal_status`** with catalog message `Local formatting used`.

### Closed feedback catalog

| `feedback_kind` | When | Message (exact) | Blocking |
| --- | --- | --- | --- |
| `silent` | Default; skip; success; soft salvage | _(none)_ | n/a |
| `minimal_status` | Hard baseline fallback after cloud attempt | `Local formatting used` | false |

Forbidden: toast spam, multi-step wizards, “cloud enhanced your text”, upgrade prompts, any message that mentions provider secrets or raw error dumps.

### Delivery flags (always)

```json
{ "state": "unsent", "auto_send": false, "live_type": false, "replace_delivered": false }
```

Inherited from #139. Diagnostics and feedback never flip these flags.

## B. Local diagnostic event contract

### Clock

- **t0 basis:** `utterance_end` (stable for skipped and attempted paths).
- Every event carries integer `t_ms` ≥ 0 relative to t0.
- Final-only Delivery deadline: **`delivery_deadline_ms = 1500`**.
- Cloud-attempt elapsed is measured from `cloud_request_started.t_ms` when present; otherwise from t0 for deadline comparison in skipped paths (N/A).

### Closed event names (ordered roles)

| Event | Role |
| --- | --- |
| `route_selected` | #141 route + cloud_request decision recorded |
| `cloud_skipped` | Cloud not attempted (not_allowed / skipped optional) |
| `cloud_request_started` | One structured cloud call begins |
| `cloud_response_received` | Response bytes arrived (may still fail validation) |
| `cloud_deadline_exceeded` | Response not ready by deadline; Delivery proceeds with baseline |
| `provider_failed` | Network/provider error (`E_PROVIDER`) |
| `schema_validation_failed` | Parse/schema failure (`E_SCHEMA`) |
| `source_derivation_failed` | Unverifiable / protected / unsafe / invalid label |
| `composition_accepted` | #139 accept / soft salvage selected |
| `fallback_baseline_selected` | Hierarchical fallback chose local baseline |
| `delivery_emitted` | Final Transcript handed to Delivery (unsent flags) |
| `late_result_retained` | **Evaluation only** — valid late candidate retained for offline compare |
| `late_result_discarded` | Late candidate not used for Delivery (production default; also invalid late) |

### Binding to #139

| #139 trigger / outcome | Diagnostic events (beyond route/delivery) | Error codes |
| --- | --- | --- |
| `succeeded` + accept/soft | `cloud_request_started` → `cloud_response_received` → `composition_accepted` | (soft: `E_UNCERTAIN_*` only) |
| `deadline_exceeded` | `cloud_request_started` → `cloud_deadline_exceeded` → `fallback_baseline_selected` | `E_DEADLINE` |
| `provider_failure` | `cloud_request_started` → `provider_failed` → `fallback_baseline_selected` | `E_PROVIDER` |
| `schema_failure` | `cloud_request_started` → (`cloud_response_received`?) → `schema_validation_failed` → `fallback_baseline_selected` | `E_SCHEMA` |
| hard reject (unsafe / unverifiable / invalid label / protected) | response path → `source_derivation_failed` → `fallback_baseline_selected` | matching `E_*` |
| `skipped` / not attempted | `cloud_skipped` → (no cloud events) → baseline delivery | none |

`delivery_emitted` is always last among Delivery-time events. Late-result events may follow Delivery only when a late response is observed after `delivery_emitted`.

### Event order invariants

1. `route_selected` is first (when present; fixtures always include it).
2. Cloud start precedes any cloud response / deadline / provider failure for that attempt.
3. At most one `cloud_request_started` per recording (I-ONE-CALL).
4. Exactly one of `composition_accepted` or `fallback_baseline_selected` before `delivery_emitted`.
5. Exactly one `delivery_emitted`.
6. `late_result_*` only after `delivery_emitted`.
7. `late_result_retained` only when `mode=evaluation`.
8. Non-decreasing `t_ms` across the timeline (ties allowed for simultaneous logical steps).

### Payload bounds (thresholds)

| Threshold | Value | Notes |
| --- | ---: | --- |
| `delivery_deadline_ms` | 1500 | Product lock |
| `max_events_per_recording` | 24 | Hard cap |
| `max_event_detail_utf8_bytes` | 256 | Free-form detail / failure class strings |
| `max_feedback_message_utf8_bytes` | 64 | Closed catalog messages |
| `max_http_body_snippet_utf8_bytes` | 256 | Truncated; never full raw bodies |
| `max_retained_late_text_utf8_bytes` | 2048 | Evaluation lane only |
| `max_late_results_per_recording` | 1 | At most one retained late candidate |
| `max_correlation_id_utf8_bytes` | 128 | Aligns with existing local diagnostics vocabulary |

### Redaction

Diagnostics **must not** include:

- API keys, bearer tokens, secret file contents
- Authorization headers
- Unbounded raw HTTP bodies (snippets only, clamped)
- Full model system prompts

Fingerprint fields use `sha256:` + 64 hex (same shape as #139 `base_fingerprint`) when retaining equality evidence without full text.

## C. Temporary late-result evaluation lane

### Mode flag

```json
{ "mode": "evaluation" | "production" }
```

### Rules

When a structured candidate arrives **after** `delivery_emitted` of the baseline (or of any already-chosen final):

| Mode | Valid late candidate | Invalid late candidate |
| --- | --- | --- |
| `evaluation` | `late_result_retained` — store fingerprint + clamped text for offline compare; **never** `replace_delivered` | `late_result_discarded` (reason: validation class) |
| `production` | `late_result_discarded` — timing-only evidence (`arrived_t_ms`, optional fingerprint); **no** upgrade path, **no** full-text retention for upgrade | `late_result_discarded` |

**Valid** means the candidate would have passed #139 composition as `accept` / soft salvage (fixture-declared in this research package; product uses the real validator).

Late results **never**:

- delay Delivery past 1500ms
- auto-replace delivered text in either mode
- open a second cloud call
- change Delivery flags

### Evaluation record shape (evaluation only)

```json
{
  "lane": "late_result_evaluation",
  "retained": true,
  "arrived_t_ms": 1675,
  "candidate_fingerprint": "sha256:…",
  "candidate_text_clamped": "…",
  "would_have_decision": "accept",
  "compare_to_delivered": true
}
```

Production mode either omits this object or sets `retained: false` with no text payload.

## D. Production cleanup boundary

### Evaluation-only (remove or compile-gate before production DPR)

| Item | Kind |
| --- | --- |
| Event `late_result_retained` | event |
| Evaluation record with `candidate_text_clamped` / full late text | field |
| Any UI affordance to “apply late result” / “upgrade transcript” | UI |
| Offline compare export of late candidates as a user-facing feature | UI |
| Mode flag left defaulting to `evaluation` | config |
| Retention of late text beyond fingerprint for upgrade experiments | storage |

### Retained forever (production diagnostics)

| Item | Kind |
| --- | --- |
| Ordered timing events (route, cloud start/response/deadline, failures, fallback, delivery) | events |
| Failure class / #139 error codes | fields |
| Fallback trigger reason | fields |
| `late_result_discarded` with timing-only (optional fingerprint) | event |
| User feedback kind chosen (silent / minimal_status) | feedback record |
| Unsent Delivery flags | delivery |
| Correlation id linking the Recording | id |

### Cleanup acceptance

Production is clean when:

1. `mode` is `production` (or evaluation lane is compile-gated off).
2. No code path can set `replace_delivered=true` from a late cloud result.
3. No retained late full-text field is written in production builds.
4. Deadline misses remain explainable from the timing + `E_DEADLINE` + `fallback_baseline_selected` alone.

## E. Alignment

- **#138** oracles: deadline fixture DPR-42 (`observed_elapsed_ms` 1675, delivery by 1500); schema/provider fallbacks DPR-40/47; dual-STT and local routes.
- **#139** fallback triggers, composition decisions, error codes, Delivery flags — inherited, not redefined.
- **#141** routes decide whether cloud is attempted; diagnostics record `route_selected` / `cloud_skipped` from that decision.
- **ADR 0006**: diagnostics stay local; no telemetry upload without separate user action (out of scope for this package’s transport).

## Validation authority

- JSON Schema describes package shape.
- The **stdlib prototype** is the authoritative gate: exact key sets, event order, feedback bounds, evaluation vs production late-result rules, retention caps, secret redaction, and property-bound mutations.
- Exit 0 iff healthy.

```bash
python3 docs/research/developer-prompt-rendering-diagnostics-prototype-2026-08-11.py
```

## Explicit non-goals

- Product Rust wiring (owned by later integration / #144 assembly)
- Live cloud benchmarks (#140)
- Re-defining organize-only composition (#139)
- Re-defining intent routing weights (#141)
- Grammar / enrichment / auto-send
- Network egress of diagnostics

## What #144 should inherit

- Production vs evaluation split as a hard mode flag
- Silent-or-minimal feedback catalog and hard-fallback message
- Event name set + t0=`utterance_end` + caps table
- Late-result rules: evaluation retain-for-compare only; production discard-for-upgrade with optional timing evidence
- Explicit evaluation-only removal list before shipping
- Unsent Delivery flags on every path
