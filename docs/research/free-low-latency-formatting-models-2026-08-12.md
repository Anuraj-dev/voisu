# Free low-latency formatting models for Voisu

**Date:** 2026-08-12  
**Question:** Can a faster free hosted model plus a better prompt make DPR formatting both useful and fast enough?  
**Evidence:** current OpenRouter and Hugging Face catalog research, model-owner/runtime documentation, the existing #138/#139/#140 DPR assets, and a fresh one-shot OpenRouter screen from Raja's Fedora host.  
**Scope:** research and live screening only. No product code was changed.

## Direct answer

**Latency is a major blocker, but it is not the only blocker.** The formatter must also produce the strict #139 candidate shape, preserve all source evidence and protected tokens, visibly format inputs that need structure, and pass the deterministic compose gate. A fast identity response is not useful formatting; a valid JSON response can still be semantically wrong.

**The best free hosted candidate found is `nvidia/nemotron-3-nano-30b-a3b:free`, but it does not qualify for production.** It was the only candidate to produce the exact approved CC-07 Structured render (P5, 2,802.88 ms). That result is valuable evidence that the prompt/model pair can understand the desired section structure. It is still almost twice the entire product deadline, and the same P5 prompt hallucinated `Goal`/`Notes` structure on CC-24 and damaged an identity control.

