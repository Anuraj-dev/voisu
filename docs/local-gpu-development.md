# Local GPU development and ASR evaluation

This is a private development workflow for the seven local ASR candidates. It does not select a production model, alter the frozen #267 corpus, or upload audio and transcript text. Keep every output path in a private local directory; the JSON files contain absolute audio paths, adjudicated references, and raw model text.

The benchmark treats English and Latin-script Hinglish as separate languages. A Hinglish reference preserves Hindi words written in Latin letters; it is not an English translation. Strict WER uses Unicode normalization, case folding, punctuation removal, and tokenization only. It never transliterates or asks an LLM to rewrite text. An optional versioned token map produces a second spelling-normalized measure beside strict WER.

The seven model IDs are:

```text
nemotron-3.5-0.6b
parakeet-tdt-0.6b-v3
qwen3-asr-0.6b
qwen3-asr-1.7b
fun-asr-nano-2512
moonshine-streaming-medium
cohere-transcribe-03-2026
```

## Prepare a private evaluation directory

Use a directory outside the repository. Put consented 16 kHz, mono, signed 16-bit PCM WAV files under `audio/`. Create one adjudicated reference row per WAV in `references.json`; references must describe what was actually spoken, including omissions, repeats, and code-switches. A negative sample has `"language": "negative"` and an empty reference.

```sh
mkdir -m 700 -p "$HOME/.local/state/voisu/local-asr-eval-v1/audio"
mkdir -m 700 -p "$HOME/.local/state/voisu/local-asr-eval-v1/results"
python3 tools/local-asr/benchmark.py inventory \
  --input-dir "$HOME/.local/state/voisu/local-asr-eval-v1/audio" \
  --references "$HOME/.local/state/voisu/local-asr-eval-v1/references.json" \
  --consent-receipt "$HOME/.local/state/voisu/local-asr-eval-v1/consent.json" \
  --corpus-id local-english-hinglish-v1 \
  --output "$HOME/.local/state/voisu/local-asr-eval-v1/inventory.json"
```

`inventory` hashes every WAV and records its duration, language, consent ID, tags, and adjudicated reference. It does not copy audio. The command refuses missing references, malformed WAVs, missing consent, duplicate IDs, or the frozen `#267` corpus ID.

Freeze the exact corpus before selecting settings. The split is deterministic from the seed, record ID, and language, and the manifest hash covers all record metadata, references, audio hashes, and splits.

```sh
python3 tools/local-asr/benchmark.py freeze \
  --inventory "$HOME/.local/state/voisu/local-asr-eval-v1/inventory.json" \
  --seed local-asr-v1 \
  --heldout-fraction 0.20 \
  --output "$HOME/.local/state/voisu/local-asr-eval-v1/corpus.json"
```

Do not edit `corpus.json` after this point. If a reference or recording changes, make a new corpus ID and seed. The benchmark rehashes the audio on every later command and rejects stale manifests.

## Install and select a development model

Build the current release binary, then register each separately pinned model manifest with the Rust development CLI. The Rust side validates the model ID, every artifact and runtime pin, the requested device, and the resource limits before it writes its private snapshot.

```sh
cargo build --release -p voisu-app --bin voisu --bin voisu-daemon
voisu local-dev register --manifest /absolute/private/manifests/qwen3-asr-0.6b.json
voisu local-dev select qwen3-asr-0.6b
voisu local-dev status --json
```

A manifest follows the v1 shape in `tools/local-asr/examples/model-manifest.example.json`. The `runtime.python` and `runtime.worker` paths are absolute, and all model and adapter files have SHA-256 and byte-count pins. `vram_bytes` is zero only for CPU. A manifest is an admission request, not evidence that the model fits.

The normal development service remains opt-in and mutually exclusive with production:

```sh
~/.local/bin/voisu-dev on
voisu mode local
~/.local/bin/voisu-dev status
```

Use `~/.local/bin/voisu-dev off` to return to the production service and `voisu mode cloud`. A model selection does not change production catalog selection.

## Replay exact audio through Rust

`benchmark.py replay` invokes the protected Rust seam once for each selected model and each frozen recording. It never launches `worker.py` or another unrestricted Python process. The current seam is:

