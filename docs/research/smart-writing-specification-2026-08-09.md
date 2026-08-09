# Smart Writing implementation specification — 2026-08-09

**Issue:** [#103](https://github.com/Anuraj-dev/voisu/issues/103) · parent map [#96](https://github.com/Anuraj-dev/voisu/issues/96)
**Status:** **approved by Raja 2026-08-09** (Sol medium independent review r4 APPROVE; §0 package
accepted with free-tier grammar-quota clarification in §7.4). Map #96 implementation may begin.
**Scope:** English Smart Writing v1; documentation and research only
**Normative constants:** [`smart-writing-spec-constants-2026-08-09.json`](./smart-writing-spec-constants-2026-08-09.json)

## 0. Raja approval ballot

**Recorded 2026-08-09:** Raja approved this package as one lock. Independent Sol medium review r4:
**APPROVE** (zero findings). Free-tier call-frequency concern answered in §7.4 (buildable; rate
limits fall back to Formatting-only).

This specification does not reopen any approved #99, #100, or #101 choice. It fixes the remaining
implementation choices conservatively:

| New choice | Specification lock |
|---|---|
| Minimal Grammar model | Groq `openai/gpt-oss-20b`, separate from reconciliation's `qwen/qwen3.6-27b`; `reasoning_effort: "low"`; strict JSON Schema output |
| Final-gate reserve | 100 ms reserved to initiate Delivery; candidate work ends at `gate_entry + 900 ms` |
| Local/HTTP bounds | formatter 50 ms; grammar HTTP 700 ms and result no later than `gate_entry + 800 ms`; safety/composition 100 ms |
| #100 release limits | tighten research-only loader limits to 64 KiB response, depth 8, 4,096 nodes, 32 KiB Validated Transcript, 32 edits, 256-byte edit fields; retain 128-byte diagnostic limit |
| Ticketing | eight focused component tickets, two integration verticals, and one host/rollout ticket; formatting-first is an intermediate vertical only |
| Multiline evidence | KWrite M1/M2/M3 accepted as document-field proof; shell and Enter-submit surfaces remain explicit release gates, not blockers to approving this specification |

**Ballot closed:** package approved. Implementation of map #96 under SW1–SW11 is authorized.

## 1. Normative sources and precedence

1. `CONTEXT.md` supplies the exact domain language. The Smart Writing additions currently present in
   the read-only primary checkout are reproduced in §2 without synonym changes.
2. `smart-writing-behavior-approval-ballot-2026-08-09.md` and the 51-fixture behavior corpus lock
   `D_cmd-A, D1-B, D3-B, D4-A, D5-A, D10-B, D14-A`.
3. `smart-writing-edit-safety-contract-2026-08-09.md` and its corpus lock the sealed baseline,
   three-rule grammar catalog, and `B1-A/B2-A/B3-A` whole-candidate failure policy.
4. `speculative-formatting-final-only-pipeline-2026-08-09.md` locks Architecture A. Its rejected
   alternatives stay rejected.
5. This document owns executable constants, the production model/API contract, release thresholds,
   ticket boundaries, and rollout gates. If prose and the JSON companion differ, CI must fail; neither
   silently overrides the other.

The #99 corpus is the canonical exact-output record. The #100 corpus is the canonical authority and
failure-policy record. This document promotes their behavior into production tests; it does not edit
either approved artifact.

## 2. Domain contracts

Use these names in product types, tests, diagnostics, and tickets:

| Term | Contract | Forbidden synonym/interpretation |
|---|---|---|
| **Validated Transcript** | The safe semantic text selected from the Source Transcripts before presentation changes are applied. It is immutable evidence, with a version and SHA-256 identity. | raw text; draft Transcript; formatter input that may be mutated in place |
| **Rendered Transcript** | The delivery-ready form of a Validated Transcript after Formatting and, when accepted, Minimal Grammar Correction. | rewritten Transcript; polished content; provider replacement text |
| **Formatting** | Meaning-preserving presentation changes such as punctuation, capitalization, spacing, line breaks, paragraphs, and list structure. It is deterministic and local. | rewriting; paraphrasing; LLM formatting |
| **Minimal Grammar Correction** | A localized correction of an obvious grammatical error that preserves meaning, vocabulary, and tone. v1 is exactly the closed catalog in §6. | rewrite; style improvement; content editing; open-ended proofreading |
| **Writing Mode** | The user-selected policy for turning a Validated Transcript into a Rendered Transcript. `Smart` applies Formatting and optional Minimal Grammar Correction; `Literal` preserves spoken wording while honoring explicit formatting commands. | application profile; editor mode; inferred per-app mode |

Additional contract types:

- `FormattingBaseline`: sealed, immutable formatter output bound to exactly one Validated Transcript.
  Only the formatting module can construct it. Provider JSON can never deserialize or forge it.
- `GrammarCandidate`: untrusted provider envelope containing identity plus localized grammar edits.
- `GrammarCapability`: `Ready(ReadyGrammarCapability)` or `Unavailable(reason)`, resolved before
  Validation. It is never a lazy credential/client loader.
- `EnglishEligibility`: resolved from Recording/provider language configuration and fail-closed. It
  is never inferred from transcript words.

Normative flow:

```text
Source Transcripts
  → existing reconciliation and quality validation
  → immutable Validated Transcript
  → deterministic local Formatting
  → sealed FormattingBaseline
  → optional structured Minimal Grammar Correction against the same identity
  → #100 whole-candidate safety gate and anchor composition
  → Rendered Transcript
  → one final-only Delivery
```

Presentation must not move into `TranscriptDecisionPipeline` and must not change
`TranscriptDecision.transcript` as semantic evidence.

## 3. Writing Mode and persisted CLI contract

### 3.1 Behavior

- A fresh install or a readable config with no `writing_mode` key resolves to `Smart`.
- An existing unreadable file or a present malformed/unknown `writing_mode` value fails closed to
  `Literal` with a bounded local diagnostic. This preserves the escape-hatch intent when Voisu cannot
  prove the user's persisted choice; it does not change the fresh-install Smart default.
- `Smart` always attempts local Formatting within its bound. It attempts Minimal Grammar Correction
  only when `EnglishEligibility` allows it and `GrammarCapability` is `Ready`.
- `Literal` never calls Minimal Grammar. It preserves wording/case/punctuation except for an explicit
  command recognized by §4. Command rendering itself remains deterministic Formatting.
- Per-Recording mode is snapshotted before Recording begins and remains stable through Delivery.

### 3.2 CLI and file

Add the same narrow pattern used by `voisu delivery`:

```text
voisu writing                 # prints: writing mode: smart|literal
voisu writing smart           # atomically persists Smart
voisu writing literal         # atomically persists Literal
```

The root key in `$XDG_CONFIG_HOME/voisu/config.toml` (fallback
`~/.config/voisu/config.toml`) is:

```toml
writing_mode = "smart"
```

The setter preserves unrelated lines and the existing `deepgram_enabled` and `delivery_mode` keys,
uses the existing same-directory atomic replace, and reports the path written. Invalid CLI values
exit 2 without changing the file. A running daemon follows the existing config contract: the new
value applies after daemon restart, and the setter's success text must say so. The help surface lists
`writing [smart|literal]`. No environment override is part of v1.

Public seams: `WritingMode::{Smart, Literal}`, `DEFAULT_WRITING_MODE = Smart`, distinct
missing/valid/invalid/unreadable resolution, read/write/merge tests, CLI parse/output tests,
fail-closed invalid/unreadable behavior, atomic-write failure, and a Recording snapshot test.

## 4. Formatting command language (`D_cmd-A`)

### 4.1 Grammar and precedence

Commands are recognized only in the Validated Transcript and only with an explicit introducer:

```text
command      ::= "command" SP command_phrase
literal      ::= "literal" SP "command" SP command_phrase
```

Matching is Unicode-whole-token, ASCII-case-insensitive for the introducers and phrase, left-to-right,
and longest phrase first. One or more transcript whitespace characters may separate phrase tokens.
The parser applies these rules in order:

1. Recognize `literal command …` before `command …`. Remove only `literal`; emit `command <phrase>`
   as ordinary spoken words and never reparse that emitted text.
2. Recognize paired/ranged constructs (`quote`/`unquote`, numbered-list runs) before scalar commands.
3. Recognize a scalar command only when the whole closed phrase matches.
4. Preserve an unknown, incomplete, unmatched, or malformed command sequence word-for-word. Never
   guess, partially consume, or drop the introducer.

Bare phrases such as `period`, `new line`, `quote`, `number one`, and `bullet` are ordinary speech.
This is the safety boundary demonstrated by F09/F10/F11/F12/F13/F15/F16/F21/F22 and F37–F42.

### 4.2 Closed v1 phrase catalog

| Phrase after `command` | Rendering |
|---|---|
| `period` | `.` with punctuation spacing |
| `comma` | `,` with punctuation spacing |
| `question mark` | `?` with punctuation spacing |
| `exclamation point` | `!` with punctuation spacing |
| `new line` | exactly one `\n` |
| `new paragraph` | exactly two `\n` characters |
| `quote` … `command unquote` | paired ASCII `"` delimiters; preserve interior casing (`D4-A`) |
| `number one`, `number two`, `number three` | an ordered run rendered as `1.`, `2.`, `3.` on separate lines |

An ordered-list run is recognized only when it begins at one, is strictly consecutive, contains at
least two markers, and every marker has non-empty item text. Otherwise the whole run is ordinary
speech. Unmatched quote/unquote sequences are also ordinary speech. Expanding this catalog requires
new positive and ambiguity fixtures plus Raja approval; it does not require changing the grammar
rule catalog, which is a separate authority.

### 4.3 Mode-specific rendering

- Literal consumes valid commands and performs only the structural/punctuation rendering above.
  F10b, F13b, F14, F16b, and F22b are exact-output tests.
- Smart performs the same command rendering, then applies the closed local formatting rules. `D1-B`
  adds terminal periods to paragraph-separated fragments with at least two lexical words. Smart
  sentence-cases line/list starts but preserves quoted interiors.
- The formatter records complete source ranges for command phrases and quote/code interiors in the
  sealed baseline so Minimal Grammar cannot touch them.

## 5. Deterministic local Formatting

Formatting has no network, model, credential, application-context, clipboard, screen, or surrounding
text dependency. Its only content input is the Validated Transcript plus Writing Mode. Implement it
as a closed, ordered formatter contract whose ID is persisted in the baseline and diagnostics.

### 5.1 Ordered rule catalog

**Authority model.** The 51 locked #99 fixture I/O pairs are the **executable oracle** for v1. Where
this section and a fixture disagree, the fixture wins. Where this section is silent on an unseen
input, the formatter **fails closed**: leave the contested span unchanged (or keep ordinary prose) and
still deliver a sealed baseline. SW3 may not invent open-ended NLP; new behavior requires new exact
fixtures plus a reviewed contract-ID bump.

**Recognition order (closed).** UTF-8 half-open `[start,end)` byte offsets throughout.

1. **Composite protected spans first (regex on the raw Validated string, before lexical tokens):**
   - URL span: `https?://\S+`
   - path span: tokens/spans matching `~?(?:\.?\.?/|/)[^\s]+` or bare `./…` `../…`
   - flag span: `(?:--[A-Za-z0-9]+(?:-[A-Za-z0-9]+)*=\S+|-[A-Za-z0-9]+)` (exact F07 form includes
     `--test-threads=4` as one protected flag span)
   - technical-identifier span: whole maximal `[A-Za-z][A-Za-z0-9]*_[A-Za-z0-9_]+` runs (must contain
     `_`; e.g. `correlation_id`). Ordinary words without `_` are never technical identifiers.
2. **Lexical tokenization** on the residual non-protected regions only: a *word token* is a maximal
   run of Unicode letters/digits/`'` (ASCII apostrophe or U+2019 between word characters); remaining
   single punctuation/symbol characters are their own tokens; ASCII horizontal whitespace separates
   tokens. Newlines are hard separators (not “adjacent”).
3. Matching for casing/grammar uses word tokens; composite spans from step 1 are never rewritten.

**Protected recognition (before any edit; local catalogs only):**

| Class | Closed recognition (v1) | Source |
|---|---|---|
| URL / path / flag / technical identifier | **composite spans** from step 1 only — never “every English word”. Ordinary alphabetic words (`hey`, `the`, `issues`, `is`, `lets`, `didnt`) remain editable | F06/F07/F08/F30/F35 |
| Number / date / time | whole-token decimal integers, common `YYYY-MM-DD` / `HH:MM` shapes present in fixtures; no silent unit conversion | F06/F35 |
| Dictionary terms | exact whole-token membership in the **Recording-time dictionary snapshot** already available to validation (same set as existing transcript-fidelity dictionary); empty set if none | product dictionary, not Groq |
| Names | exact whole-token membership in a **Recording-time protected-names snapshot** (user/dictionary-supplied); if empty, only fixture-locked proper names appearing as title-case after ordinary sentence casing may change case at sentence start—never alter interior letters of unknown tokens that look “name-like” by heuristic | fail-closed |
| Negations | whole tokens in closed set: `not`, `no`, `never`, `n't`/`n’t` contractions (`don't`, `didn't`, `won't`, …); spans containing them are protected from grammar and from filler deletion | D5-A / #100 |
| Prompt-shaped text | substrings matching closed patterns used by #100 (`system:`, `user:`, fenced triple-backtick blocks) | #100 |
| Shell command line | entire Validated Transcript matches F07 family: optional leading `$`, first token in closed verb set `{run,ls,cd,cat,grep,git,curl,ssh,sudo,rm,cp,mv,chmod,chown,docker,kubectl,python,python3,pip,cargo,npm,make,systemctl}` **and** (≥1 flag token starting with `-` **or** second token is a known tool/path token such as `cargo`/`npm`/`make`), and the Validated text has no sentence-final `.`; F07 exact I/O is normative; if ambiguous, treat as **prose** (fail closed) | F07 |
| Explicit command ranges | §4 parser spans | §4 |
| Quote / code interiors | paired ASCII/curly quotes and inline/fenced code per #100 | #100 |

1. **Identity/protected recognition:** fingerprint the Validated Transcript; mark every protected
   range above before editing. Later rules may only write **outside** protected ranges unless a rule
   explicitly documents an allowed same-token casing composition.
2. **Explicit commands:** apply §4 atomically. Preserve exact existing newlines and valid existing
   lists (F20).
3. **Technical exactness:** if recognized as a shell command line, emit identity (no sentence
   punctuation, no casing). Preserve protected URL/path/flag/identifier/number/date/dictionary bytes
   byte-for-byte (F06/F08/F30/F35).
4. **Casing:** outside protected ranges: capitalize the first letter of each sentence/line/list item;
   standalone whole-token `i` → `I`; weekday tokens `{monday…sunday}` → title case; quotation
   interiors keep Validated casing (`D4-A`). Do not normalize informal vocabulary (`gonna`, `aint`,
   `lol`) or remove fillers.
5. **Punctuation:**
   - terminal `.` on unpunctuated declarative prose that is not a shell line and not a question;
   - terminal `?` **only** when a *closed question cue* matches: (a) first content token
     case-insensitive in `{who,what,when,where,why,how,is,are,was,were,do,does,did,can,could,will,would,should,may}`;
     or (b) whole-transcript matches the approved `can you …` / trailing `ok` patterns locked by
     F01/F02 fixtures—**no open-ended question detection**;
   - retain existing correct terminal punctuation;
   - vocative/discourse commas **only** where required by exact fixture patterns F01/F02/F04/F21b/F23
     (implementation ports those patterns as regression tests, not freeform “name, …” heuristics).
6. **Paragraphs:** only `command new paragraph` invents a blank line in v1. `D10-B` forbids inferred
   email greeting/blank-line layout. Ordinary email-like prose may gain sentence punctuation but
   stays one paragraph.
7. **Lists (`D3-B`, transcript-content only):**
   - **Bullet inference (F17 only in v1):** Validated Transcript matches
     `buy` + three or more *simple item tokens* separated only by spaces/commas, with **no**
     *clause/subordination marker* token from closed set
     `{when,if,because,while,although,after,before,unless,that,which,who,and then}`.
     A *simple item token* is a single token with no internal punctuation other than hyphen inside a
     word. On any mismatch or extra clause words → keep prose.
   - **Counted enumeration (F18 shape):** phrases matching
     `… N …` with N in closed word/number set `{two…twelve,2…12}` plus an explicit list of items after
     a clear list noun—stay **inline**, add colon/separators, Oxford comma (`D14-A`); do not convert
     to bullets.
   - never consult a field, app, window, screen, clipboard, or surrounding document.
8. **Safety preservation:** do not remove/change negations, reorder self-corrections, replace names,
   convert number/date forms, delete fillers, standardize double negatives (`D5-A`), or infer email
   layout (`D10-B`). If any predicate is ambiguous, emit the prior safe text for that span.

Any new formatter rule requires a contract ID, positive fixtures, ambiguity counterexamples, and an
exact-output review. Provider JSON never carries formatter rule IDs or formatting text.

### 5.2 Sealed baseline

`FormattingBaseline` privately contains:

- Validated version and SHA-256 fingerprint;
- rendered baseline text;
- unambiguous source-to-baseline anchors for every source token eligible for grammar composition;
- complete formatter-owned quote/code/command protected source ranges;
- formatter contract ID;
- structural derivation digest.

Construction and fields stay private to the formatting module. Consumers receive only typed access.
Any wrong type, identity, or digest is an internal contract failure and falls back to the unchanged
Validated Transcript—not to provider text.

## 6. Minimal Grammar and #100 safety gate

### 6.1 Closed grammar rules

The provider may propose only these three rule IDs:

| Rule ID | Exact accepted predicate |
|---|---|
| `G_THERE_IS_PLURAL_QUANTITY` | whole word-token `is` → `are` where **only single horizontal ASCII spaces** (U+0020; one or more) separate the context word-tokens — **no** punctuation, newline, tab, or other symbol may intervene (matches #100 prototype THERE_PERIOD/COMMA/DASH/NEWLINE rejects). Order: `there` + spaces + `is` + spaces + quantity ∈ `{two…twelve,2…12}` + spaces + count noun `issues`. **and** Validated Transcript has **no** §4 command span; **and** no consecutive word-tokens `new`+`line` (case-insensitive). Accept F25/G01/C01; reject F36/F36b |
| `G_LETS_MEET_CONTRACTION` | sentence-initial whole word-token `lets` → `let's` immediately before `meet` with **only horizontal ASCII spaces** between them (no punct/newline); no §4 command span |
| `G_DIDNT_APOSTROPHE` | whole word-token `didnt` → `didn't`; apostrophe insertion only, never negation removal |

**Separability with commands:** if any §4 command span is recognized in the Validated Transcript, Smart
still runs local Formatting but **does not** call Minimal Grammar (outcome `formatting_only` even if
capability is Ready). That keeps F36b and other command+grammar separability fixtures
provider-independent.

No generic agreement, contraction, punctuation, whole-rewrite, style, or vocabulary rule exists.
Widening requires: a named rule and local predicate; positive behavior fixtures; new adversarial
false-positive/protected-span cases; zero unsafe deliveries through the production gate; independent
review; and Raja approval. Until all land together, an unknown rule rejects the whole candidate.

### 6.2 Candidate shape

Provider content is only an envelope plus ordered localized edits:

```json
{
  "base_version": "validated-en-v1",
  "base_fingerprint": "sha256:<64 lowercase hex>",
  "edits": [
    {
      "id": "bounded diagnostic identifier",
      "rule_id": "G_DIDNT_APOSTROPHE",
      "start_utf8": 2,
      "end_utf8": 7,
      "before": "didnt",
      "after": "didn't"
    }
  ]
}
```

Ranges are half-open UTF-8 byte offsets against the immutable Validated Transcript. A non-zero range
must be on scalar boundaries and cover exactly one whole lexical token. Zero-width edits are
forbidden. `before` must equal the bytes at the range. Edits must be source-ordered, unique, and
non-overlapping.

### 6.3 Deterministic validation order and failure

Validate: bounded envelope/container → freshness → edit shape in input order → UTF-8/token anchors →
protected spans → closed-rule predicate → formatter anchor mapping → overlap. An unreadable envelope
fails before freshness; an otherwise readable stale envelope wins before malformed edit contents.
Preserve first-seen error order.

Protected spans include names, dictionary terms, identifiers, URLs, paths, numbers, dates, explicit
command phrases, negations, prompt-shaped text, complete paired ASCII/curly quote interiors,
inline/fenced code, and `command quote … command unquote`. Apostrophes between word characters are
not quote delimiters. An unmatched ambiguous quote protects the whole base.

Approved policy is exact:

- `B1-A`: one invalid edit rejects the whole GrammarCandidate; keep the fresh baseline.
- `B2-A`: stale grammar keeps the fresh baseline.
- `B3-A`: any protected-span hit rejects the whole GrammarCandidate; keep the baseline.

Missing/ambiguous formatter anchors, unknown rules, malformed fields, unsorted/duplicate/overlapping
edits, and any failed predicate also reject the whole candidate. Formatting-owned casing wins in the
two allowed same-token compositions (`Lets` + apostrophe → `Let's`; `Didnt` + apostrophe → `Didn't`).
No partially applied grammar is ever delivered.

## 7. Groq Minimal Grammar model/API contract

### 7.1 Selected model

Use exact model ID **`openai/gpt-oss-20b`**, not reconciliation's
`qwen/qwen3.6-27b`. The purposes and risk envelopes differ: reconciliation selects semantic evidence,
whereas Minimal Grammar must return a tiny strict schema inside a 700 ms request bound. Groq's
official model page currently lists about 1,000 tokens/s plus JSON Schema support, and its Structured
Outputs documentation lists GPT-OSS 20B for `strict: true`. The smaller model and constrained output
are the conservative latency/shape choice. Availability and behavior remain release-gated; this
selection is not evidence that live trials already pass.

Official references: [GPT-OSS 20B](https://console.groq.com/docs/model/openai/gpt-oss-20b),
[Structured Outputs](https://console.groq.com/docs/structured-outputs), and
[Chat Completions API](https://console.groq.com/docs/api-reference).

### 7.2 Request

`GrammarAdapter` sends one non-streaming `POST /openai/v1/chat/completions` using the already-ready
credential and async client:

| Exact constant / field | Fixed value/rule |
|---|---|
| `MINIMAL_GRAMMAR_ENDPOINT` | exact `https://api.groq.com/openai/v1/chat/completions` |
| `MINIMAL_GRAMMAR_MODEL` / `model` | exact `openai/gpt-oss-20b` |
| `MINIMAL_GRAMMAR_REASONING_EFFORT` / `reasoning_effort` | exact `low` |
| `MINIMAL_GRAMMAR_STREAM` / `stream` | `false` |
| `n` | omit (API default one) |
| `MAX_MINIMAL_GRAMMAR_COMPLETION_TOKENS` / `max_completion_tokens` | 2,048 |
| `MINIMAL_GRAMMAR_RESPONSE_FORMAT` / `response_format.type` | `json_schema` |
| `MINIMAL_GRAMMAR_SCHEMA_STRICT` / `response_format.json_schema.strict` | `true` |
| schema objects | all fields required; `additionalProperties: false`; rule ID enum closed to §6.1 |
| `MINIMAL_GRAMMAR_REQUEST_RETRIES` | `0`; 429/5xx/transport error is local fallback |
| temperature/tools/store | omit; no tools; no response storage request |

The fixed system instruction says: emit only localized edits that match the three predicates; preserve
meaning, vocabulary, tone, and negation; use UTF-8 byte offsets; return an empty list when uncertain.
The user message contains only the English Validated Transcript plus its non-content version and
fingerprint. No Source Transcript, app/window/field identity, screen, clipboard, surrounding text,
dictionary contents, name list, or audio is sent. Protected-name/dictionary evaluation remains local.

The response schema is §6.2, with maximum 32 edits. Even strict schema output remains untrusted:
local bounds, fingerprint, anchors, protected spans, and rule predicates always run. Refusal, empty
choice, non-content response, trailing data, schema error, or oversize body is grammar rejection with
Formatting preserved.

### 7.3 Selected async transport and proof

Use one process-owned `reqwest::Client` built without default TLS features and with Rustls + JSON
support. Reuse that client through `ReadyGrammarCapability`; do not construct a client in the gate.
Production uses the exact HTTPS endpoint above. Tests inject a loopback endpoint through the adapter
constructor rather than a general production environment override.

The `reqwest` per-request future is polled inline by the Final Transform Gate. It must be fully async,
request-scoped, and drop-safe. Dropping it must leave zero gate-owned request/response work. A
persistent async client's pool/reactor may live, but no per-request task, response processing, child,
or socket work may continue for the dropped request.

Accept exactly one HTTP 200 response whose bounded body decodes from
`choices[0].message.content` into §6.2. Empty/multiple choices, missing content, non-200 status, or a
body after the size/deadline boundary is grammar fallback. Do not stream or retry.

Forbidden: curl, subprocesses, `spawn_blocking`, per-request `tokio::spawn`, detached handles,
ProviderReaper curl adoption, credential loading, retry/backoff sleeps, or cleanup grace after the
gate. A production-boundary cancellation test using the actual client stack is mandatory. A mock
alone proves only the adapter contract. Missing proof **blocks map #96 completion and tickets
SW5/SW10/SW11**—not issue #103's document approval. It does not turn Formatting-only into the
completed Smart Writing milestone.

### 7.4 Call frequency and Groq free-tier quota

Unlike reconciliation (which runs only when the dual-provider decision path needs a merge/repair),
**Minimal Grammar is one optional Groq request per eligible Smart Recording** after Validation —
when Writing Mode is Smart, English eligibility allows grammar, `GrammarCapability` is `Ready`, and
no §4 command span forces format-only separability. Literal never calls it. Formatting always runs
locally first and does not use the model.

The model id `openai/gpt-oss-20b` is **hosted on Groq**, not billed against an OpenAI account. Free
and Developer limits are organization rate limits (RPM/RPD/TPM/TPD), not a hard “you may never
build” cap. As of 2026-08, public free-tier figures for this model are on the order of **~30 RPM and
~1,000 RPD** (confirm live at the org limits page). That is enough for normal personal dictation and
CI live-trial budgets; heavy continuous use can exhaust daily free RPD.

**Product behavior under limit pressure (normative):**

- `MINIMAL_GRAMMAR_REQUEST_RETRIES = 0`. HTTP 429, 5xx, transport error, or timeout → **local
  Formatting baseline** (or identity), never a blocked Delivery.
- Diagnostics record a closed reason (rate limit / transport / timeout); no secret material.
- Exhausting free quota degrades grammar quality, **not** core dictation or Formatting.
- Building and shipping Smart Writing v1 on free-tier Groq is allowed; production operators who outgrow
  free RPD upgrade the Groq plan or accept more `formatting_only` outcomes. No second paid OpenAI key
  is required for this path.

## 8. Architecture A final-only pipeline

### 8.1 Pre-validation capability ownership

Immediately after `capture.finish`, `process_recording` registers the credential cleanup entry and
polls `CredentialPreparationOwner` concurrently with `ProviderCoordinator.complete_with_timings`
inside one owned inline/caught structure. There is no detached or Recording-time task.

`CredentialPreparationOwner` and the dedicated `ProviderReaper` credential lane retain child,
process-group, capped async pipes, retry/backoff state, outcome, and credential bytes before first
poll may launch. Allowed fast paths are environment/session cache; cache miss uses Tokio process for
restricted `secret-tool`. Current blocking `SecretToolStore::load` is not reusable for this owner.

Required state and cleanup semantics are copied unchanged from #101:

- `Registered → Running(pgid) → Terminal`, `Registered → Terminal`, or
  `Registered|Running → CancelRequested → Terminal`, then `Terminal → Deregistered`;
- terminal means child wait plus stdout/stderr EOF; one drive claim, idempotent kill/wait/removal;
- normal/error/timeout reaches terminal and deregisters before Validation;
- owner drop synchronously requests cancellation and process-group kill but cannot claim terminal;
- `supervise_recording` drains an adopted abnormal entry before panic reporting, adapter rebuild,
  `Completed`, or Idle; shutdown performs a final idempotent drain;
- a caught provider/prep/parsing/diagnostic panic explicitly cancel-reaps before returning an error;
  an uncaught task abort has no Delivery and the supervisor owns the retained entry.

`CREDENTIAL_PREP_WORK_DEADLINE = 13 s`. At expiry, stop retries, kill, and reap.
`CREDENTIAL_REAP_WATCHDOG = 2 s` is a diagnostic threshold—not permission to detach. On overrun,
remain Processing, log once, and await terminal cleanup even beyond the 34 s response watchdog.
Provider completion may use 15 s plus its separate 2 s abort; the overlapping 15+2 allocation is
counted once.

Only terminal `GrammarCapability::Ready` or `Unavailable(reason)` reaches Validation. A per-Recording
Unavailable is ordinary fallback only after the production async/capability path has passed release
gates.

### 8.2 English eligibility

Construct `ResolvedRecordingLanguages` from the exact parameters actually sent to active providers.
The v1 product default is explicit `en` for Groq and Deepgram; implementation must add and record the
Deepgram language parameter rather than assume English from its current omission. Normalize ASCII
case and accept `en` or an explicit `en-*` tag. `EnglishEligibility::Eligible` requires every active
provider declaration to be present, non-empty, non-`auto`/detect, mutually English-compatible, and
equal to what was sent. Absent, conflicting, invalid, auto-detect, or non-English means Ineligible;
Formatting still runs and no grammar request starts. Never inspect transcript words to decide.

### 8.3 Final Transform Gate

Gate entry is the instant immediately after `ValidationCompleted`. One absolute
`deadline = gate_entry + 1 s` governs candidate work and Delivery initiation:

| Constant | Value | Contract |
|---|---:|---|
| `FINAL_TRANSFORM_GATE_DEADLINE` | 1,000 ms | absolute gate entry to latest Delivery initiation |
| `DELIVERY_INITIATION_RESERVE` | 100 ms | reserves scheduler/call-entry headroom; candidate freezes at 900 ms |
| `CANDIDATE_PIPELINE_DEADLINE` | 900 ms | derived absolute candidate-work window from gate entry |
| `LOCAL_FORMATTER_WORK_DEADLINE` | 50 ms | local formatter maximum; miss preserves identity |
| `GRAMMAR_HTTP_REQUEST_DEADLINE` | 700 ms | relative HTTP maximum, also capped by absolute grammar cutoff |
| `GRAMMAR_RESULT_CUTOFF_FROM_GATE_ENTRY` | 800 ms | no grammar result accepted after this instant |
| `LOCAL_SAFETY_COMPOSE_WORK_DEADLINE` | 100 ms | response parse, #100 validation, anchor composition, final freeze |

The request deadline is `min(request_start + 700 ms, gate_entry + 800 ms)`. Candidate work is wrapped
once by `timeout_at(gate_entry + 900 ms, …)`. Cooperative local bounds and size limits are required;
the outer timeout is not a substitute for bounded synchronous work.

Algorithmic ownership:

1. Initialize `selected = Validated Transcript` outside timeout/panic containment.
2. Run local Formatting. On full success, persist the typed baseline text into `selected` immediately.
3. Literal stops here. Smart proceeds only for Eligible + Ready.
4. Poll exactly one async grammar request; parse into a separate GrammarCandidate.
5. Validate the whole candidate and compose through baseline source anchors in separate temporaries.
6. Atomically replace `selected` only after the complete safe composition succeeds.
7. On formatter failure/panic/bound miss, keep identity. On any later timeout/error/panic/reject,
   keep the persisted baseline. Drop the request future with no cleanup grace.
8. Freeze candidate by 900 ms and call `delivery.deliver(selected)` exactly once before 1,000 ms.
   Delivery I/O then retains its existing clipboard/libei bounds.

Panic containment wraps formatter, request polling, parse, safety, and composition. A caught gate panic
must still perform one Delivery using the last persisted safe value. The supervisor is not a Delivery
fallback.

### 8.4 Processing watchdog

Set `PROCESSING_RESPONSE_DEADLINE = 34 s`: current 33 s accounting plus the Final Transform Gate once.
It remains the Stop/Toggle/Replay client response and shutdown-ack watchdog, not a Processing→Idle
hard bound. Credential or provider reaping may safely exceed it while the daemon remains Processing.

## 9. Executable release limits

The JSON companion is a minimal constants manifest: `schema` and `spec_version`, unit declarations,
then `timing`, `limits`, `model`, and `release_thresholds` maps. Production constants and tests must
deserialize/compare against it or duplicate it in a compile-time golden; drift fails CI.

| Release constant | Value | #100 research default | Decision |
|---|---:|---:|---|
| `MAX_GRAMMAR_RESPONSE_BYTES` | 65,536 | `max_file_bytes=524,288` | tighten; network candidate is much smaller than the full research corpus file |
| `MAX_GRAMMAR_JSON_DEPTH` | 8 | 12 | tighten; production schema needs three object/array levels |
| `MAX_GRAMMAR_JSON_NODES` | 4,096 | 20,000 | tighten; 32 edits fit with large margin |
| `MAX_VALIDATED_TRANSCRIPT_UTF8_BYTES` | 32,768 | 100,000 | tighten; exceeds a normal bounded Recording while preserving gate work bounds; oversize safely keeps identity/baseline |
| `MAX_GRAMMAR_EDITS` | 32 | 64 | tighten; v1 has only three localized rules |
| `MAX_GRAMMAR_EDIT_FIELD_UTF8_BYTES` | 256 | 512 | tighten; single-token before/after and IDs need far less |
| `MAX_GRAMMAR_DIAGNOSTIC_UTF8_BYTES` | 128 | 128 | adopt |

Reject the raw body before allocation growth, then enforce decoded depth/node/field/edit bounds.
Reject non-scalar Unicode/lone surrogates before UTF-8 accounting. Size rejection is whole-candidate
fallback. The formatter has the same 32 KiB input gate; an oversize Validated Transcript remains
deliverable unchanged, never truncated.

## 10. Bounded local diagnostics

Extend `DiagnosticRecord` with one optional, versioned `SmartWritingDiagnostic`. It remains in the
existing local 200-record/7-day history and existing scrubbed export. Audio remains absent unless the
separate debug-audio opt-in is enabled.

Diagnostic bounds are exact: `MAX_SMART_WRITING_DIAGNOSTIC_TEXT_UTF8_BYTES = 2,048`,
`MAX_SMART_WRITING_DIAGNOSTIC_EDITS = 32`, and `MAX_MODEL_ID_UTF8_BYTES = 128`.

Record:

- Writing Mode, EnglishEligibility outcome, and formatter contract ID;
- bounded Validated-before and Rendered-after text, each at most 2,048 UTF-8 bytes, clamped only in
  diagnostics (never in Delivery), plus full SHA-256 fingerprints so equality remains inspectable;
- outcome enum (exactly one):
  - `literal` — Writing Mode Literal, successful path, no explicit command applied (ordinary identity);
  - `literal_commands` — Literal, successful path, at least one explicit formatting command rendered;
  - `literal_fallback` — Literal path failed closed to Validated identity (formatter panic/deadline/
    oversize/atomic command render failure), whether or not a command was recognized;
  - `formatting_only` — Smart local Formatting baseline delivered without accepted grammar
    (includes command-present Smart runs that skip grammar under §6.1 separability);
  - `formatting_and_grammar` — Smart baseline plus accepted grammar edits;
  - `identity_fallback` — **Smart only**: failed closed to Validated identity (formatter miss/panic/
    oversize). Never use for Literal — use `literal` or `literal_fallback` instead;
- up to 32 structured edit evidence entries: edit ID, rule ID, byte range, bounded before/after,
  acceptance/rejection code—never the prompt or raw provider body;
- exact model ID (max 128 bytes), whether a request began, formatter/HTTP/safety/total gate latency,
  credential prep latency, and whether the 2 s reap watchdog crossed;
- closed rejection/fallback reason codes, including mode, English ineligible, capability unavailable,
  input/response oversize, HTTP timeout/status/transport, malformed/schema/stale, protected span,
  rule context, unmappable/overlap, formatter/safety/compose panic or deadline, and cleanup overrun.

Do not record credentials, authorization/header values, secret-tool output, system/user prompts, raw
response bodies, app/window/field/screen/clipboard/surrounding context, environment dumps, or newly
captured audio. Free-form errors are scrubbed and bounded to 128 bytes; prefer enums. Existing
`MAX_STORED_TEXT` remains available for final Transcript history, but Smart Writing before/after uses
the tighter 2 KiB diagnostic clamp.

## 11. Public test seams and release thresholds

### 11.1 Hermetic CI (blocking)

| Seam/corpus | Required result |
|---|---|
| #99 behavior corpus | all 51 fixtures exact in both modes; any byte mismatch fails CI |
| Command parser | introducer/escape precedence, phrase boundaries, whitespace, unknown/incomplete commands, unmatched quote, invalid numbered run, all ambiguity fixtures |
| Formatter properties | deterministic across three hash seeds; no network/credential access; protected bytes unchanged; idempotent on already-correct structures |
| #100 corpus | all 18 fixtures exact, including decisions and ordered error codes |
| #100 adversaries | all 56 pass under three hash seeds; zero unsafe grammar deliveries |
| Limits | at/over every raw/decoded/input/edit/field bound, Unicode scalar boundaries, depth/node exhaustion; no panic/unbounded allocation |
| CLI/config | Smart fresh/key-absent default, persisted Literal, invalid/unreadable → Literal, query/set output, atomic merge, restart notice, per-Recording snapshot |
| Gate | paused-time absolute 1 s, 100 ms reserve, formatter/HTTP/safety sub-bounds, fallback persistence, panic containment, exactly one Delivery |
| Architecture A | no presentation during Recording; Ready/Unavailable only; no in-gate credential work; provider 15→17 s timing; shutdown-while-Recording semantics |
| Credential owner | register-before-poll, cache hit/miss/cancel, 13 s deadline, 2 s watchdog overrun, kill/wait/pipe EOF, panic/task-abort/teardown, single deregistration |
| Diagnostics | every outcome/reason, text/edit/model clamps, secret scrubbing, no audio without opt-in, backward-compatible serde defaults/export |

The #101 §7 seams remain individually observable, not hidden inside one integration test:

| Architecture A seam | Required observation |
|---|---|
| Abnormal-path common assertions | no Delivery on processing panic; no `Completed`/Idle before child wait + both pipe EOFs; no child/entry survives; removal exactly once |
| Gate entry / capability not lazy | Validation succeeds before any grammar request; gate receives only Ready/Unavailable; no keyring/helper/client construction in gate |
| Provider 15→17 timing | provider remains pending to 15 s then aborts by 17 s; capability already terminal; 15+2 counted once |
| Credential normal timing | 13 s work expiry plus terminal reap within watchdog overlaps provider arm; stage completes at the maximum arm, not a false 15 s outer deadline |
| Credential non-cooperative overrun | one diagnostic at 2 s; remains Processing/registered; no Validation/gate/Delivery/ack/Idle until terminal |
| CLI/shutdown watchdog overrun | caller may time out at 34 s while status remains Processing; daemon keeps ownership and shutdown awaits actor |
| Prep or unrelated provider panic | catch covers the whole concurrent machine; explicit cancel/reap/deregister; no Delivery |
| Abort `process_recording` | owner Drop kill-signals; supervisor claims and drains before `Completed`/Idle; no Delivery |
| Runtime cancellation/shutdown | supervisor drain then final idempotent shutdown drain; no live process/entry at teardown |
| Cancel/Drop/supervisor race | one kill request, one child wait/pipe drain, one removal; losing driver observes Terminal |
| Production HTTP cancel | actual client and mock both show request drop, zero residual per-request work, and one local/identity Delivery |
| Fallback persistence | formatter failure/panic → identity; every grammar/safety/compose failure/panic → exact baseline; partial work never mutates `selected` |
| Runtime Unavailable vs missing proof | a Recording may fall back only after the production path is proven; absent proof cannot close #96 |
| Literal/D3-B/#100/shutdown/envelope | no grammar in Literal; transcript-only list inference; whole-candidate reject; shutdown full Processing; constants manifest matches 34 s math |

Timing assertions use paused Tokio time for the logical contract. Small real-scheduler telemetry
tolerance may exist only in telemetry tests and never changes the logical deadlines.

The machine-readable threshold names are normative:

| Constant | Release requirement |
|---|---:|
| `BEHAVIOR_FIXTURES_REQUIRED` / `BEHAVIOR_FIXTURES_EXACT_PASS_REQUIRED` | 51 / 51 |
| `SAFETY_FIXTURES_REQUIRED` / `SAFETY_FIXTURES_EXACT_PASS_REQUIRED` | 18 / 18 |
| `ADVERSARIAL_CASES_REQUIRED` / `ADVERSARIAL_UNSAFE_DELIVER_MAX` | 56 / 0 |
| `DETERMINISM_HASH_SEEDS_REQUIRED` | 3 |
| `LOGICAL_GATE_DEADLINE_VIOLATIONS_MAX` | 0 |
| `DELIVERY_COUNT_PER_RECORDING_MAX` | 1 |
| `DELIVERY_COUNT_PER_SUCCESSFUL_RECORDING_MIN` | 1 |

`DELIVERY_COUNT_PER_RECORDING_MAX = 1` is an at-most-once ceiling on every Recording. Separately,
every **validated-success** Recording (ValidationCompleted reached without Processing error) must
deliver **exactly once**: `DELIVERY_COUNT_PER_SUCCESSFUL_RECORDING_MIN = 1`. Zero deliveries on a
validated success fails the gate even if the max is satisfied.

### 11.2 Production-boundary gates (blocking #96 completion)

Ownership: **SW5** owns (1); **SW7** owns (2); **SW10** owns hermetic integration of (1–2) into
`process_recording` plus logical gate proofs; **SW11** owns (3–4) live model trials and candidate
real-host telemetry on the exact release candidate RPM/SHA.

1. **Async cancellation (SW5, then re-proved under SW10 wiring):** actual selected HTTP client against
   a controllable local server; drop mid-upload, mid-wait, and mid-response; residual per-request work
   must be zero. Mock coverage alone fails this gate.
2. **Credential cache miss (SW7, then re-proved under SW10 wiring):** actual restricted helper process;
   cancellation, non-cooperative pipe, caught panic, processing-task abort, and daemon teardown prove
   no child/entry survives and no Idle precedes terminal reap.
3. **Live Groq model (SW11 only):** 30 uncontaminated positive trials (10 per closed rule); at least
   27 exact safe corrections, 100% of completed responses schema-valid, and zero unsafe deliveries
   after the local gate. Any unsafe delivery fails regardless of correction rate. Rate-limit/transport
   failures count as availability telemetry and local fallback, not model semantic trials.
4. **Candidate latency (split):**
   - **Logical (SW10 hermetic):** paused/injected time only; `LOGICAL_GATE_DEADLINE_VIOLATIONS_MAX = 0`;
     freeze by 900 ms and initiate Delivery by 1,000 ms on the logical clock. No wall-clock race.
   - **Real-host telemetry (SW11):** measure `ValidationCompleted → delivery.deliver start` on the
     candidate RPM with a stated protocol (monotonic clock, N≥30 successful recordings, p50/p95/max
     recorded). Soft release bar from the constants manifest: `HOST_GATE_P95_MS = 1000` and
     `HOST_GATE_MAX_MS = 1500` under normal load; a single overrun is a telemetry fail that blocks
     release, **not** a hermetic CI flake. Do not encode wall-clock 1,000 ms as a unit-test assertion.

The live/production manifest encodes these same requirements as
`LIVE_MODEL_POSITIVE_TRIALS_REQUIRED = 30`, `LIVE_MODEL_EXACT_SAFE_CORRECTIONS_MIN = 27`,
`LIVE_MODEL_COMPLETED_SCHEMA_VALID_PERCENT_MIN = 100`, `LIVE_MODEL_UNSAFE_DELIVER_MAX = 0`,
`PRODUCTION_CANCEL_RESIDUAL_REQUEST_WORK_MAX = 0`, and
`DELIVERY_COUNT_PER_SUCCESSFUL_RECORDING_MIN = 1`.

### 11.3 Host Delivery gates

Accepted #102 evidence from installed RPM `voisu-0.10.3-0.409.1786270956.git47f734edf0ed.fc43`:

| ID | KWrite result | Specification use |
|---|---|---|
| VOISU-M1 | one real `\n` produced two lines | document-field newline proof accepted |
| VOISU-M2 | blank line preserved two paragraphs | document-field paragraph proof accepted |
| VOISU-M3 | `- item` lines preserved | document-field list-marker proof accepted |

This proves KWrite only. Before enabling out-of-box Smart in a release, run the exact Rendered
Transcript path—not a test-only bypass—on:

- KWrite/document field: explicit newline, paragraph, inferred bullets, numbered list;
- ordinary shell prompt: ordinary command-like speech and shell command transcript must not acquire
  uncommanded multiline structure or execute; explicit multiline is supervised;
- at least one live chat and one web form whose Enter submits: inferred and explicit multiline must
  not cause partial/multiple submissions;
- each supported Delivery mode (`type`, `clipboard`, and guarded when enabled), including clipboard
  preservation and focus change.

The specification is approvable with these residual tests outstanding. The release is not. If the
shell/chat/form gate cannot prove atomic safe multiline Delivery, keep Smart unreleased and either
fix the general Delivery contract or return to Raja with a separately approved explicit host policy.
Do not silently add application-aware Writing Mode or collect field/screen context.

## 12. Implementation-ticket DAG for parent #96

Each ticket owns only the named modules/contracts, lands focused regressions, receives independent
review when material, and reports exact-head CI. Tickets may be implemented in parallel only where
the edges permit.

```text
SW1 Writing Mode CLI/config ──────────────────────────────────────────────┐
SW2 command parser → SW3 local formatter ──┬→ SW4 #100 safety ──┐         │
                                           │                    ├→ SW6 ──┤
SW5 async HTTP proof ──────────────────────┴────────────────────┘         │
SW7 credential owner + reaper lane ───────────────────────────────────────┼→ SW10 full Smart ──→ SW11 host/live/rollout
SW8 diagnostics ──────────────────────────────────────────────────────────┤
SW1 ─ (also) ─→ SW9 formatting-first (optional intermediate; SW1+SW2+SW3+SW8 only; cannot close #96)
```

**Edges (normative; diagram and table must match):**

| From → To | Required? |
|---|---|
| SW2 → SW3 | yes |
| SW3 → SW4 | yes (sealed baseline type) |
| SW3 → SW9 | yes if SW9 is cut |
| SW4 → SW6 | yes |
| SW5 → SW6 | yes |
| SW1 → SW9 | yes if SW9 is cut |
| SW8 → SW9 | yes if SW9 is cut |
| SW1 → SW10 | yes (Writing Mode snapshot) |
| SW4,SW5,SW6,SW7,SW8 → SW10 | yes |
| SW9 → SW10 | **no** — SW9 is optional intermediate |
| SW10 → SW11 | yes |

| Ticket | Ownership | Done when |
|---|---|---|
| **SW1 — Writing Mode CLI/config** | `config.rs`, `bin/voisu.rs`, shared `WritingMode` type only | `voisu writing [smart|literal]`, Smart default, atomic persistence/merge, help/output/restart notice, and snapshot tests pass |
| **SW2 — formatting command parser** | new core formatting parser module only | §4 closed grammar, escape/precedence, source spans, and all command/ambiguity fixtures pass; no rendering heuristics or network |
| **SW3 — local formatter + sealed baseline** | formatter module/type only; depends SW2 | all 51 #99 outputs exact; §5.1 closed predicates/fail-closed; baseline constructor/fields private; anchors/protected ranges/digest verified; 50 ms and 32 KiB limits tested |
| **SW4 — #100 safety gate port** | production grammar candidate parser/validator/composer only; depends SW3 | 18 fixtures + 56 adversaries × three seeds pass with exact error order and B1-A/B2-A/B3-A; all release limits tested; provider cannot construct baseline |
| **SW5 — async grammar client proof** | process-owned Rustls `reqwest::Client` and adapter transport boundary only | §11.2 gate (1) passes; strict-schema request works; real-server drop tests leave zero request work; no curl/process/`spawn_blocking`/spawn/retry; independent Sol review |
| **SW6 — Groq grammar adapter** | model prompt/schema/request mapping only; **depends SW4+SW5** | exact GPT-OSS 20B contract, transcript-only privacy body, three rule IDs, 700/800 ms bounds, empty/error fallback, canned harness. Live trials are **not** SW6 done-when—they belong to SW11 |
| **SW7 — credential owner + reaper lane** | `system.rs` preparation owner and dedicated credential lane only | §11.2 gate (2) passes; Architecture A register/poll/state/reap/panic/abort/shutdown seams including 13 s + 2 s overrun; terminal Ready/Unavailable before Validation |
| **SW8 — diagnostics** | `DiagnosticRecord`, history/export/view additions only | §10 schema/bounds/scrubbing/backward compatibility pass including `literal`/`literal_fallback` outcomes; no prompt/body/secret/app context; audio remains opt-in |
| **SW9 — formatting-first vertical (optional)** | gate shell using **SW1+SW2+SW3+SW8** only; grammar unused | Literal and Smart Formatting reach one final Delivery with fallback/panic/logical-deadline tests. **Cannot close map #96**; not required before SW10 |
| **SW10 — full Smart integration** | `process_recording`, supervision/shutdown wiring, English resolution, constants; **depends SW1+SW4+SW5+SW6+SW7+SW8** (SW9 not required) | components wired after Validation and before Delivery; formatting outranks grammar; 34 s watchdog; no Recording content work; **all hermetic CI** + re-proof of §11.2 (1–2) under real wiring + **logical** latency gate. Does **not** require live Groq or multi-app host matrix |
| **SW11 — host gates, live model, rollout, rollback** | packaging + supervised host evidence only; **depends SW10 merge** | exact candidate RPM/SHA; §11.2 (3–4) live model + real-host latency telemetry; KWrite retained; shell/chat/form + Delivery-mode matrix; independent exact-head review/CI; rollback rehearsal |

SW9 may merge first behind the incomplete milestone, making Formatting testable without pretending
Minimal Grammar is done. **SW10 → SW11 is mandatory.** Map #96 closes only after SW11. Missing SW5
proof forces a different proven async client or explicit Architecture B reconsideration; it never
converts SW9 into map completion.

## 13. Codebase insertion map

| Current seam | Required insertion |
|---|---|
| `crates/voisu-app/src/config.rs` | persist/resolve `WritingMode` beside Deepgram and Delivery settings |
| `crates/voisu-app/src/bin/voisu.rs` | parse/render `voisu writing [smart|literal]`; update usage |
| `crates/voisu-core/src/lib.rs` | public domain types/test seams; keep `TranscriptDecisionPipeline` presentation-free |
| `crates/voisu-app/src/system.rs` | constants; async client; GrammarAdapter; CredentialPreparationOwner; dedicated reaper lane; resolved provider languages |
| `crates/voisu-app/src/bin/voisu-daemon.rs::process_recording` | after `capture.finish`, concurrently own providers+prep; after `ValidationCompleted`, enter inline Final Transform Gate; deliver selected once |
| `supervise_recording` and shutdown | drain abnormal credential lane before `Completed`/Idle/ack/runtime teardown |
| `crates/voisu-core/src/diagnostics.rs` | optional backward-compatible Smart Writing diagnostic record and clamps |

Do not reuse `GroqReconciliationModel::request` for grammar: its `spawn_blocking` + curl + in-request
credential load violates §7.3. Reconciliation remains unchanged on `qwen/qwen3.6-27b`.

## 14. Review, CI, rollout, and rollback

### 14.1 Review and CI

- Every ticket self-reviews its exact diff and focused assertions, including timing/race dependence.
- SW4, SW5, SW7, SW10, and SW11 require independent Sol review on the exact head.
- Required CI: workspace tests, three-seed/adversarial gate, clippy `-D warnings`, lockfile advisory
  audit, exact constants-manifest comparison, and any established flake gate.
- Green hermetic CI is not production async proof or host evidence. Record those separately against
  the exact candidate artifact/SHA.
- No Smart Writing implementation begins before Raja approves this specification. No map completion
  claim occurs while required exact-head CI, independent review, async proof, live model threshold,
  or host gate is pending.

### 14.2 Rollout

1. Land SW1–SW8 behind non-default test/integration seams; do not expose an incomplete Smart default.
2. Land SW9 as the explicitly incomplete formatting-first vertical if useful for focused review.
3. Land **SW10** only after SW5 async-cancellation and SW7 credential lifecycle proofs pass **and**
   are re-proved under SW10 wiring (hermetic + production-boundary (1–2) only).
4. **After SW10 merges**, build the exact-head candidate RPM (SW11).
5. On that exact artifact, run SW11 live Groq trials, real-host latency telemetry, and the supervised
   multi-app host matrix.
6. Enable out-of-box Smart only in the release candidate that passes all SW11 gates. Existing users
   with persisted Literal remain Literal.
7. Observe bounded fallback, latency, rejection, quota, and cleanup-overrun diagnostics; never upload
   them automatically.

### 14.3 Rollback

- Immediate user rollback: `voisu writing literal`, then restart the daemon. This stops grammar and
  inferred Smart Formatting while retaining explicit command behavior.
- Release rollback: reinstall the prior known-good package. The older version must ignore the unknown
  `writing_mode` key and preserve it/unrelated config on later rewrites.
- If only live grammar quality/availability regresses, do not relax B1-A/B2-A/B3-A, deadlines, or the
  rule catalog. Hold/revert the Smart release or ship a reviewed local grammar-disable change; keep
  Formatting evidence separate.
- If cleanup ownership or late Delivery regresses, stop rollout immediately. Literal is not a waiver
  for lifecycle unsafety; revert to the last known-good package.

## 15. Definition of done and exclusions

**Issue #103 (this document)** is done when Raja approves this exact specification after independent
review. Approval authorizes ticket creation and implementation. It does **not** require production
async proof, live Groq trials, or host matrix completion—those are implementation/release gates.

**Map #96** is done only when SW1–SW11 are implemented/reviewed/merged as applicable, exact-head CI
is green, SW5/SW7 production-boundary proofs and SW10 logical gate proofs pass, SW11 live model
thresholds and host gates pass on the exact candidate RPM, and rollback is rehearsed.

Wording elsewhere that said “blocks #103/#96” for missing async proof is corrected to **block #96 /
SW5/SW10/SW11**, not document-issue #103.

Explicitly out of scope: learning from corrections; app-aware automatic mode; reading app, field,
screen, clipboard, tab, or surroundings; live mid-speech insertion/revision; rewrite/paraphrase/tone;
filler deletion or self-correction interpretation; non-English grammar; local/offline ASR replacement;
and widening the three-rule grammar catalog without the §6.1 process.

Formatting-only is a valid safe per-Recording fallback and a valid intermediate vertical. It is not
the completed Smart Writing v1 milestone.
