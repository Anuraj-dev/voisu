//! Catalog beside the Bakeoff catalog. It cannot participate in production selection.
use super::consented_pins::{PYTHON, TOTAL_DOWNLOAD_BYTES, WHEELS};
use super::{CatalogFile, FileKind, pilot_candidate, shipped_catalog};
#[derive(Clone, Copy, Debug)]
pub struct DownloadPin {
    pub name: &'static str,
    pub url: &'static str,
    pub sha256: &'static str,
    pub bytes: u64,
    pub hosts: &'static [&'static str],
    pub requirement: Option<&'static str>,
}
#[derive(Debug)]
pub struct ConsentedCatalogEntry {
    pub id: &'static str,
    pub device: &'static str,
    pub compute_type: &'static str,
    pub native_runtime: Option<DownloadPin>,
    pub revision: &'static str,
    pub license_spdx: &'static str,
    pub consent_installable: bool,
    pub weights: &'static [DownloadPin],
    /// The pinned CPython runtime of a Python worker. `None` for the
    /// whisper.cpp entry: its worker is the packaged `voisu-local-asr` binary,
    /// so the install holds only weights and the health clip.
    pub python: Option<DownloadPin>,
    pub health_clip: fn() -> DownloadPin,
    pub wheels: &'static [DownloadPin],
    pub total_download_bytes: u64,
    pub estimated_extracted_runtime_bytes: u64,
    pub consent_text: &'static str,
    /// How the whisper.cpp worker decodes this entry's model; `None` for the
    /// Python entries and the worker's greedy default.
    pub whisper_cpp_decode: Option<WhisperCppDecode>,
}

/// whisper.cpp decoding pinned per catalog entry, written into the manifest.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WhisperCppDecode {
    pub beam_size: u8,
    pub flash_attn: bool,
}

impl ConsentedCatalogEntry {
    /// The packaged whisper.cpp worker, not a Python runtime built at install.
    pub fn runs_whisper_cpp(&self) -> bool {
        self.python.is_none()
    }

    /// Every pin names a real host. An entry still on the reserved `.invalid`
    /// placeholder fails closed at install and is never offered by setup or
    /// the TUI.
    pub fn is_hosted(&self) -> bool {
        let clip = (self.health_clip)();
        self.weights
            .iter()
            .chain(std::iter::once(&clip))
            .chain(self.python.iter())
            .chain(self.native_runtime.iter())
            .chain(self.wheels)
            .all(|pin| {
                !matches!(
                    super::url_policy::validate_catalog_url(pin.url, pin.hosts),
                    Err(super::url_policy::UrlPolicyError::PlaceholderHost { .. })
                )
            })
    }

    /// Free bytes the store filesystem needs before an install starts. A
    /// whisper.cpp install has nothing to extract: the model plus headroom.
    pub fn required_free_bytes(&self) -> u64 {
        if self.runs_whisper_cpp() {
            self.total_download_bytes + WHISPER_CPP_HEADROOM_BYTES
        } else {
            2 * (self.total_download_bytes + self.estimated_extracted_runtime_bytes)
        }
    }
}

