# corpus.example — synthetic scoring examples

Every file here is SYNTHETIC: fake transcript words ("alpha bravo ..."),
fake timings, no audio, no human voice. The directory is committed so the
loader, scorer, and JSON schema can be exercised offline and documented.
Do NOT put real recordings, real references, or real pipeline text here.

Copy the layout to a private location to try it (the CLI refuses a corpus
inside a git work tree unless the path is gitignored):

```sh
mkdir -p ~/.local/state/voisu/eval-corpus
cp -r tools/transcript-quality/corpus.example/* ~/.local/state/voisu/eval-corpus/
```

## Layout (per case directory)

| File          | Required | Purpose |
| ------------- | -------- | ------- |
| `reference.txt` | yes    | The audio-adjudicated ground-truth transcript (what Raja actually said). Loader refuses an empty or missing file. |
| `case.json`   | no       | `{"id", "tags", "notes"}` metadata. `id`, when present, must equal the directory name. |
| `result.json` | no       | Captured pipeline result sidecar (`voisu-private-eval-case-result-v1`). Without it the case SKIPs unless `--replay` runs. |
| `fixture.pcm` | no       | Raw s16le/mono/16 kHz PCM for `--replay` (daemon replay fixture format). Never committed here: this example ships none. |

See the main README for the sidecar schema and the full scoring workflow.
