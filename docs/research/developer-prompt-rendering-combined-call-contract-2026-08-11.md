# Developer Prompt Rendering combined-call contract (#139)

**Issue:** [#139](https://github.com/Anuraj-dev/voisu/issues/139) · parent map [#133](https://github.com/Anuraj-dev/voisu/issues/133) · blocked by [#138](https://github.com/Anuraj-dev/voisu/issues/138) · blocks [#140](https://github.com/Anuraj-dev/voisu/issues/140), [#142](https://github.com/Anuraj-dev/voisu/issues/142)

**Artifacts:** `developer-prompt-rendering-combined-call-{schema,corpus,prototype}-2026-08-11.*`

**Status:** research proof package. Implements the approved #138 behavior corpus *as a structured one-call validator + hierarchical fallback composer*, without a grammar subsystem. Live model benchmark is **#140**.

Governing product lock (#137 / #135 / #133): English-first organize-only; preserve wording/spoken grammar; punctuation, layout, closed cue conversion, and closed labels only; Adaptive default; at most one structured cloud call; hierarchical fallback; final-only Delivery ≤1.5s; never auto-send / live-type / replace delivered text.

## Answer (ticket question)

The smallest sufficient contract is:

1. **One structured JSON candidate** with separately encoded fields for STT reconciliation, clear filler/backtrack removals, closed symbol/format conversions, layout decision, optional closed Structured labels, and an ordered **source-derivation proof**.
2. **No unchecked polished string as authority.** Composition concatenates derivation `output_text` spans; the validator proves every keep/remove/convert/label span against named STT sources.
3. **Protected-token gate** on names, numbers/dates/times, negations, commands/flags, URLs/paths, identifiers, code, and quotes (fixture-declared exact substrings).
4. **Hierarchical fallback composition** that prefers the deterministic local baseline on hard failure, preserves words on uncertain backtracking, and forces Natural on uncertain layout.
5. **Three compact model prompt contracts** (Gemini 3.5 Flash-Lite, Gemini 3.6 Flash, Groq GPT-OSS-20B) that share one semantic schema — prompt text only here.

This package deliberately does **not** resurrect Smart Writing grammar edits, whole-text rewrite, enrichment, command mode, or auto-send.

## Pipeline

```text
STT Source Transcript(s)
        │
        ├── deterministic local baseline ──> LocalBaseline (always available)
        │
        └── optional one structured cloud call
                    │
                    ▼
           CombinedCallCandidate (untrusted JSON)
                    │
         validate shape → freshness → closed catalogs
         → source derivation → protected tokens
         → invented-content / label safety
                    │
         hierarchical compose / fallback
                    │
                    ▼
           Final Transcript (Delivery unsent)
```

Cloud is conditional. Simple undisputed speech stays on the local fast path (#138 `deterministic_local` / `literal_identity`). Cloud-optional fixtures exercise this contract.

## Combined-call response shape

```json
{
  "schema_version": "1",
  "base_fingerprint": "sha256:<selected_source_utf8>",
  "reconciliation": {
    "selected_provider": "provider_a",
    "reason": "only_available"
  },
  "removals": [
    {
      "kind": "filler",
      "certainty": "clear",
      "source_provider": "provider_a",
      "source_span_text": "um"
    }
  ],
  "conversions": [
    {
      "id": "exclamation point→!",
      "source_provider": "provider_a",
      "source_span_text": "exclamation point"
    }
  ],
  "layout": {
    "decision": "natural",
    "certainty": "clear"
  },
  "labels": [
    {
      "label": "Goal",
      "source_provider": "provider_a",
      "source_span_text": "goal fix the flaky auth test"
    }
  ],
  "derivation": [
    {
      "kind": "keep",
      "source_provider": "provider_a",
      "source_text": "ship it",
      "output_text": "Ship it",
      "conversion_id": null,
      "label": null
    },
    {
      "kind": "convert",
      "source_provider": "provider_a",
      "source_text": "exclamation point",
      "output_text": "!",
      "conversion_id": "exclamation point→!",
      "label": null
    }
  ]
}
```

### Field rules (smallest sufficient)

| Field | Role |
|---|---|
| `schema_version` | Fixed `"1"`. |
| `base_fingerprint` | SHA-256 of the selected source UTF-8 text. Stale → reject. |
| `reconciliation` | STT choice + closed reason (must agree with host selection evidence). |
| `removals` | Clear/uncertain filler or backtrack spans. Uncertain backtrack is never applied. |
| `conversions` | Closed catalog only; each cue span must exist in source. |
| `layout` | `natural` \| `multi_paragraph` \| `numbered` \| `structured_sections` with certainty. Uncertain → Natural. |
| `labels` | Closed set only; each needs source span evidence. Natural policy rejects structural headers. |
| `derivation` | Ordered proof. Concatenating `output_text` reconstructs the proposal. |

There is **no** free-form `final` string field with independent authority.

### Closed catalogs

**Labels:** Goal, Context, Requirements, Constraints, Steps, Acceptance Criteria, Files, Notes.

**Conversions (research catalog aligned to #138):**

- `period→.`, `exclamation point→!`, `new line→\n`, `new paragraph→\n\n`
- `quote…unquote→"…"`
- `one→1.`, `two→2.`, `three→3.`, `four→4.`
- spoken section cues → closed labels
- `spoken steps cue→numbered_lines`

### Source-derivation proof

For every derivation span with kind ∈ {`keep`, `remove`, `convert`, `label`}:

1. `source_provider` names an available STT source.
2. `source_text` is a case-insensitive substring of that source (whitespace-normalized search is allowed for multi-word spans).
3. `convert` spans must reference a declared conversion id whose **catalog cue is covered by that span’s `source_text`** (ellipsis cues require every part, e.g. `quote` and `unquote`). Convert `output_text` must equal the conversion RHS (or the RHS with only surrounding whitespace); quote-style RHS templates expand `…` from the source interior.
4. `label` spans must emit the exact closed header form `Label:\n` (or inline `Label:`) matching `label`.
5. `layout_break` spans may emit only whitespace newlines and need no source text. Under **clear `natural` layout**, multi-paragraph output is rejected (`E_UNSAFE_SEMANTICS`) whether it arrives as a single `layout_break` (`\n\n`), adjacent single-newline spans that compose to a blank line, a keep `output_text` that embeds `\n\n`, or a `new paragraph→\n\n` convert. Multi-paragraph layout must be declared on `layout.decision` as `multi_paragraph` or `structured_sections` (not clear `natural`).
6. `remove` spans must have empty `output_text` and **match a declared `removals[]` entry** (same provider + source span). A remove span without a matching declaration is `E_UNVERIFIABLE`.
7. Consuming spans must not double-claim the same source region (`E_OVERLAP`).
8. Accept-path **keep** spans are organize-only: `output_text` must preserve the ordered content-word sequence of `source_text` (case/punctuation/whitespace may change; free rephrase that drops span content words is `E_INVENTED_CONTENT` / `E_UNSAFE_SEMANTICS`).

**Invented-content check:** every ordinary lexical atom in the composed render must be licensed by (a) a source atom, (b) a closed conversion RHS, or (c) a closed label token. Anything else is `E_INVENTED_CONTENT` / `E_UNSAFE_SEMANTICS`.

**Reconciliation:** `reconciliation.selected_provider` must equal the host `source_selection.selected_provider`. With a single available provider, `only_available` must name that provider (`E_RECONCILE` otherwise).

### Protected-token policy

Fixture (and later product) declares exact protected substrings covering:

- names
- numbers / dates / times
- negations
- commands / flags
- URLs / paths
- identifiers
- code
- quote interiors

Accepted renders must contain each protected token **exactly** (case-sensitive). Missing or altered tokens reject the whole candidate (`E_PROTECTED`).

## Hierarchical fallback composition

Order is deterministic:

| Priority | Condition | Decision | Final text |
|---|---|---|---|
| 0 | `cloud_outcome` ∈ {`skipped`} | `fallback_baseline` | local baseline (no error) |
| 1 | `schema_failure` / missing candidate when required | `fallback_baseline` | local baseline + `E_SCHEMA` |
| 2 | `provider_failure` | `fallback_baseline` | local baseline + `E_PROVIDER` |
| 3 | `deadline_exceeded` | `fallback_baseline` | local baseline + `E_DEADLINE` |
| 4 | malformed / stale / unknown catalog / unverifiable | `fallback_baseline` | local baseline |
| 5 | protected-token hit or invented content / unsafe polarity | `fallback_baseline` | local baseline + `E_UNSAFE_SEMANTICS` |
| 6 | non-closed structural header | `fallback_baseline` | local baseline + `E_INVALID_LABEL` |
| 7 | uncertain backtrack removals only soft issue | `accept_preserve_words` | local baseline Natural preserve-all words |
| 8 | uncertain layout only soft issue | `accept_natural_layout` | Natural render (no multi-paragraph/structured force) |
| 9 | otherwise | `accept` | composed derivation |

Hard failures never partially apply unsafe semantics. Soft salvage exists only for **uncertain backtracking** (preserve words) and **uncertain layout** (Natural), matching #137.

Delivery is always:

```json
{ "state": "unsent", "auto_send": false, "live_type": false, "replace_delivered": false }
```

## Model-specific prompt contracts (text only)

Shared semantic contract; three compact prompt pairs live in the corpus under `model_prompt_contracts`. Summary:

| Model | Provider | Prompt posture |
|---|---|---|
| `gemini-3.5-flash-lite` | Google | Minimal spans; default Natural; latency-first. |
| `gemini-3.6-flash` | Google | Same schema; allowed richer multi-section proofs when source-evidenced. |
| `openai/gpt-oss-20b` | Groq | Same organize-only JSON; bind structured outputs when available. |

**Do not run live cloud benchmarks in #139.** #140 owns quality/latency/quota comparison under this exact schema and the #138 oracle strings.

### Compact system prompt (shared core)

> You organize English speech into text. Preserve wording and spoken grammar. Do not invent requirements, paraphrases, explanations, or technical assumptions. Allowed: punctuation/casing, clear filler or clear backtrack removals, closed symbol/format cue conversions, layout (natural / multi-paragraph / numbered / structured_sections), and closed labels only when Structured or clearly licensed. Never auto-send. Return structured JSON decisions only — never a free-form polished string as sole authority.

### Compact response instructions (shared core)

> Return JSON with `schema_version="1"`, `base_fingerprint`, `reconciliation`, `removals`, `conversions`, `layout`, `labels`, and ordered `derivation`. Concatenating derivation `output_text` must reconstruct the proposal. Every keep/remove/convert/label `source_text` must be a substring of the named provider source. Uncertain backtrack: do not remove words. Uncertain layout: natural only. Closed labels and conversions only.

Per-model notes in the corpus add latency vs multi-section emphasis without changing semantics.

## Validation authority

- JSON Schema file describes the package and candidate shapes.
- The **stdlib prototype** is the authoritative gate: exact key sets, composition, protected tokens, fallback matrix, and property-bound mutations.
- Optional third-party JSON Schema engines are not required.

Run:

```bash
python3 docs/research/developer-prompt-rendering-combined-call-prototype-2026-08-11.py
```

## Relationship to #138 and #100

- **#138** owns spoken-input → Final Transcript behavior oracles (`DPR-*`). This package links fixtures via `related_behavior_fixture_ids` and must not contradict those finals for cloud-success paths.
- **#100** is historical shape only (sealed baseline, untrusted patches, whole-candidate rejection). This package reuses the *authority separation* idea and **rejects** grammar rule catalogs / grammar edits.

## What #140 and #142 should inherit

- **#140 (benchmark):** exact candidate schema, three prompt contracts, 1.5s deadline, fallback matrix, #138 oracle finals for success/fallback branches, no grammar.
- **#142 (integration / production wiring):** compose path (local baseline always first), validator gates before Delivery, unsent Delivery flags, skip cloud on simple local routes, retain diagnostics without auto-upgrade of delivered text.

## Explicit non-goals

- Grammar correction or scoring
- Prompt enrichment inventing requirements/edge cases
- Command/rewrite mode
- Auto-send, live typing, production late-result replacement
- Multi-call cloud pipelines
- Non-English rendering in this milestone
