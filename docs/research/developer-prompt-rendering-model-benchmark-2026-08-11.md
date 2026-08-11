# Developer Prompt Rendering model benchmark (#140)

**Date:** 2026-08-11T18:17:48+00:00
**Issue:** [#140](https://github.com/Anuraj-dev/voisu/issues/140)
**Governing:** #139 combined-call contract v1.1.2 (completeness + source-order gates intact); #138 behavior oracles.
**Companion JSON:** [`developer-prompt-rendering-model-benchmark-2026-08-11.json`](./developer-prompt-rendering-model-benchmark-2026-08-11.json)
**Harness:** [`developer-prompt-rendering-model-benchmark-harness-2026-08-11.py`](./developer-prompt-rendering-model-benchmark-harness-2026-08-11.py)

## Ticket question

Which of Gemini 3.5 Flash-Lite, Gemini 3.6 Flash, and Groq GPT-OSS-20B best satisfies the approved behavior/schema contract under semantic safety, source fidelity, structured-output validity, p50/p95 latency, 1.5-second fallback, quota, and provider-failure tests?

## Method

- **Accept gate:** import/reuse `#139` `compose_fixture` from `developer-prompt-rendering-combined-call-prototype-2026-08-11.py` (v1.1.2 completeness/order/protected/invented-content gates). **No gate weakening.**
- **Prompts:** `#139` `model_prompt_contracts` system + response instructions; user payload carries sources, host selection, fingerprint, policy, protected tokens, closed catalogs, few-shot shape.
- **Oracles:** related `#138` `expected_final` when linked; else `#139` `expected.rendered` / local baseline.
- **Groq transport:** `curl` → `POST https://api.groq.com/openai/v1/chat/completions` (Python urllib Cloudflare 403 on this host historically). Auth: `secret-tool lookup voisu-provider groq` (never stored in assets).
- **Groq body:** model `openai/gpt-oss-20b`, `temperature=0`, `reasoning_effort='low'`, strict `json_schema` binding.
- **Deadlines:** content wall latency measured; production override if `latency_ms > 1500` → `fallback_baseline` + `E_DEADLINE`. curl `--max-time 8.0` for content capture; synthetic CC-13 exercises hard deadline outcome.
- **Gemini:** harness paths + matrix defined; **not executed** — no credentials.
- **Synthetic local:** schema-invalid, provider_failure, deadline_exceeded, skipped.
- **Offline:** full #139 corpus candidate replay must match expected decisions.

## Credentials reality

| Provider | Available on this host | Live status |
|---|---|---|
| Groq `openai/gpt-oss-20b` | yes (secret-tool) | live |
| Gemini 3.5 Flash-Lite | **no** | `not_run_missing_credentials` |
| Gemini 3.6 Flash | **no** | `not_run_missing_credentials` |

## Matrix

Live content vectors: **14** × trials; synthetic: **4**; offline replay: all #139 fixtures.

| Vector | Fixture | Category |
|---|---|---|
| `V-sym-exclamation` | CC-01 | symbol_conversion |
| `V-sym-period-newline` | CC-02 | symbol_conversion |
| `V-filler-clear` | CC-03 | filler_removal |
| `V-backtrack-clear` | CC-04 | backtrack_removal |
| `V-backtrack-uncertain` | CC-05 | backtrack_uncertain |
| `V-structured-multi` | CC-07 | structured_multi_label |
| `V-dual-stt-name` | CC-16 | dual_stt_reconciliation |
| `V-protected-command-url` | CC-17 | protected_tokens |
| `V-everyday-short` | CC-18 | everyday_organize |
| `V-structured-goal` | CC-20 | structured_label |
| `V-quote-convert` | CC-21 | symbol_conversion |
| `V-protected-name` | CC-14 | protected_tokens |
| `V-negation-dual` | CC-15 | protected_negation_dual_stt |
| `V-multiparagraph` | CC-24 | layout_multiparagraph |
| `V-schema-invalid` | CC-18 | schema_invalid (local) |
| `V-deadline-outcome` | CC-13 | deadline_fallback (local) |
| `V-provider-failure` | CC-12 | provider_failure (local) |
| `V-skipped-cloud` | CC-23 | cloud_skipped (local) |

## Live results — Groq `openai/gpt-oss-20b`

| Metric | Value |
|---|---:|
| Rows (vector×trial) | 14 |
| Vectors | 14 |
| Structured parse ok rate | 0.8571428571428571 |
| Content accept rate (compose) | 0.5714285714285714 |
| Production accept rate (≤1.5s gate) | 0.5714285714285714 |
| Semantic match rate (production render) | 0.35714285714285715 |
| Content semantic match rate | 0.35714285714285715 |
| Within 1500ms rate | 0.9285714285714286 |
| Unsafe delivers (would_deliver ∧ ¬semantic) | 8 |
| Latency p50 ms | 996.085 |
| Latency p95 ms | 2470.184999999999 |
| Latency max ms | 4466.92 |
| Latency min ms | 215.47 |

### Per-vector summary

| Vector | Fixture | parse | content_accept | prod_accept | semantic | ≤1500 | unsafe | p50 ms | p95 ms |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|
| `V-backtrack-clear` | CC-04 | 1/1 | 1/1 | 1/1 | 0/1 | 1/1 | 1/1 | 985.0 | 985.0 |
| `V-backtrack-uncertain` | CC-05 | 1/1 | 1/1 | 1/1 | 0/1 | 1/1 | 1/1 | 940.1 | 940.1 |
| `V-dual-stt-name` | CC-16 | 0/1 | 0/1 | 0/1 | 1/1 | 1/1 | 0/1 | 215.5 | 215.5 |
| `V-everyday-short` | CC-18 | 1/1 | 0/1 | 0/1 | 1/1 | 1/1 | 0/1 | 1395.0 | 1395.0 |
| `V-filler-clear` | CC-03 | 1/1 | 0/1 | 0/1 | 1/1 | 1/1 | 0/1 | 1192.3 | 1192.3 |
| `V-multiparagraph` | CC-24 | 1/1 | 1/1 | 1/1 | 0/1 | 1/1 | 1/1 | 1353.6 | 1353.6 |
| `V-negation-dual` | CC-15 | 1/1 | 1/1 | 1/1 | 0/1 | 1/1 | 1/1 | 854.7 | 854.7 |
| `V-protected-command-url` | CC-17 | 1/1 | 1/1 | 1/1 | 0/1 | 1/1 | 1/1 | 998.5 | 998.5 |
| `V-protected-name` | CC-14 | 1/1 | 1/1 | 1/1 | 0/1 | 1/1 | 1/1 | 944.8 | 944.8 |
| `V-quote-convert` | CC-21 | 1/1 | 1/1 | 1/1 | 0/1 | 1/1 | 1/1 | 1127.1 | 1127.1 |
| `V-structured-goal` | CC-20 | 1/1 | 1/1 | 1/1 | 0/1 | 1/1 | 1/1 | 993.7 | 993.7 |
| `V-structured-multi` | CC-07 | 1/1 | 0/1 | 0/1 | 0/1 | 0/1 | 0/1 | 4466.9 | 4466.9 |
| `V-sym-exclamation` | CC-01 | 0/1 | 0/1 | 0/1 | 1/1 | 1/1 | 0/1 | 233.5 | 233.5 |
| `V-sym-period-newline` | CC-02 | 1/1 | 0/1 | 0/1 | 1/1 | 1/1 | 0/1 | 1243.8 | 1243.8 |

### Config

```json
{
  "reasoning_effort": "low",
  "temperature": 0.0,
  "response_format": "json_schema.strict",
  "delivery_deadline_ms": 1500,
  "curl_max_time_s": 8.0
}
```

## Gemini status

Both Gemini models are **blocked on this host** (`not_run_missing_credentials`). The harness builds the same user payload and would validate candidates through `#139` `compose_fixture`. **No Gemini latency/quality numbers are invented.**

## Synthetic / fallback gates (local)

| Vector | Decision | Error codes | Oracle match |
|---|---|---|---|
| `V-schema-invalid` | fallback_baseline | `['E_MALFORMED']` | True |
| `V-deadline-outcome` | fallback_baseline | `['E_DEADLINE']` | True |
| `V-provider-failure` | fallback_baseline | `['E_PROVIDER']` | True |
| `V-skipped-cloud` | fallback_baseline | `[]` | True |

## Offline #139 corpus replay

Fixtures: 24; decision+render match: 24; fail: 0.

## Safety / fidelity notes (Groq live)

- **Unsafe deliver** counts production paths that would deliver model-composed text while failing the oracle string match. Hierarchical fallback still protects against invented content / protected-token hits / unverifiable derivation when compose rejects.
- Completeness and source-order gates from #139 v1.1.2 remain authoritative; models that omit unremoved source words fail `E_UNVERIFIABLE` → baseline.
- Deadline: any live trial with wall latency > 1500ms is forced to `fallback_baseline` for production_decision even if content compose would accept.

## Quota / provider failure

- Rate-limited attempts (HTTP 429) are retried with backoff; final rows record `rate_limited` if still limited.
- Synthetic `V-provider-failure` confirms `E_PROVIDER` → baseline.
- Groq free/dev quotas may throttle under multi-trial matrices; this package records headers/flags without storing secrets.

## Provisional recommendation

**Status:** provisional_groq_only

Groq `openai/gpt-oss-20b` (reasoning_effort=low) achieved structured_parse_ok=0.86, content_semantic=0.36, production_semantic=0.36, within_1500ms=0.93, production_accept=0.57, unsafe_deliver=8, p50=996.1ms, p95=2470.2ms. Non-zero unsafe delivers observed under production decision mirror — do **not** ship as default without guard tightening or prompt revision. **Final three-way pick is blocked** until Gemini Flash-Lite and 3.6 Flash are live-benchmarked on this matrix.

### Caveats

- Gemini 3.5 Flash-Lite and Gemini 3.6 Flash were **not live-run** (missing credentials). No comparative winner across the three ticket models can be finalized.
- Recommendation below is **provisional** and conditioned on Groq-only evidence plus local #139/#138 gates.

### Follow-ups (blocks final pick)

- Obtain Google/Gemini API credentials and re-run `--provider gemini --live` (or `--provider all --live`).
- Compare Gemini Flash-Lite vs 3.6 Flash vs Groq on the same matrix (semantic, unsafe deliver, p50/p95, ≤1.5s rate).
- If Gemini wins quality but loses p95>1500ms, document Adaptive cloud skip vs Structured-only cloud policy for #144.

## Notes for #144

- Product binding should keep `#139` compose gates as the only accept path for model candidates (no free-form final string).
- Prefer providers/models that clear structured parse + completeness under 1500ms p95; otherwise ship local baseline under deadline.
- Re-run this harness with Gemini credentials before choosing a default cloud model.
- Groq config used here: `openai/gpt-oss-20b` + `reasoning_effort=low`.

## Verification commands

```bash
python3 docs/research/developer-prompt-rendering-model-benchmark-harness-2026-08-11.py --self-check
python3 docs/research/developer-prompt-rendering-model-benchmark-harness-2026-08-11.py --provider groq --live
python3 -m json.tool docs/research/developer-prompt-rendering-model-benchmark-2026-08-11.json >/dev/null
```