const WHISPER_CPP_HEADROOM_BYTES: u64 = 256 * 1024 * 1024;
// 2.3 GiB rounded up for the CUDA venv, plus 105 MiB CPython, rounded up to
// 128 MiB. docs/local-gpu-development.md measured the venv at 2.3 GB; using
// GiB and rounding upward leaves headroom for extracted package metadata.
pub const ESTIMATED_EXTRACTED_RUNTIME_BYTES: u64 = 2356 * 1024 * 1024 + 128 * 1024 * 1024;
pub const REQUIRED_FREE_BYTES: u64 = 2 * (TOTAL_DOWNLOAD_BYTES + ESTIMATED_EXTRACTED_RUNTIME_BYTES);
pub const EXPECTED_HEALTH_TEXT: &str = "And so my fellow Americans ask not what your country can do for you. Ask what you can do for your country.";
// Exact HF blob redirect exception: CDN rotation must fail closed, never use
// suffix matching. Small resolve-cache redirects remain on huggingface.co.
const WEIGHT_HOSTS: &[&str] = &["huggingface.co", "us.aws.cdn.hf.co"];
const WEIGHTS: &[DownloadPin] = &[
    DownloadPin {
        name: "config.json",
        url: "https://huggingface.co/distil-whisper/distil-large-v3.5-ct2/resolve/9793ccc07920e0f830e1dba0343efcdf0ef8c903/config.json",
        sha256: "90c55f775cc4e0bb17293d0bf12f96557a486f20dea886fabd8e6075a3588b21",
        bytes: 2690,
        hosts: WEIGHT_HOSTS,
        requirement: None,
    },
    DownloadPin {
        name: "model.bin",
        url: "https://huggingface.co/distil-whisper/distil-large-v3.5-ct2/resolve/9793ccc07920e0f830e1dba0343efcdf0ef8c903/model.bin",
        sha256: "c58b88b8585ffcd2135fddaaf421ce72cb223b32edea70d156aed1dea319a119",
        bytes: 1512927867,
        hosts: WEIGHT_HOSTS,
        requirement: None,
    },
    DownloadPin {
        name: "preprocessor_config.json",
        url: "https://huggingface.co/distil-whisper/distil-large-v3.5-ct2/resolve/9793ccc07920e0f830e1dba0343efcdf0ef8c903/preprocessor_config.json",
        sha256: "7ccc62c6f2765af1f3b46c00c9b5894426835a05021c8b9c01eecb6dfb542711",
        bytes: 340,
        hosts: WEIGHT_HOSTS,
        requirement: None,
    },
    DownloadPin {
        name: "tokenizer.json",
        url: "https://huggingface.co/distil-whisper/distil-large-v3.5-ct2/resolve/9793ccc07920e0f830e1dba0343efcdf0ef8c903/tokenizer.json",
        sha256: "b3c8202bbf06d8ee4232c5984baa563784ac4737e2e7fdc42fa180200d3cfcdb",
        bytes: 2480645,
        hosts: WEIGHT_HOSTS,
        requirement: None,
    },
    DownloadPin {
        name: "vocabulary.json",
        url: "https://huggingface.co/distil-whisper/distil-large-v3.5-ct2/resolve/9793ccc07920e0f830e1dba0343efcdf0ef8c903/vocabulary.json",
        sha256: "c69260f2ab26d659b7c398f9a2b2b48ed0df16c3b47d7326782fd9cba71690c1",
        bytes: 1068114,
        hosts: WEIGHT_HOSTS,
        requirement: None,
    },
];
pub const CONSENT_TEXT: &str = "Download 2,980,914,661 bytes for distil-large-v3.5-int8 and its pinned Python/CTranslate2/CUDA runtime. Licence: MIT. Development evidence: one host, one speaker, scripted prompts — not the release gate. English only. NVIDIA GPU required. Runs offline on cuda:0 with int8_float16; no automatic CPU or Cloud fallback. This is a Consented Install, not a Bakeoff Winner, and is never selected by default.";
pub const CONSENTED_ENTRY: ConsentedCatalogEntry = ConsentedCatalogEntry {
    id: "distil-large-v3.5-int8",
    device: "cuda:0",
    compute_type: "int8_float16",
    native_runtime: None,
    revision: "9793ccc07920e0f830e1dba0343efcdf0ef8c903",
    // HF model card for distil-whisper/distil-large-v3.5-ct2 declares `license: mit`.
    license_spdx: "MIT",
    consent_installable: true,
    weights: WEIGHTS,
    python: Some(PYTHON),
    wheels: WHEELS,
    whisper_cpp_decode: None,
    health_clip: clip_pin,
    total_download_bytes: TOTAL_DOWNLOAD_BYTES,
    estimated_extracted_runtime_bytes: ESTIMATED_EXTRACTED_RUNTIME_BYTES,
    consent_text: CONSENT_TEXT,
};
/// Reuse the Bakeoff catalog's immutable health fixture pin without duplicating it.
pub fn consented_health_clip() -> &'static CatalogFile {
    let files = pilot_candidate(&shipped_catalog())
        .expect("shipped pilot")
        .files;
    files
        .iter()
        .find(|file| file.name == "jfk.wav")
        .expect("shipped fixture")
}
pub(super) fn clip_pin() -> DownloadPin {
    let file = consented_health_clip();
    debug_assert_eq!(file.kind, FileKind::HealthFixture);
    DownloadPin {
        name: file.name,
        url: file.url,
        sha256: file.sha256_hex,
        bytes: file.bytes,
        hosts: &["raw.githubusercontent.com"],
        requirement: None,
    }
}
pub(super) const WORKER_FILES: &[(&str, &[u8])] = &[
    (
        "worker.py",
        include_bytes!("../../../../tools/local-asr/worker.py"),
    ),
    (
        "voisu_asr/__init__.py",
        include_bytes!("../../../../tools/local-asr/voisu_asr/__init__.py"),
    ),
    (
        "voisu_asr/adapters.py",
        include_bytes!("../../../../tools/local-asr/voisu_asr/adapters.py"),
    ),
    (
        "voisu_asr/audio.py",
        include_bytes!("../../../../tools/local-asr/voisu_asr/audio.py"),
    ),
    (
        "voisu_asr/candidate_adapters.py",
        include_bytes!("../../../../tools/local-asr/voisu_asr/candidate_adapters.py"),
    ),
    (
        "voisu_asr/candidates.py",
        include_bytes!("../../../../tools/local-asr/voisu_asr/candidates.py"),
    ),
    (
        "voisu_asr/manifest.py",
        include_bytes!("../../../../tools/local-asr/voisu_asr/manifest.py"),
    ),
    (
        "voisu_asr/protocol.py",
        include_bytes!("../../../../tools/local-asr/voisu_asr/protocol.py"),
    ),
    (
        "voisu_asr/service.py",
        include_bytes!("../../../../tools/local-asr/voisu_asr/service.py"),
    ),
    (
        "voisu_asr/windowing.py",
        include_bytes!("../../../../tools/local-asr/voisu_asr/windowing.py"),
    ),
];

