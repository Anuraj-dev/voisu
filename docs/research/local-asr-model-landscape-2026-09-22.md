# Local ASR model landscape — 2026-09-22

## Executive conclusion

Voisu’s current local artifact is Whisper `base.en`: an English-only, 74M-parameter model. The current worker runs `/usr/bin/whisper-cli` with `--no-gpu`, and the project’s locked bakeoff recorded 12.0% WER on the full held-out set and 6.56% on the public-only subset. It also failed the current critical-content and negative-insertion gates. The exact numbers are project evidence, not a claim about Whisper base.en on every machine or corpus.

The most useful candidates are:

1. **NVIDIA Nemotron 3.5 ASR Streaming 0.6B** — best first streaming integration candidate. It has an official GGUF, NVIDIA’s native `NeMo-Speech.cpp` runtime, live microphone support, 40+ language/locales in the runtime documentation, and controllable streaming latency.
2. **Qwen3-ASR-1.7B** — strongest accuracy/multilingual candidate to test, especially for English accents, Hindi, Chinese dialects, and difficult acoustic conditions. Its official streaming path currently requires the vLLM backend, so it is not a drop-in replacement for Voisu’s current CPU-only `whisper-cli` worker.
3. **Parakeet TDT 0.6B v3** — excellent quality-to-size ratio and an official GGUF/native C++ path. It is full-utterance/offline in `NeMo-Speech.cpp`; NeMo also documents buffered/chunked inference, but that is not the same as cache-aware live streaming.
4. **Fun-ASR-Nano-2512** — attractive if Chinese, Japanese, dialects, or accents matter. It combines a SenseVoice encoder with a Qwen3-0.6B head and has a llama.cpp/GGUF path, but it is not currently a true streaming model.
5. **Moonshine Streaming Medium/Small** — the low-resource, genuinely streaming option for English. It is much smaller, but its benchmark quality is below the larger candidates and its current Transformers implementation is not fully efficient streaming.

## Comparison table

WER is lower-is-better. Numbers below are publisher-reported and are not directly comparable unless the dataset, normalization, decoding mode, and runtime are the same. Voisu should make the final decision from the same local corpus and the same host.

| Candidate | Footprint | Languages | Published quality evidence | Streaming / latency | Voisu integration judgment |
|---|---:|---|---|---|---|
| Current Whisper `base.en` | 74M params; project artifact 147.96 MB | English | Voisu bakeoff: 12.0% all / 6.56% public-only; current gates failed | Offline windowed decoding; current worker forces CPU | Baseline only; already integrated, but not good enough as the production Local winner |
| **Qwen3-ASR-0.6B** | 1.88 GB official safetensors | 30 languages + 22 Chinese dialects; includes Hindi | LibriSpeech clean/other **2.11/4.55%**; GigaSpeech **8.88%**; CommonVoice-en **9.92%**; FLEURS-en **4.39%** | Unified offline/streaming model; official streaming currently vLLM-only | Strong smaller Qwen experiment, but needs a Python/vLLM sidecar or a new native runtime; CPU feasibility is unproven |
| **Qwen3-ASR-1.7B** | About 4.7 GB official repository weights | Same 52-language/dialect scope | LibriSpeech clean/other **1.63/3.38%**; GigaSpeech **8.45%**; CommonVoice-en **7.39%**; FLEURS-en **3.35%**; MLS-en **4.58%** | Official vLLM streaming; timestamps require a separate Qwen forced aligner | Best quality-first candidate; likely GPU-first and substantially heavier than the current worker |
| **Nemotron 3.5 ASR Streaming 0.6B** | Official Q8 GGUF about 742 MB; 0.6B params | 35 language labels in the card; runtime docs describe 40+ language/locales | FLEURS WER: English **7.91%**, Hindi **6.81%**, Spanish **4.11%**, French **9.03%** using a 1.12 s frame | Cache-aware streaming; NeMo docs advertise controllable **80 ms–1 s** latency; live mic and `--stream` are supported | **Best first live Voisu prototype.** Native C++/ggml runtime, CPU/CUDA/Vulkan/Metal packages, and local GGUF path reduce deployment risk |
| **Parakeet TDT 0.6B v3** | Official Q8 GGUF about 714 MB; 0.6B params | 25 European languages | Open ASR card: average **6.34%**; LibriSpeech clean/other **1.93/3.59%**; GigaSpeech **9.59%**; TED-LIUM **2.75%** | Full-utterance in current `NeMo-Speech.cpp`; NeMo supports buffered/chunked inference, not cache-aware streaming | **Best offline/native quality experiment.** Requires a separate worker adapter and cannot be advertised as live streaming without measuring end-to-end behavior |
| **Fun-ASR-Nano-2512** | About 1.59 GB BF16 GGUF; about 850 MB Q8 GGUF in the llama.cpp path; ~800M trainable parameters | Chinese, English, Japanese, seven Chinese dialect groups, and 26 regional accents | Official table: LibriSpeech clean/other **1.76/4.33%**; FLEURS-en **5.96%**; WenetSpeech meeting **6.60%** | Offline/chunked; no reliable native word/character timestamps and no built-in streaming | **Very interesting regional-language candidate.** Native llama.cpp path is attractive, but license and model-specific behavior must be checked before packaging |
| **Moonshine Streaming Medium** | 245M params; intended for sub-1 GB edge budgets | English | AMI **10.68%**, GigaSpeech **9.46%**, LibriSpeech clean/other **2.08/5.00%**, average **6.65%** | Streaming sliding-window encoder; 80 ms lookahead in the documented architecture; current Transformers path is not fully efficient streaming | Good low-memory English prototype; likely a latency option rather than the accuracy winner |
| **Moonshine Streaming Small** | 123M params | English | AMI **12.54%**, GigaSpeech **10.41%**, LibriSpeech clean/other **2.49/6.78%**, average **7.84%** | Same streaming architecture | More realistic for weak CPUs than Qwen/Parakeet; test only if memory or startup time dominates |
| Distil-Whisper `distil-large-v3` | 756M params; about 49% smaller than large-v3 | English | Publisher reports 6.3× relative latency and short/long-form WER **9.7/10.8%**, versus large-v3 **8.4/11.0%** on its evaluation | Not a native streaming model; compatible with major Whisper runtimes and has GGML/CT2 variants | Practical English quality upgrade if we want to stay in the Whisper ecosystem; still needs a worker/runtime benchmark |
| **Cohere Transcribe 03-2026** | 2B params; official BF16 file about 4.13 GB | 14 languages | Dated March 2026 Open ASR table: average **5.42%**, with LS-clean **1.25%** and LS-other **2.37%** | Offline Transformers path; no official Voisu-suitable streaming runtime found in this survey | Quality ceiling/research candidate, not first desktop integration; HF access is gated and it is much heavier |

