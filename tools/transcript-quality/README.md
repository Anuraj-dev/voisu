# transcript-quality

Private evaluator for saved Recordings. It scores Source Transcripts, the
current guarded pipeline, and (when it exists) Intent Reconstruction against
an audio-adjudicated reference.

This binary is not a workspace member, is not in the Fedora package, and is
not wired into `voisu --help`. Do not put private audio or Source Transcripts
in this directory.

## Run

```sh
cargo run --manifest-path tools/transcript-quality/Cargo.toml -- \
  --manifest /path/to/manifest.json \
  --out /path/to/report.json
```

`--out` is optional. The default is `tools/transcript-quality/out/transcript-quality-report.json`
(gitignored) when the manifest lives in this git work tree. A private manifest
outside the repository still writes `transcript-quality-report.json` beside it.
The tool refuses to write a git-tracked path unless `--out` is gitignored or
outside the repo. If `git check-ignore` fails (any status other than 0 or 1),
the path is refused. An ignored or out-of-tree symlink whose target is a
tracked file is refused. Tests must pass `--out` under a tempfile.

`--deliver-scratch <path>` is recognized and refused. Default evaluation writes
report files only. Delivery into a scratch editor is not implemented here
because it would have to live in `voisu-app`; this tool will not type into the
focused application.

`--help` prints usage.

## Manifest

JSON object `{ "recordings": [ ... ] }`, a JSON array, a single recording
object, or JSONL (one recording per line).

Each Recording may include:

| Field | Role |
| --- | --- |
| `correlation_id` | Stable id for the Recording |
| `audio_path` | Private audio. Read for presence only; never copied into git |
| `source_transcripts.groq` / `.deepgram` | Inline Source Transcript text |
| `source_transcripts.groq_path` / `.deepgram_path` | Files with the same text |
| `final_transcript` / `final_transcript_path` | Current guarded pipeline Transcript, if saved |
| `reference_path` / `reference_text` | Audio-adjudicated reference |
| `reference_kind` | `adjudicated` or `script`. Absent is not spoken truth |
| `adjudicated` | `true` marks a reference as spoken truth without `reference_kind` |
| `speaker` | Speaker label |
| `tags` | Free-form test tags |
| `rendering_policy` | `natural` / `adaptive` / `structured` for the organizer call |

Relative paths resolve against the manifest directory.

The original reading script is not spoken truth. A reference is used only when
`reference_kind` is `adjudicated` (or equivalent) or `adjudicated: true`.
`reference_kind` of `script`, `reading_script`, or `prompt`, a
`reading-script` / `script` tag, or an unmarked reference is missing evidence.
Audio may be absent for synthetic text-vs-text rows; it is how humans create
references, not a runtime gate.

## Arms

1. **completeness_aware_source** — evaluator heuristic (ticket 02 is not in
   product). Discount repeated filler, duplicated loops, and known outro
   garbage; prefer the materially fuller safe Source Transcript. Consecutive
   duplicate tokens are collapsed (`deploy now now` does not beat `deploy now`).
   A contiguous short fragment does not beat a longer sibling.
2. **guarded_pipeline** — the saved current pipeline Transcript
   (`final_transcript`). If that field is missing, the arm is `missing`. This
   tool does not re-run the organizer on the completeness-selected source and
   call that the current pipeline.
3. **intent_reconstruction** — skipped with reason `missing` until ticket 05.
   A missing reconstruction is never scored as zero error.

Fixture, provider, reference, or source failures are `missing`, never a
perfect score.

## Report

Every Recording is printed, then aggregates. Aggregates do not replace
per-Recording rows.

Each scored arm reports strict word error (insertions, deletions,
substitutions), critical semantic errors (negation, numbers, units, names,
commands, paths, URLs, code tokens, missing clauses), and section loss
(prefix or body deleted relative to the reference). The saved pipeline is not
compared against the completeness-selected source. Aggregates use
corpus-weighted WER (sum of edits over sum of reference tokens) and keep every
per-Recording row.

The JSON has a `stable` object (sorted keys, no wall-clock) and a `volatile`
object for evaluator scoring time (not pipeline execution latency). Identical
inputs hash to the same `stable_fingerprint`.

## Mark and promote

Private host tool equivalent to `mark-last good|bad`. It is not in
`voisu --help`, packages, or service units.

```sh
cargo run --manifest-path tools/transcript-quality/Cargo.toml --bin mark-last -- \
  good --note 'optional note'
```