/// Catalog lookup is exact; receipts cannot authorize arbitrary entries.
pub fn consented_catalog_entries() -> [&'static ConsentedCatalogEntry; 3] {
    [&CONSENTED_ENTRY, &PARAKEET_ENTRY, &WHISPER_CPP_ENTRY]
}

pub fn consented_entry(id: &str) -> Option<&'static ConsentedCatalogEntry> {
    consented_catalog_entries()
        .into_iter()
        .find(|entry| entry.id == id)
}

pub const NEMO_LIBRARY: &str =
    "runtime/nemo-speech-0.1.0-linux-x86_64-cpu/lib/libnemo_speech_asr_c.so.1";
pub const PARAKEET_ENTRY: ConsentedCatalogEntry = ConsentedCatalogEntry {
    id: "parakeet-tdt-0.6b-v3",
    device: "cpu",
    compute_type: "q8_0",
    revision: "541d1f99c6b0c3cd0b11a95167540bb8edefd82b",
    license_spdx: "CC-BY-4.0",
    consent_installable: true,
    weights: &[super::consented_pins::PARAKEET_WEIGHTS],
    native_runtime: Some(super::consented_pins::NEMO_CPU),
    python: Some(PYTHON),
    wheels: super::consented_pins::CPU_WHEELS,
    health_clip: clip_pin,
    total_download_bytes: 769_772_225,
    // CPython: 98,070,981 extracted bytes; venv copies Python executables,
    // plus pip, NumPy (~70 MiB), bytecode and the 12,238,405-byte native
    // archive. Reserve 384 MiB including metadata and publication headroom.
    estimated_extracted_runtime_bytes: 384 * 1024 * 1024,
    whisper_cpp_decode: None,
    consent_text: "Download 769,772,225 bytes for parakeet-tdt-0.6b-v3 and its pinned Python/NumPy/NeMo-Speech.cpp v0.1.0 CPU runtime. Licence: CC-BY-4.0. English only. Runs offline on CPU. Development evidence: EN WER 12.8%, warm p50 0.61 s on 4 cores, peak RAM 0.9 GB; one host, one speaker — not the release gate. No automatic GPU or Cloud fallback. This is a Consented Install, not a Bakeoff Winner, and is never selected by default.",
};

