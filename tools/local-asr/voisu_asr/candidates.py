"""Identity pins for the low-VRAM Local ASR candidates.

Pure data with no runtime imports, so the manifest validator and the
adapters share one table without an import cycle. Each candidate pins every
file its loader reads (weights, config, tokenizer, preprocessor, vocabulary)
at a fixed upstream revision, the precision it was measured at, and the
measured peak VRAM that a CUDA manifest may not undercut. Mirrored by
`CANDIDATE_PINS` in `crates/voisu-app/src/local_model/development.rs`.
"""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path
from typing import Iterable, Mapping

WHISPER_CPP = "whisper.cpp"
CTRANSLATE2 = "ctranslate2"
TRANSFORMERS_CTC = "transformers-ctc"
MIB = 1024 * 1024

# Decoding is fixed, not a manifest option: one temperature (0.0) and no
# temperature fallback. Fallback resamples hard audio at higher temperatures,
# which made repeated runs of one clip return different transcripts.
# CTranslate2 beam search is repeatable, so it keeps beam 5. whisper.cpp 1.8.5
# beam search is not (three runs of one 62 s clip gave three transcripts on
# CUDA and on CPU), and greedy with flash attention looped into repeated text,
# so whisper.cpp decodes greedily with flash attention off.
BEAM_SIZE = 5
BEST_OF = 5
WHISPER_CPP_BEST_OF = 1


@dataclass(frozen=True)
class Candidate:
    model_id: str
    backend: str
    source: str
    revision: str
    # The file the runtime loads as the model; model_path names it for whisper.cpp.
    weights: str
    # Every file the loader reads, relative to the model root: (name, sha256, bytes).
    files: tuple[tuple[str, str, int], ...]
    # Manifest language -> Whisper language token (None = model detection).
    languages: tuple[tuple[str, str | None], ...]
    # Measured peak VRAM on the RTX 5050; a CUDA manifest's vram_bytes may not be lower.
    min_vram_bytes: int
    # (option key, CUDA value, CPU value) the model was measured at, or None.
    precision: tuple[str, str, str] | None

    def whisper_language(self, language: str) -> str | None:
        return dict(self.languages)[language]

    @property
    def language_codes(self) -> frozenset[str]:
        return frozenset(code for code, _ in self.languages)

    def precision_for(self, device: str) -> str | None:
        if self.precision is None:
            return None
        return self.precision[1] if device.startswith("cuda:") else self.precision[2]


_EN_AUTO = (("en", "en"), ("auto", None))
_EN = (("en", "en"),)
# Whisper-Hindi2Hinglish-Apex writes Latin-script Hinglish when decoded with the
# English token (upstream usage: language="en"). Voisu's `hi` means Hinglish, so
# both manifest languages select that token; Devanagari output is not a mode.
_HINGLISH = (("en", "en"), ("hi", "en"))
# CTranslate2 stores float16 weights and quantizes on load.
_CT2_PRECISION = ("compute_type", "int8_float16", "int8")
_CT2_VOCABULARY = ("vocabulary.json", "c69260f2ab26d659b7c398f9a2b2b48ed0df16c3b47d7326782fd9cba71690c1", 1_068_114)


def _ct2_files(
    config: tuple[str, int],
    model: tuple[str, int],
    preprocessor: tuple[str, int],
    tokenizer: tuple[str, int],
) -> tuple[tuple[str, str, int], ...]:
    return (
        ("config.json", *config),
        ("model.bin", *model),
        ("preprocessor_config.json", *preprocessor),
        ("tokenizer.json", *tokenizer),
        _CT2_VOCABULARY,
    )


