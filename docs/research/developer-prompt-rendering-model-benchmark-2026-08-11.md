# Developer Prompt Rendering model benchmark (#140)

**Date:** 2026-08-12T02:22:15+00:00
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
- **Gemini transport:** `curl` → `POST https://generativelanguage.googleapis.com/v1beta/models/{id}:generateContent` with `x-goog-api-key` header file; `responseMimeType=application/json` and preferred `responseJsonSchema`.
- **Deadlines:** content wall latency measured; production override if `latency_ms > 1500` → `fallback_baseline` + `E_DEADLINE`. curl `--max-time 8.0` for content capture; synthetic CC-13 exercises hard deadline outcome.
- **Gemini:** live-run on this matrix when credentials resolve; candidates scored via the same `#139` compose gates.
- **Synthetic local:** schema-invalid, provider_failure, deadline_exceeded, skipped.
- **Offline:** full #139 corpus candidate replay must match expected decisions.

## Credentials reality

| Provider | Live status |
|---|---|
| Groq `openai/gpt-oss-20b` | `live` |
| Gemini 3.5 Flash-Lite | `live` |
| Gemini 3.6 Flash | `live` |

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
| Status | live |
| Structured parse ok rate | 0.8571428571428571 |
| Content accept rate (compose) | 0.7142857142857143 |
| Production accept rate (≤1.5s gate) | 0.6428571428571429 |
| Semantic match rate (production render) | 0.2857142857142857 |
| Content semantic match rate | 0.2857142857142857 |
| Within 1500ms rate | 0.9285714285714286 |
| Unsafe delivers (would_deliver ∧ ¬semantic) | 8 |
| Latency p50 ms | 877.365 |
| Latency p95 ms | 2001.414999999999 |
| Latency max ms | 3502.2 |
| Latency min ms | 177.42 |

### Per-vector summary

| Vector | Fixture | parse | content_accept | prod_accept | semantic | ≤1500 | unsafe | p50 ms | p95 ms |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|
| `V-backtrack-clear` | CC-04 | 1/1 | 1/1 | 1/1 | 0/1 | 1/1 | 1/1 | 929.1 | 929.1 |
| `V-backtrack-uncertain` | CC-05 | 1/1 | 1/1 | 1/1 | 0/1 | 1/1 | 1/1 | 876.3 | 876.3 |
| `V-dual-stt-name` | CC-16 | 1/1 | 1/1 | 1/1 | 0/1 | 1/1 | 1/1 | 835.5 | 835.5 |
| `V-everyday-short` | CC-18 | 1/1 | 0/1 | 0/1 | 1/1 | 1/1 | 0/1 | 954.0 | 954.0 |
| `V-filler-clear` | CC-03 | 0/1 | 0/1 | 0/1 | 1/1 | 1/1 | 0/1 | 196.4 | 196.4 |
| `V-multiparagraph` | CC-24 | 1/1 | 1/1 | 1/1 | 0/1 | 1/1 | 1/1 | 710.3 | 710.3 |
| `V-negation-dual` | CC-15 | 1/1 | 1/1 | 1/1 | 0/1 | 1/1 | 1/1 | 854.3 | 854.3 |
| `V-protected-command-url` | CC-17 | 1/1 | 1/1 | 1/1 | 0/1 | 1/1 | 1/1 | 1143.3 | 1143.3 |
| `V-protected-name` | CC-14 | 1/1 | 1/1 | 1/1 | 0/1 | 1/1 | 1/1 | 848.5 | 848.5 |
| `V-quote-convert` | CC-21 | 1/1 | 1/1 | 0/1 | 0/1 | 0/1 | 0/1 | 3502.2 | 3502.2 |
| `V-structured-goal` | CC-20 | 1/1 | 1/1 | 1/1 | 0/1 | 1/1 | 1/1 | 1153.3 | 1153.3 |
| `V-structured-multi` | CC-07 | 0/1 | 0/1 | 0/1 | 0/1 | 1/1 | 0/1 | 177.4 | 177.4 |
| `V-sym-exclamation` | CC-01 | 1/1 | 1/1 | 1/1 | 1/1 | 1/1 | 0/1 | 878.4 | 878.4 |
| `V-sym-period-newline` | CC-02 | 1/1 | 0/1 | 0/1 | 1/1 | 1/1 | 0/1 | 1193.3 | 1193.3 |

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

