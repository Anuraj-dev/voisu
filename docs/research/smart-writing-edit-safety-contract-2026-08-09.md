# Smart Writing grammar-edit safety contract (#100 architecture reset)

**Issue:** [#100](https://github.com/Anuraj-dev/voisu/issues/100) · parent [#96](https://github.com/Anuraj-dev/voisu/issues/96) · blocked by [#99](https://github.com/Anuraj-dev/voisu/issues/99) · blocks [#103](https://github.com/Anuraj-dev/voisu/issues/103)

**Artifacts:** `smart-writing-edit-safety-{schema,corpus,prototype}-2026-08-09.*`

**Status:** approved research proof; Raja selected B1-A, B2-A, and B3-A on 2026-08-09. #103 owns release thresholds.

The approved #99 product lock is unchanged: `D_cmd-A, D1-B, D3-B, D4-A, D5-A, D10-B, D14-A`.

## Reset: formatting is not an untrusted edit

The earlier draft mixed formatting edits and model grammar edits in one JSON candidate. Its
`F_trusted_precomputed` label let a candidate replace the whole transcript without proving who
created that replacement. That is an authority bug, not a missing predicate. The reset deletes the
mechanism.

```text
ValidatedTranscript
        │
        ├── deterministic local formatter ──> FormattingBaseline (sealed/typed)
        │                                      │
        └── grammar request ──> untrusted JSON GrammarCandidate
                                               │
                         validate against ValidatedTranscript
                                               │
                         compose through baseline source anchors
                                               │
                                      RenderedTranscript
```

`FormattingBaseline` is created only by the deterministic formatter. It contains the exact base
identity, rendered text, formatter-owned source anchors, complete formatter-owned quote/code source
ranges, a formatter contract ID, and a structural derivation digest. It is immutable and never
deserialized from provider JSON. In Rust, its fields and constructor must remain private to the
formatting module; consumers receive the value through a typed interface. The Python proof models
this with a sealed constructor, verifies the capability structure and digest before use, and rejects
dictionaries, strings, stale values, or tampered capabilities.

The safety gate therefore has no formatting rule IDs, no formatting edits, and no whole-text rewrite
operation. Exact command whitespace, paragraphs, quotes, lists, casing, and punctuation are local
formatter behavior, tested separately against approved #99 outputs. `D3-B` list inference sees only
the Validated Transcript; ambiguous prose such as `buy milk when hungry` stays prose.

## Grammar candidate

Provider JSON contains only:

- the base `version` and SHA-256 fingerprint;
- an ordered list of localized grammar edits;
- for each edit: a diagnostic ID, closed `rule_id`, UTF-8 half-open byte range, exact `before`, and
  exact `after`.

The closed research catalog is deliberately narrow:

| Rule | Accepted context |
|---|---|
| `G_THERE_IS_PLURAL_QUANTITY` | whole-token `is` → `are`, immediately after `there`, immediately before the safe quantity domain `two`…`twelve` / `2`…`12`, followed by the explicit approved count noun `issues` |
| `G_LETS_MEET_CONTRACTION` | sentence-initial whole-token `lets` → `let's`, immediately before `meet` |
| `G_DIDNT_APOSTROPHE` | whole-token `didnt` → `didn't`; punctuation only, never removal of negation |

This accepts the approved #99 examples while rejecting `the price is two dollars`, `there is 0 gas`,
mass/singular-s nouns, unlisted plural nouns, and `the app lets users export`. The count-noun catalog
is intentionally not inferred from suffixes. Widening a rule requires new adversarial evidence in
#103; a generic grammar rewrite rule is out of scope.

## Validation and composition

Validation order is deterministic: candidate container/envelope shape and size, freshness, individual
edit shape in input order, UTF-8/token anchors, protected spans, closed rule predicate, formatter
anchor mapping, then overlap. A stale envelope therefore wins over malformed edit contents, while an
unreadable/malformed envelope still fails before freshness. Errors retain first-seen order.
Diagnostics are scrubbed and bounded.

- The Validated Transcript is immutable evidence. Every grammar anchor is checked against it.
- Non-zero spans must cover exactly one whole lexical token. Zero-width grammar edits are forbidden.
- Names, dictionary terms, identifiers, URLs, paths, numbers, dates, explicit command phrases,
  negations, and prompt-shaped text are protected. The formatting capability additionally protects
  the full source ranges for paired ASCII single/double quotes, paired curly single/double quotes,
  paired inline/fenced code, and `command quote … command unquote`, including multiline interiors.
  ASCII/curly apostrophes between word characters remain apostrophes, not opening quotes. Ambiguous
  unmatched quotation delimiters protect the whole base rather than guessing.
- A fresh typed baseline is authoritative. Grammar never regenerates or replaces it.
- Formatter source anchors map an accepted base token into the baseline. Missing or ambiguous anchors
  reject the whole grammar candidate and retain formatting.
- Formatter casing wins during the two approved same-token compositions (`Lets` + apostrophe becomes
  `Let's`; `Didnt` + apostrophe becomes `Didn't`). No other overlap exists because formatting is not
  represented as edits at this gate.
- Same-candidate overlap, duplicate ranges, unsorted edits, stale envelopes, unknown rules, malformed
  fields, or any invalid edit reject the **whole grammar candidate**.

The fallback matrix is small:

| Condition | Result |
|---|---|
| fresh safe grammar | baseline plus accepted grammar (`both` or `grammar_only`) |
| stale, malformed, unsafe, overlapping, or unmappable grammar | unchanged typed baseline (`formatting_only` or `unchanged`) |
| missing/wrong-type/stale formatting capability | unchanged Validated Transcript; this is an internal contract failure, not provider authority |

Invalid grammar can never suppress valid formatting because the two authorities are no longer in one
candidate. A duplicate grammar offset, for example, rejects grammar after the formatter has already
produced its baseline.

## Schema authority and proof

The JSON Schema describes the research corpus. The runner implements the declared Draft 2020-12
subset with Python's standard library and fails on unsupported validation keywords. That validator is
the authoritative corpus gate; optional `jsonschema` behavior is not used. File loading rejects
oversize/deep JSON and converts decoder/depth `RecursionError` or `MemoryError` into bounded proof
failures. If schema validation fails, semantic metadata, #99 cross-links, and fixtures do not run.

Run:

```bash
python3 docs/research/smart-writing-edit-safety-prototype-2026-08-09.py
```

The proof checks every corpus fixture, exact #99 cross-links, authoritative schema rejection cases,
typed-baseline forgery, whole-text rewrite attempts, stale candidates, invalid-at-duplicate-offset
fallback, context false positives, protected spans, Unicode boundaries, deterministic error order,
and pretty-printed JSON. It is hermetic and does not call Groq or mutate product code.

## Approved failure policy

| ID | Selected | Failure behavior |
|---|---|---|
| **B1-A** | yes | One invalid grammar edit rejects the whole grammar candidate; keep formatting. |
| **B2-A** | yes | Stale grammar keeps a fresh typed formatting baseline. |
| **B3-A** | yes | Any protected-span hit rejects the whole grammar candidate; keep formatting. |

#103 must turn these approved choices into product tests and select release limits. This prototype proves
the authority boundary and failure behavior; it does not implement the production formatter or model
prompt.