/// Voisu's Hugging Face repo, pinned to one commit so the bytes behind the URL
/// cannot change; the sha256 below is checked after download anyway.
pub const WHISPER_CPP_Q8_URL: &str = "https://huggingface.co/Anurajjit/voisu-distil-large-v3.5-ggml/resolve/33b03a8479479b1ad96d4b7b51ebdeecbd1d9b71/ggml-distil-large-v3.5-q8_0.bin";
/// distil-large-v3.5 quantized to q8_0 with whisper.cpp 1.9.5's
/// `whisper-quantize` (the Phase 0 verdict).
pub const WHISPER_CPP_Q8: DownloadPin = DownloadPin {
    name: "ggml-distil-large-v3.5-q8_0.bin",
    url: WHISPER_CPP_Q8_URL,
    sha256: "7e570abdf13b681354a2ecc93802e25bf204dd6f8c0dd9f6ecb9478b71b231d7",
    bytes: 818_305_955,
    hosts: WEIGHT_HOSTS,
    requirement: None,
};
/// The JFK health clip's size; the entry's total adds it to the model.
const JFK_BYTES: u64 = 352_078;
/// The whisper.cpp entry. Its `device` is `gpu`; the worker runs where the
/// `local_device` setting says when it loads.
pub const WHISPER_CPP_ENTRY: ConsentedCatalogEntry = ConsentedCatalogEntry {
    id: super::development::WHISPER_CPP_DISTIL_ID,
    device: "gpu",
    compute_type: "q8_0",
    native_runtime: None,
    revision: "whisper.cpp-1.9.5",
    // distil-whisper/distil-large-v3.5 is MIT; the q8_0 quant is a derivative.
    license_spdx: "MIT",
    consent_installable: true,
    weights: &[WHISPER_CPP_Q8],
    python: None,
    wheels: &[],
    health_clip: clip_pin,
    total_download_bytes: WHISPER_CPP_Q8.bytes + JFK_BYTES,
    estimated_extracted_runtime_bytes: 0,
    // Phase 0 (2026-11-18): beam 5 with flash attention on is accurate and
    // deterministic on CPU, Vulkan and CUDA for this quant.
    whisper_cpp_decode: Some(WhisperCppDecode {
        beam_size: 5,
        flash_attn: true,
    }),
    consent_text: "Download 818,658,033 bytes for distil-large-v3.5 (whisper.cpp q8_0). Licence: MIT. English only. Runs on this computer with your GPU through Vulkan, or on the CPU if you choose `voisu local-device cpu`; audio never leaves it. No automatic CPU or Cloud fallback. This is a Consented Install, not a Bakeoff Winner, and is never selected by default.",
};

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::local_model::url_policy::{UrlPolicyError, validate_catalog_url};

    /// A pin still on the reserved placeholder host, as a new entry has
    /// before its files are published.
    pub(crate) const PLACEHOLDER_PIN: DownloadPin = DownloadPin {
        url: "https://voisu-model-host.invalid/whisper-cpp/model.bin",
        hosts: &["voisu-model-host.invalid"],
        ..WHISPER_CPP_Q8
    };
    pub(crate) const PLACEHOLDER_ENTRY: ConsentedCatalogEntry = ConsentedCatalogEntry {
        weights: &[PLACEHOLDER_PIN],
        ..WHISPER_CPP_ENTRY
    };

    #[test]
    fn every_catalog_entry_is_hosted_and_a_placeholder_pin_is_not() {
        assert!(WHISPER_CPP_ENTRY.is_hosted());
        assert!(CONSENTED_ENTRY.is_hosted());
        assert!(PARAKEET_ENTRY.is_hosted());
        assert!(!PLACEHOLDER_ENTRY.is_hosted());
    }

    #[test]
    fn the_whisper_cpp_model_url_passes_the_catalog_policy() {
        let pin = WHISPER_CPP_ENTRY.weights[0];
        assert!(validate_catalog_url(pin.url, pin.hosts).is_ok());
        assert_eq!(
            validate_catalog_url(PLACEHOLDER_PIN.url, PLACEHOLDER_PIN.hosts),
            Err(UrlPolicyError::PlaceholderHost {
                host: "voisu-model-host.invalid".into()
            })
        );
    }

    #[test]
    fn the_whisper_cpp_entry_pins_the_phase_0_quant_and_needs_model_plus_256_mib() {
        let entry = &WHISPER_CPP_ENTRY;
        assert!(entry.runs_whisper_cpp());
        assert_eq!(entry.weights.len(), 1);
        assert_eq!(
            (entry.weights[0].sha256, entry.weights[0].bytes),
            (
                "7e570abdf13b681354a2ecc93802e25bf204dd6f8c0dd9f6ecb9478b71b231d7",
                818_305_955
            )
        );
        assert_eq!(clip_pin().bytes, JFK_BYTES);
        assert_eq!(
            entry.total_download_bytes,
            entry.weights[0].bytes + clip_pin().bytes
        );
        assert!(
            entry
                .consent_text
                .contains(&format_with_commas(entry.total_download_bytes))
        );
        assert_eq!(
            entry.required_free_bytes(),
            entry.total_download_bytes + 256 * 1024 * 1024
        );
        // Python entries keep their two-copies-of-everything reserve.
        assert_eq!(CONSENTED_ENTRY.required_free_bytes(), REQUIRED_FREE_BYTES);
        assert_eq!(
            consented_entry(entry.id).map(|found| found.id),
            Some(entry.id)
        );
    }

    fn format_with_commas(value: u64) -> String {
        let digits = value.to_string();
        let mut out = String::new();
        for (index, digit) in digits.chars().enumerate() {
            if index > 0 && (digits.len() - index).is_multiple_of(3) {
                out.push(',');
            }
            out.push(digit);
        }
        out
    }
}
