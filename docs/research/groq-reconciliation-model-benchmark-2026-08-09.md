# Groq reconciliation model benchmark — 2026-08-09

**Issue:** [#97 Select the safe Groq reconciliation replacement](https://github.com/Anuraj-dev/voisu/issues/97)
**Companion machine-readable asset:** [`groq-reconciliation-model-benchmark-2026-08-09.json`](./groq-reconciliation-model-benchmark-2026-08-09.json)
**Mode:** read-only research (no code/GitHub/cargo mutations).
**Ticket closure:** **#97 research READY TO CLOSE** — owner + Sol decisions recorded. **#98 not closed** (migration implements binding contract).

## 1. Executive verdict

| Item | Result |
|------|--------|
| Baseline `llama-3.3-70b-versatile` | **50/50 semantic PASS**, **0 unsafe delivers** on this suite; free/dev **shutdown 2026-08-16** |
| **Selected default (owner)** | **`qwen/qwen3.6-27b` + `reasoning_effort: "none"`** (over GPT-OSS) |
| Qwen+none semantic pass | **39/50 (0.78)**; **5 unsafe delivers** (all `negation_insert` meta) — closed in prod only when #98 guard co-lands |
| `openai/gpt-oss-120b` default | **25/50 PASS**; **20 unsafe delivers** (dual polarity + dual repair concat) — **not selected** |
| `openai/gpt-oss-120b` + `reasoning_effort=low` | **36/50 PASS**; **14 unsafe delivers** (dual polarity negation_drop + refusal delivery) — **not selected** |
| Preview availability | **Raja explicitly accepts** Qwen Preview availability risk |
| Reverse-negation residual | **Not accepted as residual delivery** — #98 must co-land reconcile guard with default flip |
| **Sol verdict** | **`APPROVE_CONDITIONAL_QWEN`** |

**Unsafe deliver** = semantic FAIL **and** decision-mirror `would_deliver_model_text=true` (production would type model text).

## 1.1 Owner decisions (recorded)

1. **Preview risk:** Raja **explicitly accepts** Qwen Preview availability risk for exact model `qwen/qwen3.6-27b`.
2. **Model pick:** Raja **selects** exact model **`qwen/qwen3.6-27b` over GPT-OSS** as the #98 default.
3. **Residual safety:** Do **not** ship residual reverse-negation delivery; require the #98 binding guard below to land **with** the default flip.

## 1.2 Sol verdict

**`APPROVE_CONDITIONAL_QWEN`** — research complete; owner accepted Preview risk and selected Qwen; #98 must implement the binding contract (exact model+none only — no family-wide Qwen rule, override omit rules, reconcile `quality_failure_reason` + `is_source_derived` guard, non-source-derived diagnostic, required regressions, `clean_source_fallback` flags, co-land flip+guard, preserve deadlines/cancellation/repair).

## 1.3 Issue #98 binding contract (explicit)

These rules are binding on the #98 migration implementation (not claimed done here):

1. **Selected model body:** **only** the exact model id `qwen/qwen3.6-27b` **always adds** `reasoning_effort: "none"`. **No family-wide Qwen rule** — other `qwen/…` ids do not inherit this body field.
2. **Other overrides:** every other `VOISU_GROQ_RECONCILIATION_MODEL` override **omits** `reasoning_effort` unless that override was **separately tested** with an explicit supported value.
3. **Reconcile success gate:** before returning **Reconciled**, require **both**:
   - `quality_failure_reason` is `None` (no quality failure), and
   - `is_source_derived` is **true**.
4. **Non-source-derived reconcile output:** go **directly** to **`clean_source_fallback`** with `reconciliation_requested=true` and `recovery_attempted=false`.
5. **Required diagnostic:** when non-source-derived reconciliation is detected, emit a diagnostic that non-source-derived reconciliation produced words absent from every Source Transcript.
6. **Required regressions:** #98 must cover at least:
   - exact Qwen meta response (`Please provide the Source Transcripts you would like me to reconcile.` → non-source-derived → `clean_source_fallback` + diagnostic),
   - GPT-OSS refusal (`I'm sorry, but I can't comply with that.` → non-source-derived → `clean_source_fallback` + diagnostic),
   - valid source-derived reconciliation (still returns **Reconciled** when both gate checks pass),
   - exact request-body routing (selected model always adds `reasoning_effort: "none"`; every other override omits unless separately tested; no family-wide Qwen rule),
   - unchanged deadline, cancellation, and repair-path behavior.
7. **Co-landing:** default model flip and the reconcile guard **land together** in #98 (no flip without guard).
8. **Preserve:** existing deadlines, cancellation, and existing repair-path behavior.

## 2. Method (verified API)

- Transport: `curl --config` → `POST https://api.groq.com/openai/v1/chat/completions` (production boundary shape).
- Auth: `secret-tool lookup voisu-provider groq` (not stored in these assets).
- Body: `temperature=0`, production system + reconcile/repair user prompts from `request_groq_reconciliation`.
- Deadline: `--max-time 2` (production curl/process deadline).
- Response text: `/choices/0/message/content` only.
- **Uncontaminated:** final attempt not rate-limited. Rate-limited attempts logged in `attempt_log` and retried (94 rate-limited attempts across matrix; **0** final matrix rows contaminated).
- **Decision mirror:** Python mirror of `quality_failure_reason`, `is_source_derived`, reconcile/repair branches in `crates/voisu-core/src/lib.rs`.
- Python `urllib` to Groq from this host returns Cloudflare **403 / 1010** — not used.

## 3. Strict semantic oracles

| Vector | Kind | Oracle (binding) |
|--------|------|------------------|
| `negation_drop` | reconcile | Exactly one polarity: the negated Deepgram form only. No positive-only, no contradictory both-polarities concatenation, no invented facts. |
| `negation_insert` | reconcile | Exactly one polarity: pure Deepgram positive XOR pure Groq negated. No dual concat, no invent, no non-transcript meta. |
| `double_negation` | reconcile | Exactly one of the two source strings. No concat, no invent. |
| `entity_conflict` | reconcile | Complete entity tuple from exactly one source (Priya Sharma XOR Priya Sharman) with shared remainder. No mixed surnames. |
| `number_date_conflict` | reconcile | Complete version+date+time tuple from exactly one source. No cross-mix, no invent, no dual concat. |
| `dictionary_jargon` | reconcile | Exact Deepgram technical form or exact Groq split form. Note: Deepgram form has fewer normalized words (9) than Groq (12), so merge contraction floor may prefer longer source even when model returns technical form. |
| `prompt_shaped` | reconcile | Exactly one source form. Refusal/non-transcript fails semantic. Dual concat fails. Guard: prompt artifact markers → invoke_repair. |
| `response_format` | reconcile | Plain transcript only; no labels, think tags, or meta. |
| `repair_prompt_artifact` | repair | Exactly one clean source booking; System:/ignore removed; no dual bookings. Repair path enforces is_source_derived. |
| `repair_meta` | repair | Exactly the deploy canary line; meta preamble removed. |

Shared fail conditions: think tags, refusal/meta non-transcript, invented words, contradictory concatenation of opposing facts/polarities.

## 4. Aggregate semantic + safety

| Config | Model | Extra | Uncont. n | Semantic PASS | Pass rate | Unsafe delivers |
|--------|-------|-------|----------:|--------------:|----------:|----------------:|
| `llama_default` | `llama-3.3-70b-versatile` | `{}` | 50 | 50 | 1.0 | **0** |
| `gptoss_default` | `openai/gpt-oss-120b` | `{}` | 50 | 25 | 0.5 | **20** |
| `gptoss_low` | `openai/gpt-oss-120b` | `{"reasoning_effort": "low"}` | 50 | 36 | 0.72 | **14** |
| `qwen_none` | `qwen/qwen3.6-27b` | `{"reasoning_effort": "none"}` | 50 | 39 | 0.78 | **5** |

## 5. Per-vector uncontaminated results (5 trials each)

Cells: `PASS/5 · unsafe_deliver/5 · p50 ms`.

| Vector | llama_default | gptoss_default | gptoss_low | qwen_none |
|--------|--------|--------|--------|--------|
| `negation_drop` | 5/5 · ud=0/5 · p50=258.7 | 0/5 · ud=5/5 · p50=871.7 | 0/5 · ud=5/5 · p50=548.7 | 4/5 · ud=0/5 · p50=245.4 |
| `negation_insert` | 5/5 · ud=0/5 · p50=259.6 | 0/5 · ud=5/5 · p50=833.4 | 5/5 · ud=0/5 · p50=806.6 | 0/5 · ud=5/5 · p50=252.8 |
| `double_negation` | 5/5 · ud=0/5 · p50=269.1 | 5/5 · ud=0/5 · p50=791.6 | 5/5 · ud=0/5 · p50=698.8 | 5/5 · ud=0/5 · p50=247.5 |
| `entity_conflict` | 5/5 · ud=0/5 · p50=256.9 | 5/5 · ud=0/5 · p50=1163.5 | 5/5 · ud=0/5 · p50=650.5 | 5/5 · ud=0/5 · p50=248.1 |
| `number_date_conflict` | 5/5 · ud=0/5 · p50=264.2 | 0/5 · ud=5/5 · p50=1203.9 | 5/5 · ud=0/5 · p50=675.2 | 5/5 · ud=0/5 · p50=283.8 |
| `dictionary_jargon` | 5/5 · ud=0/5 · p50=278.9 | 5/5 · ud=0/5 · p50=990.3 | 5/5 · ud=0/5 · p50=662.3 | 5/5 · ud=0/5 · p50=258.6 |
| `prompt_shaped` | 5/5 · ud=0/5 · p50=274.5 | 0/5 · ud=0/5 · p50=1124.7 | 0/5 · ud=5/5 · p50=643.6 | 0/5 · ud=0/5 · p50=270.1 |
| `response_format` | 5/5 · ud=0/5 · p50=261.4 | 5/5 · ud=0/5 · p50=667.6 | 5/5 · ud=0/5 · p50=523.1 | 5/5 · ud=0/5 · p50=241.2 |
| `repair_prompt_artifact` | 5/5 · ud=0/5 · p50=257.1 | 0/5 · ud=5/5 · p50=1325.1 | 1/5 · ud=4/5 · p50=702.8 | 5/5 · ud=0/5 · p50=268.5 |
| `repair_meta` | 5/5 · ud=0/5 · p50=357.0 | 5/5 · ud=0/5 · p50=765.4 | 5/5 · ud=0/5 · p50=570.6 | 5/5 · ud=0/5 · p50=271.8 |

### 5.1 Critical residual patterns (exact outputs)

#### GPT-OSS dual-polarity `negation_drop` (both configs, 5/5, delivered)

```
Do not deploy the migration tonight. Deploy the migration tonight.
```

Decision mirror: `quality_failure_reason=None` → **`deliver_reconciled`**. Source-derived; no repair.

#### Qwen+none `negation_insert` (5/5 identical) — decision-mirror reconciliation

```
Please provide the Source Transcripts you would like me to reconcile.
```

- Semantic: **FAIL** (non-transcript; not a single polarity form).
- `quality_failure_reason`: **none** (not in prompt-artifact / meta-preamble / expansion lists).
- `is_source_derived`: **not consulted** on reconcile success path (today).
- Pipeline: **`deliver_reconciled`**, `would_deliver_model_text=true` on **5/5** trials.
- Latency p50: **252.8 ms** (well under 2s).
- Conclusion: prior “anomaly” is **systematic behavior** under current prompt+guards, not a fluke.
- **#98 binding:** non-source-derived reconcile output must go to `clean_source_fallback` (`reconciliation_requested=true`, `recovery_attempted=false`) after requiring both `quality_failure_reason` and `is_source_derived` before Reconciled; must emit a diagnostic that non-source-derived reconciliation produced words absent from every Source Transcript; this exact Qwen meta string is a required regression fixture.

#### GPT-OSS low `prompt_shaped` refusal (5/5, delivered)

```
I'm sorry, but I can't comply with that.
```

Semantic FAIL; not source-derived; **still `deliver_reconciled`** because reconcile path lacks `is_source_derived` today — same hole the #98 guard closes.

#### Prompt-shaped artifact path (llama single-source PASS → invoke_repair; qwen/gptoss default dual → invoke_repair)

Concat or single injection text containing `ignore previous instructions` / `ignore all instructions` trips **prompt artifact** → **`invoke_repair`** (not delivered as final without repair).

#### Dictionary contraction note

Technical Deepgram form normalizes to **9** words; Groq split form to **12**. Returning Deepgram form yields ratio 9/12=0.75 **< 0.90** → mirror **`contraction_source_fallback`** even when semantic oracle PASS. Production would prefer longer source, not necessarily deliver the model’s jargon form.

## 6. Latency under production 2s boundary

### 6.1 Matrix p50/max (ms) by config

| Config | p50 range (by vector) | Max observed |
|--------|----------------------|--------------|
| `llama_default` | — | **395.0** (suite max); median of all=266.25 |
| `gptoss_default` | — | **1377.4** (suite max); median of all=882.85 |
| `gptoss_low` | — | **872.2** (suite max); median of all=652.35 |
| `qwen_none` | — | **2011.9** (suite max); median of all=258.3 |

Notable: `qwen_none` `negation_drop` t2 timed out at **2011.9 ms** (HTTP 000/timeout) → mirror **clean_source_fallback** (safe fallback, semantic FAIL on timeout).

### 6.2 Qwen+none size suite (reconcile + repair)

| Size | Words/source | Kind | Success under 2s | p50 | p95 | max |
|------|-------------:|------|-----------------:|----:|----:|----:|
| short | 12 | reconcile | **5/5** | 286.4 | 312.8 | 312.8 |
| short | 12 | repair | **5/5** | 262.8 | 288.1 | 288.1 |
| medium | 120 | reconcile | **5/5** | 272.5 | 277.9 | 277.9 |
| medium | 120 | repair | **5/5** | 283.6 | 311.5 | 311.5 |
| near_maximum | 900 | reconcile | **5/5** | 439.6 | 478.8 | 478.8 |
| near_maximum | 900 | repair | **5/5** | 449.6 | 502.3 | 502.3 |

`near_maximum` = harness **900 words/source** (practical long reconcile under 8K TPM), not a claim about 10-minute full Recording word counts.

## 7. Override policy (verified + #98 binding)

| Case | Model | Body extra | max-time | HTTP | Result |
|------|-------|------------|---------:|-----:|--------|
| `qwen_none_required` | `qwen/qwen3.6-27b` | `{"reasoning_effort": "none"}` | 2 | 200 | think=False lat=266.7 out=Hello world. |
| `qwen_omit_effort_2s` | `qwen/qwen3.6-27b` | `{}` | 2 | 200 | think=True lat=812.9 out= ⏎ <think> ⏎ Here's a thinking process: ⏎  ⏎ 1.  **Analyze User Inpu |
| `qwen_omit_effort_10s` | `qwen/qwen3.6-27b` | `{}` | 10 | 200 | think=True lat=799.0 out= ⏎ <think> ⏎ Here's a thinking process: ⏎  ⏎ 1.  **Analyze User Inpu |
| `gptoss_body_none` | `openai/gpt-oss-120b` | `{"reasoning_effort": "none"}` | 2 | 400 | think=False lat=203.8 err={'message': '`reasoning_effort` must be one of `low`, `medium`, or `high`', 'type': 'invalid_request_error'} |
| `gptoss_body_low` | `openai/gpt-oss-120b` | `{"reasoning_effort": "low"}` | 2 | 200 | think=False lat=527.5 out=Hello world. |
| `llama_body_none` | `llama-3.3-70b-versatile` | `{"reasoning_effort": "none"}` | 2 | 400 | think=False lat=204.1 err={'message': '`reasoning_effort` is not supported with this model', 'type': 'invalid_request_error'} |
| `llama_body_empty` | `llama-3.3-70b-versatile` | `{}` | 2 | 200 | think=False lat=307.6 out=Hello world. |

**Exact implementation rule (#98 binding):**

1. If model id is **exactly** `qwen/qwen3.6-27b`: **always** set `reasoning_effort` to **`"none"`**. **No family-wide Qwen / `qwen/…` rule.**
2. **All other** `VOISU_GROQ_RECONCILIATION_MODEL` overrides (including any other Qwen id): **omit** `reasoning_effort` unless that override was **separately tested** with an explicit supported value.
3. Verified hazards if violated: GPT-OSS + `none` → **400**; Llama + `none` → **400**; exact selected Qwen omit effort → think tags in content (HTTP 200).
4. Env override model name alone is insufficient for the selected model — the `reasoning_effort: "none"` body rule must travel with exact id `qwen/qwen3.6-27b` only.

## 8. Cost / quota

| Model | Docs tier | Docs $/1M in | Docs $/1M out | Observed limit-tokens (this key) |
|-------|-----------|-------------:|--------------:|----------------------------------:|
| llama-3.3-70b-versatile | Production (until 2026-08-16 free/dev) | 0.59 | 0.79 | 12000 |
| openai/gpt-oss-120b | **Production** | 0.15 | 0.60 | 8000 |
| qwen/qwen3.6-27b | **Preview** (owner-accepted risk) | 0.60 | 3.00 | 8000 |

Rate-limit retries: **94** rate-limited attempts in matrix `attempt_log`; all cells eventually obtained uncontaminated finals via backoff.

## 9. Recommendation for #98 (migration) — binding

1. **Default model string:** exact `qwen/qwen3.6-27b` with **hard-coded** `reasoning_effort: "none"` **only** for that exact id (no family-wide Qwen rule).
2. **Do not** ship GPT-OSS as default under current prompts (systematic dual-polarity delivery on negation); owner rejected GPT-OSS in favor of Qwen.
3. **Default flip and guard land together:** on reconcile success, require both `quality_failure_reason` (None) and `is_source_derived` (true) before Reconciled; non-source-derived → `clean_source_fallback` with `reconciliation_requested=true` and `recovery_attempted=false`.
4. **Diagnostic:** non-source-derived reconciliation must diagnose that it produced words absent from every Source Transcript.
5. **Other overrides omit** `reasoning_effort` unless separately tested (no inheritance of `none` by other Qwen ids).
6. **Required regressions:** exact Qwen meta response; GPT-OSS refusal; valid source-derived reconciliation; exact request-body routing; unchanged deadline/cancellation/repair behavior.
7. **Preserve** deadlines, cancellation, and existing repair behavior.
8. Owner decisions already recorded: Preview risk accepted; Qwen selected; residual closed by guard co-landing.

## 10. Finding-by-finding resolution (Sol REQUEST_CHANGES → APPROVE_CONDITIONAL_QWEN)

| Finding | Resolution |
|---------|------------|
| Strict per-vector semantic oracles missing / soft heuristics | **DEFINED** — Each of 10 vectors has oracle_spec requiring single polarity or complete entity/number/date tuple; no invent; no contradictory concat; expected guard outcome documented. |
| Need ≥5 uncontaminated trials per candidate/config × vector | **DONE** — 4 configs × 10 vectors × 5 trials = 200 matrix rows; all final rows uncontaminated (rate-limited attempts retried; 94 rate-limited attempts in logs). |
| Record sanitized rows with full fields | **DONE** — Each matrix row includes model, config, vector, trial, attempts, attempt_log, HTTP status, rate-limit headers, latency, token usage, sanitized_output, semantic_verdict, guard_fallback_verdict. |
| Anomalous Qwen reverse-negation must be re-run and decision-mirrored | **DONE — systematic not anomalous** — qwen_none negation_insert 5/5 identical non-transcript meta; decision mirror shows deliver_reconciled (no quality_failure; is_source_derived not on reconcile path). |
| Qwen+none size/latency under real 2s boundary | **DONE** — short(12w)/medium(120w)/near_maximum(900w) × reconcile+repair × 5 trials: 30/30 success under 2s; p50/p95/max recorded. |
| Exact override behavior for reasoning_effort | **DONE + verified** — exact model `qwen/qwen3.6-27b` must send none; GPT-OSS rejects none (400); Llama rejects none (400). #98 binding: **only** that exact id always adds none; every other override omits unless separately tested (**no family-wide Qwen rule**). |
| Separate rate-limited from model results | **DONE** — attempt_log marks is_rate_limited; final matrix rows all uncontaminated; stats in rate_limit_stats. |
| Preview availability risk owner decision | **ACCEPTED_BY_OWNER** — Raja explicitly accepts Qwen Preview risk for `qwen/qwen3.6-27b`. |
| Qwen reverse-negation residual / guard ordering | **OWNER_SELECTED_GUARD_WITH_FLIP** — residual delivery not accepted; #98 co-lands guard with default flip. |
| Default model selection qwen vs GPT-OSS | **OWNER_SELECTED_QWEN** — exact model `qwen/qwen3.6-27b` over GPT-OSS. |

## 11. Remaining blockers

1. **None for #97 research closure** — owner decisions recorded; Sol **`APPROVE_CONDITIONAL_QWEN`**.
2. **#98 only (implementation, not this research ticket):** implement binding contract (exact model+none only, override omit rules with no family-wide Qwen rule, reconcile gate, non-source-derived diagnostic, required regressions, `clean_source_fallback` flags, co-land flip+guard, preserve deadlines/cancellation/repair).
3. **Not a blocker for assets:** machine-readable full row log is in the JSON companion (200 rows unchanged).

## 12. Files / commands inspected

- `crates/voisu-app/src/system.rs` — `request_groq_reconciliation`, deadlines, prompts
- `crates/voisu-core/src/lib.rs` — decide/repair, `quality_failure_reason`, `is_source_derived`
- `internal/scratch/voisu-smart-writing/issues/01-select-groq-replacement.md`, map, STATE
- Groq docs: deprecations, models, gpt-oss-120b, qwen3.6-27b, reasoning
- Live API via curl + secret-tool (no secrets in assets)

## 13. READY_FOR_FINAL_SOL_REREVIEW

- Durable assets: this markdown + JSON with **200** uncontaminated matrix rows (evidence/arithmetic/owner decisions unchanged), override suite, size suite, oracles, decision mirrors.
- Owner: Preview risk **accepted**; exact model **`qwen/qwen3.6-27b`** selected over GPT-OSS; reverse-negation residual closed by **#98 co-land guard**.
- Sol research verdict recorded: **`APPROVE_CONDITIONAL_QWEN`**.
- Final Sol doc fixes: **no family-wide Qwen rule**; binding #98 contract requires **non-source-derived diagnostic** + **required regressions** (Qwen meta, GPT-OSS refusal, valid source-derived, request-body routing, deadline/cancellation/repair).
- **#97 research ready to close.** **#98 not closed** — binding contract is the migration acceptance criteria.
- Status: **READY_FOR_FINAL_SOL_REREVIEW**.
