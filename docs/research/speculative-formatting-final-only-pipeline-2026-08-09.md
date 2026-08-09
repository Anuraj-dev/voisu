# Speculative formatting within the final-only pipeline — 2026-08-09

**Issue:** [#101](https://github.com/Anuraj-dev/voisu/issues/101) · Parent [#96](https://github.com/Anuraj-dev/voisu/issues/96)
**Mode:** architecture research only (no Smart Writing source changes)
**Raja choice:** **Architecture A** — no Recording-time worker speculation (**reconfirmed**; concurrent owned pre-validation prep)
**Closure:** one implementable recommendation; #99/#100/#102/#103 own product, safety, host matrix, and executable constants.

## 1. Recommendation (answer to #101)

**Voisu SHOULD NOT overlap content-bearing Formatting or Minimal Grammar Correction with Recording in v1.**

This is an **evidence-backed rejected hypothesis**, not a missing feature. Provisional Deepgram text is not semantic truth; Validated identity is fixed only after provider completion and validation; current cloud HTTP is `spawn_blocking` + curl and cannot drop-safely cancel; and current credential load may run `secret-tool`, retry sleeps, and helper threads whose cleanup is best-effort. #103 therefore needs a dedicated credential-preparation owner, not the current helper wrapped in `spawn_blocking`.

**v1 pipeline (preserve + inserts):**

```text
Recording capture (audio only)
  → Stop / Shutdown-while-Recording
  → process_recording:
       capture.finish
       ★ concurrent pre-validation stage — one inline owned structure, polled by process_recording
         (NO detached task, Recording worker, or sequential post-provider prep):
         ├─ ProviderCoordinator.complete_with_timings   // Deepgram + Groq Sources
         │    // current maximum: 15 s completion + 2 s abort attempt
         └─ CredentialPreparationOwner + registered cleanup entry (NEW)
              // registry retains Child/pipes/state before any child launch
              // work deadline 13 s; cancellation/reap watchdog 2 s
              // watchdog overrun stays Processing and awaits terminal reap
       TranscriptDecisionPipeline.validate              // Validated Transcript
       ★ Final Transform Gate (NEW, inline)             // ValidationCompleted → freeze/initiate ≤ 1 s
       DeliveryAdapter.deliver                          // once; final-only
  → supervise_recording: await process → credential lane + provider drain → Completed → Idle
```

| Stage | Content-bearing format/grammar? | Cloud Minimal Grammar? |
|-------|----------------------------------|------------------------|
| Recording | **No** | **No** |
| Concurrent pre-validation (Sources ∥ prep) | **No** presentation edits | Credential + async client readiness only (not grammar HTTP) |
| Validation | **No** (meaning only) | Reconcile/repair only (existing curl path) |
| Final Transform Gate | **Yes** (local first; formatting wins) | Only if `GrammarCapability::Ready` + English-eligible; **async transport** |
| Delivery | Final string only | **No** |

**Milestone semantics (do not collapse these):**

| Situation | Product meaning |
|-----------|-----------------|
| **No production-proven** async grammar transport / request-ready capability path in the codebase | **Implementation blocker** for #103 and map **#96**. **Not** permission to declare Smart Writing v1 complete with formatting-only. Requires a proven async client **or** Architecture B reconsideration. |
| Production path exists and passed production-boundary gates; a **single Recording** hits `Unavailable`, timeout, error, #100 reject, or ineligibility | **Ordinary per-recording local fallback** (format/identity), recorded in diagnostics — safe and expected |

## 2. Current pipeline (source-symbol evidence)

```text
actor_loop (voisu-daemon.rs)
  Recording(ActiveRecording { pump → ProviderCoordinator, … })
  Stop | PumpTerminated | Shutdown-while-Recording
    → spawn_recording_processing → tokio::spawn(process_recording)  // AFTER Stop
    → tokio::spawn(supervise_recording)
  process_recording: finish → complete_with_timings → validate → deliver
  supervise_recording: await process (Delivery on success) → reaper.drain → Completed → Idle
```

| Fact | Exact symbol / path |
|------|---------------------|
| `process_recording` starts only after Stop, pump end, or Shutdown-while-Recording | `spawn_recording_processing`, `process_recording` (`voisu-daemon.rs`) |
| Shutdown-while-Recording runs **full Processing including Delivery**, then Idle | `ActorMessage::Shutdown` arm + same spawn path |
| Reaper drain **after** processing (after Delivery on success) | `supervise_recording` after `processing.await` |
| Only `is_final: true` enters Source Transcript; interims never | `TranscriptAccumulator::ingest` (`system.rs`) |
| **Groq reconcile transport not drop-safe for the gate** | `GroqReconciliationModel::request` → `spawn_blocking` → curl; cancel needs owned await + reap (`RECONCILIATION_CLEANUP_GRACE`) |
| **Credential lookup can block with secret-tool / retries / sleep** | same request: `SecretStore::load(&mut SecretToolStore, Provider::Groq)` inside `spawn_blocking`; `SecretToolStore` shells `secret-tool`; `lookup_retry_backoff` / `keyring_retry_backoff` use `std::thread::sleep` (`system.rs`) |
| Language today is env-defaulted Whisper param, not Smart Writing eligibility | `GroqRequestParams::from_config`: `VOISU_GROQ_LANGUAGE` default `"en"`; `DEEPGRAM_STREAMING_PARAMS` has **no** `language` field |
| Stages: `ValidationCompleted` then `DeliveryCompleted` | `LifecycleStage` (`voisu-core`) |

**Insert concurrent capability prep with `complete_with_timings` immediately after `capture.finish`; insert the gate after `ValidationCompleted`, before `deliver`.** Do not fold presentation into `TranscriptDecisionPipeline` (#90). **Do not reuse curl/`spawn_blocking` reconcile transport or in-gate secret-tool load for Minimal Grammar.**

**Deadlines today** (`system.rs:38-55`): capture finalize 2 s; provider completion 15 s; provider/recovery abort 2 s; reconcile 3 s; clipboard 2 s; libei 5 s; `PROCESSING_RESPONSE_DEADLINE` = `2+15+2+5+2+6+1` = **33 s**. `ProviderCoordinator::complete_with_timings` first waits up to 15 s and only then gives pending-provider abort up to 2 s (`voisu-core/src/lib.rs:2819-2968`), so that arm can take **17 s**. `ProviderReaper::drain_to_completion(2 s)` repeats passes until terminal cleanup and has no 2 s total bound (`system.rs:3154-3283`).

## 3. Final Transform Gate (Architecture A)

### 3.1 Ownership

Owned **inline** by `process_recording` on the same async stack that awaits validate then deliver.

**Forbidden in the gate:** Recording-time cache/worker/channel; gate `tokio::spawn` of format/grammar; retained gate `JoinHandle`; detached tasks; shared presentation mutexes; `ActiveRecording` presentation fields; **subprocess / curl / `spawn_blocking` / per-request spawned task** as grammar transport; **any credential acquisition** (secret-tool, Secret Service, file fallback, cache fill) inside the 1 s candidate budget.

**Required:** sequential ownership of one candidate pipeline future; drop ends **all gate-owned request work**; at most one in-flight grammar HTTP request; gate inputs are **already resolved**.

### 3.2 Pre-gate GrammarCapability preparation (concurrent, owned)

**`GrammarCapability`** is an explicit gate input enum — **never a lazy loader**:

```text
GrammarCapability::Ready(ReadyGrammarCapability) | GrammarCapability::Unavailable(reason)
```

`ReadyGrammarCapability` **must already contain**: (1) **resolved credential handle/material** for Groq Minimal Grammar (no further keyring/`secret-tool` on use), and (2) a **production-proven async HTTP client/transport** for request-scoped drop-safe calls (§3.3).

**Hard concurrent contract (Architecture A — no ambiguity):**

1. **Start:** immediately after `capture.finish`, `process_recording` **MUST** register credential cleanup, start prep, and poll it concurrently with `ProviderCoordinator.complete_with_timings`.
2. **Structure:** one inline owner/combinator owns both arms; its credential lease is declared outside the borrowed concurrent future. **Forbidden:** detached task, Recording worker, fire-and-forget, or prep beginning only after providers return. A provider failure cancels prep but still awaits terminal cleanup before returning the error.
3. **Provider arm:** preserve current reality: completion may consume `PROVIDER_COMPLETION_DEADLINE` **15 s**, then pending-provider abort may consume `RECOVERY_ABORT_DEADLINE` **2 s**. Its normal maximum is **17 s**, not 15 s.
4. **Prep arm:** #103 defines `CREDENTIAL_PREP_WORK_DEADLINE = 13 s` and `CREDENTIAL_REAP_WATCHDOG = 2 s`. At the work deadline, cancellation stops retries/backoff, kills any credential process group, and begins terminal reap. The 2 s value is a watchdog/diagnostic threshold, **not proof of terminal reap**.
5. **Join semantics:** if credential cleanup reaches terminal within its normal 13+2 schedule, it is hidden under the provider arm's existing 15+2 response allocation; those **15+2 seconds are counted exactly once**. Provider finishing early does not permit validation while prep or its cleanup remains live.
6. **Overrun safety:** if credential reap is still pending at the 2 s watchdog, emit one secret-free overrun diagnostic and continue awaiting the same owner. Actor state remains **Processing**. There is **no** Validation, gate, Delivery, completion acknowledgement, or Idle transition until the credential child/process group and owned pipe drains are terminal.
7. **Outcome:** only after terminal ownership is proved does prep yield `Ready` or `Unavailable(reason)`. This wait is deliberately allowed to exceed the 34 s CLI/shutdown watchdog; safety outranks a false latency bound.

**Required #103 primitive — `CredentialPreparationOwner` + dedicated `ProviderReaper` credential lane:**

- `register_credential()` first inserts an `Arc<CredentialCleanupEntry>`; only the first owner poll may then launch work. The entry—not a task local—stores `Child`, capped async stdout/stderr readers, retry/backoff state, outcome, and credential bytes. There is no prep `tokio::spawn`, `spawn_blocking`, reader thread, or movable live `JoinHandle`.
- Fast paths read env/session cache. Cache miss enables Tokio `process` and launches restricted `secret-tool lookup` in its own process group while the already-registered entry retains every handle.
- State transitions are `Registered→Running(pgid)→Terminal`, `Registered→Terminal` (no child), or `Registered|Running→CancelRequested→Terminal`, then `Terminal→Deregistered`. `Terminal` means `Child::wait` and both pipe EOFs. `DriveClaim = Free|Normal|Supervisor` changes under one mutex/RAII guard: one path polls the retained state; panic/drop releases the claim so the supervisor can resume it. An atomic `kill_requested` makes SIGKILL idempotent; entry-id removal makes deregistration idempotent, preventing double kill/wait/removal.
- Normal owner drives to `Terminal`, then deregisters **before validation**. This is never deferred past the gate or Delivery. Error/timeout becomes `Unavailable(reason)` only after that sequence.
- Owner `Drop` synchronously sets cancel and sends process-group SIGKILL as a backstop; it cannot declare terminal or remove the entry. Immediately after `processing.await`, `supervise_recording` claims, drives, and deregisters the credential lane **before** panic diagnostics, adapter rebuild, or `Completed`; shutdown's final reaper clone repeats the drain before runtime teardown.
- Credential bytes remain in `Credential`, never `Debug`/logged, and enter `ReadyGrammarCapability` only after `Credential::new` accepts them.

```text
pin!(reap = prep.cancel_and_drive_terminal())
if timeout(CREDENTIAL_REAP_WATCHDOG, &mut reap).await timed_out { diagnose; reap.await }
```

An async Secret Service client could avoid a helper later, but it is **not the #103 boundary chosen here**. #103 implements the registered state above; it must not wrap current `SecretToolStore::load`/`run_restricted`, whose blocking sleep/helper-thread and best-effort reap contracts cannot supply retained terminal ownership.

Wrap the **whole provider+capability concurrent state machine** in `AssertUnwindSafe(...).catch_unwind()`. A caught provider/prep/parsing/diagnostic panic explicitly cancels, drives terminal, deregisters, then returns a Processing error. The registered entry is the last resort only for an uncaught later panic, task abort, or teardown; those paths have no Delivery, so supervisor reap does not violate pre-gate ordering.

If prep fails at runtime for a Recording → `Unavailable(reason)` only after terminal reap; gate uses local Formatting/identity and **records** the reason. That is per-recording fallback only after a production-ready path exists (§1 milestone table).

### 3.3 Production boundary for Minimal Grammar HTTP

Current Groq paths use **curl children behind `spawn_blocking`** and complete only after kill/reap. **Not** an accepted Minimal Grammar path under Architecture A.

| Layer | Allowed | Forbidden for grammar |
|-------|---------|------------------------|
| Persistent shared infrastructure | Long-lived async HTTP client / pool / reactor owned by the process | Claiming the pool “is” the request; response work after request future drop |
| **Per-request, gate-owned work** | Fully **async**, **request-scoped** future the gate polls directly | Subprocess; `spawn_blocking`; per-request `tokio::spawn`; background handle; ProviderReaper curl; in-gate credential load |
| Drop / timeout | Drop cancels request progress; **no gate-owned** task/child/socket/response processing remains | Post-timeout cleanup grace; detach-then-reap |

**Product proof vs per-recording fallback:** if the chosen HTTP library/client cannot **prove** this drop contract (production-boundary cancellation test on the real stack, not only a mock), that is an **#103/#96 implementation blocker** — Architecture A cannot ship Minimal Grammar, and **formatting-only is not Smart Writing v1 complete**; choose a proven async client or reconsider Architecture B. Once the adapter+capability **have** passed those gates, ordinary per-recording `Unavailable`/timeout/error may fall back locally.

### 3.4 Inputs / English eligibility

```text
In:  Validated Transcript (immutable identity),
     Writing Mode,
     EnglishEligibility (fail-closed),
     LocalFormatter,
     GrammarCapability  // Ready(cred + async client) | Unavailable(reason) — never lazy
Out: selected Rendered string for one Delivery
```

**Grammar body (closed):** English Validated Transcript text only.
**Forbidden:** Deepgram provisional text, app context, screen, clipboard, surrounding text, field type, browser tab.
**GrammarAdapter:** uses only `Ready` capability material; **never** calls `SecretStore::load` / secret-tool / credential cache fill inside the gate.

**EnglishEligibility (fail-closed):** from **resolved recording/provider language configuration**, not transcript text. Grammar only when explicitly English-allowed. Skip on **absent, conflicting, `auto`/detect, empty, or non-`en`**. **Never** infer English from Validated text alone. Exact wiring/normalization: **#103**.

**Current limitation:** `VOISU_GROQ_LANGUAGE` defaults `"en"`; Deepgram streaming params omit language. Unclear config → not eligible until #103 wires resolution.

**D3-B (#99):** list inference = **transcript-content only** on Validated string; not application context.

### 3.5 Absolute one-second candidate deadline

One absolute `deadline = gate_entry + 1s` starts only at gate entry / `ValidationCompleted` (after capability is already `Ready` or terminally `Unavailable`). Provider and credential preparation/cleanup are pre-validation and **never consume or start this second**; credential reap may instead overrun the 34 s watchdog while Processing.

`timeout_at(deadline - delivery_reserve, candidate_pipeline)` wraps the **entire** candidate path: (1) local Formatting (bounded), (2) optional Groq request if `Ready`+eligible+Smart, (3) #100 validation, (4) compose/render (formatting wins), (5) freeze `selected`.

**Reserve an explicit tail** to **initiate** `delivery.deliver(selected)` before the second elapses (constant: **#103**). Delivery I/O after initiate keeps today’s 2 s / 5 s bounds.

| Rule | Detail |
|------|--------|
| Logical hard end | Freeze by `deadline`; no new grammar/format work after freeze |
| Delivery reserve | Call `deliver` start before `gate_entry+1s` |
| Cooperative bounds | Format / #100 / compose: deterministic input/work/response bounds; cooperative; timeout cannot preempt unbounded poll |
| On timeout/error/unsafe/ineligible/`Unavailable` | Freeze already-persisted format/identity; drop async grammar request future; **no** gate cleanup grace past `deadline` |
| Tests | Paused-time logical 1 s; small real-scheduler telemetry tolerance **without** relaxing the logical second |

### 3.6 Formatting outranks grammar (composition)

- **Identity** = immutable Validated Transcript.
- Local Formatting → deterministic format edits / baseline under strict bounds.
- Grammar → **structured edits against identity** only; #100 validates/rejects.
- **Compose** format + approved grammar edits on the **same identity**; on overlap **Formatting wins**. Grammar success must not replace/discard local formatting.
- Grammar skipped/fails/times out/panics/#100 rejects/`Unavailable` → format baseline else identity.
- Literal: format/commands only; ignore grammar capability for cloud calls.

### 3.7 Algorithm

```text
// AFTER capture.finish — lease/state lives outside the borrowed concurrent future
entry = reaper.credential_lane.register() // entry owns all live process state
prep = CredentialPreparationOwner::new(entry) // first poll may launch only after registration
concurrent = AssertUnwindSafe(async {
  join_owned(complete_with_timings(audio), prep.poll_outcome()).await
}).catch_unwind()
(provider_result, prep_result) = match concurrent.await {
  Ok(result) => result
  Err(panic) => { prep.cancel_and_drive_terminal().await; return ProcessingError(panic) }
}
// If credential reap crosses its 2s watchdog: log once, stay Processing, keep awaiting.
// Normal/error paths join Terminal + deregister before validation; owner Drop only kill-signals.
if provider_result is Err(error) { prep.cancel_and_drive_terminal().await; return error }
capability = prep.finish_terminal(prep_result).await

// after ValidationCompleted — GATE ENTRY starts 1s clock
gate_entry = Instant::now(); deadline = gate_entry + 1s
identity = validated_text
eligible = EnglishEligibility::from_resolved_config(...)  // fail-closed; #103
selected = identity.clone() // OUTSIDE timeout/catch; always available

candidate = AssertUnwindSafe(async {
  format_baseline = LocalFormat(identity)?
  selected = format_baseline.clone() // persist immediately after complete formatting
  if WritingMode::Smart && eligible.allows_grammar()
     && matches!(capability, Ready(cap)):
    // All intermediate grammar state remains separate from selected.
    grammar_edits = GrammarAdapter::propose_edits(cap, identity).await?
    safe_edits = SafetyGate#100::validate(identity, grammar_edits)?
    composed = compose(identity, format_edits, safe_edits)? // formatting wins overlaps
    selected = composed // one atomic replacement only after full safe success
}).catch_unwind()

match timeout_at(deadline - delivery_reserve, candidate).await {
  Ok(Ok(Ok(()))) => diagnostic = enhanced_or_formatted
  Ok(Ok(Err(error))) | Ok(Err(panic)) | Err(timeout) =>
    diagnostic = fallback_reason
    // selected remains formatted if formatting completed, otherwise identity
}
delivery.deliver(selected)  // once; initiate before gate_entry+1s
```

### 3.8 Panic containment (inside the gate)

Panics from local formatter, grammar future poll, #100 validation, or compose/render must not escape as uncaught `process_recording` panic that skips Delivery.

**Boundary:** initialize `selected = identity` outside `timeout_at`/`catch_unwind`; after local Formatting completes, persist its baseline into `selected` before grammar begins. `AssertUnwindSafe` + `FutureExt::catch_unwind` wraps the candidate future. Grammar, #100 validation, and composition use separate temporaries and replace `selected` only after full safe success. Thus a later timeout/error/panic drops the request future but preserves formatted `selected`; a formatter failure preserves identity; Delivery runs **exactly once**.

**Do not** rely on `supervise_recording` for a gate presentation panic — it rebuilds adapters **without** Delivery. Presentation recovery is gate-local; the supervisor lane is only the last-resort credential cleanup path when processing cannot return normally.

### 3.9 Shutdown

**Preserve today:** Shutdown-while-Recording → same processing as Stop → Delivery if validated → credential/provider drains → Idle → shutdown acknowledgement. If processing panics/is cancelled, there is no Delivery: supervisor drains the credential lane, then `Completed` permits Idle/ack. The top-level shutdown reaper clone performs the final idempotent drain before runtime teardown.

**Overrun UX:** after #103 adds the gate, `PROCESSING_RESPONSE_DEADLINE = 34 s` remains only the Stop/Toggle/Replay client response timeout and shutdown-ack watchdog (`voisu.rs:111-117`; `voisu-daemon.rs:255-279`). If credential reap is non-cooperative, a Stop caller may time out while `status` truthfully remains `processing`; the daemon continues owning/reaping and may deliver later. Shutdown logs that the acknowledgement watchdog elapsed, then continues awaiting the actor rather than detaching it. The service manager's `TimeoutStopSec` remains the external last-resort kill. Diagnostics must say `credential_cleanup_overrun`, elapsed time, and “Processing until terminal reap”; never claim fallback, Delivery, cleanup completion, or Idle before observing them.

## 4. Latency budget

| Stage | Bound |
|------|------:|
| Capture finalize | ≤ 2 s (`CAPTURE_FINALIZE_DEADLINE`) |
| **Concurrent pre-validation stage** | Normal maximum **17 s**; no false hard bound on credential reap |
| · provider completion | ≤ 15 s (`PROVIDER_COMPLETION_DEADLINE`) |
| · pending-provider abort after deadline | additional ≤ 2 s (`RECOVERY_ABORT_DEADLINE`) |
| · credential work | 13 s deadline (#103 constant) |
| · credential kill/reap | 2 s watchdog; at overrun remain Processing and await terminal reap |
| · normal overlap | credential terminal by 15 s; provider arm may run to 17 s; count **15+2 once** |
| Validation/recovery allocation | existing `2 × 3 s + 1 s` in response-budget constant; **not** grammar |
| **Final Transform Gate candidate pipeline** | **`timeout_at` absolute `gate_entry+1s` minus Delivery reserve** |
| · local Formatting / optional grammar HTTP / #100 / compose | all inside candidate `timeout_at` |
| Initiate Delivery | reserved tail before `gate_entry+1s` |
| Delivery I/O after initiate | ≤ 2 s clipboard / ≤ 5 s libei |
| Reaper after process | Normal credential lane empty; panic/abort lane drains before `Completed`; 2 s passes repeat to terminal |

**`PROCESSING_RESPONSE_DEADLINE` 33 → 34 math:** today `2 capture + 15 provider completion + 2 recovery/provider abort + 2 clipboard + 5 libei + 6 reconciliation/recovery + 1 existing headroom = 33 s`. Add the Final Transform Gate once: **34 s**. Capability prep begins concurrently and, when its owner is terminal by its 13 s work + 2 s watchdog schedule, adds no nominal seconds; the existing provider **15+2 is counted exactly once**. This **34 s is a CLI/shutdown response watchdog**, not a hard Processing→Idle or process-lifetime bound. A non-cooperative credential reap or repeated `ProviderReaper` passes may safely exceed it.

## 5. Failure table

| Case | Delivery | Milestone note |
|------|----------|----------------|
| Capture / provider / validation failure | **No**; no gate | — |
| **No production-proven async transport/capability in product** | N/A for “done” | **Blocks #103/#96**; not formatting-only completion |
| Prep work timeout; credential owner terminally reaps | Yes, only then: **format/identity**; reason recorded | Per-recording OK |
| Credential reap crosses 2 s watchdog | **Not yet**; stay Processing and await terminal reap | CLI/shutdown may cross 34 s; no detach/reaper handoff |
| Credential child never becomes terminal before service-manager kill | **No** | External hard-stop; never report Idle/cleanup success |
| Caught concurrent prep/provider/parsing panic | **No** | Explicit cancel + terminal drive + deregister, then Processing error |
| Uncaught `process_recording` panic or task abort with live child | **No** | Supervisor credential-lane drain precedes `Completed`/Idle |
| LocalFormat bound miss | Yes: **identity** | Per-recording |
| English ineligible / auto / conflict / absent | Yes: **format/identity**; no GrammarAdapter | Per-recording |
| Prep → `Unavailable(reason)` (runtime, after production path exists) | Yes: **format/identity**; reason recorded | Per-recording OK |
| Grammar timeout / error / drop / #100 reject after formatting | Yes: persisted **format** once within gate budget | Per-recording OK |
| Panic in formatter (caught) | Yes: **identity** once | Supervisor does **not** deliver |
| Panic in grammar poll / #100 / compose after formatting (caught) | Yes: persisted **format** once | Supervisor does **not** deliver |
| Uncaught panic outside gate | No Delivery (non-design path) | — |
| Literal / Shutdown-while-Recording | Format-only / Processing as Stop | — |

## 6. Invariants

1. `delivery_count ≤ 1`; no Delivery before `ValidationCompleted`; no interim Deepgram text delivered.
2. No content-bearing format/grammar while `ActorState::Recording`.
3. Grammar body = English Validated Transcript only; transport fully async/drop-safe — **curl/`spawn_blocking`/process not accepted**.
4. Gate never acquires credentials; input is `Ready` or `Unavailable` only; no live per-request work after freeze/drop.
5. Formatting outranks grammar on compose; grammar cannot discard format edits.
6. English eligibility config-resolved/fail-closed; never text-inferred alone.
7. `selected` starts as identity outside catch/timeout, becomes format immediately after formatting, and becomes enhanced only by one atomic assignment after grammar + #100 + compose all succeed; later failures cannot erase formatting.
8. Presentation never mutates Source/Validated selection inside `TranscriptDecisionPipeline`.
9. Missing production async/capability proof **blocks #96 completion**; does not redefine Smart Writing as format-only.
10. Prep registration precedes its first poll/child launch; prep then starts concurrently with `complete_with_timings` immediately after `capture.finish` via one inline owned structure — no detached task, Recording worker, or sequential post-provider prep.
11. Provider completion may take 15 s plus 2 s abort. Credential work has a 13 s deadline and 2 s reap watchdog. Normal cleanup drives terminal/deregisters before Validation; abnormal panic/cancellation remains supervisor-retained and blocks `Completed`/Idle. No live credential work reaches gate/Delivery.
12. The one-second hard deadline starts only at `ValidationCompleted`; pre-validation capability cleanup never spends it. Initiate one Delivery before the second ends.
13. Caught gate panics deliver exactly once with the persisted safe fallback; Idle only after process and terminal cleanup as today. The 34 s response constant is a watchdog, not an Idle guarantee.

## 7. Public observable test seams

| Seam | Proves |
|------|--------|
| Abnormal-path common assertions | No Delivery on processing panic; no `Completed`/Idle before child wait + pipe EOF; no surviving process; retained entry removed exactly once |
| Gate entry / capability not lazy | Validate Ok before grammar request; gate gets `Ready`/`Unavailable` only — no in-gate `SecretStore::load`/secret-tool |
| Credential cache-hit + cache-miss/cancel | Registration precedes first poll/child; normal hit/miss/cancel drives terminal and removes retained entry before Validation |
| Provider 15→17 timing (paused/controlled) | Provider remains pending through 15 s, abort then completes at 17 s; capability is already terminal; no validation/gate before provider arm returns; response math counts 15+2 once |
| Credential normal timing | Work reaches 13 s, kill/reap becomes terminal within 2 s, provider may continue to 17 s; stage ends at max of arms, never at a claimed 15 s outer deadline |
| Credential non-cooperative overrun | Reap exceeds 2 s watchdog; state remains Processing; retained entry remains registered; no validation, gate, Delivery, completion ack, or Idle until terminal reap |
| CLI/shutdown watchdog overrun | At 34 s Stop client times out/status remains Processing; daemon still owns child and later continues only after reap. Shutdown logs watchdog, awaits actor, then preserves Processing→Delivery→Idle/ack |
| Injected prep-poll panic | Whole concurrent catch explicitly cancel-reaps retained state; no Delivery; entry removed before Processing error |
| Unrelated provider parse/diagnostic panic with child live | Same explicit caught cleanup; proves panic containment is around the whole concurrent machine, not only prep polling |
| Abort `process_recording` future | Owner Drop kill-signals; supervisor claims retained state; no Delivery/`Completed`/Idle until child wait + pipe EOF; no process survives; entry removed |
| Daemon shutdown / runtime cancellation | Abort processing while shutdown waits: supervisor drains before `Completed`/Idle/ack; final shutdown drain is empty/idempotent; no process or retained entry survives runtime teardown |
| Cancel/Drop/supervisor race | Concurrent requests produce one SIGKILL, one child wait/pipe drain, and one entry removal; losing poller observes Terminal |
| Production-boundary cancel (real client) + mock | Drop mid-flight → no continued request/response work; local/identity once within logical 1 s |
| Absolute `timeout_at` + paused-time | Whole pipeline under one deadline + Delivery reserve; logical second non-racy; telemetry tolerance does not relax it |
| Fallback persistence | Format completes, then grammar timeout/error/panic, #100 panic/reject, or compose panic → exact formatted baseline once; formatter timeout/panic → identity once; partial grammar/composition never mutates `selected` |
| Runtime `Unavailable` vs product-proof absence | Per-recording local fallback OK after production path exists; unproven async client **cannot** close #96/#103 |
| Literal / #100 / D3-B / Shutdown / envelope | Grammar unused when rejected; transcript-only lists; Shutdown full Processing + one Delivery; 34 s watchdog = current 33 + gate only, with explicit allowed cleanup overrun |

## 8. Ticket ownership

| Ticket | Owns |
|--------|------|
| **#101 (this)** | No Recording overlap; concurrent owned pre-validation prep; provider 15+2 reality; dedicated credential owner + overrun lifecycle; inline gate; absolute 1 s + Delivery reserve; stable fallback; async-only transport; eligibility shape; milestone vs per-recording fallback; seams |
| **#99** | Approved Smart/Literal behaviors (incl. D3-B) |
| **#100** | Structured edit / protected-span safety API |
| **#102** | Real multiline host Delivery evidence (Raja present) |
| **#103** | Executable constants, async client, registered credential state/lane, panic/task-abort/teardown proofs, diagnostics/CLI behavior — **blocked on proven async transport** |
| **#97/#98** | Closed; reconcile curl stays for validation only |

## 9. Rejected alternatives

| Idea | Why rejected |
|------|----------------|
| Mid-Recording format worker / channel / clause cache | Architecture A; Stop ownership races |
| Mid-Recording / provisional Groq grammar | Violates #96 Validated-only |
| **Reuse curl + `spawn_blocking` for Minimal Grammar** | Not drop-safe; **not accepted under A** |
| **Credential load inside the 1 s gate** | secret-tool/retries/sleep blow the deadline; not drop-safe with Architecture A |
| Lazy capability / on-demand keyring in GrammarAdapter | Same; gate inputs must be Ready/Unavailable |
| Detached / spawned / Recording-worker prep | Loses cleanup ownership; only inline polling of pre-registered retained state is allowed |
| **Sequential prep after `complete_with_timings` returns** | Violates concurrent contract; can add latency outside the owned provider window |
| Treat credential cleanup as hard-capped/droppable at 2 s | Current process cleanup cannot prove terminal reap; would detach or enter gate over live credential work |
| Defer a normal credential entry to post-Delivery generic drain | Allows gate/Delivery before a security-sensitive child is gone; the lane is fallback only on no-Delivery abnormal exits |
| Treat 34 s response watchdog as a hard Idle bound | Contradicts guaranteed terminal drain and shutdown's unbounded actor join |
| Declare #96 complete with formatting-only when async grammar unproven | **Milestone lie**; blocker, not fallback policy |
| Per-request `tokio::spawn` / background HTTP handle | Detach after timeout |
| Gate cleanup grace after 1 s | Violates hard second |
| Grammar free-text replace of format output | Formatting must outrank |
| Infer English from Validated text alone | Fail-open |
| Rely on `supervise_recording` to deliver after gate panic | No Delivery on panic rebuild |
| Application context for lists/grammar | Out of #96; D3-B transcript-content only |

## 10. Future reconsideration

Recording-time Formatting overlap **or** process-backed grammar HTTP transport requires Architecture B (reusable abort/reap) **and** measured need. Credential lookup is the deliberately separate pre-validation owned-process boundary above. If no async grammar client can meet the production drop contract, **reconsider architecture** before claiming #96 — do not ship “format-only Smart Writing” as the closed map.

## 11. Evidence index

- `crates/voisu-app/src/bin/voisu-daemon.rs:1428-1485,1916-2108`: `process_recording` is spawned; today it runs `capture.finish` → `complete_with_timings` → validate → `ValidationCompleted` → deliver.
- `crates/voisu-app/src/bin/voisu-daemon.rs:2110-2190,1272-1282`: supervisor drains before `Completed`; only `Completed` permits Idle. `:1372-1412`: shutdown-while-Recording uses normal processing and acknowledges only from Idle. `:255-279`: shutdown watchdog then unbounded actor join and terminal drain.
- `crates/voisu-app/src/bin/voisu.rs:111-117`: Stop/Toggle/Replay use `PROCESSING_RESPONSE_DEADLINE` as client response deadline.
- `crates/voisu-app/src/system.rs:38-55`: exact 15 s provider, 2 s recovery, and 33 s response constants. `:3059-3078,3457-3468,3558-3582`: Groq language default, Deepgram params, and final-only accumulator. `:3154-3283`: `ProviderReaper::drain_to_completion` repeats bounded passes until terminal.
- `crates/voisu-core/src/lib.rs:2819-2968`: provider waits up to 15 s, then awaits pending abort under a separate 2 s deadline. `:224-231`: `ValidationCompleted` precedes `DeliveryCompleted`.
- `crates/voisu-app/src/system.rs:626-950,1032-1145,2087-2300,4207-4243`: current credential path, retry sleeps, `secret-tool`, helper threads, bounded join detachment/best-effort reap, and reconciliation's `spawn_blocking` load; these are evidence for the new owner rather than an implementation to reuse.
- `crates/voisu-core/src/lib.rs:989-994,1181-1191`: current reconciliation cleanup grace. `TranscriptDecisionPipeline`, `CancelRegistry`, `DeliveryAdapter::deliver`, and `SecretStore` remain their named public seams.
- Map #96 / main `CONTEXT.md`; #99 ballot D3-B

## 12. Closure and blockers

**#101 answer:** no content-bearing format/grammar during Recording. Immediately after `capture.finish`, `ProviderReaper` registers an entry that retains all credential process state before the owner's first poll can launch a child; the owner lives outside the caught concurrent future. Provider completion may spend **15 s + 2 s abort**, counted once. Normal credential work has a 13 s deadline and 2 s reap watchdog, drives terminal/deregisters before validation, and never defers cleanup past Delivery. A caught concurrent panic explicitly cancel-reaps; an uncaught `process_recording` panic/task abort synchronously kill-signals in owner Drop, while `supervise_recording` claims the retained state and blocks `Completed`/Idle until child wait + pipe EOF. Those abnormal paths have no Delivery. Only terminal `Ready`/`Unavailable` reaches validation, whose `ValidationCompleted` starts the absolute one-second gate-to-freeze/Delivery-initiation clock. Formatting remains persisted across later failures. Minimal Grammar still requires proven fully async request-scoped HTTP. Proposed `PROCESSING_RESPONSE_DEADLINE` is **33→34 s**, a CLI/shutdown watchdog—not a hard Idle bound.

**Blockers (explicit):** (1) **#103/#96** production-proven async Minimal Grammar transport plus the registered credential state/lane, including real cache-miss, non-cooperative reap, caught panic, process-task abort, and teardown proofs. Absence is an implementation blocker—not permission to close Smart Writing as formatting-only. (2) **#100** structured-edit safety API. (3) **#102** host multiline Delivery evidence (Raja present). Once (1) is proven, ordinary per-recording fallback is allowed and recorded.

**No runtime code in this ticket.**
