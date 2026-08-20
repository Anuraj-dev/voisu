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

## Tests

```sh
cargo test --manifest-path tools/transcript-quality/Cargo.toml -- --test-threads=4
```

Synthetic fixtures live in `fixtures/`. They are short fake sentences, not
Raja audio.
