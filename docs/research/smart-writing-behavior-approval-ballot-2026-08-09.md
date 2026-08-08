# Smart Writing behavior approval ballot

**Issue:** [#99](https://github.com/Anuraj-dev/voisu/issues/99) · Parent map [#96](https://github.com/Anuraj-dev/voisu/issues/96)
**Corpus:** `smart-writing-behavior-corpus-2026-08-09.json` · Schema: `smart-writing-behavior-schema-2026-08-09.json`
**Approval state:** `pending` — not approved; **#99 is not closable** from this artifact.
**Version:** `3.0.0-pending`

## Scope boundary (deliberate)

Issue **#99** asks Raja to approve **exact Smart/Literal behavior examples**.
It does **not** define a release-scoring engine, safety evaluator, edit representation, or command parser implementation.

| Later issue | Owns |
|-------------|------|
| **#100** | Machine safety / structured edit validation (protected spans, rejection policy, etc.) |
| **#103** | Implementation thresholds and runtime after examples are approved |

Exact expected strings here are **approval examples**, not CI/release claims.
`formatting_changes` / `grammar_changes` are concise human labels classifying proposed edits; they are not pass/fail formulas.

## Inventory counts

| Metric | Value |
|--------|------:|
| `fixtures_total` | 51 |
| `fixtures_locked` | 21 |
| `fixtures_open` | 30 |
| `decisions_total` | 7 |
| `decisions_pending` | 7 |

Inventory only. These counts are not release denominators. Exact expected strings are approval examples for issue #99, not CI/release claims. #100 defines machine safety/edit validation; #103 defines implementation thresholds after Raja approves.

## How to use this ballot

1. Review **locked** fixtures in the JSON (canonical single Literal/Smart pairs; no open decisions).
2. For each pending decision below, pick **exactly one** option id.
3. Open fixtures blocked by that decision list complete alternatives — including full cross-products where multiple decisions apply.
4. **D_cmd** is required before any command suite can be treated as settled; no command behavior is locked.
5. Recommendations are not approvals.

## Pending decisions

### D_cmd — Command syntax strategy

**Question:** How should explicit formatting commands be recognized in the Validated Transcript? This is a product choice; #99 does not lock a parser.

**Recommendation (not approval):** Prefer A (explicit introducer) for meaning safety; accept B only if Raja wants Wispr-like bare phrases and accepts noun-phrase ambiguity risk on counterexamples.

**Blocks open fixtures:** `F09`, `F09b`, `F10`, `F10b`, `F11`, `F12`, `F13`, `F13b`, `F14`, `F14b`, `F15`, `F15b`, `F16`, `F16b`, `F21`, `F21b`, `F22`, `F22b`, `F36`, `F36b`, `F37`, `F38`, `F39`, `F40`, `F41`, `F42`

| Option id | Label | Summary |
|-----------|-------|---------|
| `D_cmd-A` | Explicit introducer | Only `command <phrase>` fires. Bare lexicon phrases are ordinary speech. `literal command <phrase>` escapes to spoken words without firing. |
| `D_cmd-B` | Natural bare commands | Bare phrases may fire as commands with an explicit literal escape for word-sense. Accepts documented ambiguity risk on noun phrases (bullet train, brand new line, etc.). |

**Your choice:** `________`  _(pending)_

### D1 — Terminal punctuation after paragraph command

**Question:** When a new paragraph command fires, should Smart add terminal periods on short multi-word fragments?

**Recommendation (not approval):** Yes for fragments with two or more content words.

**Blocks open fixtures:** `F14`, `F14b`

| Option id | Label | Summary |
|-----------|-------|---------|
| `D1-A` | Breaks only | Emit paragraph break and case lines; no added periods. |
| `D1-B` | Smart terminal punct | Also add periods on multi-word fragments. |

**Your choice:** `________`  _(pending)_

### D3 — List inference without list commands

**Question:** May Smart invent bullets/lists without spoken number/bullet commands?

**Recommendation (not approval):** No for v1.

**Blocks open fixtures:** `F17`

| Option id | Label | Summary |
|-----------|-------|---------|
| `D3-A` | No inference | Prose commas only. |
| `D3-B` | Infer bullets | Allow Smart to invent list structure. |

**Your choice:** `________`  _(pending)_

### D4 — Capitalization inside quotes

**Question:** When quote commands fire, may Smart sentence-case a full quoted sentence?

**Recommendation (not approval):** Preserve validated in-quote casing by default.

**Blocks open fixtures:** `F21`, `F21b`

| Option id | Label | Summary |
|-----------|-------|---------|
| `D4-A` | Preserve in-quote casing | Outer clause only. |
| `D4-B` | Sentence-case full quotes | Also case inside full quoted sentences. |

**Your choice:** `________`  _(pending)_

### D5 — Double negatives

**Question:** Should Minimal Grammar standardize double negatives, or preserve spoken voice?

**Recommendation (not approval):** Preserve voice; only apostrophe/case.

**Blocks open fixtures:** `F27`

| Option id | Label | Summary |
|-----------|-------|---------|
| `D5-A` | Preserve double negative | I didn't see no error. |
| `D5-B` | Standardize negation | I didn't see any error. |

**Your choice:** `________`  _(pending)_

### D10 — Email greeting structure

**Question:** Should Smart invent a greeting line and blank line for hi <name> openings?

**Recommendation (not approval):** Only for clear hi/hello/dear openings; else single paragraph.

**Blocks open fixtures:** `F04`

| Option id | Label | Summary |
|-----------|-------|---------|
| `D10-A` | Structured email | Greeting + blank line + body. |
| `D10-B` | Single paragraph | Punctuate without invented blank lines. |

**Your choice:** `________`  _(pending)_

### D14 — Oxford comma

**Question:** Prefer Oxford comma in Smart serial lists?

**Recommendation (not approval):** Prefer Oxford comma.

**Blocks open fixtures:** `F18`

| Option id | Label | Summary |
|-----------|-------|---------|
| `D14-A` | Oxford comma | a, b, and c |
| `D14-B` | No Oxford comma | a, b and c |

**Your choice:** `________`  _(pending)_

## Open fixtures and alternatives

### `F09` · punctuation_command · decisions: `D_cmd`

**Input:** `ship it exclamation point`

**Roles:** must_change, command

**Rationale:** Named punctuation. A and B disagree on whether bare exclamation point is a command.

**Formatting change notes:** sentence case on Smart (both alts where applicable), terminal period when command does not fire (A), punctuation command render when bare fires (B)

**Grammar change notes:** _(none)_

#### `F09-A` — Strategy A (introducer-only): bare phrase does not fire; words remain.

```
LITERAL:
ship it exclamation point
SMART:
Ship it exclamation point.
```

#### `F09-B` — Strategy B (natural bare): bare exclamation point fires to !.

```
LITERAL:
ship it!
SMART:
Ship it!
```

### `F09b` · punctuation_command · decisions: `D_cmd`

**Input:** `ship it command exclamation point`

**Roles:** must_change, command

**Rationale:** Same intent with explicit introducer present in the Validated Transcript.

**Formatting change notes:** exclamation render when phrase fires, sentence case on Smart

**Grammar change notes:** _(none)_

#### `F09b-A` — Strategy A: introducer+phrase fires; command words consumed.

```
LITERAL:
ship it!
SMART:
Ship it!
```

#### `F09b-B` — Strategy B: word command stays; bare exclamation point still fires.

```
LITERAL:
ship it command!
SMART:
Ship it command!
```

### `F10` · punctuation_command · decisions: `D_cmd`

**Input:** `stop period new line next item`

**Roles:** must_change, command

**Rationale:** Stacked bare punctuation/line commands.

**Formatting change notes:** sentence case / line case when commands fire (B), terminal period when no fire (A)

**Grammar change notes:** _(none)_

#### `F10-A` — Strategy A: bare period/new line do not fire.

```
LITERAL:
stop period new line next item
SMART:
Stop period new line next item.
```

#### `F10-B` — Strategy B: bare period and new line fire.

```
LITERAL:
stop.
next item
SMART:
Stop.
Next item
```

### `F10b` · punctuation_command · decisions: `D_cmd`

**Input:** `stop command period command new line next item`

**Roles:** must_change, command

**Rationale:** Introducer form of period+newline stack.

**Formatting change notes:** period and newline renders, Smart line casing

**Grammar change notes:** _(none)_

#### `F10b-A` — Strategy A: introducer-gated period and new line fire.

```
LITERAL:
stop.
next item
SMART:
Stop.
Next item
```

#### `F10b-B` — Strategy B: command words remain as content; bare period/new line still fire around them.

```
LITERAL:
stop command.
command next item
SMART:
Stop command.
Command next item
```

### `F11` · punctuation_command · decisions: `D_cmd`

**Input:** `the period of the moon is twenty seven days`

**Roles:** must_preserve, command, command_safety_counterexample

**Rationale:** Noun-phrase counterexample for period. Both strategies propose preserve; B carries documented ambiguity risk if bare commands over-fire.

**Formatting change notes:** sentence case, terminal period

**Grammar change notes:** _(none)_

#### `F11-A` — Strategy A: bare period never fires (noun sense).

```
LITERAL:
the period of the moon is twenty seven days
SMART:
The period of the moon is twenty seven days.
```

#### `F11-B` — Strategy B desired preserve: product should still keep noun period (ambiguity risk acknowledged).

```
LITERAL:
the period of the moon is twenty seven days
SMART:
The period of the moon is twenty seven days.
```

### `F12` · punctuation_command · decisions: `D_cmd`

**Input:** `are you free question mark`

**Roles:** must_change, command

**Rationale:** Bare question mark command choice.

**Formatting change notes:** sentence case, question mark render under B

**Grammar change notes:** _(none)_

#### `F12-A` — Strategy A: bare question mark does not fire.

```
LITERAL:
are you free question mark
SMART:
Are you free question mark.
```

#### `F12-B` — Strategy B: bare question mark fires to ?.

```
LITERAL:
are you free?
SMART:
Are you free?
```

### `F13` · line_paragraph · decisions: `D_cmd`

**Input:** `first thought new line second thought`

**Roles:** must_change, command

**Rationale:** Mid-utterance new line.

**Formatting change notes:** sentence/line case, newline under B

**Grammar change notes:** _(none)_

#### `F13-A` — Strategy A: bare new line does not fire.

```
LITERAL:
first thought new line second thought
SMART:
First thought new line second thought.
```

#### `F13-B` — Strategy B: bare new line fires.

```
LITERAL:
first thought
second thought
SMART:
First thought
Second thought
```

### `F13b` · line_paragraph · decisions: `D_cmd`

**Input:** `first thought command new line second thought`

**Roles:** must_change, command

**Rationale:** Introducer form of mid-utterance new line.

**Formatting change notes:** newline, line casing

**Grammar change notes:** _(none)_

#### `F13b-A` — Strategy A: introducer+new line fires.

```
LITERAL:
first thought
second thought
SMART:
First thought
Second thought
```

#### `F13b-B` — Strategy B: command word kept; bare new line fires after it.

```
LITERAL:
first thought command
second thought
SMART:
First thought command
Second thought
```

### `F15` · line_paragraph · decisions: `D_cmd`

**Input:** `please press new line after the title when you typeset it`

**Roles:** must_preserve, command, command_safety_counterexample

**Rationale:** Meta speech about new line; must-preserve counterexample.

**Formatting change notes:** sentence case, terminal period

**Grammar change notes:** _(none)_

#### `F15-A` — Strategy A: bare new line does not fire; meta speech preserved.

```
LITERAL:
please press new line after the title when you typeset it
SMART:
Please press new line after the title when you typeset it.
```

#### `F15-B` — Strategy B desired preserve of meta speech (ambiguity risk if bare new line over-fires).

```
LITERAL:
please press new line after the title when you typeset it
SMART:
Please press new line after the title when you typeset it.
```

### `F15b` · line_paragraph · decisions: `D_cmd`

**Input:** `literal command new line then continue`

**Roles:** must_preserve, command, command_safety_counterexample

**Rationale:** Escape/meta forms for speaking command phrases as words. Alternatives are complete proposed renders under each strategy.

**Formatting change notes:** sentence case, terminal period

**Grammar change notes:** _(none)_

#### `F15b-A` — Strategy A: literal escapes introducer+phrase → words command new line remain.

```
LITERAL:
command new line then continue
SMART:
Command new line then continue.
```

#### `F15b-B` — Strategy B: literal is ordinary word; bare new line may still be intended as escape context — proposed preserve of words.

```
LITERAL:
literal command new line then continue
SMART:
Literal command new line then continue.
```

### `F16` · list · decisions: `D_cmd`

**Input:** `number one apples number two oranges number three pears`

**Roles:** must_change, command

**Rationale:** Enumerated list via spoken number commands.

**Formatting change notes:** list structure and item casing under B, sentence case under A

**Grammar change notes:** _(none)_

#### `F16-A` — Strategy A: bare number one/two/three do not fire.

```
LITERAL:
number one apples number two oranges number three pears
SMART:
Number one apples number two oranges number three pears.
```

#### `F16-B` — Strategy B: bare list commands fire to separate numbered lines.

```
LITERAL:
1. apples
2. oranges
3. pears
SMART:
1. Apples
2. Oranges
3. Pears
```

### `F16b` · list · decisions: `D_cmd`

**Input:** `command number one apples command number two oranges command number three pears`

**Roles:** must_change, command

**Rationale:** Introducer form of enumerated list. Under B, number phrases still fire; proposed exact outputs match multiline list.

**Formatting change notes:** numbered list lines, item casing on Smart

**Grammar change notes:** _(none)_

#### `F16b-A` — Strategy A: introducer-gated list commands fire to separate lines.

```
LITERAL:
1. apples
2. oranges
3. pears
SMART:
1. Apples
2. Oranges
3. Pears
```

#### `F16b-B` — Strategy B: command words may remain while bare number phrases also fire — proposed render keeps markers and drops introducer words as content noise avoided by treating number phrases as commands.

```
LITERAL:
1. apples
2. oranges
3. pears
SMART:
1. Apples
2. Oranges
3. Pears
```

### `F22` · quotation · decisions: `D_cmd`

**Input:** `use the exact error quote connection refused unquote in the ticket`

**Roles:** must_preserve, command

**Rationale:** Quoted technical string. In-quote casing preserved in both B Smart/Literal (no D4 here).

**Formatting change notes:** sentence case, terminal period, quote pairing under B

**Grammar change notes:** _(none)_

#### `F22-A` — Strategy A: bare quote/unquote do not fire.

```
LITERAL:
use the exact error quote connection refused unquote in the ticket
SMART:
Use the exact error quote connection refused unquote in the ticket.
```

#### `F22-B` — Strategy B: bare quote/unquote fire to ASCII quotes around connection refused.

```
LITERAL:
use the exact error "connection refused" in the ticket
SMART:
Use the exact error "connection refused" in the ticket.
```

### `F22b` · quotation · decisions: `D_cmd`

**Input:** `use the exact error command quote connection refused command unquote in the ticket`

**Roles:** must_preserve, command

**Rationale:** Introducer form of technical quotes.

**Formatting change notes:** quote pairing, sentence case, terminal period

**Grammar change notes:** _(none)_

#### `F22b-A` — Strategy A: introducer-gated quote/unquote fire.

```
LITERAL:
use the exact error "connection refused" in the ticket
SMART:
Use the exact error "connection refused" in the ticket.
```

#### `F22b-B` — Strategy B: command words remain; bare quote/unquote fire around the span.

```
LITERAL:
use the exact error command "connection refused" command in the ticket
SMART:
Use the exact error command "connection refused" command in the ticket.
```

### `F36` · separability · decisions: `D_cmd`

**Input:** `there is two issues new line next topic`

**Roles:** must_change, command, separability_format_without_grammar

**Rationale:** Formatting-with-deliberate-grammar-miss example for human approval. grammar_changes empty: this fixture intentionally does not correct is→are.

**Formatting change notes:** sentence/line case, newline under B, terminal period on first line under B Smart

**Grammar change notes:** _(none)_

#### `F36-A` — Strategy A: bare new line does not fire; grammar error is intentionally preserved in Smart (no is→are).

```
LITERAL:
there is two issues new line next topic
SMART:
There is two issues new line next topic.
```

#### `F36-B` — Strategy B: bare new line fires; Smart cases lines and adds a period but intentionally preserves is two (not are). A corrected There are two… is a different example, not this fixture.

```
LITERAL:
there is two issues
next topic
SMART:
There is two issues.
Next topic.
```

### `F36b` · separability · decisions: `D_cmd`

**Input:** `there is two issues command new line next topic`

**Roles:** must_change, command, separability_format_without_grammar

**Rationale:** Introducer form of separability example. Still not a grammar-correction fixture.

**Formatting change notes:** newline, line casing, period on first line Smart

**Grammar change notes:** _(none)_

#### `F36b-A` — Strategy A: introducer new line fires; grammar error deliberately preserved.

```
LITERAL:
there is two issues
next topic
SMART:
There is two issues.
Next topic.
```

#### `F36b-B` — Strategy B: command word may remain; bare new line fires; grammar error still deliberately preserved.

```
LITERAL:
there is two issues command
next topic
SMART:
There is two issues command.
Next topic.
```

### `F38` · punctuation_command · decisions: `D_cmd`

**Input:** `yes comma I can join`

**Roles:** must_change, command

**Rationale:** Bare comma command.

**Formatting change notes:** sentence case, comma render under B, terminal period

**Grammar change notes:** _(none)_

#### `F38-A` — Strategy A: bare comma does not fire.

```
LITERAL:
yes comma I can join
SMART:
Yes comma I can join.
```

#### `F38-B` — Strategy B: bare comma fires to ,.

```
LITERAL:
yes, I can join
SMART:
Yes, I can join.
```

### `F39` · command_safety · decisions: `D_cmd`

**Input:** `the bullet train is fast`

**Roles:** must_preserve, command_safety_counterexample

**Rationale:** Noun-phrase counterexample: bullet train.

**Formatting change notes:** sentence case, terminal period

**Grammar change notes:** _(none)_

#### `F39-A` — Strategy A: bare bullet does not fire.

```
LITERAL:
the bullet train is fast
SMART:
The bullet train is fast.
```

#### `F39-B` — Strategy B desired preserve of noun phrase bullet train (ambiguity risk if bare bullet over-fires to a list marker).

```
LITERAL:
the bullet train is fast
SMART:
The bullet train is fast.
```

### `F40` · command_safety · decisions: `D_cmd`

**Input:** `the quote was approved`

**Roles:** must_preserve, command_safety_counterexample

**Rationale:** Noun-phrase counterexample: the quote was approved.

**Formatting change notes:** sentence case, terminal period

**Grammar change notes:** _(none)_

#### `F40-A` — Strategy A: bare quote does not fire.

```
LITERAL:
the quote was approved
SMART:
The quote was approved.
```

#### `F40-B` — Strategy B desired preserve (ambiguity risk if bare quote inserts ").

```
LITERAL:
the quote was approved
SMART:
The quote was approved.
```

### `F41` · command_safety · decisions: `D_cmd`

**Input:** `we launched a brand new line yesterday`

**Roles:** must_preserve, command_safety_counterexample

**Rationale:** Noun-phrase counterexample: brand new line.

**Formatting change notes:** sentence case, terminal period

**Grammar change notes:** _(none)_

#### `F41-A` — Strategy A: bare new line does not fire.

```
LITERAL:
we launched a brand new line yesterday
SMART:
We launched a brand new line yesterday.
```

#### `F41-B` — Strategy B desired preserve of brand new line (ambiguity risk if bare new line over-fires).

```
LITERAL:
we launched a brand new line yesterday
SMART:
We launched a brand new line yesterday.
```

### `F42` · command_safety · decisions: `D_cmd`

**Input:** `the number one rule is safety`

**Roles:** must_preserve, command_safety_counterexample

**Rationale:** Noun-phrase counterexample: number one rule.

**Formatting change notes:** sentence case, terminal period

**Grammar change notes:** _(none)_

#### `F42-A` — Strategy A: bare number one does not fire.

```
LITERAL:
the number one rule is safety
SMART:
The number one rule is safety.
```

#### `F42-B` — Strategy B desired preserve of number one rule (ambiguity risk if bare number one starts a list).

```
LITERAL:
the number one rule is safety
SMART:
The number one rule is safety.
```

### `F37` · punctuation_command · decisions: `D_cmd`

**Input:** `the reporting period ended yesterday`

**Roles:** must_preserve, command, command_safety_counterexample

**Rationale:** Noun-phrase counterexample: reporting period.

**Formatting change notes:** sentence case, terminal period

**Grammar change notes:** _(none)_

#### `F37-A` — Strategy A: bare period does not fire.

```
LITERAL:
the reporting period ended yesterday
SMART:
The reporting period ended yesterday.
```

#### `F37-B` — Strategy B desired preserve of reporting period (ambiguity risk).

```
LITERAL:
the reporting period ended yesterday
SMART:
The reporting period ended yesterday.
```

### `F04` · email · decisions: `D10`

**Input:** `hi jordan thanks for the update i will review the proposal by thursday and send comments`

**Roles:** must_change

**Rationale:** Email structure inference is a product choice (D10). No command dependency.

**Formatting change notes:** name/weekday casing, sentence punctuation, optional greeting/paragraph structure

**Grammar change notes:** _(none)_

#### `F04-A` — Structured email: greeting line + blank line + body.

```
LITERAL:
hi jordan thanks for the update i will review the proposal by thursday and send comments
SMART:
Hi Jordan,

Thanks for the update. I will review the proposal by Thursday and send comments.
```

#### `F04-B` — Single paragraph: no invented blank line.

```
LITERAL:
hi jordan thanks for the update i will review the proposal by thursday and send comments
SMART:
Hi Jordan, thanks for the update. I will review the proposal by Thursday and send comments.
```

### `F17` · list · decisions: `D3`

**Input:** `buy milk eggs bread`

**Roles:** must_change

**Rationale:** List inference without spoken list commands (D3). Not a D_cmd fixture.

**Formatting change notes:** sentence case, comma list or bullet structure

**Grammar change notes:** _(none)_

#### `F17-A` — No list inference: prose commas.

```
LITERAL:
buy milk eggs bread
SMART:
Buy milk, eggs, bread.
```

#### `F17-B` — Infer bullet list without list commands.

```
LITERAL:
buy milk eggs bread
SMART:
Buy:
- milk
- eggs
- bread
```

### `F18` · list · decisions: `D14`

**Input:** `the API has three errors not found forbidden and unauthorized`

**Roles:** must_preserve

**Rationale:** Oxford comma taste (D14). Must not invent HTTP codes (exact strings avoid 404/403/401).

**Formatting change notes:** sentence case, colon before list, serial comma choice

**Grammar change notes:** _(none)_

#### `F18-A` — Colon + Oxford comma.

```
LITERAL:
the API has three errors not found forbidden and unauthorized
SMART:
The API has three errors: not found, forbidden, and unauthorized.
```

#### `F18-B` — Colon without Oxford comma.

```
LITERAL:
the API has three errors not found forbidden and unauthorized
SMART:
The API has three errors: not found, forbidden and unauthorized.
```

### `F27` · grammar · decisions: `D5`

**Input:** `i didnt see no error in the logs`

**Roles:** must_preserve

**Rationale:** Double-negative handling (D5). Grammar notes: B's any is a grammar choice, not formatting.

**Formatting change notes:** sentence case, terminal period

**Grammar change notes:** apostrophe didnt→didn't, optional double-negative rewrite under B only

#### `F27-A` — Preserve double-negative voice; only apostrophe/case.

```
LITERAL:
i didnt see no error in the logs
SMART:
I didn't see no error in the logs.
```

#### `F27-B` — Standardize to single negation.

```
LITERAL:
i didnt see no error in the logs
SMART:
I didn't see any error in the logs.
```

### `F14` · line_paragraph · decisions: `D_cmd`, `D1`

**Input:** `intro command new paragraph body text`

**Roles:** must_change, command

**Rationale:** Cross-product D_cmd × D1 on introducer-containing input. Four complete alternatives.

**Formatting change notes:** paragraph break, line casing, optional terminal periods on fragments

**Grammar change notes:** _(none)_

#### `F14-A-breaks` — A: introducer new paragraph fires; Smart cases lines; no added periods (D1-A).

```
LITERAL:
intro

body text
SMART:
Intro

Body text
```

#### `F14-A-punct` — A: introducer fires; Smart adds periods on fragments (D1-B).

```
LITERAL:
intro

body text
SMART:
Intro.

Body text.
```

#### `F14-B-breaks` — B: command word remains; bare new paragraph fires; no added periods (D1-A).

```
LITERAL:
intro command

body text
SMART:
Intro command

Body text
```

#### `F14-B-punct` — B: command word remains; bare new paragraph fires; Smart periods (D1-B).

```
LITERAL:
intro command

body text
SMART:
Intro command.

Body text.
```

### `F14b` · line_paragraph · decisions: `D_cmd`, `D1`

**Input:** `intro new paragraph body text`

**Roles:** must_change, command

**Rationale:** Bare-input cross-product D_cmd × D1. A-breaks and A-punct are intentionally identical because D1 only applies when a paragraph command fires.

**Formatting change notes:** optional paragraph break under B, line casing, optional fragment periods under B+D1-B, terminal period under A

**Grammar change notes:** _(none)_

#### `F14b-A-breaks` — A: bare new paragraph does not fire; D1 irrelevant — no break to punctuate.

```
LITERAL:
intro new paragraph body text
SMART:
Intro new paragraph body text.
```

#### `F14b-A-punct` — A: same as no-fire (D1 cannot apply without a paragraph command firing).

```
LITERAL:
intro new paragraph body text
SMART:
Intro new paragraph body text.
```

#### `F14b-B-breaks` — B: bare new paragraph fires; no added periods (D1-A).

```
LITERAL:
intro

body text
SMART:
Intro

Body text
```

#### `F14b-B-punct` — B: bare new paragraph fires; Smart periods (D1-B).

```
LITERAL:
intro

body text
SMART:
Intro.

Body text.
```

### `F21` · quotation · decisions: `D_cmd`, `D4`

**Input:** `she said quote we ship friday unquote and hung up`

**Roles:** must_change, command

**Rationale:** Cross-product D_cmd × D4 on bare quote input. A-preserve and A-case identical because D4 needs quotes to fire.

**Formatting change notes:** outer sentence case/punct, quote delimiters under B, optional in-quote casing under B+D4-B

**Grammar change notes:** _(none)_

#### `F21-A-preserve` — A: bare quote/unquote do not fire; D4 N/A — no quote delimiters added.

```
LITERAL:
she said quote we ship friday unquote and hung up
SMART:
She said quote we ship friday unquote and hung up.
```

#### `F21-A-case` — A: same no-fire render (D4 cannot apply without quote commands firing).

```
LITERAL:
she said quote we ship friday unquote and hung up
SMART:
She said quote we ship friday unquote and hung up.
```

#### `F21-B-preserve` — B: bare quote/unquote fire; preserve in-quote casing (D4-A).

```
LITERAL:
she said "we ship friday" and hung up
SMART:
She said, "we ship friday," and hung up.
```

#### `F21-B-case` — B: bare quote/unquote fire; sentence-case inside quote (D4-B).

```
LITERAL:
she said "we ship friday" and hung up
SMART:
She said, "We ship Friday," and hung up.
```

### `F21b` · quotation · decisions: `D_cmd`, `D4`

**Input:** `she said command quote we ship friday command unquote and hung up`

**Roles:** must_change, command

**Rationale:** Cross-product D_cmd × D4 on introducer-containing quote input.

**Formatting change notes:** quote delimiters, outer punctuation/casing, optional in-quote casing

**Grammar change notes:** _(none)_

#### `F21b-A-preserve` — A: introducer quotes fire; preserve in-quote casing (D4-A).

```
LITERAL:
she said "we ship friday" and hung up
SMART:
She said, "we ship friday," and hung up.
```

#### `F21b-A-case` — A: introducer quotes fire; case inside quote (D4-B).

```
LITERAL:
she said "we ship friday" and hung up
SMART:
She said, "We ship Friday," and hung up.
```

#### `F21b-B-preserve` — B: command words remain; bare quote/unquote fire; preserve in-quote (D4-A).

```
LITERAL:
she said command "we ship friday" command and hung up
SMART:
She said command, "we ship friday," command and hung up.
```

#### `F21b-B-case` — B: command words remain; bare quote/unquote fire; case inside (D4-B).

```
LITERAL:
she said command "we ship friday" command and hung up
SMART:
She said command, "We ship Friday," command and hung up.
```

## Locked fixtures (review index)

These do not depend on open decisions. Approve as a set of exact examples.

| Id | Category | Roles | Modes differ |
|----|----------|-------|--------------|
| `F01` | casual | must_change, mode_delta | yes |
| `F02` | casual | must_preserve, mode_delta | yes |
| `F03` | casual | must_preserve, mode_delta | yes |
| `F05` | email | must_change, mode_delta | yes |
| `F06` | technical | must_preserve, mode_delta | yes |
| `F07` | technical | must_preserve, already_correct | no |
| `F08` | technical | must_preserve, mode_delta | yes |
| `F19` | already_correct | already_correct, must_preserve | no |
| `F20` | already_correct | already_correct, must_preserve | no |
| `F23` | informal | must_preserve, mode_delta | yes |
| `F24` | informal | must_preserve, mode_delta | yes |
| `F25` | grammar | must_change | yes |
| `F26` | grammar | must_change | yes |
| `F28` | safety | must_preserve, safety | yes |
| `F29` | safety | must_preserve, safety | yes |
| `F30` | safety | must_preserve, safety | yes |
| `F31` | safety | must_preserve, safety | yes |
| `F32` | safety | must_preserve, safety | yes |
| `F33` | safety | must_preserve, safety | yes |
| `F34` | safety | must_preserve, safety | yes |
| `F35` | already_correct | already_correct, must_preserve | no |

## Notes on independence (approval layer only)

- **F26:** sentence casing and terminal punctuation are listed under `formatting_changes`; apostrophe insertion in `lets`→`let's` is listed under `grammar_changes`.
- **F36 / F36b:** alternatives intentionally keep `is two` (no `are`). A grammar-corrected render would be a **different** example, not an alternate pass of the same exact fixture.
- Meaning preservation is judged from **full proposed strings** and must-preserve counterexamples, not from a #99 lexical engine.

## Approval checklist

- [ ] I have not approved anything by silence.
- [ ] Each decision above has exactly one chosen option id.
- [ ] I understand command syntax (D_cmd) is open; nothing command-dependent is locked.
- [ ] I understand #100/#103 own safety machinery and thresholds.
- [ ] Issue #99 remains open until I explicitly say otherwise.

---

*If this ballot and the JSON disagree, the JSON corpus wins for structural fields.*
