# Grok 4.6 vs Sol medium first-pass review — 2026-08-13

## Decision

On [First/second/third numbered steps](https://github.com/Anuraj-dev/voisu/issues/186) / [PR #191](https://github.com/Anuraj-dev/voisu/pull/191), **Grok 4.6 stayed inside ticket intent. Sol medium did not.**

Both models agreed the locked Adaptive/Natural/Structured oracles pass. Sol both rounds asked for numbered-step conversion on the Literal route and labelled it P1 / fix-before-merge. Grok both rounds labelled that same change DISCARD: lists are organize, not spoken marks. The orchestrator discarded Sol's P1. No product code was changed from these four first-pass reviews.

Raja's rule for this map: do not apply a review finding that changes the intent of the ticket. Ticket 186 acceptance does not mention Literal. Spec Implementation Decisions bind Literal to spoken-mark conversion. `LiteralIdentity` is documented as marks / quotes / `new line` only.

## Setup

| Field | Value |
|---|---|
| Date | 2026-08-13 IST |
| Subject | `feat/first-second-third-numbered-steps` @ `e0cf413` |
| Prompt | identical for all four runs (`/tmp/voisu-186-review-prompt.md`) |
| Sol | `codex exec` · `gpt-5.6-sol` · `model_reasoning_effort=medium` · `--sandbox read-only` |
| Grok | `spawn_subagent` · `grok-4.6` · `capability_mode=read-only` |
| Rounds | 2 independent first-pass reviews per model, launched in parallel |

`codex exec review --base` cannot take extra instructions (`--base` conflicts with `[PROMPT]`). Sol therefore used the same agent prompt as Grok, not the native `review` subcommand.

## Wall clock and tokens

| Run | Start (IST) | End (IST) | Wall (s) | Tokens / tools |
|---|---|---|---:|---|
| Sol r1 | 12:29:09 | 12:31:39 | 149.49 | in 716,409 (cached 650,752) · out 4,302 · reasoning 1,685 |
| Sol r2 | 12:29:09 | 12:31:42 | 152.90 | in 676,924 (cached 594,944) · out 5,613 · reasoning 3,679 |
| Grok r1 | — | — | 347.91 | 54 tool calls · 1 turn |
| Grok r2 | — | — | 351.12 | 43 tool calls · 1 turn |

Sol was about 2.3× faster. Grok used far fewer tools and did not report token counts from the spawn path.

## Finding counts

| Run | Verdict | P0 | P1 | P2 | DISCARD |
|---|---|---:|---:|---:|---:|
| Sol r1 | fix-before-merge | 0 | 1 | 0 | 0 |
| Sol r2 | fix-before-merge | 0 | 1 | 0 | 0 |
| Grok r1 | ship | 0 | 0 | 0 | 7 |
| Grok r2 | ship | 0 | 0 | 0 | 6 |

## The one disagreement

**Should `first` / `second` / `third` become numbered lines in Literal writing mode?**

| Reviewer | Call | Quoted justification |
|---|---|---|
| Sol r1 and r2 | P1, in-intent | Spec solution list says the local baseline converts “these words anywhere, including Literal writing mode,” and the bullet list includes first/second/third. |
| Grok r1 and r2 | DISCARD | Literal is marks/quotes only. Numbered lines are structural organize. Ticket 186 acceptance does not mention Literal. |
| Orchestrator | **DISCARD** | Ticket 186 acceptance is the four Adaptive/human-readable oracles. Implementation Decisions say Literal is for spoken-mark conversion. User stories 20–21 say “marks” for Literal. Applying Sol's fix would change the documented `LiteralIdentity` contract. |

Sol labelled its own finding `In-intent: yes` and listed nothing to discard. That is the failure mode Raja called out on #190: the reviewer treats a wider reading of the spec as the ticket.

## What both models agreed

- The deployment oracle becomes three numbered lines under Adaptive, Natural, and Structured.
- `The first time I tried this` stays a sentence.
- `Cup, milk, eggs, bread` stays a sentence.
- `new line` / `new paragraph` still convert.
- Command-shaped `dash dash` is untouched.
- No Whisper-prompt, third-model, or default-on Qwen change.

## Grok-only DISCARD list (not applied)

Both Grok rounds also discarded, correctly:

- first+second without third
- mid-utterance / prefaced lists (`okay first…`)
- grocery bullets, `fourth`, `firstly`, `1st`
- teaching `steps one/two/three` to accept first/second/third
- changing cloud admission (later ticket)

## Orchestrator action

No code change from these four reviews. PR #191 stays on the locked ticket oracles. Cloud `@codex review` is the re-review for merge, with the same intent filter.

## Raw artifacts

Local only, not committed:

- `/tmp/voisu-186-review-prompt.md`
- `/tmp/voisu-186-reviews/sol-r1.md` and `sol-r2.md`
- `/tmp/voisu-186-reviews/sol-r1.jsonl` and `sol-r2.jsonl`
- `/tmp/voisu-186-reviews/grok-r1.md` and `grok-r2.md`
