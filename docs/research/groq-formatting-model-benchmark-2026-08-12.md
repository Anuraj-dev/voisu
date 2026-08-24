# Groq formatting-model benchmark — 2026-08-12

## Decision

**`qwen/qwen3.6-27b` is the best Groq model for the next lightweight formatting experiment.** With reasoning disabled and the prompt tightened through P8, it produced 11 exact outputs from 11 successful responses across repeated structured, quoted, paragraph, and identity cases. Successful calls took **325.54–505.42 ms**. One additional request was rate-limited and is excluded from the semantic denominator.

This is **not approval to make Qwen the production formatting authority**. Qwen does not support Groq's schema-enforced Structured Outputs, and its performance collapses under Voisu's current full #139 derivation contract. The next experiment should therefore test a smaller operation contract whose result is composed and validated locally. Every invalid, unverifiable, late, or semantically unsafe result must continue to fall back to the local baseline.

The immediate blocker is the **full derivation contract**, not raw Groq inference speed:

- Lightweight Qwen P8: 11/11 successful outputs exact, 325.54–505.42 ms.
- Full #139 Qwen contract: one 5,002.52 ms timeout; one parse-and-compose acceptance that still missed the oracle; one parsed response rejected by the compose gate as `E_UNVERIFIABLE`.
- Fresh full #139 GPT-OSS checks: GPT-OSS 20B produced one rejected derivation, one unsafe oracle-miss acceptance, and one rate limit; GPT-OSS 120B produced one timeout and two unsafe oracle-miss acceptances.
- Prior full-contract GPT-OSS 20B run: 85.7% parsed, only 28.6% semantically exact, with 8 unsafe deliveries among 14 rows.

The separate **“Thank you for watching!” bug remains unfixed and orthogonal** to model selection. That failure is in source selection/validation; choosing a faster formatter does not repair it.

## Evidence and scope

The live catalogue request and inference calls used Raja's authenticated Groq account. This report intentionally excludes credentials, organization identifiers, raw headers, and other account metadata. No product code was changed.

The live catalogue returned these relevant active IDs:

- `openai/gpt-oss-20b`
- `openai/gpt-oss-120b`
- `qwen/qwen3.6-27b`
- `llama-3.1-8b-instant`
- `llama-3.3-70b-versatile`
- `groq/compound`
- `groq/compound-mini`

It also returned task-mismatched safety, prompt-classification, audio, and other specialized models. Those were excluded from formatting evaluation.

Groq's official catalogue describes GPT-OSS 20B/120B as production models at about 1,000/500 tokens per second and Qwen 3.6 27B as a preview model at about 500 tokens per second. Groq documents strict constrained-decoding JSON Schema only for GPT-OSS 20B and 120B; Qwen supports JSON Object Mode but not schema enforcement. Sources: [Groq supported models](https://console.groq.com/docs/models), [Groq Structured Outputs](https://console.groq.com/docs/structured-outputs).