**No currently tested free hosted candidate is production-ready.** Nano 30B is suitable only for one further **Structured-only, compose-gated experiment**. It must not handle Natural, Adaptive layout, or identity-preserving traffic unless new evidence reverses the repeated over-formatting and exactness failures. OpenRouter currently lists this exact route at zero prompt/completion price, but does not advertise `response_format` or `structured_outputs` for it. [Current model catalog](https://openrouter.ai/api/v1/models), [Nano 30B endpoint metadata](https://openrouter.ai/api/v1/models/nvidia/nemotron-3-nano-30b-a3b%3Afree/endpoints), [NVIDIA model card](https://huggingface.co/nvidia/NVIDIA-Nemotron-3-Nano-30B-A3B-BF16).

**The `Thank you for watching!` defect is not fixed.** It is orthogonal to model speed: DPR currently treats any non-empty Groq text as available, prefers Groq, and constructs a successful transcript decision before the older validator runs. Silence/source-quality rejection must be restored before formatting or model routing.

## The actual latency target

Voisu's 1,500 ms limit is a Recording-to-Delivery deadline, not a fresh timer granted to the LLM. Current diagnostics showed cloud requests beginning approximately 524–823 ms after stop-of-speech, leaving roughly 677–976 ms for the HTTP request, generation, response parsing, compose, diagnostics, and Delivery.

The live model therefore needs to be substantially faster than 1.5 seconds. The benchmark protocol sets a practical qualification target of warm p50 ≤350 ms and warm p95 ≤600 ms, while scoring every row against its real remaining budget. See the [free-model benchmark protocol](../../internal/scratch/developer-prompt-rendering/free-model-benchmark-protocol-2026-08-12.md).

This distinction matters because the previous Groq `openai/gpt-oss-20b` evaluation had p50 877 ms and p95 2,001 ms, yet only 28.6% semantic match and eight unsafe accepts. It was neither reliably in the residual budget nor correct enough. See the [#140 live benchmark](./developer-prompt-rendering-model-benchmark-2026-08-11.md).

## Fresh OpenRouter evidence

The screen used exact currently zero-price OpenRouter IDs and progressively stronger prompts against formatting, structure, identity, protected-token, and real #139 oracle cases. OpenRouter documents that `:free` variants have no model charge but may have different availability and rate limits, and that `structured_outputs` is the capability for schema-enforced output. [Free variants](https://openrouter.ai/docs/guides/routing/model-variants/free), [structured outputs](https://openrouter.ai/docs/guides/features/structured-outputs), [current model catalog](https://openrouter.ai/api/v1/models).

### Stage 1 — broad five-second screen

| Exact free candidate | Latency / transport | Observed output result |
|---|---:|---|
| `cohere/north-mini-code:free` | timeout, **5,001 ms** | No usable output |
| `google/gemma-4-26b-a4b-it:free` | timeout, **5,001 ms** | No usable output |
| `google/gemma-4-31b-it:free` | HTTP 429, **446 ms** | Availability failure; no candidate |
| `inclusionai/ling-3.0-tiny:free` | **1,330.92 ms** | Semantic/negation failure |
| `liquid/lfm-2.5-2.6b:free` | HTTP 400, **195 ms** | Reasoning mandatory; requested configuration rejected |
| `nvidia/nemotron-3-nano-30b-a3b:free` | **1,798.21 ms** | Good Markdown organization; protected content passed in this screen |
| `nvidia/nemotron-3-super-120b-a12b:free` | timeout, **5,001 ms** | No usable output |
| `nvidia/nemotron-3-ultra-550b-a55b:free` | timeout, **5,002 ms** | No usable output |
| `nvidia/nemotron-3.5-lightning:free` | **1,681.57 ms** | Semantic/negation failure |
| `nvidia/nemotron-nano-9b-v2:free` | timeout, **5,000 ms** | No usable output |
| `openai/gpt-oss-20b:free` | HTTP 400 | Reasoning mandatory; requested configuration rejected |
| `poolside/laguna-s-2.1:free` | **4,719.84 ms** | Invented/changed content; exactness failure |
| `poolside/laguna-xs-2.1:free` | **1,762.36 ms** | Mishandled numbered cues |

Nano 30B was the clear Stage 1 leader on combined latency and visible formatting quality, even though 1,798.21 ms already misses Voisu's full deadline.

### Stage 2 — prompt P1

| Candidate | Probe | Latency | Outcome |
|---|---|---:|---|
| Nano 30B | organize | **721 ms** | Empty/unparseable output |
| Nano 30B | structure | **1,227 ms** | Visually structured, but changed protected exactness through Markdown/narrow no-break space |
| Nano 30B | identity | **784 ms** | Over-formatted and dropped `Please` |
| Lightning | organize | **4,755 ms** | Kept both the abandoned and corrected statement |
| Lightning | structure | **2,058 ms** | Dropped `exactly` |
| Lightning | identity | timeout, **5,001 ms** | No usable output |
| Ling 3 / Laguna XS | — | HTTP 429 | Availability failure; no comparison output |

P1 showed why latency alone is insufficient: Nano 30B entered the nominal whole-deadline range twice, but none of its three probes was contract-correct.

### Nano 30B prompt iterations P4 and P3

| Prompt | Probe | Latency | Outcome |
|---|---|---:|---|
| P4 | organize | **1,012.61 ms** | Incorrectly retained both contradictory abandoned and corrected clauses |
| P4 | structure | **1,340.65 ms** | Identity/no useful formatting |
| P4 | identity | **706.88 ms** | Dropped `Please` |
| P3 | structure | **1,798.76 ms** | Visually good; protected content mostly exact |
| P3 | identity | **820.59 ms** | Changed `Voisu` to `Voiso` and added backticks |

An earlier commentary described the P4 organize output as correct; the preserved output shows it was not. It retained both mutually contradictory clauses and is scored as a failure here.

Strict P3 controls did not produce a faster alternative: Nano 9B timed out at **5,000.59 ms**, and LFM2.5 with low reasoning timed out at **5,000.43 ms**.

### Nano 30B P3 against real oracles

| Fixture | Latency | Outcome |
|---|---:|---|
| CC-07 Structured developer prompt | **2,108.64 ms** | Useful and protected, but not exact approved render |
| CC-24 multi-paragraph layout | **1,126.58 ms** | Identity output; formatting failure |
| CC-17 protected technical input | **1,084.26 ms** | Identity output; exact-oracle failure |

P3 was promising on the narrow Structured case but did not generalize to layout or the exact technical oracle.

### Nano 30B P5

| Fixture/probe | Latency | Outcome |
|---|---:|---|
| CC-07 Structured developer prompt | **2,802.88 ms** | **Exact approved render** |
| CC-24 multi-paragraph layout | **875.74 ms** | Invented `Goal` and `Notes` structure |
| Identity control | **893.65 ms** | Dropped `Please` and invented `Goal`/`Notes` structure |

P5 is the strongest positive result in the entire hosted-free investigation: one exact complex Structured render. It is also the clearest evidence against general rollout. The same prompt overfit Structured formatting and damaged two inputs that did not license those labels.

### Full #139 top-level contract trials

| Trial | Transport/latency | Contract result |
|---|---:|---|
| 1 | HTTP 200 JSON, **3,857.78 ms** | Returned the full required top-level object shape, but only one derivation span; insufficient for complete source-ordered derivation |
| 2 | partial HTTP 200 then timeout, **5,001.48 ms** | Partial/unparseable response; no candidate |

The full contract substantially increased latency and exposed incomplete derivation reliability. A top-level JSON shape is not enough: the #139 compose gate requires complete, source-ordered evidence.

### Earlier strict-schema screen

| Exact free candidate | Fixture | Fresh result | Qualification implication |
|---|---|---:|---|
| `liquid/lfm-2.5-2.6b:free` | CC-07 | hard timeout at 8,000 ms | Far outside the whole deadline |
| `liquid/lfm-2.5-2.6b:free` | CC-24 | hard timeout at 8,000 ms | Far outside the whole deadline |
| `google/gemma-4-26b-a4b-it:free` | CC-07 | HTTP 429 at 950 ms | No candidate; free-route availability failure |
| `google/gemma-4-26b-a4b-it:free` | CC-24 | hard timeout at 8,000 ms | Far outside the whole deadline |
| `nvidia/nemotron-nano-9b-v2:free` | CC-07 | hard timeout at 8,000 ms | Far outside the whole deadline |
| `nvidia/nemotron-nano-9b-v2:free` | CC-24 | hard timeout at 8,000 ms | Far outside the whole deadline |

All three IDs currently advertise `response_format` and `structured_outputs` in OpenRouter's official catalog. Their current endpoint metadata can be inspected directly: [LFM2.5 endpoints](https://openrouter.ai/api/v1/models/liquid/lfm-2.5-2.6b%3Afree/endpoints), [Gemma 4 endpoints](https://openrouter.ai/api/v1/models/google/gemma-4-26b-a4b-it%3Afree/endpoints), [Nemotron Nano endpoints](https://openrouter.ai/api/v1/models/nvidia/nemotron-nano-9b-v2%3Afree/endpoints).

The metadata made them reasonable candidates to test, but it did not predict the observed queueing/availability from Raja's host. OpenRouter explains that routing statistics and provider preferences are dynamic rather than latency guarantees. [Provider routing](https://openrouter.ai/docs/guides/routing/provider-selection).

### Earlier exploratory free-form screen

| Exact free candidate | Fresh result | Correctness/contract result |
|---|---:|---|
| `nvidia/nemotron-3.5-lightning:free` | timeout at 5,000 ms | No output; not viable on latency |
| `inclusionai/ling-3.0-tiny:free` | response in **1,627.67 ms** | `exact_match=false`; omitted the Context heading and mishandled numbered cues |

The current OpenRouter metadata for these models does not advertise `response_format` or `structured_outputs`; it exposes ordinary generation/tool parameters instead. Therefore even Ling's late, incorrect response cannot be promoted into the existing DPR path without weakening the schema contract, which this project must not do. [Lightning endpoints](https://openrouter.ai/api/v1/models/nvidia/nemotron-3.5-lightning%3Afree/endpoints), [Ling endpoints](https://openrouter.ai/api/v1/models/inclusionai/ling-3.0-tiny%3Afree/endpoints), [OpenRouter model capabilities](https://openrouter.ai/docs/guides/overview/models).

### Evidence boundary

These are iterative **one-shot screening results**, not p50/p95 benchmarks. They establish Nano 30B as the best candidate found, but do not establish stable speed, accuracy, or a production ranking. Free-host queueing, cold starts, 429s, endpoint changes, and time of day can move these results. Nano 30B still needs repeated warm/cold, provider-pinned trials, and its current metadata does not advertise strict structured outputs. It cannot be called production-ready.

## Hugging Face hosted inference is not a free production alternative

Hugging Face currently gives free users only **$0.10 of monthly Inference Providers credits**, explicitly subject to change; usage beyond credits is pay-as-you-go. The older `hf-inference` serverless path uses the same credit system. This is enough for a tiny experiment, not a durable zero-cost production service. [Hugging Face Inference Providers pricing](https://huggingface.co/docs/inference-providers/en/pricing).

A fresh unauthenticated official router-catalog snapshot contained no provider route marked `is_free:true`. Hosted Qwen/Gemma routes can be interesting paid or credit-funded experiments, but they do not answer the request for a genuinely free production formatter. [Hugging Face router model catalog](https://router.huggingface.co/v1/models).

Dedicated endpoints are paid and scale-to-zero introduces a cold start; free Spaces sleep or use shared queued hardware. Those properties conflict with a predictable sub-second dictation path. [Inference Endpoint pricing](https://huggingface.co/docs/inference-endpoints/pricing), [scale-to-zero behavior](https://huggingface.co/docs/inference-endpoints/main/en/guides/autoscaling), [Spaces hardware and sleep](https://huggingface.co/docs/hub/spaces-gpus), [ZeroGPU quotas](https://huggingface.co/docs/hub/main/spaces-zerogpu).

## Why a faster model plus a better prompt is not sufficient

The hypothesis is directionally useful, but four independent gates remain:

1. **Residual latency:** the model usually has well under one second, not the full 1.5 seconds.
2. **Structured-output reliability:** Voisu needs the complete `StructuredCandidate`, not a free-form polished string. OpenRouter and Hugging Face both document schema output, but support is model/provider specific. [OpenRouter structured outputs](https://openrouter.ai/docs/guides/features/structured-outputs), [HF structured outputs](https://huggingface.co/docs/inference-providers/en/guides/structured-output).
3. **Semantic usefulness:** the current compose gate can prove a response is source-derived while still accepting unchanged text. Complex Structured and layout fixtures need a separate “identity is not success” assertion.
4. **Upstream transcript safety:** the formatter must never receive or legitimize known silence hallucinations. That decision belongs before DPR source selection.

A stronger prompt should include explicit derivation completeness, closed conversions/labels, protected-token rules, a compact schema example, and an example showing useful Structured formatting. But a prompt cannot guarantee perfection, remove free-tier queueing, or replace deterministic validation.

## Local Ollama contingency

Nano 30B now deserves the next narrowly gated hosted experiment. Local Ollama remains the zero-network contingency if repeated Nano 30B trials confirm that hosted latency cannot fit the residual budget.

Fresh host inspection found a Ryzen 5 5500U with 6 cores/12 threads, AVX2, 14 GiB total RAM, approximately 5.1 GiB available during inspection, integrated AMD graphics, and no installed Ollama runtime. Until host testing proves GPU offload, plan for CPU inference. No model has been installed or benchmarked on this host yet. Local inference removes shared free-tier queueing and network latency, while Ollama can enforce a supplied JSON Schema and report load, prompt-evaluation, and generation timings. [Ollama structured outputs](https://docs.ollama.com/capabilities/structured-outputs), [Ollama chat API](https://docs.ollama.com/api/chat).

### 1. LFM2.5 1.2B Instruct — speed-first

Start with `LiquidAI/lfm2.5-1.2b-instruct:q4_k_m`. The official Ollama package is 731 MB, and Liquid describes the 1.17B model as CPU/on-device oriented with strong owner-reported instruction-following results. Those published figures justify testing; they do not prove Ryzen 5500U latency or Voisu correctness. [Official Ollama package](https://ollama.com/LiquidAI/lfm2.5-1.2b-instruct), [Liquid model card](https://huggingface.co/LiquidAI/LFM2.5-1.2B-Instruct).

Why first: it has the best chance of staying memory-resident and minimizing prompt/decode time. Main risk: a 1.2B model may fail complete derivation, protected tokens, or subtle formatting rules.

### 2. Qwen3 4B Instruct — quality challenger

Then test `qwen3:4b-instruct`, the 2.5 GB Q4_K_M instruct-only Ollama package. Qwen describes the 2507 checkpoint as non-thinking and improved for instruction following; it is Apache-2.0. Use a deliberately small context because DPR inputs do not require the advertised maximum and runtime/KV memory must leave room for Voisu. [Ollama Qwen3 4B](https://ollama.com/library/qwen3:4b-instruct), [Qwen owner model card](https://huggingface.co/Qwen/Qwen3-4B-Instruct-2507).

Why second: it is the most credible quality challenger likely to fit this host. Main risk: prompt prefill and structured JSON decode may still exceed the residual deadline on CPU.

### Required local test

Use Ollama on `localhost:11434`, temperature 0, a supplied JSON Schema, a small context/output cap, and warm residency. Run the exact same #139 parser/compose path and the compact benchmark vectors:

- long Natural dictation;
- CC-07 Structured sections/steps;
- CC-24 multi-paragraph layout;
- CC-17 protected technical tokens;
- simple Natural control;
- silence hallucination boundary;
- malformed/schema fallback;
- unsafe invention fallback.

Measure cold load separately from warm request time. A production candidate needs zero unsafe Deliveries or protected-token mutations, ≥99% schema parse, ≥95% exact useful formatting on core vectors, ≤5% identity on format-required vectors, and warm p95 ≤600 ms with at least 95% of requests inside their real residual budget. Full method: [benchmark protocol](../../internal/scratch/developer-prompt-rendering/free-model-benchmark-protocol-2026-08-12.md).

If LFM 1.2B is fast but inaccurate and Qwen 4B is accurate but late, do not force either into production. The next design move should be a smaller response contract and/or stronger deterministic local formatting, not a weaker safety gate.

## `Thank you for watching!`: current status and fix boundary

This was **diagnosed, not fixed**. The current implementation still:

- filters only empty source strings;
- prefers a non-empty Groq source;
- constructs a `TranscriptDecision` directly when DPR context exists;
- calls the older quality validator only when DPR context does not exist.

Evidence is in [`dpr_source_context`](../../crates/voisu-app/src/dpr_pipeline.rs) and the daemon's [DPR decision branch](../../crates/voisu-app/src/bin/voisu-daemon.rs). No model swap or system prompt can fix that control-flow bypass.

The required product fix remains:

1. reuse/extract deterministic source-quality classification before `dpr_source_context`;
2. reject known hallucinated suffixes and wordless/unsafe sources before routing;
3. add the exact regression: Deepgram empty + Groq `Thank you for watching!` + DPR enabled ⇒ zero cloud calls and zero Delivery;
4. only benchmark formatting after that boundary is closed.

## Recommendation

Do not rotate more free hosted models directly into the production path. Nano 30B is the best candidate found, but the evidence supports only one further **Structured-policy-only** experiment behind the existing local baseline and compose gate. “Structured-only” here describes the Voisu policy/input class; it does not imply provider-enforced JSON Schema, which OpenRouter does not currently advertise for this route.

Run the next work in this order:

1. Freeze Nano 30B P5 and test only Structured CC-07-class inputs with full #139 parsing/compose, no free-form authority, and local fallback on every late/incomplete result.
2. Repeat enough warm/cold trials to measure p50/p95 and determine whether prompt/response-contract reduction can approach the actual residual budget without losing complete derivation.
3. Keep Natural, Adaptive layout, and identity inputs off this cloud route; P5 damaged both CC-24 and the identity control.
4. If Nano 30B remains too slow, benchmark local `LiquidAI/lfm2.5-1.2b-instruct:q4_k_m`, then `qwen3:4b-instruct`.
5. Fix the upstream silence hallucination regression independently before any rollout decision.

Until both correctness and warm residual-budget latency pass repeated trials, local baseline formatting remains the only safe Delivery authority.
