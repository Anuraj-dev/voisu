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

`--out` is optional. The default is `transcript-quality-report.json` next to
the manifest, so a private manifest does not write into the git tree. Tests
must pass `--out` under a tempfile.

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
| `reference_kind` | `adjudicated` (default when a reference is present) or `script` |
| `speaker` | Speaker label |
| `tags` | Free-form test tags |
| `rendering_policy` | `natural` / `adaptive` / `structured` for the organizer call |

Relative paths resolve against the manifest directory.

The original reading script is not spoken truth. `reference_kind` of `script`,
`reading_script`, or `prompt`, or a `reading-script` / `script` tag, treats the
reference as missing evidence.

## Arms

1. **completeness_aware_source** — evaluator heuristic (ticket 02 is not in
   product). Discount repeated filler, duplicated loops, and known outro
   garbage; prefer the materially fuller safe Source Transcript.
2. **guarded_pipeline** — `voisu_core::organize_local_baseline` plus
   `format_validated` on the selected source. This tool does not reimplement
   the organizer.
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
(prefix or body deleted relative to the reference or to the source that fed
the organizer).

The JSON has a `stable` object (sorted keys, no wall-clock) and a `volatile`
object for timestamps and local-call latency. Identical inputs hash to the
same `stable_fingerprint`.

## Tests

```sh
cargo test --manifest-path tools/transcript-quality/Cargo.toml -- --test-threads=4
```

Synthetic fixtures live in `fixtures/`. They are short fake sentences, not
Raja audio.