## Qwen3-ASR findings

Qwen3-ASR is not just a speech front-end. The official package exposes Transformers and vLLM backends, and the model card explicitly says the same family supports offline and streaming inference. The important limitation for Voisu is that the documented streaming implementation is currently **vLLM-only** and does not return timestamps. The standard examples use CUDA devices and bfloat16; the official card does not establish acceptable CPU-only latency.

The Qwen3-ASR-1.7B table is compelling, but it is a vendor evaluation using vLLM, bfloat16, greedy decoding, and no language parameter. Its numbers should be treated as a shortlist signal, not a deployment guarantee. Qwen3-ASR-0.6B is the better first Qwen download for a local experiment because it is materially smaller while retaining the same broad language/dialect coverage.

## Streaming distinction

There are three different things being called “streaming” in model documentation:

1. **Cache-aware streaming:** the model retains encoder/decoder state across audio chunks. This is the desired shape for Caps Lock dictation. Nemotron 3.5 is the clearest candidate in this survey.
2. **Buffered/chunked inference:** the runtime repeatedly transcribes overlapping or bounded chunks. This can reduce perceived latency but may repeat, revise, or lose words at boundaries. Parakeet TDT supports this style through NeMo, but `NeMo-Speech.cpp` currently rejects live/stream requests for the TDT model.
3. **Repeated offline decoding:** the application re-runs a full-window model as audio grows. It can look live but costs more CPU/GPU and can revise earlier text. Qwen’s official vLLM streaming path is stronger than this, but it still requires a separate serving runtime.

Voisu should not mark a model as “streaming” until it reports first-partial latency, stable-prefix behavior, endpoint latency, stop-to-final latency, and boundary error rate on real microphone captures.

## Integration options

### Option A — Keep one daemon and add a runtime adapter

This is the cleanest architecture. The daemon keeps the current Local/Cloud routing contract, while the Local worker selects a model adapter from the verified catalog:

```text
Local worker supervisor
  ├── whisper-cli adapter       (current base.en / Whisper family)
  ├── nemo-speech adapter       (Nemotron streaming / Parakeet GGUF)
  ├── qwen-vllm adapter         (Qwen3-ASR 0.6B/1.7B sidecar)
  └── funasr/llama.cpp adapter  (Fun-ASR-Nano)
```

The daemon should still load one selected model per session. Cloud and Local switching remains a routing decision; changing ASR models should be a setup/configuration decision, not a surprise at Caps Lock time.

### Option B — Native C++ sidecar first

For a small implementation delta, package `NeMo-Speech.cpp` and test Nemotron 3.5 first. Its official runtime already provides a CLI, native C API, local HTTP API, realtime WebSocket support, and CPU/CUDA/Vulkan/Metal backends. Voisu can initially process a sealed WAV through the CLI, then move to a long-lived local HTTP/C API worker once latency and lifecycle behavior are proven.