It resolves the newest completed correlation ID from
`$XDG_STATE_HOME/voisu/diagnostics/history.jsonl` (default
`~/.local/state/voisu/diagnostics/`), refuses to mark while a Recording is
active or when debug audio is already missing, copies the PCM with mode 0600
into `~/.local/state/voisu/dev-audio/promoted/`, and snapshots both Source
Transcripts, the current final, any reconstruction candidate, decision
evidence, model ID, and timing. Secrets are scrubbed. Rolling debug capture
under `diagnostics/audio` is left in place (seven-day expiry); only the
promoted copy is permanent.

A later adjudicated reference can be attached without replacing raw evidence:

```sh
cargo run --manifest-path tools/transcript-quality/Cargo.toml --bin mark-last -- \
  attach-reference rec-… --file /path/to/reference.txt
```

Repeated marks of the same Recording do not create a second corpus entry.
The tool refuses to write under a git work tree. Do not copy private audio
into this repository.

## Tests

```sh
cargo test --manifest-path tools/transcript-quality/Cargo.toml -- --test-threads=4
```

Synthetic fixtures live in `fixtures/`. They are short fake sentences, not
Raja audio. Mark/promote tests use tempdirs and synthetic PCM only.

---

# Score corpus (B1) — audio-adjudicated evaluation

`score-corpus` is the measurement machine that certifies transcript-quality
changes: it scores a directory of cases — each a real Recording with a
reference you (Raja) adjudicated by listening — against the pipeline's final
Transcript, and prints one number per change.

- Corpus and results stay **local**: raw voice is never committed, never
  uploaded, and never copied out by this tool.
- CI runs the scorer with **synthetic fixtures only** (`corpus.example/` and
  tempdirs): zero network, zero provider keys, zero daemon.
- Every case without a scorable result is **SKIPped with a reason**. Nothing
  is faked; a missing result is never scored as a perfect score.

## Corpus layout

Default corpus directory: `~/.local/state/voisu/eval-corpus/` (outside the
repository). `score-corpus` and `capture-result` also accept any gitignored
in-tree path — `/tools/transcript-quality/corpus/` is gitignored for exactly
that purpose — and refuse a tracked git-tree corpus outright.

```
<corpus-dir>/
  <case-id>/                 # case id = directory name, [A-Za-z0-9_-] only
    reference.txt            # REQUIRED: the adjudicated ground-truth transcript
    case.json                # optional {"id", "tags": [...], "notes"}
    result.json              # optional pipeline-result sidecar (schema below)
    fixture.pcm              # optional raw s16le/mono/16 kHz PCM for --replay
```

`case.json.id`, when present, must equal the directory name. `fixture.pcm`
must be a regular file (no symlinks) of 1 byte to 32 MiB — the same cap the
daemon's replay reader enforces. The loader fails closed with an error naming
the case: missing/empty `reference.txt`, unsafe case name, malformed
`case.json`/`result.json`, wrong `schema` or `case_id` inside the sidecar.

`corpus.example/` in this directory is a committed, fully SYNTHETIC corpus
(fake words, no audio) used by tests and docs. Copy it somewhere private to
try the commands:

```sh
mkdir -p ~/.local/state/voisu/eval-corpus
cp -r tools/transcript-quality/corpus.example/* ~/.local/state/voisu/eval-corpus/
```

## Result sidecar (`result.json`, `voisu-private-eval-case-result-v1`)

```json
{
  "schema": "voisu-private-eval-case-result-v1",
  "case_id": "the case directory name",
  "origin": "history | manual | replay",
  "captured_at_unix_ms": 0,
  "correlation_id": "optional original Recording correlation ID",
  "source_transcripts": [ { "provider": "groq|deepgram", "text": "..." } ],
  "final_transcript": "the pipeline's final Transcript (null if none)",
  "error": null,
  "delivery": { "delivered": true, "method": "clipboard_fallback", "fallback_reason": null },
  "telemetry": { "telemetry_schema": 2, "recording_duration_ms": 3200,
                 "stop_to_finalized_ms": 940, "stop_to_delivered_ms": 1012 }
}
```

Transcript text and delivery fallback reasons stay in the private sidecar.
The scored run JSON (below) carries numbers only, so it can be committed and
diffed. `origin: "replay"` is reserved — see the replay section.

## Adjudicating the six #205 recordings