```text
voisu local-dev replay --json --manifest MODEL_MANIFEST --audio WAV
```

The command must emit one JSON object on stdout and keep diagnostics on stderr. The required base fields are `version`, `model_id`, `language`, `requested_device`, `observed_device`, `resident`, `inference_ms`, `text`, `status`, and `error`. For benchmark evidence, add the exact `model_revision`, `runtime_revision`, receipt or artifact hashes, `backend`, `host_profile`, and protected execution identity. Include `load_ms` with an explicit `phase: "cold"` and `inference_ms` with `phase: "warm"` only when the worker session is resident and the phase is measured. A one-shot invocation that reloads weights for every recording has cold evidence only; the report leaves warm p50/p95 unavailable.

Run all selected models against the same manifest. The model manifest path identifies the model; the audio path always comes from the frozen corpus.

```sh
python3 tools/local-asr/benchmark.py replay \
  --corpus "$HOME/.local/state/voisu/local-asr-eval-v1/corpus.json" \
  --model-manifest nemotron-3.5-0.6b=/absolute/private/manifests/nemotron-3.5-0.6b.json \
  --model-manifest parakeet-tdt-0.6b-v3=/absolute/private/manifests/parakeet-tdt-0.6b-v3.json \
  --model-manifest qwen3-asr-0.6b=/absolute/private/manifests/qwen3-asr-0.6b.json \
  --model-manifest qwen3-asr-1.7b=/absolute/private/manifests/qwen3-asr-1.7b.json \
  --model-manifest fun-asr-nano-2512=/absolute/private/manifests/fun-asr-nano-2512.json \
  --model-manifest moonshine-streaming-medium=/absolute/private/manifests/moonshine-streaming-medium.json \
  --model-manifest cohere-transcribe-03-2026=/absolute/private/manifests/cohere-transcribe-03-2026.json \
  --rust-cli voisu \
  --output "$HOME/.local/state/voisu/local-asr-eval-v1/results/replay.json"
```

The runner records raw model text and delivered text in separate fields. It records unavailable model/audio pairs as unavailable evidence so a broken candidate remains visible. `--fixture` is for synthetic contract tests only and marks the evidence as non-quality evidence.

## Score a replay and inspect the report

Score strict raw recognition first. Delivered text is scored separately only when the protected seam supplied it. The report includes S/D/I counts, negative insertions, critical-content errors by names/numbers/commands, and sample counts for English, Hinglish, and negative strata.

```sh
python3 tools/local-asr/benchmark.py score \
  --corpus "$HOME/.local/state/voisu/local-asr-eval-v1/corpus.json" \
  --evidence "$HOME/.local/state/voisu/local-asr-eval-v1/results/replay.json" \
  --model nemotron-3.5-0.6b \
  --model parakeet-tdt-0.6b-v3 \
  --model qwen3-asr-0.6b \
  --model qwen3-asr-1.7b \
  --model fun-asr-nano-2512 \
  --model moonshine-streaming-medium \
  --model cohere-transcribe-03-2026 \
  --output "$HOME/.local/state/voisu/local-asr-eval-v1/results/report.json"
```

The scorer rejects a stale corpus hash, changed audio, changed language or split, duplicate results, missing protected Rust evidence, missing runtime/model identity, and unknown models. A selected model with no results is `untested`; one whose protected runs all report an unavailable status is `unavailable`; partial coverage is `measured_partial`. These states are retained in the report rather than converted into a ranking.

If a spelling normalization is agreed before held-out scoring, store only a versioned deterministic token map, for example `{ "version": "hinglish-spelling-v1", "token_map": { "accha": "achha" } }`, outside the repository and pass it with `--normalizer`. The strict result remains the primary score and the normalized result is labeled separately. No transliteration or generative correction is performed.

## Make a scoped recommendation

Recommendations are filtered by language and, when supplied, exact host and duration profiles from measured evidence. There is no universal score. A missing warm timing or memory measurement leaves that label empty.

```sh
python3 tools/local-asr/benchmark.py recommend \
  --report "$HOME/.local/state/voisu/local-asr-eval-v1/results/report.json" \
  --language hinglish \
  --duration-profile short \
  --host-profile rtx-5050 \
  --output "$HOME/.local/state/voisu/local-asr-eval-v1/results/recommendation.json"
```

