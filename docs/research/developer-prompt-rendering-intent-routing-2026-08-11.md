# Developer Prompt Rendering — Weighted Intent Routing (#141)

**Date:** 2026-08-11  
**Issue:** [#141](https://github.com/Anuraj-dev/voisu/issues/141) — Prototype weighted intent routing and local fast paths  
**Parent map:** [#133](https://github.com/Anuraj-dev/voisu/issues/133)  
**Contract:** [#137](https://github.com/Anuraj-dev/voisu/issues/137)  
**Behavior oracle:** [#138](https://github.com/Anuraj-dev/voisu/issues/138) (READ ONLY sibling corpus; labels and routes aligned)  
**Blocks / feeds:** [#144](https://github.com/Anuraj-dev/voisu/issues/144) specification assembly  

## Question answered

What weighted combination of **locally available** surface/process hints, speech shape, STT agreement, utterance complexity, pause timing, and rendering policy reliably selects:

| Route | Meaning |
| --- | --- |
| `literal_identity` | Selected source text is the final transcript; no organize pass. |
| `deterministic_local` | Local Natural-shaped organize only (punctuation, layout, context-safe cues). |
| `local_with_optional_cloud` | Local baseline always available; one optional/required cloud organize attempt may run for disputed or complex speech. |

…**without a network call or material latency** on the decision path itself.

## Non-goals (routing)

- Not a renderer, grammar engine, or cloud client.
- Does not invent Structured labels; routing only chooses **local vs cloud-attempt** under the active policy.
- Does not read clipboard, page DOM, screenshots, or conversation history.
- Does not auto-send or replace delivered text.
- Does not schedule wall-clock sleeps; signal collection is O(local) under remaining headroom before the **1.5s Delivery** deadline.

## Owned package

| File | Role |
| --- | --- |
| `developer-prompt-rendering-intent-routing-2026-08-11.md` | This decision writeup |
| `developer-prompt-rendering-intent-routing-schema-2026-08-11.json` | Structural schema for routing corpus |
| `developer-prompt-rendering-intent-routing-corpus-2026-08-11.json` | Fixtures + expected decisions |
| `developer-prompt-rendering-intent-routing-prototype-2026-08-11.py` | Stdlib-only pure-local router + checker + mutations |

## Inputs (observation)

All inputs are already in process or cheap local OS metadata. Missing optional fields **degrade** to speech-only; they never hard-fail.

| Field | Required | Notes |
| --- | --- | --- |
| `policy` | yes | `natural` \| `adaptive` \| `structured` (Adaptive is product default) |
| `primary_text` | yes | Selected Source Transcript (primary provider rank) |
| `provider_state` | yes | Dual-STT agreement class from #138 |
| `surface_hint` | no | `shell`, `terminal`, `coding_agent`, `gui_agent`, `messaging`, `browser`, `unknown`, or null |
| `process_hint` | no | Optional `{class, name}` already known to the daemon (focus/process probe) |
| `timing` | no | Optional pause boundaries + certainty (`clear` \| `uncertain`) |
| `kind_hint` | no | Optional `everyday_message` \| `developer_prompt` when already classified; ignored if absent |

Speech-only fallback: `surface_hint=null`, `process_hint=null`, `timing=null` — decisions still fire from text shape + policy + STT state.

## Decision outputs

| Field | Values |
| --- | --- |
| `route` | `literal_identity` \| `deterministic_local` \| `local_with_optional_cloud` |
| `cloud_request` | `not_allowed` \| `allowed` \| `required` |
| `rule_id` | First matching ordered rule id (reproducible) |
| `complexity_score` | Non-negative integer from explicit weights |
| `contributions` | Ordered list of `{signal, weight, detail}` that built the score |
| `surface_degraded` | true when surface/process were absent |

Binding with #138:

- `route=literal_identity` ⇒ `cloud_request=not_allowed`.
- `route=deterministic_local` ⇒ `cloud_request=not_allowed` (local fast path).
- `route=local_with_optional_cloud` ⇒ `cloud_request` is `allowed` (dispute / Adaptive complex) or `required` (Structured complex).
- Natural policy never selects a cloud attempt (even multi-section speech stays local Natural-shaped).
- Structured policy: routing decides local vs cloud **attempt** only; closed labels are applied by a later renderer and are never invented here.

## Ordered rules (first match wins)

Priority is hard and deterministic. Weights apply only inside the complexity rule.

| Order | Rule id | Condition | Route | Cloud |
| ---: | --- | --- | --- | --- |
| 1 | `R_DISPUTE_CLOUD` | `provider_state ∈ {protected_token_disagreement, semantic_disagreement}` **and** policy ≠ `natural` | `local_with_optional_cloud` | `allowed` |
| 2 | `R_DISPUTE_POLICY_FORBID` | Same dispute states **and** policy = `natural` | `deterministic_local` | `not_allowed` |
| 3 | `R_LITERAL_PREFORMATTED` | Primary text already shows multi-line numbered/bullet structure | `literal_identity` | `not_allowed` |
| 4 | `R_LITERAL_COMMAND` | Command-shaped speech **and** (shell/terminal surface **or** process class shell/terminal **or** strong CLI flag evidence) | `literal_identity` | `not_allowed` |
| 5 | `R_NATURAL_LOCAL` | policy = `natural` | `deterministic_local` | `not_allowed` |
| 6 | `R_COMPLEX_CLOUD` | `complexity_score ≥ COMPLEXITY_CLOUD_THRESHOLD` (24) | see policy row | see policy row |
| 7 | `R_DEFAULT_LOCAL` | otherwise | `deterministic_local` | `not_allowed` |

Dispute states are evaluated **before** literal preformatted/command so Adaptive/Structured protected-token disagreement on a numbered list or `cargo test …` stays cloud-eligible. Natural still forbids cloud (`R_DISPUTE_POLICY_FORBID`).

### Policy row under `R_COMPLEX_CLOUD`

| Policy | Route | Cloud |
| --- | --- | --- |
| `adaptive` | `local_with_optional_cloud` | `allowed` |
| `structured` | `local_with_optional_cloud` | `required` |
| `natural` | unreachable (caught by `R_NATURAL_LOCAL`) | — |

Agreement classes that **do not** open a cloud path by themselves:

- `exact_agreement`
- `punctuation_only_agreement`
- `safe_complementary`
- `single_provider`

Pause timing **never** opens a cloud path. Clear long pauses may later affect local layout only; uncertain pauses stay Natural-shaped local. Timing is recorded in contributions for diagnostics but weight toward cloud is **0**.

## Explicit complexity weights

All weights are fixed integers. No randomness. Score = sum of contributions.

### Section cues (speech shape)

Closed structural cue phrases aligned with #138 labels. Cues count only when they look like **section headers**, not every mid-sentence noun.

**Header patterns** (any one):

- start of utterance or after sentence pause (`.`, `!`, `?`, newline)
- colon form (`goal:`, `context:`)
- structural `the goal is` / `goal is` (and the same shape for other cues)

**Strong** cues may fire alone. **Weak** cues (common nouns) fire only when ≥1 strong cue is already present.

**Multi-section stream:** when the utterance **starts** with a strong section label, subsequent bare catalog labels in the same utterance are collected as free-standing introducers (spoken dictation like `goal … context … requirements …`). Trailing NP heads and determiner/compound-prefixed forms are excluded.

| Signal id | Weight | Strength | Match |
| --- | ---: | --- | --- |
| `section_goal` | 12 | strong | `goal` |
| `section_context` | 12 | strong | `context` |
| `section_requirements` | 12 | strong | `requirements` |
| `section_constraints` | 12 | strong | `constraints` |
| `section_acceptance_criteria` | 14 | strong | `acceptance criteria` |
| `section_steps` | 12 | weak | `steps` |
| `section_files` | 10 | weak | `files` |
| `section_notes` | 10 | weak | `notes` |

Distinct cue hits only (each cue contributes at most once). Ordinary compound prose such as “the project goal depends on business context” scores **0** section weight and stays local.

### Length assists (only when ≥2 distinct section cues already present)

| Signal id | Weight | Condition |
| --- | ---: | --- |
| `words_ge_40` | 4 | word_count ≥ 40 and section_cue_count ≥ 2 |
| `words_ge_80` | 6 | word_count ≥ 80 and section_cue_count ≥ 2 |

Bare long everyday prose without section structure does **not** accumulate these assists and must not cloud.

### Surface / process assists (optional; never sole cloud cause)

Applied only when the hint is present. Missing hints contribute nothing (`surface_degraded=true`).

| Signal id | Weight | Condition |
| --- | ---: | --- |
| `surface_coding_agent_sections` | +4 | surface=`coding_agent` and section_cue_count ≥ 1 |
| `surface_gui_agent_sections` | +3 | surface=`gui_agent` and section_cue_count ≥ 1 |
| `surface_messaging_short` | −6 | surface=`messaging`, word_count < 30, section_cue_count = 0 |
| `surface_browser_short` | −4 | surface=`browser`, word_count < 25, section_cue_count = 0 |
| `process_coding_boost` | +2 | process.class in `{coding_agent, gui_agent}` and section_cue_count ≥ 1 |

Floor: complexity score is clamped to ≥ 0 after negative assists.

### Threshold

```
COMPLEXITY_CLOUD_THRESHOLD = 24
```

Rationale (aligned with #138):

- One section only (`goal …`) → score 12 (+ optional surface 4) = 16 < 24 → **local**.
- Two sections (`goal` + `context`) → 24 → **cloud-eligible** under Adaptive/Structured.
- Full eight-section developer dictation → well above threshold.

## Command / preformatted detection (literal path)

### Preformatted (`R_LITERAL_PREFORMATTED`)

True when primary text contains a newline and matches at least one of:

- Multi-line numbered list: `^\s*\d+[\.)]\s+\S` on ≥2 lines
- Multi-line bullets: `^\s*[-*]\s+\S` on ≥2 lines

### Command-shaped (`R_LITERAL_COMMAND`)

True when speech looks like a **real CLI invocation**, not everyday English where a runner word is only a verb. Require **positive** CLI evidence — do **not** treat any bare word after a runner as CLI.

**Any** of:

1. **CLI flag evidence** — any token matching `^--?[A-Za-z0-9][\w-]*$` or double-dash `^--[\w-]+(?:=.*)?$`.
2. **Leading runner + positive follow-on** — first token ∈ known runners **and** the second token is one of: known CLI subcommand (`test`, `build`, `install`, `status`, …), nested runner (`run cargo …`), flag-shaped token, or a path-like token is present anywhere. Bare single-token runners (`cargo`) count. Everyday seconds (`make sure…`, `go ahead…`, `run this/by…`) reject.
3. **Runner in first three tokens + CLI follow-on** — a known runner appears among the first three tokens **and** immediately after it there is a known CLI subcommand or nested runner, **or** a path-like token is present anywhere (absolute `/…`, `./…`, `../…`, `~/…`, bazel `//target`, slash-containing paths, or common source extensions like `.rs` / `.py`).

Known runners (must match prototype `RUNNER_TOKENS`): `run`, `cargo`, `npm`, `pnpm`, `yarn`, `git`, `docker`, `kubectl`, `make`, `python`, `python3`, `pip`, `curl`, `ssh`, `scp`, `go`, `bazel`, `ninja`.

**and** at least one of (command anchor):

- `surface_hint ∈ {shell, terminal}`
- `process_hint.class ∈ {shell, terminal}`
- Strong CLI evidence: ≥1 flag token matching `^--[\w-]+` (double-dash), which is rare in everyday speech

Prose spoken while focused on a shell is **not** command-shaped → stays `deterministic_local`. Examples that must stay local: “please restart the service when you can”, “make sure the service restarts after deploy”, “go ahead and restart when you can”, “run this by the team tomorrow”, “run errands tomorrow”, “make dinner later”, “go shopping”, “python is great”. True CLI still matches: `cargo test --package voisu-core`, `make install`, `go test ./…`, `run cargo test --workspace …`, `git status --short`.

## Negative cases (must hold)

| Id | Statement | Enforced by |
| --- | --- | --- |
| N1 | Do **not** cloud simple short everyday messages | `R_DEFAULT_LOCAL` + messaging assist; score ≪ 24 |
| N2 | Do **not** force cloud when a single provider (or exact/punct/complementary agreement) agrees on simple speech | Agreement alone never opens cloud; needs dispute or complexity |
| N3 | Do **not** skip cloud-eligibility when protected-token dual-STT disagreement exists **unless** policy forbids (`natural`) | `R_DISPUTE_CLOUD` / `R_DISPUTE_POLICY_FORBID` |
| N4 | Natural policy never attempts cloud, including multi-section developer speech | `R_NATURAL_LOCAL` before complexity |
| N5 | Pause timing alone never selects cloud | timing weight = 0 |
| N6 | Missing surface/process never hard-fails | degrade flags; speech-only path |

## Surface coverage matrix

| Surface / process | Role in routing |
| --- | --- |
| `shell` | Assists literal command identity when command-shaped |
| `terminal` | Same as shell (terminal harnesses) |
| `coding_agent` | Mild complexity assist when section cues present |
| `gui_agent` | Mild complexity assist when section cues present |
| `messaging` | Short-message bias toward local |
| `browser` | Short-form bias toward local |
| `unknown` / null | Speech-only universal fallback |

## Latency budget

| Step | Bound |
| --- | --- |
| Signal collection | O(n) over primary text length + O(1) already-held surface/process/timing structs |
| Scoring + rule walk | O(number of section cues + rules) — fixed catalogs |
| Network I/O | **None** on this path |
| Wall-clock sleeps | **Forbidden** in prototype tests |
| Delivery deadline | ≤ 1.5s end-to-end for the product; routing itself must leave headroom for local organize and optional cloud |

The prototype asserts that the router imports no network modules and performs no I/O beyond reading its sibling package files during self-check.

## Decision table (summary)

| Speech / STT / policy snapshot | Route | Cloud |
| --- | --- | --- |
| Already multi-line numbered/bullet list | `literal_identity` | `not_allowed` |
| Shell/terminal + `cargo test --workspace …` / `cargo test --package voisu-core` / `make install` | `literal_identity` | `not_allowed` |
| Strong `--flags` command shape, no surface | `literal_identity` | `not_allowed` |
| Shell + prose (`make sure…` / `go ahead…` / `run this by…` / `run errands…` / `make dinner…` / “please restart…”) | `deterministic_local` | `not_allowed` |
| “hey can you send the notes…”, Adaptive, single STT | `deterministic_local` | `not_allowed` |
| Dual-STT exact / punct / complementary agreement | `deterministic_local` | `not_allowed` |
| Protected-token or semantic disagreement, Adaptive/Structured | `local_with_optional_cloud` | `allowed` |
| Same disagreement on preformatted list or cargo CLI, Adaptive | `local_with_optional_cloud` | `allowed` |
| Same disagreement, Natural (including preformatted/CLI) | `deterministic_local` | `not_allowed` |
| Multi-section developer, Adaptive | `local_with_optional_cloud` | `allowed` |
| Multi-section developer, Structured | `local_with_optional_cloud` | `required` |
| Multi-section developer, Natural | `deterministic_local` | `not_allowed` |
| Single “goal fix the flaky auth test”, Adaptive | `deterministic_local` | `not_allowed` |
| Ordinary compound “the project goal depends on business context” | `deterministic_local` | `not_allowed` |
| Clear or uncertain pause, simple speech | `deterministic_local` | `not_allowed` |
| Long everyday prose, no section cues | `deterministic_local` | `not_allowed` |
| Speech-only (no surface), complex multi-section, Adaptive | `local_with_optional_cloud` | `allowed` |

## Alignment with #138 route labels

Fixtures cross-link to DPR ids where the routing slice is comparable:

| #141 fixture | #138 witness | Shared claim |
| --- | --- | --- |
| IRI-01 | DPR-01 | Simple Adaptive everyday → local |
| IRI-03 | DPR-04 | Preformatted list → literal |
| IRI-04 | DPR-18 | Shell command → literal |
| IRI-06 | DPR-31 | Exact dual-STT → local fast path |
| IRI-07 | DPR-32 | Punctuation-only agreement → local |
| IRI-08 | DPR-33 | Safe complementary → local |
| IRI-10 | DPR-34 | Protected-token disagreement → optional cloud |
| IRI-11 | DPR-35 | Semantic disagreement → optional cloud |
| IRI-13 | DPR-37 / DPR-46 | Complex Adaptive → optional cloud |
| IRI-14 | DPR-39 / DPR-43 | Complex Structured → required cloud attempt |
| IRI-15 | DPR-26 | Complex Natural → local, no labels/cloud |
| IRI-16 | DPR-27 | Simple developer Adaptive → local Natural-shaped |
| IRI-17 | DPR-28 | Simple Structured section → local (no cloud) |
| IRI-18 / IRI-19 | DPR-06 / DPR-07 | Timing does not force cloud |

Cloud **outcomes** (succeeded / rejected / skipped / deadline) remain #138/#139 concerns. This package only decides whether a cloud attempt is **allowed/required/not_allowed**.

## Ambiguities resolved conservatively

1. **Surface-only coding_agent with no section cues** → local, not cloud (speech shape wins).
2. **Prose in a shell window** → not literal unless command-shaped.
3. **Natural + dual-STT protected disagreement** → policy forbids cloud; stay local with primary rank (N3 exception).
4. **Ordinary nouns vs section introducers** — `goal` / `context` / `requirements` / `constraints` / `acceptance criteria` are **strong** cues. `steps` / `files` / `notes` are **weak** (common nouns) and count only when at least one strong cue is already present. Cues require header structure (start/pause/colon/`X is`) or multi-section stream after a leading section label. Mid-sentence compounds like “project goal” / “business context” / `release notes` / `next steps` do not open cloud.
5. **`kind_hint` absent** — inferred only via section cues / length; never required.
6. **Threshold ties** — score ≥ 24 is inclusive (two 12-weight strong cues cloud-eligible).
7. **Structured simple** — local route; Structured rendering may still emit a single source-supported label **locally** later; routing does not invent labels and does not force cloud for one section.

## Handoff notes for #144

1. Promote `COMPLEXITY_CLOUD_THRESHOLD = 24` and the ordered rule table into the approved architecture spec as the binding local fast-path policy.
2. Keep cloud request state orthogonal: routing emits `allowed|required|not_allowed`; the runtime still applies ≤1 cloud call, 1.5s budget, and hierarchical local fallback from #138/#139.
3. Surface/process probes must remain **best-effort** and optional; speech-only path is production-mandatory.
4. Do not let timing, messaging surface, or single-provider availability open cloud.
5. Wire diagnostics: emit `rule_id`, `complexity_score`, and `contributions` (no secrets, no full prompts beyond existing transcript policy).
6. Implementation tickets should port the prototype function, not re-derive weights from scratch; any threshold change needs new fixtures + dual review.
7. Structured label inventory stays closed (`Goal`…`Notes`); routing never adds labels.

## Verification

```bash
python3 docs/research/developer-prompt-rendering-intent-routing-prototype-2026-08-11.py
python3 -m json.tool docs/research/developer-prompt-rendering-intent-routing-schema-2026-08-11.json >/dev/null
python3 -m json.tool docs/research/developer-prompt-rendering-intent-routing-corpus-2026-08-11.json >/dev/null
```

Exit 0 required when the package is healthy.