CANDIDATES: dict[str, Candidate] = {
    item.model_id: item
    for item in (
        Candidate(
            "whisper-cpp-large-v3-turbo-q5-0",
            WHISPER_CPP,
            "https://huggingface.co/ggerganov/whisper.cpp",
            "5359861c739e955e79d9a303bcbc70fb988958b1",
            "ggml-large-v3-turbo-q5_0.bin",
            (
                (
                    "ggml-large-v3-turbo-q5_0.bin",
                    "394221709cd5ad1f40c46e6031ca61bce88931e6e088c188294c6d5a55ffa7e2",
                    574_041_195,
                ),
            ),
            _EN_AUTO,
            1186 * MIB,
            None,
        ),
        Candidate(
            "whisper-cpp-small-q8-0",
            WHISPER_CPP,
            "https://huggingface.co/ggerganov/whisper.cpp",
            "5359861c739e955e79d9a303bcbc70fb988958b1",
            "ggml-small-q8_0.bin",
            (("ggml-small-q8_0.bin", "49c8fb02b65e6049d5fa6c04f81f53b867b5ec9540406812c643f177317f779f", 264_464_607),),
            _EN_AUTO,
            786 * MIB,
            None,
        ),
        Candidate(
            "faster-whisper-large-v3-turbo-int8",
            CTRANSLATE2,
            # The repo now redirects to dropbox-dash/faster-whisper-large-v3-turbo (same commit).
            "https://huggingface.co/mobiuslabsgmbh/faster-whisper-large-v3-turbo",
            "0a363e9161cbc7ed1431c9597a8ceaf0c4f78fcf",
            "model.bin",
            _ct2_files(
                ("b0253ea6c0d3bea6b1e19e91a02acfd3b53f4467362efcb5a3e6b16c9b3a9b7e", 2_263),
                ("e76620f83d5f5b69efd3d87e3dc180c1bd21df9fbebacfd4335e5e1efcc018da", 1_617_884_929),
                ("7ccc62c6f2765af1f3b46c00c9b5894426835a05021c8b9c01eecb6dfb542711", 340),
                ("297b13372ac43916285644fb9687add3cc62ee2a1adb60da3dc25cc94c1871fd", 2_710_337),
            ),
            _EN_AUTO,
            1280 * MIB,
            _CT2_PRECISION,
        ),
        Candidate(
            "whisper-hindi2hinglish-apex-int8",
            CTRANSLATE2,
            # No published CTranslate2 build: these are the reproducible float16
            # ct2-transformers-converter outputs (ctranslate2 4.8.2, transformers
            # 4.56.1) of upstream model.safetensors
            # 5560830fe8fa09b8cdf1b5926d0cc39a2586eadaad685d809e156432c4ebca80.
            "https://huggingface.co/Oriserve/Whisper-Hindi2Hinglish-Apex",
            "f3214eed20b4e4d4144e739982d911f87b9cb223",
            "model.bin",
            _ct2_files(
                ("a0feddc18de0c285ed147e5483c8d1bc911bd45e23104ae0726d79594e7a6b1d", 3_300),
                ("1a47e83dbb61709cb703159c4ca16dae4f2d33bffab1f21d7615c4d0b46786b2", 1_617_884_929),
                ("654cf18d3e163b948ceaf9766da56ce0b52de265d58673cf61c9376f126bd499", 357),
                ("5c1bf30c9e716e1477bedef846b01be0013daecb89e9e3ef7ab89b23c178df1b", 3_930_645),
            ),
            _HINGLISH,
            1314 * MIB,
            _CT2_PRECISION,
        ),
        Candidate(
            "distil-large-v3.5-int8",
            CTRANSLATE2,
            "https://huggingface.co/distil-whisper/distil-large-v3.5-ct2",
            "9793ccc07920e0f830e1dba0343efcdf0ef8c903",
            "model.bin",
            _ct2_files(
                ("90c55f775cc4e0bb17293d0bf12f96557a486f20dea886fabd8e6075a3588b21", 2_690),
                ("c58b88b8585ffcd2135fddaaf421ce72cb223b32edea70d156aed1dea319a119", 1_512_927_867),
                ("7ccc62c6f2765af1f3b46c00c9b5894426835a05021c8b9c01eecb6dfb542711", 340),
                ("b3c8202bbf06d8ee4232c5984baa563784ac4737e2e7fdc42fa180200d3cfcdb", 2_480_645),
            ),
            _EN,
            1248 * MIB,
            _CT2_PRECISION,
        ),
        Candidate(
            "granite-speech-5.0-470m-turboctc",
            TRANSFORMERS_CTC,
            "https://huggingface.co/ibm-granite/granite-speech-5.0-470m-turboctc",
            "286456107c8ba1161f5c22dfe85466402c88333b",
            "model.safetensors",
            (
                ("config.json", "41b72a7596bebe5106573f9e0f64c6b0acdac145373386946e7d1ef277fe0c9f", 926),
                ("generation_config.json", "e9eef52b79e24b329889157f75e02aea236c429552a4a986dd5fffb349809208", 158),
                ("model.safetensors", "8b98a8c34fd5fcb081caef719638eded31bb6d197d62053eefc5c1703aaf1ad4", 946_180_704),
                ("preprocessor_config.json", "4ff826570a74cd33092d467b16eb1c5c22c46c6f1b78a35aed1decdf90f4a3fe", 327),
                ("processor_config.json", "3d7c14d3ed7a5218b2fd6797eaa5b3aa615c681938bd1734a510d7a6f8c1b9fb", 430),
                ("tokenizer.json", "3ee80b02f0119a040a70eb909c20fac8271c173d7e71d195a3b35f77780061e6", 1_137_492),
                ("tokenizer_config.json", "5c7ddc8d2a6b0f6b009e0a7629af67f5d29a98d5e722fe435cf1be2da84c778f", 222),
            ),
            _EN,
            1114 * MIB,
            ("dtype", "bfloat16", "float32"),
        ),
    )
}

CANDIDATE_OPTIONS: dict[str, frozenset[str]] = {
    WHISPER_CPP: frozenset({"revision", "n_threads"}),
    CTRANSLATE2: frozenset({"revision", "compute_type", "n_threads"}),
    TRANSFORMERS_CTC: frozenset({"revision", "dtype"}),
}


def candidate_error(
    item: Candidate,
    device: str,
    language: str,
    model_path: Path,
    options: Mapping[str, object],
    vram_bytes: int,
    pins: Iterable[tuple[Path, str, int]],
) -> str | None:
    """Return why a manifest does not name this candidate's measured identity, or None."""

    model_id = item.model_id
    if language not in item.language_codes:
        return f"{model_id} does not support language {language}"
    if options.get("revision") != item.revision:
        return f"{model_id} requires revision {item.revision}"
    if item.precision is not None:
        key, measured = item.precision[0], item.precision_for(device)
        if key in options and options[key] != measured:
            return f"{model_id} on {device} was measured only at {key}={measured}"
    if device.startswith("cuda:") and vram_bytes < item.min_vram_bytes:
        return f"{model_id} vram_bytes {vram_bytes} is below its measured peak {item.min_vram_bytes}"
    root = model_path if model_path.is_dir() else model_path.parent
    if model_path.is_file() and model_path != root / item.weights:
        return f"{model_id} model_path must be {item.weights}"
    pinned = {path: (sha, size) for path, sha, size in pins}
    for name, sha, size in item.files:
        actual = pinned.get(root / name)
        if actual is None:
            return f"{model_id} file {name} must be pinned"
        if actual != (sha, size):
            return f"{name} does not match the published {model_id} artifact"
    return None