The report and recommendation can support a development choice on the measured host. They do not establish public quality, Fedora support, GPU compatibility, or a production winner. User recordings supplied later are required for real quality evidence.

## Remaining-model identities

These three stay in the development catalog even if another candidate scores better. None of them is a Hindi option for this evaluation.

- `fun-asr-nano-2512` is Fun-ASR-Nano-2512 only. Fun-ASR-MLT-Nano is a different checkpoint and is rejected. The native CPU profile is the documented llama.cpp encoder (`funasr-encoder-f16.gguf`) plus Qwen3-0.6B decoder GGUF from `FunAudioLLM/Fun-ASR-Nano-GGUF` revision `46e849502a867080d66d351b8dfb1018b607e509`, runtime `runtime-llamacpp-v0.2.6` (`funasr-llamacpp-linux-x64-avx2`). Linux llama.cpp packages have no Fun-ASR-Nano CUDA backend (CUDA zips are Windows-only); a CUDA request uses `FunAudioLLM/Fun-ASR-Nano-2512-hf` revision `d93b302ee7fd505e1b3576120fc142fc6f7820e1` through Transformers 5.17.0 rather than CPU spill.
- `moonshine-streaming-medium` is CPU-first English. Official moonshine-voice Medium Streaming artifacts from `https://download.moonshine.ai/model/medium-streaming-en/quantized` are `encoder.ort`, `adapter.ort`, `cross_kv.ort`, `decoder_kv.ort`, `frontend.ort`, `streaming_config.json`, and `tokenizer.bin` with `model_arch=MEDIUM_STREAMING`. The Hugging Face Transformers safetensors checkpoint is not this adapter. The older non-streaming `encoder_model.ort` / `decoder_model_merged.ort` names 404 for this model.
- `cohere-transcribe-03-2026` is English-only here. Upstream lists 14 languages and does not include Hindi. Hugging Face revision `b1eacc2686a3d08ceaae5f24a88b1d519620bc09` is gated (`gated=auto`); a 2026-09-22 fetch of `config.json` returned HTTP 401 `x-error-code: GatedRepo`. Inference still requires local artifacts and no network.

## Current blockers

No Raja English/Hinglish dictation corpus is checked in, so quality rankings and Model Recommendations stay unmeasured. Synthetic unittest fixtures exercise the manifest, scoring, stale-evidence rejection, negative insertion accounting, remaining-model identity, and recommendation behavior only.

Qwen3-ASR 0.6B and 1.7B have a pinned isolated Transformers CUDA runtime (`tools/local-asr/runtimes/qwen-transformers`) and host snapshots under `target/local-asr/`. A 2026-09-22 RTX 5050 (8151 MiB) offline JFK run observed `cuda:0` with no CPU spill: 0.6B peak RAM 2465 MiB / VRAM 1804 MiB; 1.7B peak RAM 4869 MiB / VRAM 4196 MiB, under the 6113 MiB 75% admission budget and 2038 MiB desktop reserve. Production Local selection stays closed.

Fun-ASR-Nano GGUF hashes, Fun-ASR-Nano-2512-hf `model.safetensors` SHA-256 `1bbb6dcc5d8b75084a399d48c4d4b0f3aa1d3f09f2616ae40ed2c4fba03d89c9`, and the llama.cpp `runtime-llamacpp-v0.2.6` Linux x64 AVX2 package (`aaebc5470f846ce915200b35d6e9f9bd0a0d3ed399d39e49bdeb7a1f1782bc70`) are pinned. A 2026-09-22 JFK smoke on this RTX 5050 host observed: Fun-ASR llama.cpp CPU Transcript in 1.67s; Fun-ASR-Nano-2512-hf Transformers CUDA `cuda:0` peak RAM 2576 MiB / VRAM 1625 MiB, no CPU spill, offline; Moonshine Voice 0.1.5 CPU load 0.274s / cold 1.994s / warm 1.934s. Cohere Transcribe remains Hugging Face gated (`401 GatedRepo`, no accepted token). Root still needs the real-host protected launcher gate before any Model Recommendation is treated as measured. English only for these three; none is a Hindi option.
