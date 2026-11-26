# Growing the local ASR corpus

The bakeoff can only pick a winner on a corpus large enough to tell models
apart. This page covers adding clips and checking the corpus against the
thresholds. The full inventory → freeze → replay → score workflow is in
[local-gpu-development.md](local-gpu-development.md).

Everything below lives in the private evaluation directory
(`~/.local/state/voisu/local-asr-eval-v1/` by default). Nothing goes in the
repository.

## What the corpus needs

`benchmark.py check-corpus` checks the corpus against these thresholds. They
mirror `crates/voisu-app/src/local_worker/bakeoff.rs`:

| Requirement | Target |
|---|---|
| English speech clips | ≥ 100 |
| Held-out English speech | ≥ 80 |
| Tuning English speech | ≤ 40 |
| Negative clips (silence, room noise, no speech) | ≥ 20 |
| Speech in each of 1–10 s, 10–30 s, 30–120 s | ≥ 1 |
| Long clips (120–600 s, i.e. 2–10 min) | ≥ 5 |
| Speech with no split, duplicate audio, clips over 600 s | 0 |

Hinglish clips can stay in the corpus. They are listed but never count.

Tuning clips never enter the winner decision; the cap of 40 only bounds the
tuning split. `tools/transcript-quality/bakeoff-contract-v1.json` (exactly 20
tuning clips) is the frozen r7 attestation contract. It does not gate this
corpus; `check-corpus` is the gate. Do not move tuned clips into held-out to meet the held-out
threshold: clips you tuned on would contaminate the decision set. Record new
held-out clips instead.

Duration bands include their upper bound: a clip of exactly 120 000 ms is in
the 30–120 s band, not the long band.

One corpus serves both the Rust bakeoff lock and the local-accuracy P0a
target.

The RSS growth bound (≤ 128 MiB over a 200-recording soak) is a soak gate,
not a corpus property. The run record reports peak RSS for each run.

## Recording

- Use 16 kHz, mono, signed 16-bit PCM WAV, one utterance or passage per file.
- Name files by kind, for example `en-scripted-0042.wav`, `en-long-0003.wav`
  or `neg-0007.wav`. The file name becomes the record ID, so never reuse or
  rename one after a freeze.
- Spread clip lengths: mostly 1–30 s dictation, some 30–120 s, and at least 5
  clips of 2–5 min of continuous speech. Clips of 60–120 s do not count
  toward the long-clip threshold.
- For negatives, record room tone, keyboard noise, a fan, or a far-off TV
  with no nearby speech. The reference is `""`.
- The corpus is a single speaker for now: you. Record only your own voice.
  The consent receipt covers you; clips without consent are refused.

## References

Add one row per WAV to `references.json`.
[`tools/local-asr/examples/references.example.json`](../tools/local-asr/examples/references.example.json)
shows each kind. The fields:

- `reference` is what was actually said. It is not what you meant to say.
- `reference_source`:
  - `"scripted"` means you read a prompt. Use the prompt as the
    reference, and fix it wherever you slipped.
  - `"adjudicated"` means spontaneous speech, transcribed by listening.
- `split`: `"heldout"` (scored for the decision) or `"tuning"` (free for
  settings work, at most 40, never scored for the decision). New clips are held out by default: put
  `"split": "heldout"` in every new row (the example rows do, and
  `add-captures` drafts already carry it). Use `"tuning"` only on purpose.
  Rows already in the corpus without a split keep the hashed split they got
  at freeze time; leave them as they are so their splits do not change.
- `critical_tokens` lists the words a wrong transcript must not get wrong.
  Use numbers as spoken ("forty two"), names, negations ("do not"), and
  required phrases. Each entry is one unit and may span several words.
- `tags` are free-form labels (`"numbers"`, `"spontaneous"`, `"long"`).

### Spontaneous speech

A reference for unscripted speech is a verbatim transcript written by ear:

1. Draft it. `tools/dev bench add-captures` turns recent dictations into
   drafts prefilled with Voisu's own transcript. Or type it from scratch.
   Do not paste in a model's output and leave it uncorrected; that biases
   the corpus toward that model.
