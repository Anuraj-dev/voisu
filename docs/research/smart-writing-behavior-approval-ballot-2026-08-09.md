# Smart Writing behavior approval record

**Issue:** [#99](https://github.com/Anuraj-dev/voisu/issues/99) · Parent map [#96](https://github.com/Anuraj-dev/voisu/issues/96)
**Corpus:** `smart-writing-behavior-corpus-2026-08-09.json` · Schema: `smart-writing-behavior-schema-2026-08-09.json`
**Approval state:** `approved` by Raja on 2026-08-09
**Version:** `3.1.0-approved`

## Approved decisions

| Decision | Approved option | Product behavior |
|----------|-----------------|------------------|
| `D_cmd` | `D_cmd-A` | Formatting commands require the explicit `command <phrase>` introducer. Bare command-like words remain ordinary speech. |
| `D1` | `D1-B` | When an explicit paragraph command fires, Smart also punctuates multi-word fragments. |
| `D3` | `D3-B` | Smart may infer a list when the **Validated Transcript itself** clearly expresses list intent. Ambiguous prose remains prose. |
| `D4` | `D4-A` | Preserve casing inside quotations. |
| `D5` | `D5-A` | Preserve double negatives and the speaker's intended voice. |
| `D10` | `D10-B` | Do not invent special email greeting/blank-line structure in v1; keep a punctuated single paragraph. |
| `D14` | `D14-A` | Use the Oxford comma in Smart serial lists. |

Compact selection: `D_cmd-A, D1-B, D3-B, D4-A, D5-A, D10-B, D14-A`.

## Meaning of “context-aware lists”

List inference may use only the English **Validated Transcript** currently being rendered. It does not
authorize reading the active application, field type, screen, clipboard, surrounding document text, or
any other application context. Clear list-like utterances may become bullets; ambiguous phrases fail
closed to ordinary prose. #100 defines the safety representation and #103 defines the executable
conservative inference contract and tests.

## Approved corpus state

| Metric | Value |
|--------|------:|
| Fixtures total | 51 |
| Fixtures locked | 51 |
| Fixtures open | 0 |
| Decisions total | 7 |
| Decisions pending | 0 |

Every formerly open fixture now contains only the exact Literal/Smart output selected by the approved
decision tuple. The JSON corpus is the canonical machine-readable behavior record.

## Scope boundary

This approval settles the exact Smart/Literal examples requested by #99. It does not itself define:

- structured patch representation, protected spans, or rejection policy — issue #100;
- runtime thresholds, parser/evaluator implementation, diagnostics, or release gates — issue #103;
- application-aware mode switching or application-context collection — out of scope for map #96.

With these choices recorded and all fixture alternatives resolved, issue #99 is ready to close and #100
is unblocked.
