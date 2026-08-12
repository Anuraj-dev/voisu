# Developer Prompt Rendering — specification

**Issue:** [#144](https://github.com/Anuraj-dev/voisu/issues/144) · parent map [#133](https://github.com/Anuraj-dev/voisu/issues/133)  
**Status:** **approved planning baseline** (2026-08-12). Blueprint for #145 and product PRs — does **not** change product code by itself.  
**Language:** English v1 only  
**Date:** 2026-08-12  

This document is the single product rulebook for **Developer Prompt Rendering (DPR)**. Implementation follows this file and the research packages it names. If something is not here and not in those packages, it is not required for v1.

---

## 1. What this feature is

Voisu turns **English speech** into **clean text** you can paste into coding agents, chats, editors, and everyday messages.

It **organizes** what you said. It does **not** rewrite your meaning, fix grammar, or invent requirements.

**Default policy is Adaptive.** You can also pick Natural or Structured. The final text is delivered **once**, after you stop speaking, within **1.5 seconds** of utterance end (or sooner). Delivery never auto-sends and never silently replaces text that already went out.

Today’s shipped Smart Writing path stays until an approved DPR implementation **replaces** it under a later rollout plan (#145). This spec is that replacement’s blueprint.

---

## 2. What it never does (v1)

| Forbidden | Why |
|---|---|
| Grammar “fixing” or style rewrite | Out of product scope for DPR |
| Inventing goals, requirements, steps, or tech assumptions | Must stay source-faithful |
| Command / rewrite mode with special introducers | Not in v1 |
| Non-English rendering / translation | English-first only |
| Reading clipboard, page DOM, screenshots, or chat history to rewrite you | Privacy + scope |
| Auto-send into the target app | User always owns Submit |
| Live typing while you are still speaking | Final-only Delivery |
| Replacing text already delivered when a late cloud result arrives | No second write |
| Free-form “polished string” from the model as sole authority | Must prove source derivation |

---

## 3. Plain terms

Use these names in code, tests, and diagnostics:

| Term | Meaning |
|---|---|
| **Source Transcript** | What one STT provider heard |
| **Selected source** | The source text chosen for this utterance (one provider; dual-STT may agree or disagree) |
| **Local baseline** | Deterministic organize result computed on-device from the selected source. Always available. |
| **Combined-call candidate** | Untrusted JSON from at most one cloud organize call |
| **Final Transcript** | Text handed to Delivery after local and optional cloud paths finish their gates |
| **Policy** | `natural`, `adaptive`, or `structured` — how aggressively to organize layout/labels |
| **Route** | How heavy this utterance’s path is: identity, local-only, or local + optional cloud |
| **Delivery** | Putting the Final Transcript into the focused app (type and/or clipboard), **unsent** |

---

## 4. Policies (user-facing)

Persisted CLI-selectable; default **Adaptive**.

| Policy | Behavior in one sentence |
|---|---|
| **Natural** | Clean punctuation and light layout only. **No cloud.** No structural section headers. |
| **Adaptive** (default) | Local organize always. Cloud **may** run for disputed or complex speech if it finishes in time and passes safety. Layout can be natural, multi-paragraph, or numbered when source supports it. |
| **Structured** | Same safety as Adaptive, but prefers closed section labels (`Goal`, `Steps`, …) when the speech clearly asks for structure. Cloud is **required to attempt** on complex structured speech; if cloud fails or is late, still deliver a safe local baseline (never block Delivery). |

Closed Structured labels (only these):

`Goal`, `Context`, `Requirements`, `Constraints`, `Steps`, `Acceptance Criteria`, `Files`, `Notes`

Labels appear only when the source speech supports them. The model (or local path) must not invent a section the user did not speak toward.

---

## 5. End-to-end flow

```text
You stop speaking
        │
        ▼
STT Source Transcript(s)  (one or two providers)
        │
        ▼
Pick selected source (+ note agreement / dispute)
        │
        ├──────────────────────────────┐
        ▼                              ▼
Local baseline (always)         Intent route (#141)
        │                              │
        │                     maybe start one cloud call
        │                              │
        │                     wait only until deadline
        │                              ▼
        │                     validate candidate (#139)
        │                              │
        └──────── compose / fallback ──┘
                       │
                       ▼
              Final Transcript
                       │
                       ▼
         Delivery once (unsent flags)
```

**Clock:** `t0 = utterance_end`.  
**Hard Delivery budget:** **1500 ms** from `t0` to start handing text to Delivery.  
Cloud work that is not **accepted** before that budget **does not delay** the user: deliver the local baseline.

---

## 6. Local baseline

The local baseline is the **safe default** for every utterance.

It may:

- keep spoken words in order  
- apply light punctuation / capitalization  
- apply **closed** spoken cues (e.g. “exclamation point” → `!`, “new line” → newline)  
- apply layout the local rules can prove from speech  

It must **not**:

- invent words or sections  
- remove words unless a clear, local rule allows (e.g. clear filler when implemented)  
- depend on the network  

Research oracles live in the **#138 behavior corpus**. Product tests promote those fixtures; they do not invent a second expected-output language.

---

## 7. When cloud runs

Routing is **local and cheap** (no network on the decision itself). Rules are ordered; first match wins. Full table and weights: intent-routing package (#141).

Short version:

| Situation | Route | Cloud |
|---|---|---|
| Natural policy | Local only | Never |
| Providers disagree on protected or semantic content (Adaptive/Structured) | Local + optional cloud | May attempt |
| Speech already looks preformatted (lists etc.) | Identity / local | Usually none |
| Shell/command-shaped on a shell surface | Identity | None |
| Complex speech under Adaptive | Local + optional cloud | Allowed |
| Complex speech under Structured | Local + optional cloud | Attempt required |
| Simple agreement | Local | Skip |

**One structured cloud call maximum** per utterance. No second “polish” call. No grammar call.

If cloud is skipped or fails, Delivery still happens with the local baseline.

---

## 8. Cloud response and safety

The cloud does **not** return a free-form final string as authority.

It returns a **structured candidate** (JSON) with:

- which STT source it used  
- clear filler / clear backtrack removals  
- closed conversions  
- layout decision  
- optional closed labels  
- ordered **derivation** spans that rebuild the proposal when concatenated  

Product then:

1. Parses and schema-checks the candidate  
2. Checks freshness (`base_fingerprint` matches selected source)  
3. Proves every span against real source text (order + completeness)  
4. Checks protected tokens (names, negations, URLs, flags, code, …)  
5. Rejects invented content and illegal labels  
6. **Composes** an accept, a soft salvage, or a hard fallback to baseline  

Authoritative gate research: combined-call package **#139** (v1.1.2 completeness + source-order). Product ports that gate; it does not weaken it.

### Fallback (simple)

| Outcome | Final text |
|---|---|
| Cloud skipped / not attempted | Local baseline |
| Timeout, provider error, bad schema | Local baseline |
| Unverifiable / unsafe / bad label / protected hit | Local baseline |
| Uncertain backtrack only | Soft: local baseline Natural, **preserve all words** (`accept_preserve_words`) |
| Uncertain layout only | Soft: Natural layout only (`accept_natural_layout`) |
| All checks pass in time | Composed cloud render |

Hard failures never “half apply” a bad rewrite.

---

## 9. Model policy (latency first)

Research live matrix: model benchmark **#140** (PR #152).  
On that matrix the recommendation was **`three_way_no_production_ready_default`** (no sole production-ready cloud default). Local baseline remains Delivery authority whenever cloud is late or rejected.

| Model | Role in v1 |
|---|---|
| **Groq `openai/gpt-oss-20b`** (`reasoning_effort: low`) | **Preferred in-budget cloud candidate** — best chance of finishing under 1.5s on measured host runs |
| **Gemini 3.5 Flash-Lite** | **Kept** — strong structured parse on the matrix; on measured runs p50 was **above** 1.5s, so it must not hold Delivery |
| **Gemini 3.6 Flash** | Available for eval; too slow on measured runs for the final-only gate |

**Rules:**

1. Delivery always has a local baseline ready; **never wait past 1.5s** for any cloud model.  
2. Prefer Groq when a cloud attempt is scheduled and credentials/capability are ready.  
3. Gemini is **not ditched**. Use it when wall time is proven in-budget, for evaluation/offline compare, or as an explicit config alternative — never as a reason to slip Delivery.  
4. If Gemini quality is wanted but latency stays high: everyday Adaptive stays local-first; Structured may attempt cloud only when the result is ready before the deadline. **No late upgrade** of already delivered text.  
5. No provider’s raw string ships without the #139 compose accept path.

Credentials: existing secret-store patterns (`voisu-provider` keys). Never log API keys or put them in research assets.

---

## 10. Delivery

Same product promises as the rest of Voisu, restated for DPR:

| Flag | Value |
|---|---|
| Delivery state | `unsent` |
| `auto_send` | `false` |
| `live_type` | `false` |
| `replace_delivered` | `false` |

- Final result only after speech ends (not progressive rewrite of the field).  
- Clipboard preserve + type path remain as today; honest fallback if type is unavailable.  
- Host multiline insert is already proven in daily use (#143 closed). Spec does not re-open “does Delivery work?”  
- Residual caution for #145 rollout: some chat UIs treat newline as send — Structured multi-line into those surfaces should be tested at ship time for the apps you care about, not re-litigated as a map research ticket.

---

## 11. Feedback and diagnostics

Full contract: diagnostics package **#142**.

**User feedback**

- Default: **silent**. The text appearing is the feedback.  
- If cloud was attempted and hard-fell back to baseline: optional one non-blocking status: **`Local formatting used`**.  
- Never claim “cloud enhanced your text” when baseline was delivered.  
- Never spam, never block the UI, never dump provider errors to the user.

**Diagnostics (local)**

- Ordered event timeline from `utterance_end` (`route_selected`, cloud start/end, deadline, accept/fallback, `delivery_emitted`, …).  
- Bounded, redacted (no secrets, no raw HTTP bodies, no audio).  
- Modes: `evaluation` vs `production`.  
  - Evaluation may keep one **late** valid candidate for offline compare.  
  - Production records that something late arrived (timing only) and **never** delivers it as a second write.

Evaluation-only fields must be compile-gated or removed before calling DPR “production complete.”

---

## 12. CLI and config

Mirror existing small CLI patterns (`voisu delivery`, etc.):

```text
voisu rendering                 # show current policy
voisu rendering adaptive        # default
voisu rendering natural
voisu rendering structured
```

Config (same file family as today):

```toml
rendering_policy = "adaptive"   # natural | adaptive | structured
```

- Snapshot policy at Recording start; do not flip mid-utterance.  
- Unknown / corrupt value: fail closed to **Natural** (safest local-only) and log a bounded diagnostic — or follow the same fail-closed style already used for other config keys if product standardizes that. Fresh install default remains **Adaptive**.  
- Daemon restart rules match existing config apply behavior; success text must say when restart is needed.

Exact flag names may be bikeshed in implementation tickets as long as semantics match this section.

---

## 13. Relationship to Smart Writing

| Topic | Rule |
|---|---|
| Shipped Smart Writing | Remains until DPR implementation is approved and rolled out |
| Grammar subsystem | **Not** part of DPR; do not port Minimal Grammar into DPR |
| Smart / Literal modes | **Not** DPR’s modes; DPR uses Natural / Adaptive / Structured |
| Behavior corpora | Smart Writing behavior corpus is **superseded** for DPR planning by the #138 DPR corpus |
| Shared infrastructure | STT, reconciliation, Delivery, secret store, portals — reuse |

---

## 14. Normative research sources

These packages are **accepted inputs**. Implementation must not contradict them without a new approved revision.

| Package | Role |
|---|---|
| [#138](https://github.com/Anuraj-dev/voisu/issues/138) behavior corpus + schema | What correct finals look like |
| [#139](https://github.com/Anuraj-dev/voisu/issues/139) combined-call contract + prototype (**v1.1.2** completeness + source-order) | Cloud JSON + compose / fallback gate |
| [#140](https://github.com/Anuraj-dev/voisu/issues/140) model benchmark | Latency / quality evidence; model policy above |
| [#141](https://github.com/Anuraj-dev/voisu/issues/141) intent routing | Local route + cloud allow/require |
| [#142](https://github.com/Anuraj-dev/voisu/issues/142) diagnostics | Feedback + event timeline + eval vs prod |
| [#137](https://github.com/Anuraj-dev/voisu/issues/137) product contract | Original product lock answers |
| [#143](https://github.com/Anuraj-dev/voisu/issues/143) | Closed — Delivery already proven; no extra matrix required for planning |

File stems (2026-08-11 research set):

- `docs/research/developer-prompt-rendering-behavior-*`  
- `docs/research/developer-prompt-rendering-combined-call-*`  
- `docs/research/developer-prompt-rendering-model-benchmark-*`  
- `docs/research/developer-prompt-rendering-intent-routing-*`  
- `docs/research/developer-prompt-rendering-diagnostics-*`  

**Precedence:** this specification owns product locks and ship gates. Corpora own exact oracle strings and schemas. If a bug is found in research, fix research with a dated revision and update this document’s pointer — do not silently weaken gates in product.

---

## 15. Ship gates (before calling DPR “done”)

Minimum bar:

1. **Corpus promotion:** #138 fixtures run as product (or shared) tests for Final Transcript behavior on the local path; #139 compose mutations remain green in ported form.  
2. **Deadline:** no path blocks Delivery past 1500 ms waiting on cloud.  
3. **Safety:** no accept path without compose gates; protected tokens and no invented content.  
4. **Delivery flags:** unsent; no auto-send; no live-type; no replace-delivered; no production late upgrade.  
5. **Diagnostics:** production mode has no eval-only late-copy lane.  
6. **Config/CLI:** three policies persist and snapshot per recording.  
7. **Host smoke:** real install on Fedora KDE can dictate → Final Transcript → insert for Natural and Adaptive; Structured checked on the apps you care about at rollout.  
8. **Rollout:** Smart Writing remains available until DPR path is explicitly enabled/switched by approved release plan.

Residuals allowed into #145 as explicit tasks (not silent “approved safe”):

- #138 checker edge cases (e.g. some multi-word deletion provenance)  
- #141 residual false-literal edges  
- #140: Groq can still accept under oracle miss → product must keep gates tight; optional prompt hardening  
- Gemini in-budget only if measured later  

---

## 16. What #145 should plan (not implement here)

#145 turns this blueprint into an **ordered execution DAG** of implementation tickets/PRs. Expected shape (guide only):

1. Domain types + policy CLI/config (no behavior change yet)  
2. Local baseline organizer + #138 tests  
3. Intent router (#141)  
4. Combined-call schema + compose gate (#139 port)  
5. Cloud client (Groq first; Gemini optional/latency-gated) with credential prep outside the 1.5s gate  
6. Wire Recording → route → baseline → optional cloud → compose → Delivery  
7. Diagnostics + feedback (#142 production surface)  
8. Integration tests + host smoke + rollout switch from Smart Writing  

#145 may split or reorder for dependency safety; it may not drop the safety or Delivery locks above.

---

## 17. Approval

| Role | Action |
|---|---|
| This document | Product blueprint for DPR |
| Dual independent review | Grok residual-land + GPT-5.6 Sol high (Raja-authorized dual ballot) |
| #145 | May produce the execution DAG against this baseline |
| Product code | Only under #145’s approved DAG (or explicit follow-on tickets) |

**Ballot (closed 2026-08-12):**

- [x] Independent residual-land review (Grok): **APPROVE_WITH_NITS** (P2 polish only; no product-lock changes)  
- [x] Independent Sol high (`gpt-5.6-sol`): **APPROVE** — findings none; `BALLOT: APPROVE`  
- [x] Grok orchestrator concurs with Sol  
- [x] §§1–12 product locks accepted as planning baseline  
- [x] Model policy §9 accepted (Groq in-budget preferred; Gemini retained, latency-gated)  
- [x] Ship gates §15 accepted  

**Effect:** this specification is the **approved planning baseline** for map #133. It authorizes **#145** (execution DAG) and later implementation PRs that follow that DAG. It does **not** by itself change shipped product behavior.

---

## 18. One-page summary

1. Organize English speech; preserve meaning and spoken wording.  
2. Adaptive default; Natural and Structured available.  
3. Local baseline always; at most one structured cloud call.  
4. Cloud must prove its work from the source; otherwise baseline.  
5. Prefer Groq when cloud must be fast; keep Gemini but never past the 1.5s Delivery line.  
6. Deliver once, unsent; no auto-send; no late replace.  
7. Quiet UX; small diagnostics; research corpora are the tests.  
8. Build only after #145 orders the work.

That is Developer Prompt Rendering for v1.