For each recording that is still in rolling debug capture
(`~/.local/state/voisu/diagnostics/audio/`, seven-day TTL):

```sh
# 1. Promote the newest completed Recording's audio + evidence into the corpus
#    (labels it, refuses while a Recording is active, writes private modes):
cargo run --manifest-path tools/transcript-quality/Cargo.toml --bin mark-last -- \
  good --note 'issue 205'

# 2. Listen to ~/.local/state/voisu/dev-audio/promoted/<correlation-id>/audio.pcm
#    and type EXACTLY what you said into the corpus case, then attach it:
cargo run --manifest-path tools/transcript-quality/Cargo.toml --bin mark-last -- \
  attach-reference rec-XXXX --file /path/to/what-i-said.txt

# 3. Build an eval-corpus case from the promoted evidence: copy (or symlink is
#    refused — copy) the promoted entry into the corpus and capture the
#    pipeline result from history.jsonl:
mkdir -p ~/.local/state/voisu/eval-corpus/rec-XXXX
cp ~/.local/state/voisu/dev-audio/promoted/rec-XXXX/reference.txt \
   ~/.local/state/voisu/eval-corpus/rec-XXXX/reference.txt
cargo run --manifest-path tools/transcript-quality/Cargo.toml -- \
  capture-result --corpus ~/.local/state/voisu/eval-corpus --id rec-XXXX

# 4. (Optional) record what the case is for:
printf '{ "tags": ["names"], "notes": "issue 205 dictation" }\n' \
  > ~/.local/state/voisu/eval-corpus/rec-XXXX/case.json
```

`capture-result` reads `history.jsonl` by default (or a
`voisu history --json` array via `--history <path>`), writes one
`result.json` sidecar per record (mode 0600, dirs 0700), never touches raw
audio, and warns when `reference.txt` is still missing. Re-running is
idempotent: the sidecar is overwritten from the newest capture. Without
`--id`, every record in the file is captured.

If a recording already expired out of debug capture but you saved its
snapshot, write the sidecar by hand (`"origin": "manual"`) using the schema
above.

## Scoring

```sh
cargo run --manifest-path tools/transcript-quality/Cargo.toml -- \
  score-corpus ~/.local/state/voisu/eval-corpus --json /tmp/run-a.json
```

Prints a per-case table (case, WER, I/D/S, delivery, notes) and one aggregate
line — `corpus_wer` (total edits / total reference tokens), `mean_case_wer`
(unweighted mean of per-case rates), `source_corpus_wer` (the evaluator's
completeness-selected Source Transcript arm), `delivery` rate, the median
`stop_to_delivered_ms`, and the run fingerprint. `--json` writes the
machine-readable run (below); the output path goes through the same
git-tracked-path refusal as `--out`.

Per case: strict word error from `align_words` (I/D/S breakdown),
`critical_error_count` (negation/number/name/code/... — count only, tokens
stay private), `section_loss`, the optional completeness arm
(`source_wer` + `selected_source`), the delivery outcome, and the telemetry
trio. Aggregates never replace per-case rows.

### Run JSON schema — `voisu-private-score-corpus-v1`

Field order below is fixed and tested (`run_json_field_order_and_schema_are_stable`).

```jsonc
{
  "schema": "voisu-private-score-corpus-v1",
  "corpus_dir": "/absolute/corpus/path",
  "cases": [
    {
      "id": "case-id",
      "tags": ["from case.json"],
      "notes": "from case.json (null if absent)",
      "status": "scored | no_final | skipped",
      "reason": null,
      "wer": { "error_rate": 0.2, "insertions": 0, "deletions": 1,
               "substitutions": 0, "reference_tokens": 5 },
      "source_wer": null,
      "selected_source": null,
      "delivery": "delivered | not_delivered | unknown",
      "delivery_method": "clipboard_fallback | compositor_submitted | null",
      "critical_error_count": 0,
      "section_loss": false,
      "telemetry": { "telemetry_schema": 2, "recording_duration_ms": 3200,
                     "stop_to_finalized_ms": 940, "stop_to_delivered_ms": 1012 }
    }
  ],
  "aggregate": {
    "cases_total": 3, "corpus_wer": 0.1364, "delivered": 1,
    "delivery_denominator": 2, "delivery_rate": 0.5,
    "mean_case_wer": 0.1476, "median_stop_to_delivered_ms": 1176.0,
    "no_final": 0, "scored": 3, "skipped": 0,
    "source_corpus_wer": 0.0455, "source_mean_case_wer": 0.0476,
    "total_deletions": 2, "total_insertions": 0,
    "total_reference_tokens": 22, "total_substitutions": 1
  },
  "run_fingerprint": "sha256:... (hash of everything except corpus_dir)"
}
```