### Option C — Python sidecar for Qwen

Run Qwen3-ASR behind a localhost-only process supervised by Voisu. The sidecar owns Python, PyTorch/vLLM, model loading, and GPU memory; Voisu owns audio capture, deadlines, admission, transcript delivery, and process isolation. This is more operationally expensive, but it avoids pretending that a Qwen checkpoint is compatible with `whisper-cli` or GGML.

## Recommended bakeoff order

1. Re-run the existing locked Voisu corpus with the current `base.en` as the baseline.
2. Add **Nemotron 3.5 Q8 GGUF** using `NeMo-Speech.cpp`, first offline and then cache-aware streaming.
3. Add **Parakeet TDT 0.6B v3 Q8 GGUF** for offline quality comparison.
4. Add **Qwen3-ASR-0.6B** through a localhost vLLM/Transformers sidecar.
5. Add **Qwen3-ASR-1.7B** only if the 0.6B quality/latency tradeoff is promising and the host has enough GPU/RAM headroom.
6. Add Fun-ASR-Nano or Moonshine only for the language/resource profiles where they are specifically valuable.

Measure on the same host and same audio:

- WER plus a weighted critical-term error rate for names, commands, and domain words.
- False insertion rate on silence and non-speech.
- First partial, stable partial, end-of-speech, and final-delivery latency.
- Real-time factor, peak RSS, peak VRAM, model load time, and idle memory.
- Boundary errors when speech crosses chunks.
- Behavior during model warmup, worker restart, timeout, and Cloud ↔ Local switching.
- License/redistribution terms and whether weights can be shipped or must be downloaded after consent.

## Current host constraint

A read-only inventory found an AMD Ryzen AI 7 350, 16 logical CPUs, about 22 GiB RAM, an NVIDIA RTX 5050 Max-Q, and an AMD Radeon 860M. **Correction, 2026-09-22:** `nvidia-smi` failed inside the agent sandbox, but succeeded outside it with exit 0, driver 610.57.04, and 8151 MiB VRAM. The matching open kernel module is installed and loaded. No driver repair is indicated. Voisu's current Whisper worker still passes `--no-gpu` explicitly.

GPU ASR throughput, runtime compatibility, and access under the actual daemon/worker restrictions remain unverified. The proposed next work is a Nemotron CUDA vertical slice followed by the full seven-model development evaluation in [the development plan](../../internal/specs/2026-09-22-local-gpu-development.md). A working driver alone does not establish model execution, sufficient peak memory, or GPU availability inside Voisu's worker sandbox.

## Sources

- [Qwen3-ASR-1.7B official model card](https://huggingface.co/Qwen/Qwen3-ASR-1.7B/raw/main/README.md)
- [Qwen3-ASR-0.6B official repository](https://huggingface.co/Qwen/Qwen3-ASR-0.6B/tree/main)
- [OpenAI Whisper official README](https://github.com/openai/whisper/blob/main/README.md)
- [OpenAI Whisper official model card](https://github.com/openai/whisper/blob/main/model-card.md)
- [NVIDIA Nemotron 3.5 ASR official model card](https://huggingface.co/nvidia/nemotron-3.5-asr-streaming-0.6b)
- [NVIDIA NeMo-Speech.cpp official runtime](https://github.com/NVIDIA/NeMo-Speech.cpp)
- [NeMo-Speech.cpp ASR model guide](https://github.com/NVIDIA/NeMo-Speech.cpp/blob/main/docs/asr/models.md)
- [NVIDIA Parakeet TDT 0.6B v3 official model card](https://huggingface.co/nvidia/parakeet-tdt-0.6b-v3)
- [Fun-ASR official repository and benchmark table](https://github.com/QwenAudio/Fun-ASR)
- [Moonshine Streaming official model card](https://huggingface.co/moonshine-ai/moonshine-streaming-medium)
- [Distil-Whisper official model collection](https://huggingface.co/distil-whisper)
- [Cohere Transcribe release and dated Open ASR results](https://huggingface.co/blog/CohereLabs/cohere-transcribe-03-2026-release)

## Caveats

- Model-card WERs are not a guarantee for Voisu. Accent, microphone, noise, command vocabulary, silence handling, and normalization can reorder the candidates.
- GPU throughput figures are often measured on A100/H100 or a vendor-specific runtime. They do not predict CPU latency on Raja’s PC.
- Community GGUF/ONNX conversions may have different licenses, tensor layouts, and numerical behavior from the upstream model. Use the upstream artifact or record conversion provenance and verify hashes.
- No model should become a production Local selection from this survey alone. It first needs the existing locked-corpus bakeoff and a real microphone/worker gate.
