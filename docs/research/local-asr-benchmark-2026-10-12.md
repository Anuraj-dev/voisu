# Local ASR benchmark — Raja's English + Hinglish corpus (2026-10-12)

Development evidence on one host. It is **not** the #267 production gate, does not make any model a Bakeoff Winner, and says nothing about Fedora or other GPUs. Per-clip transcripts and audio stay private; this file holds aggregates only.

## Method

- Corpus `local-english-hinglish-v1`: 92 clips recorded by Raja (40 English, 40 Latin-script Hinglish, 12 silence/noise negatives), 663 s of audio, frozen with `benchmark.py freeze` (seed `local-asr-v1`, 20% held-out). Private dir: `~/.local/state/voisu/local-asr-eval-v1/`.
- References are the **scripted prompts** (`prompts.md`), not clip-by-clip adjudicated speech. A misspoken clip counts against the model.
- Replay through the protected Rust seam (`voisu local-dev replay`), one resident worker per model, strict WER (Unicode normalisation only; no transliteration, no spelling map). Numbers spoken as words and Devanagari output both count as errors.
- Host: RTX 5050 Laptop, 8151 MiB VRAM, 4 CPU cores allotted to the worker. One run, one speaker.
- GPU run: 13 registered models, all 92 clips. CPU run: the 7 models with weights under 1 GB (whisper-cpp-large-v3-turbo-q5-0 produced 91/92 clips: one timeout, so `measured_partial`).
- Cohere Transcribe was not tested (gated; no local artifacts).

## GPU results (cuda:0)

| Model | EN WER | Hinglish WER | Warm p50 | Cold load | Peak VRAM | Peak RAM | Neg. insertions (words) |
|---|---|---|---|---|---|---|---|
| distil-large-v3.5-int8 | **9.0%** | 79.8% | 292 ms | 4.7 s | 1.09 GB | 1.8 GB | 18 |
| faster-whisper-large-v3-turbo-int8 | 9.0% | 121.6% | 303 ms | 3.8 s | 1.15 GB | 1.9 GB | 41 |
| whisper-cpp-large-v3-turbo-q5-0 | 10.3% | 95.0% | 414 ms | 2.1 s | 1.18 GB | 0.5 GB | 22 |
| whisper-cpp-small-q8-0 | 10.8% | 90.1% | 135 ms | 1.0 s | 0.78 GB | 0.5 GB | 24 |
| granite-speech-5.0-470m-turboctc | 12.6% | 81.4% | 23 ms | 6.9 s | 1.07 GB | 1.7 GB | 21 |
| parakeet-tdt-0.6b-v3 | 12.8% | 80.8% | 44 ms | 1.0 s | 0.94 GB | 0.4 GB | 0 |
| moonshine-streaming-medium (CPU-only model) | 13.6% | 101.4% | 2386 ms | 7.9 s | — | 0.9 GB | 0 |
| whisper-hindi2hinglish-apex-int8 | 14.0% | **32.3%** | 301 ms | 4.7 s | 1.15 GB | 1.9 GB | 12 |
| fun-asr-nano-2512 | 16.1% | 89.6% | 149 ms | 10.0 s | 1.79 GB | 2.6 GB | 12 |
| qwen3-asr-1.7b | 17.4% | 171.5% | 547 ms | 12.0 s | 4.27 GB | 4.9 GB | 0 |
| phonon-2 | 17.5% | 82.5% | 464 ms | 23.5 s | 1.43 GB | 3.8 GB | 3 |
| qwen3-asr-0.6b | 20.0% | 147.3% | 278 ms | 8.2 s | 1.85 GB | 2.5 GB | 1 |
| nemotron-3.5-0.6b | 25.7% | 92.4% | 44 ms | 1.2 s | 1.03 GB | 0.5 GB | 3 |

## CPU-only results (4 cores, weights < 1 GB)

| Model | EN WER | Hinglish WER | Warm p50 | p95 | Peak RAM |
|---|---|---|---|---|---|
| parakeet-tdt-0.6b-v3 | 12.8% | 80.8% | 0.61 s | 1.2 s | 0.9 GB |
| whisper-cpp-small-q8-0 | 10.8% | 90.1% | 2.1 s | 2.4 s | 0.8 GB |
| granite-speech-5.0-470m-turboctc | 12.4% | 81.4% | 0.88 s | 1.3 s | 3.6 GB |
| phonon-2 | 17.5% | 82.5% | 1.0 s | 2.1 s | 5.1 GB |
| moonshine-streaming-medium | 13.6% | 101.4% | 2.3 s | 9.8 s | 0.9 GB |
| nemotron-3.5-0.6b | 25.5% | 91.6% | 0.68 s | 1.9 s | 1.0 GB |
| whisper-cpp-large-v3-turbo-q5-0 | 10.0% | 95.4% | 13.2 s | 13.5 s | 1.2 GB |

The Hinglish Whisper and the other models over 1 GB of weights were **not** run on CPU.

## Findings

- **English quality:** `distil-large-v3.5-int8` is the best English model (9.0% WER, digits kept as digits, fewest silence insertions among the top two). Its critical-content errors across all 92 clips: 26 names, 11 numbers, 21 commands.
- **Hinglish:** only `whisper-hindi2hinglish-apex-int8` is usable (32.3%). Every other model is 80%+ because it emits Devanagari, translates to English, or hallucinates.
- **VRAM:** nothing tested beats the ~1.1 GB winners by using more VRAM. `qwen3-asr-1.7b` is the only model above 4 GB (4.27 GB peak) and it is worse. Larger models (for example full Whisper large-v3) were not tested.
- **CPU-only English:** `parakeet-tdt-0.6b-v3` (0.6 s, no invented text on silence). `whisper-cpp-small-q8-0` is more accurate but slower and invents text on silence.
- **Silence hallucination:** all Whisper-family models insert words on silent clips (12–41 words across 12 clips). Parakeet, moonshine and qwen insert none or almost none.

## Recommendation (development evidence only)

| Host | English | English + Hinglish |
|---|---|---|
| ≥ 4 GB VRAM | distil-large-v3.5-int8 | whisper-hindi2hinglish-apex-int8 |
| CPU only | parakeet-tdt-0.6b-v3 | none measured (Hinglish Whisper on CPU untested) |

## Reproduce

Evidence files (private, absolute paths and raw text inside): `~/.local/state/voisu/local-asr-eval-v1/results/{replay2.json,report.json,replay_cpu.json,report_cpu.json}`. Usage: `docs/local-gpu-development.md`.