2. Listen to the whole clip and correct the draft word by word. Replay the
   unclear parts.
3. Keep repeats and false starts that were actually said ("the the
   release"). Leave out fillers (um, uh, er); scoring ignores them anyway.
4. Write numbers the way they were said. Normalized scoring treats "25" and
   "twenty five" as equal, but strict WER does not.
5. Set `"reference_source": "adjudicated"`. For a draft from add-captures,
   also set `review_status` to `"reviewed"` and remove the `"unreviewed"`
   tag.

## Check, then freeze

Check before freezing. On an inventory, `--seed` predicts the splits that
freeze would hash for rows that don't pin one:

```sh
EVAL="$HOME/.local/state/voisu/local-asr-eval-v1"
python3 tools/local-asr/benchmark.py inventory --input-dir "$EVAL/audio" \
  --references "$EVAL/references.json" --consent-receipt "$EVAL/consent.json" \
  --corpus-id local-english-v2 --output "$EVAL/inventory.json"
python3 tools/local-asr/benchmark.py check-corpus --inventory "$EVAL/inventory.json" --seed local-asr-v1
```

The command prints each threshold with `ok` or `MISS`, then lists what is
missing (for example `12 more held-out speech clips`). It exits with 1 until
every threshold is met. `--json` prints the same result as JSON.

When the check passes, freeze under a new corpus ID with the same seed,
`local-asr-v1`, then run the check again on the frozen corpus. Keep the seed:
a new one re-hashes the split of every row that does not pin one (with
`local-asr-v2`, 10 of the 40 existing English clips change split).

```sh
python3 tools/local-asr/benchmark.py freeze --inventory "$EVAL/inventory.json" \
  --seed local-asr-v1 --heldout-fraction 0.2 --output "$EVAL/corpus.json"
python3 tools/local-asr/benchmark.py check-corpus --corpus "$EVAL/corpus.json"
```

## Run records

`tools/dev bench run` writes `run_records.json` next to `report.json`. You
can also build it from any scored run:

```sh
python3 tools/local-asr/benchmark.py records --corpus "$EVAL/corpus.json" \
  --evidence RUN/replay.json --report RUN/report.json \
  --threads distil-large-v3.5-int8=8 --incumbent distil-large-v3.5-int8 --output RUN/run_records.json
```

There is one record per model, runtime, device/threads and window
configuration. Each record holds:

- strict WER, normalized WER and critical-token error rate, each with a
  clip-bootstrap CI;
- p50 and p95 latency;
- peak RSS;
- insertions on negative clips, with measured and expected negative counts;
- per duration band of each scope: clips measured/expected, reference tokens
  and strict WER.

Each metric is reported over all English clips (`english_all`, which matches
the earlier tables) and over held-out English (`english_heldout`, the
decision scope). The file also has paired comparisons and a winner-rule
verdict.

The verdict is `winner` only when every check passes:

- `corpus_lock_met`: the frozen corpus passes `check-corpus`. Until it does,
  every verdict is `pending`, whatever the other checks say.
- `complete_coverage`: both models were measured on every held-out English
  clip and every negative clip, and no paired comparison dropped a clip. A
  candidate that failed any clip is `not_winner`.
- `paired_ci_better`: the paired clip-bootstrap CI of normalized WER
  (candidate minus incumbent) lies below zero.
- `zero_negative_insertions` and `no_critical_regression`.

An English reference that normalizes to nothing (for example only "um")
makes that metric `invalid_reference` instead of failing the run.

Not measured yet, shown as `n/m` or `not_measured` rather than left blank:

- the system memory delta: the protected replay does not report it;
- thread count, unless the model manifest declares `limits.cpu_cores` (`bench
  run` records that cap as the thread count and says so in
  `threads_source`) or `records --threads` sets it. It is a declared cap, not
  an observed thread count.

Known limitation: a critical-token entry counts as hit when its tokens occur
anywhere in the hypothesis, not only in the span aligned to it. Critical
tokens go through the same spelling map as normalized WER.