Groq documents `reasoning_effort: "low"|"medium"|"high"` for GPT-OSS and `reasoning_effort: "none"|"default"` for Qwen 3.6 27B. `reasoning_format: "hidden"` suppresses reasoning from the returned answer. Source: [Groq reasoning controls](https://console.groq.com/docs/reasoning).

Free-plan limits for GPT-OSS 20B, GPT-OSS 120B, and Qwen 3.6 27B are documented as 30 requests/minute, 1,000 requests/day, 8,000 tokens/minute, and 200,000 tokens/day. Groq notes that limits are organization-wide and the account Limits page is authoritative for account-specific values. Source: [Groq rate limits](https://console.groq.com/docs/rate-limits).

## Evaluation rules

- **Exact** means byte-for-byte equality with the approved oracle output for that fixture.
- A valid HTTP response is not automatically a semantic pass.
- A response accepted by the current parser and compose gate is still recorded as unsafe when it differs from the approved oracle.
- HTTP 400, timeout, and rate-limit responses are transport/capability results, not semantic attempts.
- Latencies below are end-to-end wall times in milliseconds from the live benchmark.

The prompt iterations had distinct purposes:

- **P5:** initial lightweight direct-format prompt.
- **P6:** improved core-shape instructions.
- **P7:** explicit closed rules tested against harder correction, negation, punctuation, protected-token, and quote cases.
- **P8:** P7 plus an explicit quote example, followed by three repeats per fixture.

## Initial P5 model screen

| Model | Fixture | Latency (ms) | Exact? | Evidence |
|---|---|---:|---|---|
| GPT-OSS 20B | CC07 structured | 922.67 | No | Path casing changed |
| GPT-OSS 20B | CC24 paragraphs | 575.34 | No | Oracle mismatch |
| GPT-OSS 20B | identity | 591.00 | Yes | Exact identity preservation |
| GPT-OSS 120B | CC07 structured | 1,359.94 | No | Oracle mismatch |
| GPT-OSS 120B | CC24 paragraphs | 743.82 | No | Oracle mismatch |
| GPT-OSS 120B | identity | 1,541.45 | Yes | Exact identity preservation |
| Qwen 3.6 27B, default reasoning | CC07 / identity schema attempts | 2,970–3,060 | No semantic result | HTTP 400 |
| Qwen 3.6 27B, default reasoning | CC24, JSON Object Mode | 2,918.57 | Yes | Exact, but other two attempts returned 400 |
| Llama 3.1 8B, JSON Object Mode | CC07 structured | 518.50 | Yes | Exact |
| Llama 3.1 8B, JSON Object Mode | CC24 paragraphs | 316.90 | No | Oracle mismatch |
| Llama 3.1 8B, JSON Object Mode | identity | 382.03 | Yes | Exact |
| Llama 3.3 70B | three fixtures | 317–623 | No | 0/3 exact |
| Compound Mini | three fixtures | 1,480–2,640 | Identity only | 1/3 exact |
| Compound | CC07 structured | 2,134.65 | Yes | Exact |
| Compound | CC24 paragraphs | 3,825.59 | No | Oracle mismatch |
| Compound | identity | — | No semantic result | Rate-limited |

The initial screen established two things. First, Groq latency is easily inside the 2–4 second target for most ordinary responses. Second, speed alone does not produce safe formatting: every model except the temporary Llama 8B control missed at least two of the three initial cases or failed transport/capability checks.

`llama-3.1-8b-instant` is not a viable new dependency despite its strong initial speed. Groq scheduled both it and `llama-3.3-70b-versatile` to shut down for Free and Developer tiers on **2026-08-16**. Source: [Groq model deprecations](https://console.groq.com/docs/deprecations).

Compound and Compound Mini are also unsuitable. They are agentic systems able to select built-in tools, which is unnecessary authority and variability for deterministic transcript rendering. Source: [Groq Compound systems](https://console.groq.com/docs/compound/systems).

## GPT-OSS prompt progression

### P6: core success

P6 made both GPT-OSS models exact on all three core cases:

| Model | CC07 (ms) | CC24 (ms) | Identity (ms) | Exact |
|---|---:|---:|---:|---:|
| GPT-OSS 20B | 739.31 | 613.34 | 737.62 | 3/3 |
| GPT-OSS 120B | 1,178.64 | 625.06 | 849.11 | 3/3 |

### P6: risk-screen failure

The same prompt then scored **0/6 exact for each GPT-OSS model** on the harder risk screen:

| Model | Exact | Latency range (ms) |
|---|---:|---:|
| GPT-OSS 20B | 0/6 | 623.28–722.59 |
| GPT-OSS 120B | 0/6 | 453.59–837.45 |

This is the clearest evidence that three attractive core examples are insufficient. Both GPT-OSS sizes learned the visible shape but failed the preservation and decision boundary.

### P7: explicit closed rules on GPT-OSS 20B

| Fixture | Latency (ms) | Exact? |
|---|---:|---|
| CC03 | 601.87 | Yes |
| CC04 | 581.91 | Yes |
| CC05 | 578.22 | No |
| CC17 | 605.08 | Yes |
| CC18 | 668.74 | No |
| CC21 | 489.11 | No |

P7 improved GPT-OSS 20B from 0/6 to 3/6 on the risk fixtures, but it remained far below a safe acceptance threshold.

## Qwen optimization

The initial Qwen failures were not evidence that Qwen was intrinsically slow. They occurred with default reasoning and unsupported schema attempts. Setting `reasoning_effort: "none"` and `reasoning_format: "hidden"` removed the unnecessary reasoning path and exposed a much faster direct-formatting candidate.

### P7

| Suite | Exact | Latency range (ms) | Miss |
|---|---:|---:|---|
| Core | 3/3 | 343.70–509.36 | None |
| Risk | 5/6 | 319.49–386.15 | CC21 quote handling |

### P8 repeated stability screen

P8 added a quote example. Each fixture was attempted three times.

| Fixture | Successful exact outputs | Latency range (ms) | Other result |
|---|---:|---:|---|
| CC07 structured | 3/3 | 504.08–505.42 | None |
| CC21 quote | 3/3 | 325.54–343.10 | None |
| CC24 paragraphs | 2/2 | 331.65–349.15 | One HTTP 429 at 220.53 ms |
| Identity | 3/3 | 336.64–345.56 | None |
| **Total** | **11/11** | **325.54–505.42** | One rate-limited request excluded from semantic denominator |

The HTTP 429 is consistent with the documented free-tier token limits. It is availability evidence and must inform product fallback behavior, but it says nothing about the semantic quality of an output because no output was produced.

P8 is the strongest positive result in the entire free-model evaluation: exact outputs, repeated calls, sub-second latency, and coverage of the quote case that failed P7. It remains a small screen, not production proof.

## Full #139 contract results

### Qwen 3.6 27B using JSON Object Mode

| Fixture | Latency (ms) | Parse | Compose | Oracle | Result |
|---|---:|---|---|---|---|
| CC07 structured | 5,002.52 | — | — | — | Timeout |
| CC24 paragraphs | 862.30 | Pass | Accept | Miss | **Unsafe acceptance** |
| CC17 protected command | 1,028.79 | Pass | Fallback `E_UNVERIFIABLE` | Not delivered | Safe fallback, but no cloud benefit |

Qwen's direct-format strength therefore does not transfer to the current full derivation payload. The contract creates more work, more ways to be incomplete, and at least one case where existing validation accepts a semantically wrong oracle result.

### Fresh GPT-OSS full-contract screen

| Model | Fixture | Latency (ms) | Parse | Compose | Oracle / protection | Result |
|---|---|---:|---|---|---|---|
| GPT-OSS 20B | CC07 structured | 4,431.73 | Pass | Fallback: `E_INVENTED_CONTENT` + `E_UNSAFE_SEMANTICS` | Oracle miss; protected tokens preserved | Safe rejection, over the desired latency budget |
| GPT-OSS 20B | CC24 paragraphs | 896.25 | Pass | Accept | Oracle miss; protected tokens preserved | **Unsafe acceptance** |
| GPT-OSS 20B | CC17 protected command | 216.38 | — | — | No candidate | HTTP 429 from free-tier TPM limit |
| GPT-OSS 120B | CC07 structured | 5,002.88 | — | — | No candidate | Timeout |
| GPT-OSS 120B | CC24 paragraphs | 1,243.33 | Pass | Accept | Oracle miss; protected tokens preserved | **Unsafe acceptance** |
| GPT-OSS 120B | CC17 protected command | 1,307.09 | Pass | Accept | Oracle miss; protected tokens preserved | **Unsafe acceptance** |

These results remove the remaining model-size ambiguity. GPT-OSS 120B does not rescue the contract: it timed out on the structured case and the compose gate accepted both returned candidates despite oracle mismatches. GPT-OSS 20B likewise produced an unsafe accepted oracle miss. Protection checks passing in these rows did not establish full semantic correctness.

### Prior GPT-OSS 20B full-contract benchmark

| Metric | Result |
|---|---:|
| Rows | 14 |
| Parse success | 85.7% (12/14) |
| Semantic exactness | 28.6% (4/14) |
| Unsafe deliveries | 8 |
| Latency p50 | 877.365 ms |
| Latency p95 | 2,001.415 ms |

GPT-OSS 20B also demonstrates that strict JSON shape is not semantic safety. Constrained decoding can guarantee the fields and types while the model still supplies the wrong derivation. The host must retain semantic verification and fail closed.

## Interpretation

1. **Latency is no longer the primary blocker.** Qwen P8 is consistently around one-third to one-half second, and GPT-OSS is generally below 1.2 seconds on the tuned lightweight prompts.
2. **Prompt quality helps, but examples can overfit.** GPT P6 moved from weak P5 results to 3/3 core exact, then failed all six risk cases. The benchmark must keep adversarial preservation cases beside attractive formatting examples.
3. **The current #139 derivation contract fails across every durable general-purpose model tested.** Fresh Qwen, GPT-OSS 20B, and GPT-OSS 120B evidence shows timeouts, rate limits, unverifiable candidates, and unsafe oracle-miss acceptances. High parse success can coexist with low semantic exactness.
4. **Strict schema is useful but insufficient.** It prevents malformed structure; it cannot prove that the operations are supported by the transcript.
5. **A free endpoint requires an immediate fallback.** The live 429 and the documented organization-wide quotas mean a cloud result cannot be required for Delivery.

## Recommended next experiment

Design and benchmark a **smaller operation contract** for Qwen rather than accepting final free-form Markdown or retaining the entire current derivation object. A suitable experiment should:

- represent only a closed set of locally composable operations, such as paragraph boundaries, list boundaries, and heading spans;
- carry source spans or token indices for every operation so the host can verify support;
- prohibit the model from emitting replacement prose as authority;
- preserve the local protected dictionary, technical tokens, quotes, negations, and correction decisions;
- use `reasoning_effort: "none"`, `reasoning_format: "hidden"`, and JSON Object Mode;
- validate JSON shape locally because Qwen lacks Groq schema enforcement;
- compose the final text deterministically on the host;
- reject unsupported, overlapping, out-of-range, late, malformed, or semantically uncertain operations;
- fall back immediately on timeout, HTTP error, 429, or any validation failure;
- run the complete approved corpus plus repeated latency trials before any rollout decision.

The acceptance bar should remain user-visible behavior, not model fluency: zero unsupported text, zero lost protected tokens, zero unsafe deliveries, exact identity on no-op fixtures, exact approved formatting on structural fixtures, and bounded tail latency under repeated free-tier calls.

## Final status

- **Best lightweight candidate:** `qwen/qwen3.6-27b`, P8, reasoning disabled.
- **Production ready:** No.
- **Current blocker:** the full derivation/validation contract fails on all durable Groq candidates tested and does not preserve lightweight Qwen's speed and fidelity.
- **Next action:** prototype and benchmark a smaller verifiable operation contract behind deterministic local composition and fallback.
- **“Thank you for watching!” bug:** diagnosed separately, still unfixed, and not solved by changing models.