## Live results — Gemini `gemini-3.5-flash-lite`

| Metric | Value |
|---|---:|
| Rows (vector×trial) | 14 |
| Vectors | 14 |
| Status | live |
| Structured parse ok rate | 1.0 |
| Content accept rate (compose) | 0.5 |
| Production accept rate (≤1.5s gate) | 0.0 |
| Semantic match rate (production render) | 0.7142857142857143 |
| Content semantic match rate | 0.42857142857142855 |
| Within 1500ms rate | 0.0 |
| Unsafe delivers (would_deliver ∧ ¬semantic) | 0 |
| Latency p50 ms | 2098.13 |
| Latency p95 ms | 4477.314999999999 |
| Latency max ms | 7914.19 |
| Latency min ms | 1831.06 |

### Per-vector summary

| Vector | Fixture | parse | content_accept | prod_accept | semantic | ≤1500 | unsafe | p50 ms | p95 ms |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|
| `V-backtrack-clear` | CC-04 | 1/1 | 1/1 | 0/1 | 1/1 | 0/1 | 0/1 | 2418.3 | 2418.3 |
| `V-backtrack-uncertain` | CC-05 | 1/1 | 0/1 | 0/1 | 1/1 | 0/1 | 0/1 | 2048.5 | 2048.5 |
| `V-dual-stt-name` | CC-16 | 1/1 | 0/1 | 0/1 | 1/1 | 0/1 | 0/1 | 1831.1 | 1831.1 |
| `V-everyday-short` | CC-18 | 1/1 | 0/1 | 0/1 | 1/1 | 0/1 | 0/1 | 2035.9 | 2035.9 |
| `V-filler-clear` | CC-03 | 1/1 | 0/1 | 0/1 | 1/1 | 0/1 | 0/1 | 2268.4 | 2268.4 |
| `V-multiparagraph` | CC-24 | 1/1 | 1/1 | 0/1 | 1/1 | 0/1 | 0/1 | 2047.1 | 2047.1 |
| `V-negation-dual` | CC-15 | 1/1 | 0/1 | 0/1 | 1/1 | 0/1 | 0/1 | 1955.9 | 1955.9 |
| `V-protected-command-url` | CC-17 | 1/1 | 1/1 | 0/1 | 0/1 | 0/1 | 0/1 | 2037.6 | 2037.6 |
| `V-protected-name` | CC-14 | 1/1 | 0/1 | 0/1 | 0/1 | 0/1 | 0/1 | 2159.9 | 2159.9 |
| `V-quote-convert` | CC-21 | 1/1 | 1/1 | 0/1 | 0/1 | 0/1 | 0/1 | 2147.8 | 2147.8 |
| `V-structured-goal` | CC-20 | 1/1 | 1/1 | 0/1 | 1/1 | 0/1 | 0/1 | 2433.3 | 2433.3 |
| `V-structured-multi` | CC-07 | 1/1 | 0/1 | 0/1 | 0/1 | 0/1 | 0/1 | 7914.2 | 7914.2 |
| `V-sym-exclamation` | CC-01 | 1/1 | 1/1 | 0/1 | 1/1 | 0/1 | 0/1 | 1936.0 | 1936.0 |
| `V-sym-period-newline` | CC-02 | 1/1 | 1/1 | 0/1 | 1/1 | 0/1 | 0/1 | 2626.7 | 2626.7 |

### Config

```json
{
  "temperature": 0.0,
  "responseMimeType": "application/json",
  "endpoint_family": "generativelanguage.googleapis.com/v1beta generateContent",
  "maxOutputTokens": 3500,
  "delivery_deadline_ms": 1500,
  "curl_max_time_s": 8.0
}
```