Case rows of skipped/no-final cases carry `null` WERs and a `reason`.
`run_fingerprint` lets a slice report "same corpus, one number changed" —
identical scoring outcomes fingerprint identically regardless of where the
corpus lives.

### Comparing two runs

```sh
cargo run --manifest-path tools/transcript-quality/Cargo.toml -- \
  compare /tmp/run-a.json /tmp/run-b.json
```

Prints a per-case delta table (`dWER` in percentage points, `dI/dD/dS`,
status transitions, `only in a/b`) plus aggregate deltas. The one-number
recipe for a PR: score before, merge, score after, `compare`, paste the
aggregate delta.

## Replay path (host-only) and what SKIP means

`score-corpus --replay` covers cases **without** a `result.json`: it copies
the case's `fixture.pcm` into the daemon's private fixtures directory
(`~/.local/state/voisu/diagnostics/fixtures/<case-id>.pcm`, mode 0600, staged
copy removed afterwards), runs `voisu replay <case-id>.pcm` (`--voisu <path>`
or `$VOISU_BIN`, default `voisu`), and removes the staged fixture. Never use
`--replay` in CI: it needs the installed daemon and live provider keys.

Every replay outcome is a documented SKIP with a stable reason (so run JSONs
diff cleanly; the daemon's output goes to stderr):

| Situation | Skip reason |
| --- | --- |
| No result.json, no `--replay` | `no result.json; run capture-result or pass --replay` |
| `voisu` binary not found | `voisu binary not found (pass --voisu or set VOISU_BIN)` |
| Daemon not running (exit 3 / "daemon unavailable") | `daemon unavailable (start voisu-daemon and retry)` |
| Replay ran but cannot be scored | `replay ran but produced no machine-readable transcript (daemon gap; see the README replay section)` |
| Replay rejected | `replay failed (see stderr)` |

### Daemon replay gaps found while building B1 (for a later slice)

1. **Replay output is not machine-readable.** `voisu replay` prints only the
   human `response.message` ("replayed fixture through N Source
   Transcript(s)"). There is no `--json`, and the replay `Response.evidence`
   (`LifecycleEvidence`) carries providers, timings, selection, and decision
   reasons — but **no Source Transcript texts and no final Transcript text**.
   Until the daemon emits them, replay can run the real pipeline but the
   harness cannot score it, so the outcome is always a SKIP. A later slice
   can add `voisu replay --json` (print the full `Response`) plus transcript
   text in the evidence, then write a sidecar with `"origin": "replay"`.
2. **Fixtures are name-bound, not path-bound.** The daemon accepts only a
   plain file name and reads it from its private
   `diagnostics/fixtures/` directory (absolute paths are rejected); hence the
   harness's staging step.
3. **Replays leave no diagnostic record.** Only live Recordings persist to
   `history.jsonl`, so `voisu history`/`voisu export` cannot capture a replay
   outcome either — the machine-readable sidecar has to be built by the
   caller (this tool, in a later slice).
4. **Fixture format has no metadata.** A fixture is raw s16le/mono/16 kHz
   PCM with no header, duration, or sidecar, so corpus cases document the
   format by convention (`fixture.pcm`) and validate size only.
5. **Replay uses the daemon's live configuration** (dictionary snapshot,
   Deepgram toggle resolved at invocation), so replay results are only as
   reproducible as the daemon state — worth remembering when comparing
   replay-based runs across daemon restarts.

## Privacy rules (hard)

- A real corpus is Raja's raw voice: mode 0700 directories, mode 0600 files,
  local-only. `mark-last`, `capture-result`, and the corpus guard never
  write inside a git work tree unless the exact path is gitignored, and the
  guard fails closed if `git check-ignore` errors.
- Committed code ships NO audio and NO real transcripts: `corpus.example/`
  and `fixtures/` are synthetic text only; tests synthesize PCM in tempdirs.
- Run JSONs contain numbers, tags, and your own `notes` — never transcript
  text, critical-error tokens, or fallback reasons. Sidecars contain text
  and must stay in the private corpus.
- No B1 code path uploads, exports, or deletes anything.
