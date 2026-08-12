# Developer Prompt Rendering — execution DAG

**Issue:** [#145](https://github.com/Anuraj-dev/voisu/issues/145) · parent map [#133](https://github.com/Anuraj-dev/voisu/issues/133)  
**Planning baseline:** [#144](https://github.com/Anuraj-dev/voisu/issues/144) — `developer-prompt-rendering-specification-2026-08-12.md` + constants JSON  
**Status:** **approved** planning DAG (Sol high residual-land: REQUEST_CHANGES → fix → **APPROVE**, findings none, 2026-08-12). Planning only — no product rewrite by this document.  
**Date:** 2026-08-12  

This document is the **ordered implementation plan** for Developer Prompt Rendering (DPR). It turns the approved #144 specification into dependency-ordered tickets/PRs with acceptance criteria. It does **not** change shipped Smart Writing by itself.

---

## 1. Purpose and scope

| In scope for this DAG | Out of scope |
|---|---|
| Ordered product tickets after #145 is accepted | Silent product rewrite of Smart Writing |
| Explicit residual tasks from #138–#142 | Re-opening #144 product locks |
| Review, CI, and host gates per step | Grammar subsystem / Minimal Grammar port |
| Rollout switch that keeps Smart Writing until flip | Non-English rendering, command/rewrite mode, auto-send |

**Done for map #133 planning:** #138–#142 research closed; #143 Delivery proven closed; #144 dual ballot closed (Sol + Grok APPROVE, 2026-08-12); this file lands #145.

**Done for product DPR:** every ticket below is green **and** rollout is explicitly enabled under `DPR-T8`. Until then, `final_transform_and_deliver` / Smart Writing remains the production path.

---

## 2. Non-negotiable locks (do not weaken in any ticket)

Copied from the approved #144 baseline and constants. Any PR that softens these is out of order and must be rejected.

| Lock | Value |
|---|---|
| Language / mission | English v1; **organize-only** (no grammar rewrite subsystem) |
| Policies | `natural` \| `adaptive` (default) \| `structured` |
| Local baseline | **Always** computed; Delivery authority whenever cloud is late/rejected/skipped |
| Cloud | **≤1** structured call per utterance; never free-form string authority |
| Accept path | **#139 compose** (v1.1.2 completeness + source-order) is the **sole** accept path for model text |
| Delivery deadline | **≤1500 ms** from `utterance_end` to start of Delivery handoff |
| Delivery flags | `unsent`; `auto_send=false`; `live_type=false`; `replace_delivered=false` |
| Late cloud | **No production late replace / upgrade** of already delivered text |
| Preferred in-budget model | Groq `openai/gpt-oss-20b` (`reasoning_effort: low`) |
| Gemini | **Retained**, **latency-gated** — must not hold Delivery past 1.5s |
| #140 discipline | No sole production-ready cloud default; gates stay tight even when model “accepts” under oracle miss |
| Smart Writing | **Stays shipped** until explicit DPR rollout (`DPR-T8`) |
| Secrets | Never commit `.env` / keys; `secret-tool` / existing `voisu-provider` patterns only |
| Dirty local work | Preserve uncommitted `crates/voisu-app/src/bin/voisu.rs` CLI text; **do not mix** into DPR PRs |

Normative research oracles (cite; do not re-derive product locks):

| Package | Role in product |
|---|---|
| #138 behavior | Local Final Transcript oracles + fixture promotion |
| #139 combined-call v1.1.2 | Schema + compose / hierarchical fallback |
| #140 model benchmark | Latency/quality evidence; model policy only |
| #141 intent routing | Pure-local route + `cloud_request` |
| #142 diagnostics | Feedback + event timeline + eval vs production cleanup |

---

## 3. Dependency graph

```text
                    ┌─────────────────────┐
                    │  #144 SPEC (done)   │
                    └──────────┬──────────┘
                               │
                    ┌──────────▼──────────┐
                    │  #145 THIS DAG      │
                    └──────────┬──────────┘
                               │
              ┌────────────────▼────────────────┐
              │ DPR-T0  Types + rendering_policy │
              │         CLI/config (no behavior) │
              └────┬───────────────┬─────────────┘
                   │               │
         ┌─────────▼─────┐   ┌─────▼──────────────┐
         │ DPR-T1 Local  │   │ DPR-T2 Intent      │
         │ baseline+#138 │   │ router (#141)      │
         │ + residual R1 │   │ + residual R2      │
         └───────┬───────┘   └──────────┬─────────┘
                 │                      │
                 │    ┌─────────────────┘
                 │    │
         ┌───────▼────▼───────┐
         │ DPR-T3 Compose     │  (#139 port; sole model accept path)
         │ gate + schema      │
         └──────────┬─────────┘
                    │
         ┌──────────▼─────────┐
         │ DPR-T4 Cloud client│  Groq first; Gemini optional/latency-gated
         │ + residual R3      │  (#140 gate discipline)
         └──────────┬─────────┘
                    │
         ┌──────────▼─────────┐
         │ DPR-T5 Wire        │  Recording → route → baseline →
         │ pipeline (flagged) │  optional cloud → compose → Delivery
         └────┬──────────┬────┘
              │          │
    ┌─────────▼──┐  ┌────▼──────────────┐
    │ DPR-T6     │  │ DPR-T7 Integration│
    │ Diagnostics│  │ tests (hermetic)  │
    │ prod (#142)│  └────────┬──────────┘
    │ + residual │           │
    │ R4         │           │
    └─────┬──────┘           │
          └────────┬─────────┘
                   │
         ┌─────────▼─────────┐
         │ DPR-T8 Host smoke │  Fedora KDE real install +
         │ + rollout switch  │  explicit Smart Writing → DPR flip
         └───────────────────┘
```

**Parallelism after T0:**

- `DPR-T1` ∥ `DPR-T2` (non-overlapping modules once types land).
- `DPR-T6` may start after T5 exposes event hooks; prefer completing T3 first so failure codes match #139.
- `DPR-T7` may **start** after T5 (pipeline hooks exist) but **cannot complete green** until T6 production cleanup is merged — T7 asserts the diagnostics ship gate.
- Host smoke (`DPR-T8`) is driver-owned and last.

**Hard sequence:** T0 → (T1 ∥ T2) → T3 → T4 → T5 → T6 → T7 → T8  
(T7 work may draft in parallel with late T6, but T7 merge-ready requires T6 done.)

---

## 4. Suggested module seams (guidance, not rigid paths)

| Concern | Prefer | Notes |
|---|---|---|
| Domain types, routes, policies, closed labels | `voisu-core` | Pure, testable, no HTTP |
| Local baseline organizer | `voisu-core` (new DPR module; **not** grammar_safety) | May reuse closed cue / layout helpers from `formatting` only where semantics match #138 |
| Intent router | `voisu-core` | Port #141 prototype; no network |
| Compose / schema / protected tokens | `voisu-core` | Port #139; pure function of baseline + candidate |
| Cloud HTTP clients | `voisu-app` | Secrets, timeouts, provider adapters |
| Pipeline orchestration | `voisu-app` | Parallel to `smart_writing::final_transform_and_deliver` behind flag |
| CLI / config | `voisu-app` `config.rs` + `bin/voisu.rs` | Mirror `voisu delivery` / `voisu writing`; **separate commits** from dirty CLI-text WIP |
| Diagnostics | `voisu-core` types + `voisu-app` emission | #142 production surface |

Do **not** port Minimal Grammar, grammar HTTP, or Smart Writing edit-safety catalogs into DPR.

---

## 5. Tickets (dependency order)

Ticket IDs below are **plan IDs**. On #145 acceptance, file GitHub issues (or stack PRs) with these titles and acceptance blocks. One feature worktree → one PR; do not fold large multi-commit slices.

### DPR-T0 — Domain types + `rendering_policy` CLI/config (no behavior change)

**Depends on:** #145 accepted  
**Goal:** Land shared vocabulary and persisted policy **without** changing Final Transcript behavior.

**Work:**

1. Types: `RenderingPolicy { Natural, Adaptive, Structured }`, route enums aligned with #141 (`literal_identity` \| `deterministic_local` \| `local_with_optional_cloud`), `cloud_request` states, closed Structured labels constant list, Delivery flag constants, `DELIVERY_DEADLINE_MS = 1500`.
2. Config key `rendering_policy` (constants JSON); default **Adaptive** on missing key / fresh install.
3. Fail-closed: unknown/corrupt value → **Natural** (local-only safest) + bounded diagnostic (spec §12). Mirror existing hand-parsed TOML style in `config.rs`.
4. CLI: `voisu rendering` / `voisu rendering {natural|adaptive|structured}` — show and set; success text must say when daemon restart is needed (match `voisu writing` / `voisu delivery` patterns).
5. Snapshot rule documented and unit-tested: policy snapshotted at Recording start; mid-utterance config flip must not affect in-flight work (implementation may wire snapshot in T5; pure resolution tests in T0).

**Acceptance:**

- [ ] Unit tests: default Adaptive; unknown → Natural; round-trip persist.
- [ ] CLI help/usage includes `rendering` without breaking existing verbs.
- [ ] **No** change to `final_transform_and_deliver` outcomes; Smart Writing path still default.
- [ ] **Dirty `voisu.rs` operational guard:** implement T0 from a **dedicated clean worktree** branched from a recorded `main` (or release) head that does **not** carry the local CLI-wording WIP. Before commit, verify the staged diff for `crates/voisu-app/src/bin/voisu.rs` contains **only** the `rendering` verb changes required by this ticket — no pre-existing usage-text simplification. Unrelated dirty work stays on the developer’s main checkout and is never mixed into the DPR PR.
- [ ] Workspace tests + clippy green; independent first-pass + authoritative **Sol** APPROVE (batchable with other early heads per §7).

**Out of scope:** router, baseline, cloud, Delivery path switch.

---

### DPR-T1 — Local baseline organizer + #138 corpus tests (+ residual R1)

**Depends on:** T0  
**Goal:** Deterministic on-device organize that always produces a Delivery-ready baseline.

**Work:**

1. Implement local baseline for Natural-shaped organize: word order preserved; light punctuation/casing; **closed** spoken cues; layout only when local rules prove it from speech.
2. Promote #138 fixtures as product (or shared) tests for **local path** Final Transcript expectations. Prefer loading research JSON or a checked-in promotion subset — do not invent a second oracle language.
3. **Residual R1 (explicit, not silent):**
   - Multi-word deletion provenance: any local remove must be rule-justified and test-covered; if provenance is incomplete in research checker, product must **fail closed** (keep words) rather than invent deletion authority.
   - TypeError / checker fragility in research Python is research debt — product Rust must use typed APIs so those classes of holes do not reappear as panics.
4. Natural policy baseline must never emit Structured section headers that speech does not support.
5. No network, no model, no grammar catalog.

**Acceptance:**

- [ ] #138 local-route fixtures that define expected finals for local baseline: product tests green. **Intermediate PRs** may track a short deferred-ID list with rationale; **ship bar (T7/T8)** requires full promotion of every applicable #138 local-path fixture unless an **approved dated baseline revision** explicitly removes a case.
- [ ] Property tests or corpus mutations where practical: invented words rejected; protected-token substrings preserved when present in source.
- [ ] R1: explicit tests for multi-word clear-filler / clear-backtrack if implemented; if not implemented yet, baseline **preserves all words** (safer).
- [ ] Benchmark/latency: local path leaves headroom under 1500 ms on CI hosts (no sleep-based flakiness).
- [ ] Smart Writing still default path in daemon.
- [ ] Independent first-pass + Sol APPROVE (batchable).

**Out of scope:** cloud, compose accept of model text, daemon wire.

---

### DPR-T2 — Intent router (#141 port + residual R2)

**Depends on:** T0  
**Parallel with:** T1  
**Goal:** Pure-local routing decision with ordered rules and complexity weights from the #141 package.

**Work:**

1. Port `route` + `cloud_request` + `rule_id` + `complexity_score` + `contributions` from the intent-routing prototype — **do not re-derive weights from scratch**.
2. Ordered rules first-match-wins (dispute → preformatted → shell/command → complexity/policy table). Natural never allows cloud.
3. Surface/process hints optional; speech-only path mandatory when hints absent.
4. **Residual R2 (false-literal edges):** shell/terminal prose must **not** take `literal_identity` unless command-shaped. Port adversarial fixtures (e.g. “make sure…”, “go ahead…”, “run this by…”, “make dinner…”) as product tests. Prefer local organize over literal for prose-in-shell.
5. Emit diagnostic-friendly fields (for T6) without secrets.

**Acceptance:**

- [ ] #141 corpus fixtures promoted (or representative subset with full rule-id coverage) → product tests green. T7 ship bar requires full applicable #141 promotion unless a dated baseline revision removes cases.
- [ ] R2 adversarial shell-prose fixtures assert `deterministic_local` + `cloud_request=not_allowed` (not literal).
- [ ] Natural + dual-STT protected disagreement → local, no cloud (N3).
- [ ] Structured complex → `local_with_optional_cloud` + `cloud_request=required` (attempt policy; runtime still falls back if late).
- [ ] No I/O / no timers in the pure router function.
- [ ] Independent first-pass + Sol APPROVE (batchable).

**Out of scope:** starting HTTP; composing candidates.

---

### DPR-T3 — Combined-call schema + #139 compose gate (sole accept path)

**Depends on:** T0; strongly prefer T1 baseline type available for fallback composition  
**Goal:** Port the #139 v1.1.2 validator + hierarchical compose so **no** model string can ship without gates.

**Work:**

1. Rust types + parse for structured candidate JSON (`schema_version`, `base_fingerprint`, reconciliation, removals, conversions, layout, labels, ordered `derivation`).
2. Gates (authoritative research: combined-call contract + prototype):
   - schema / catalog closed-ness  
   - freshness (`base_fingerprint` matches selected source)  
   - derivation completeness + **source order** (v1.1.2)  
   - protected tokens  
   - invented-content rejection  
   - illegal labels rejected  
3. Hierarchical compose outcomes: `accept` \| soft salvage (`accept_preserve_words`, `accept_natural_layout`) \| hard `fallback_baseline`.
4. Soft paths never half-apply unsafe rewrites.
5. Promote #139 corpus + property mutations as product tests (decision + rendered where applicable).

**Acceptance:**

- [ ] Offline #139 corpus replay: decisions match prototype expectations. **Intermediate PRs** may track a short parity gap list without weakening gates; **T7/T8 ship bar** requires full #139 mutation + decision parity for the normative corpus unless an **approved dated baseline revision** removes a case.
- [ ] Mutation-style tests: missing derivation span → reject; reordered source → reject; protected token altered → reject; illegal label → reject; uncertain backtrack → preserve words.
- [ ] **Invariant:** there is **no** public API that accepts raw model prose as Final Transcript.
- [ ] No network in this ticket.
- [ ] Independent first-pass + authoritative **Sol** APPROVE (non-skippable priority review).

**Out of scope:** live providers; daemon wire.

---

### DPR-T4 — Cloud client (Groq first; Gemini latency-gated) + residual R3

**Depends on:** T3 (compose is the only accept path)  
**Goal:** At most one structured cloud attempt with deadline-aware cancellation; credentials outside the 1.5s critical path where possible.

**Work:**

1. Groq adapter for `openai/gpt-oss-20b` with `reasoning_effort: low`; compact prompts from #139/#140 research (organize-only contract).
2. Gemini adapters optional behind config / eval: `gemini-3.5-flash-lite`, `gemini-3.6-flash` — **latency-gated**; never default sole production path.
3. Wall-clock budget: caller supplies remaining ms until Delivery deadline; client must not extend past it. On timeout → no candidate (baseline wins).
4. Credential resolution uses existing secret store (`voisu-provider` / `secret-tool`); failures → skip cloud, baseline.
5. **Residual R3 (#140 gate discipline):**
   - Product must **not** treat any model as sole production-ready default.
   - Groq can still “compose-accept” under oracle miss on research matrix — product keeps #139 gates tight; optional prompt hardening only if fixtures prove need (do not loosen completeness/order).
   - Gemini quality may be used offline/eval or when measured in-budget later; everyday Adaptive remains local-first when cloud is late.
6. Rate-limit / HTTP errors → baseline path (no retry storms inside the 1.5s gate).
7. Never log API keys, raw HTTP bodies, or audio.

**Acceptance:**

- [ ] Unit/integration with mocked HTTP: happy JSON → candidate to compose; 4xx/5xx/timeout → `None` + error class for diagnostics.
- [ ] Deadline test: slow mock never blocks past supplied budget; no delivered late upgrade API.
- [ ] ≤1 call enforced per attempt helper; **orchestration-level count is asserted in T5/T7** (including retries and provider fallback — one utterance never issues a second cloud call after a first attempt starts or fails).
- [ ] Document preferred model + Gemini roles matching constants JSON.
- [ ] Live host optional smoke (driver): Groq key via secret-tool; not required for merge if mocks cover contracts — but live smoke required before T8 rollout.
- [ ] Independent first-pass + authoritative **Sol** APPROVE (material safety: cloud/deadline).

**Out of scope:** replacing Smart Writing in the daemon.

---

### DPR-T5 — Wire pipeline (flagged): Recording → route → baseline → optional cloud → compose → Delivery

**Depends on:** T1, T2, T3, T4  
**Goal:** End-to-end DPR path **behind an explicit enablement flag** defaulting **off** so Smart Writing remains production.

**Work:**

1. New orchestration entry (name bikeshed OK) parallel to `final_transform_and_deliver`, e.g. `dpr_transform_and_deliver`, selected only when rollout flag / config says DPR is active **and** English organize path applies.
2. Clock: `t0 = utterance_end`; start Delivery handoff by **1500 ms** with best safe Final Transcript (baseline if cloud not accepted).
3. Flow:
   - snapshot `rendering_policy`  
   - select source (+ agreement/dispute class already available from dual-STT)  
   - compute local baseline (always)  
   - intent route (cheap, local)  
   - if cloud allowed/required **and** credentials ready **and** remaining budget: start ≤1 structured call  
   - wait only until deadline; compose if candidate ready  
   - Final Transcript → existing Delivery (unsent flags unchanged)  
4. Natural: never start cloud. Structured required-attempt: still deliver baseline if cloud fails/late.
5. Cancel or ignore late cloud for **production delivery** (T6 records timing-only discard).
6. Do not auto-send; do not live-type; do not replace_delivered.
7. Default config: DPR path **disabled** → Smart Writing unchanged.

**Acceptance:**

- [ ] Hermetic tests: Natural simple → baseline, zero HTTP. Adaptive complex with fast mock accept → composed text. Slow mock → baseline by 1500 ms (**injected/paused clock**, not wall sleep). Compose reject → baseline. Dispute + Natural → no HTTP.
- [ ] **≤1 cloud call per utterance (orchestration):** counting mock proves (a) zero calls for Natural and all `cloud_request=not_allowed` routes; (b) at most **one** total HTTP attempt per utterance including retries, provider fallback, and dual-provider temptation — never a second structured call after the first attempt is started or fails.
- [ ] Delivery flags asserted on all paths.
- [ ] Feature flag off: existing Smart Writing tests still pass; daemon behavior unchanged.
- [ ] Feature flag on (test-only): pipeline honors route + compose + deadline.
- [ ] No production late replace API.
- [ ] Independent first-pass + authoritative **Sol** APPROVE (non-skippable priority review).

**Out of scope:** full host install matrix (T8); evaluation late-copy (compile-gated only — see T6).

---

### DPR-T6 — Diagnostics production surface (#142 + residual R4)

**Depends on:** T5 (hooks); T3 for error codes  
**Goal:** Bounded redacted timeline + user feedback rules; production cleanup complete.

**Work:**

1. Events from `utterance_end`: `route_selected`, cloud start/end/skip, deadline, compose decision / fallback trigger, `delivery_emitted`, optional `late_result_discarded` (timing-only).
2. User feedback: default **silent**; optional non-blocking **`Local formatting used`** only when cloud was attempted and hard-fell back. Never claim cloud enhanced when baseline delivered.
3. Modes: `evaluation` vs `production` — **evaluation late full-text retention is compile-gated only** (Cargo feature and/or separate evaluation build). A runtime config flag alone is **not** sufficient; production artifacts must **omit** late full-text fields and any “apply late result” path entirely (spec §11 / §15).
4. **Residual R4 (production cleanup):**
   - Production builds: no `late_result_retained` full text; no UI to apply late result; production is the only default mode in release artifacts.
   - Evaluation may retain late valid candidate for offline compare **only** when the binary was built with an explicit non-production feature (e.g. `dpr_eval_late_retain`) — never Delivery rewrite; never present in production packages.
5. Retention caps + secret redaction per #142 prototype.
6. Align reason codes with #139 error codes where applicable.

**Acceptance:**

- [ ] Unit tests for feedback selection matrix and production vs evaluation late-result rules.
- [ ] Production mode / default release build tests prove: no retained late full-text field; no apply-late API; `replace_delivered` never set true from late cloud.
- [ ] Compile-gate test (or cfg): evaluation late-retain symbols are absent unless the eval feature is enabled.
- [ ] Diagnostics contain no API keys / raw bodies / audio.
- [ ] Prototype corpus promotions or equivalent decision tests green for production cleanup cases.
- [ ] Independent first-pass + authoritative **Sol** APPROVE (material safety: production cleanup).

---

### DPR-T7 — Hermetic integration suite

**Depends on:** T5 to **start**; **T6 must be complete before T7 can finish green**  
**Goal:** One place that locks ship gates §15 without real network or compositor.

**Work:**

1. Integration tests covering the §15 ship-gate checklist in-process:
   - **full applicable** #138 local-path promotion and **full** #139 mutation/decision parity (close any intermediate deferrals from T1/T3 unless a dated baseline revision removed cases)  
   - deadline never blocked by cloud (injected clock)  
   - safety (no accept without compose)  
   - **≤1 cloud call** per utterance including retries/fallback (counting mock)  
   - Delivery flags  
   - diagnostics **production** mode (no eval late full-text in default build)  
   - three policies persist + snapshot  
2. Regression: Smart Writing path still works when DPR flag off.
3. Document how to run: `voisu-cargo test …` (workspace threads policy).

**Acceptance:**

- [ ] Named test module/file documents each ship gate with at least one asserting test.
- [ ] Zero open corpus deferrals from T1/T3 remain unless explicitly approved in a dated research/spec revision.
- [ ] T6 complete; production-diagnostics ship gate asserted here.
- [ ] CI green under existing flake + clippy `-D warnings` policy.
- [ ] Independent first-pass + authoritative **Sol** APPROVE before T8.

---

### DPR-T8 — Host smoke + explicit rollout switch

**Depends on:** T7 green; T6 production cleanup  
**Goal:** Real Fedora KDE proof and **explicit** switch from Smart Writing to DPR. This is the only ticket allowed to make DPR the default organize path.

**Work:**

1. Host smoke (driver-owned, non-delegable): dictate → Final Transcript → insert for **Natural** and **Adaptive**; Structured on apps you care about at ship (note chat UIs that treat newline as send).
2. Live Groq path once (secret-tool credentials): confirm deadline fallback still works when throttled/slow.
3. Rollout mechanism (choose one, document in PR):
   - config flag e.g. `prompt_rendering = "dpr" | "smart_writing"` with default flip only in this ticket after smoke, **or**
   - versioned release note + default Adaptive DPR after smoke checklist signed.
4. Smart Writing remains available as rollback until a later removal ticket (not required here).
5. Do not remove grammar subsystem in this ticket unless explicitly scoped; prefer path selection over mass deletion.

**Acceptance:**

- [ ] Host checklist filled (Natural, Adaptive, at least one Structured surface).
- [ ] Delivery remains unsent; no auto-send observed.
- [ ] Production diagnostics only (no eval late upgrade; production package has no late full-text lane).
- [ ] T7 ship-gate suite green with **full** #138/#139 corpus parity (no silent deferrals).
- [ ] Rollback path documented (`smart_writing` or prior package).
- [ ] Release/plan note: DPR replaces Smart Writing only after this ticket’s explicit enablement.
- [ ] Independent first-pass + authoritative **Sol** APPROVE (non-skippable priority review).
- [ ] Map #133 may close only when #145 (this DAG) was accepted **and** product tickets through T8 are done — or map stays open with T8 as remaining destination work (orchestrator choice; do not claim map closed at T0–T7).

---

## 6. Research residuals as first-class tasks

These must appear as acceptance criteria or sub-bullets on the owning ticket. They are **not** “already safe.”

| ID | Source | Owning ticket | Task |
|---|---|---|---|
| **R1** | #138 | T1 | Multi-word deletion provenance; typed fail-closed removes; no checker TypeError class in product |
| **R2** | #141 | T2 | Residual false-literal edges: shell prose stays local organize, not identity |
| **R3** | #140 | T4 (+ T5/T8 policy) | No sole production-ready cloud default; tight compose gates; Groq preferred in-budget; Gemini retained latency-gated; optional prompt hardening only with fixtures |
| **R4** | #142 | T6 (+ T7 assert) | Production cleanup: eval late full-text is **compile-gated only** (not runtime config); production artifacts omit the lane; retain timing/failure evidence only |

Additional hygiene (not separate product locks):

- #139 hermetic prototype ≠ product runtime → T3 must re-prove mutations in Rust, not “trust the Python.”
- #143 Delivery proven → T8 does **not** re-open “does Delivery work?” as research; only app-specific multiline/send hazards at smoke time.
- Intermediate corpus deferrals on T1/T3 are temporary bookkeeping only; **T7/T8 close them** or require a dated approved baseline revision.

---

## 7. Cross-cutting process gates

| Gate | Rule |
|---|---|
| Review | **Every material T0–T8 head** requires independent first-pass **and** authoritative **Sol** APPROVE before merge-ready. Batch **2–3** finished heads into one Sol session when quota-tight. **Non-skippable priority Sol:** T3 (compose), T5 (wire/deadline/call-count), T8 (rollout). T4 (cloud) and T6 (prod diagnostics) are also material safety and must not skip Sol. |
| CI | `voisu-cargo test --workspace -- --test-threads=4`; clippy `-D warnings`; lockfile advisory as today |
| Secrets | Never commit `.env`; never paste keys into issues/PRs/chat |
| Worktrees | One feature worktree → one PR; no folding large multi-commit DPR slices into unrelated PRs |
| Dirty CLI | Uncommitted `voisu.rs` usage-text simplification stays on the main checkout; DPR tickets that touch `voisu.rs` use a **clean worktree** and staged-diff guard (see T0) |
| Orchestrator | Does not implement product Rust inline; dispatches implementers per AGENTS.md |
| Residual-land of **this DAG** | Sol high residual-land review required before treating #145 as approved |
| Clock tests | Deadline assertions use injected/paused clocks — not wall-clock sleeps |

---

## 8. Explicit non-goals (any ticket)

- Grammar scoring/correction subsystem  
- Inventing requirements / enrichment  
- Command/rewrite mode, translation, summarization  
- Auto-send, live typing, production late replace  
- Second cloud “polish” call  
- Non-English v1  
- Reading clipboard / DOM / screenshots / chat history to rewrite speech  
- Silent default flip to DPR before T8  

---

## 9. Suggested GitHub issue filing order (after #145 approve)

File issues (or a tracked checklist on #145) in this order; link parent #133:

1. `DPR-T0: Domain types + rendering_policy CLI/config`  
2. `DPR-T1: Local baseline + #138 tests (R1)`  
3. `DPR-T2: Intent router #141 port (R2)`  
4. `DPR-T3: #139 compose gate port`  
5. `DPR-T4: Groq cloud client + Gemini latency-gated (R3)`  
6. `DPR-T5: Wire flagged DPR pipeline`  
7. `DPR-T6: Diagnostics production surface (R4)`  
8. `DPR-T7: Hermetic ship-gate integration`  
9. `DPR-T8: Host smoke + Smart Writing → DPR rollout`  

Labels: `wayfinder:task` (or project equivalent); milestone/map #133.

---

## 10. Acceptance of #145 itself

#145 is **approved** when:

1. This document is on `main` (or issue-body equivalent accepted by Raja) **and**  
2. Residual-land **Sol** review returns APPROVE / APPROVE_WITH_NITS (nits do not drop locks) **and**  
3. `internal/STATE.md` points the next agent at **DPR-T0** (first implementation ticket) **and**  
4. Smart Writing is still the shipped organize path (no unauthorized replacement).

### Ballot (closed 2026-08-12)

| Round | Model | Result |
|---|---|---|
| Residual-land r1 | GPT-5.6 Sol high | **REQUEST_CHANGES** — 4×P1 (R4 compile-gate, corpus ship bar, Sol coverage, ≤1-call e2e test) + 2×P2 (T7↔T6 dep, dirty `voisu.rs` guard) |
| Fix round | Grok orchestrator | All six findings applied in this file |
| Residual-land r2 | GPT-5.6 Sol high | **APPROVE** — findings none; prior findings all FIXED; R1–R4 + Delivery/safety locks held |

**Effect:** this DAG is the **approved execution plan** for map #133 product work. Implementation begins at **DPR-T0**. It does **not** change shipped Smart Writing until **DPR-T8** explicitly rolls out.

---

## 11. One-page operator summary

| Step | What ships | User-visible? |
|---|---|---|
| T0 | Types + `voisu rendering` | Config only; behavior unchanged |
| T1 | Local baseline | No (flagged off) |
| T2 | Router | No |
| T3 | Compose gates | No |
| T4 | Cloud client | No |
| T5 | Full pipeline behind flag | Only if test flag on |
| T6 | Diagnostics | Silent / minimal status only when DPR on |
| T7 | Hermetic gates | No |
| T8 | Host smoke + default flip | **Yes — DPR goes live** |

Build in order. Keep the baseline. Trust only compose. Deliver once, unsent, on time.