## Live results — Gemini `gemini-3.6-flash`

| Metric | Value |
|---|---:|
| Rows (vector×trial) | 14 |
| Vectors | 14 |
| Status | live |
| Structured parse ok rate | 0.35714285714285715 |
| Content accept rate (compose) | 0.35714285714285715 |
| Production accept rate (≤1.5s gate) | 0.0 |
| Semantic match rate (production render) | 0.7142857142857143 |
| Content semantic match rate | 0.5714285714285714 |
| Within 1500ms rate | 0.0 |
| Unsafe delivers (would_deliver ∧ ¬semantic) | 0 |
| Latency p50 ms | 8010.125 |
| Latency p95 ms | 8012.687 |
| Latency max ms | 8012.96 |
| Latency min ms | 4514.8 |

### Per-vector summary

| Vector | Fixture | parse | content_accept | prod_accept | semantic | ≤1500 | unsafe | p50 ms | p95 ms |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|
| `V-backtrack-clear` | CC-04 | 0/1 | 0/1 | 0/1 | 1/1 | 0/1 | 0/1 | 8010.0 | 8010.0 |
| `V-backtrack-uncertain` | CC-05 | 1/1 | 1/1 | 0/1 | 1/1 | 0/1 | 0/1 | 6105.0 | 6105.0 |
| `V-dual-stt-name` | CC-16 | 1/1 | 1/1 | 0/1 | 1/1 | 0/1 | 0/1 | 6952.8 | 6952.8 |
| `V-everyday-short` | CC-18 | 1/1 | 1/1 | 0/1 | 1/1 | 0/1 | 0/1 | 7973.9 | 7973.9 |
| `V-filler-clear` | CC-03 | 0/1 | 0/1 | 0/1 | 1/1 | 0/1 | 0/1 | 8010.2 | 8010.2 |
| `V-multiparagraph` | CC-24 | 1/1 | 1/1 | 0/1 | 1/1 | 0/1 | 0/1 | 6076.2 | 6076.2 |
| `V-negation-dual` | CC-15 | 0/1 | 0/1 | 0/1 | 1/1 | 0/1 | 0/1 | 8011.4 | 8011.4 |
| `V-protected-command-url` | CC-17 | 0/1 | 0/1 | 0/1 | 0/1 | 0/1 | 0/1 | 8010.1 | 8010.1 |
| `V-protected-name` | CC-14 | 1/1 | 1/1 | 0/1 | 0/1 | 0/1 | 0/1 | 4514.8 | 4514.8 |
| `V-quote-convert` | CC-21 | 0/1 | 0/1 | 0/1 | 0/1 | 0/1 | 0/1 | 8012.5 | 8012.5 |
| `V-structured-goal` | CC-20 | 0/1 | 0/1 | 0/1 | 1/1 | 0/1 | 0/1 | 8013.0 | 8013.0 |
| `V-structured-multi` | CC-07 | 0/1 | 0/1 | 0/1 | 0/1 | 0/1 | 0/1 | 8011.4 | 8011.4 |
| `V-sym-exclamation` | CC-01 | 0/1 | 0/1 | 0/1 | 1/1 | 0/1 | 0/1 | 8010.1 | 8010.1 |
| `V-sym-period-newline` | CC-02 | 0/1 | 0/1 | 0/1 | 1/1 | 0/1 | 0/1 | 8010.5 | 8010.5 |

### Config

```json
{
  "temperature": 0.0,
  "responseMimeType": "application/json",
  "endpoint_family": "generativelanguage.googleapis.com/v1beta generateContent",
  "maxOutputTokens": 3500,
  "delivery_deadline_ms": 1500,
  "curl_max_time_s": 8.0
}
```

## Synthetic / fallback gates (local)

| Vector | Decision | Error codes | Oracle match |
|---|---|---|---|
| `V-schema-invalid` | fallback_baseline | `['E_MALFORMED']` | True |
| `V-deadline-outcome` | fallback_baseline | `['E_DEADLINE']` | True |
| `V-provider-failure` | fallback_baseline | `['E_PROVIDER']` | True |
| `V-skipped-cloud` | fallback_baseline | `[]` | True |

## Offline #139 corpus replay

Fixtures: 24; decision+render match: 24; fail: 0.

## Safety / fidelity notes

- **Unsafe deliver** counts production paths that would deliver model-composed text while failing the oracle string match. Hierarchical fallback still protects against invented content / protected-token hits / unverifiable derivation when compose rejects.
- Completeness and source-order gates from #139 v1.1.2 remain authoritative; models that omit unremoved source words fail `E_UNVERIFIABLE` → baseline.
- Deadline: any live trial with wall latency > 1500ms is forced to `fallback_baseline` for production_decision even if content compose would accept.

## Quota / provider failure

- Rate-limited attempts (HTTP 429) are retried with backoff; final rows record `rate_limited` if still limited.
- Synthetic `V-provider-failure` confirms `E_PROVIDER` → baseline.
- Free/dev quotas may throttle under multi-trial matrices; this package records headers/flags without storing secrets.

## Provisional recommendation

**Status:** three_way_no_production_ready_default

`openai/gpt-oss-20b`: parse=0.9, content_sem=0.3, prod_sem=0.3, within_1500ms=0.9, prod_accept=0.6, unsafe=8, p50=877.4ms, p95=2001.4ms `gemini-3.5-flash-lite`: parse=1.0, content_sem=0.4, prod_sem=0.7, within_1500ms=0.0, prod_accept=0.0, unsafe=0, p50=2098.1ms, p95=4477.3ms `gemini-3.6-flash`: parse=0.4, content_sem=0.6, prod_sem=0.7, within_1500ms=0.0, prod_accept=0.0, unsafe=0, p50=8010.1ms, p95=8012.7ms Ranked first under research scoring: `gemini-3.5-flash-lite` — but **no production-ready default** on this matrix (production_accept_rate=0 (no in-budget cloud accepts); within_1500ms=0.00). For #144: keep local baseline as Delivery default; treat cloud as optional only when in-budget + compose accept; document Adaptive cloud-skip vs Structured-only cloud if Gemini quality wins past 1.5s.

### Caveats

- Three-way comparison uses the same #139 compose gates and #138 oracles.
- Latency includes network/queue; production must still enforce 1.5s deadline.
- Oracle match is exact string equality after light whitespace normalize.
- Ranking uses unsafe↓ → prod_accept → within_1500ms → parse → content_semantic. Production semantic alone is not used (baseline fallback can inflate it).

### Follow-ups

- Research rank leader was `gemini-3.5-flash-lite` (not production-ready).
- If a Gemini model wins quality but loses p95>1500ms, document Adaptive cloud skip vs Structured-only cloud policy for #144.
- Re-run with higher trials if free-tier throttling produced sparse ok rows.
- Bind the chosen model in product only behind #139 compose_fixture accept.

## Notes for #144

- Product binding should keep `#139` compose gates as the only accept path for model candidates (no free-form final string).
- Prefer providers/models that clear structured parse + completeness under 1500ms p95; otherwise ship local baseline under deadline.
- Groq config used here: `openai/gpt-oss-20b` + `reasoning_effort=low`.
- Gemini config: `responseMimeType=application/json`, preferred `responseJsonSchema`, models `gemini-3.5-flash-lite` / `gemini-3.6-flash`.

## Verification commands

```bash
python3 docs/research/developer-prompt-rendering-model-benchmark-harness-2026-08-11.py --self-check
python3 docs/research/developer-prompt-rendering-model-benchmark-harness-2026-08-11.py --provider groq --live
python3 docs/research/developer-prompt-rendering-model-benchmark-harness-2026-08-11.py --provider gemini --live --trials 1
python3 -m json.tool docs/research/developer-prompt-rendering-model-benchmark-2026-08-11.json >/dev/null
```

