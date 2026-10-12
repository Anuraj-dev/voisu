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
    pub revision: &'static str,
    pub license_spdx: &'static str,
    pub consent_installable: bool,
    pub weights: &'static [DownloadPin],
    pub python: DownloadPin,
    pub health_clip: fn() -> DownloadPin,
    pub wheels: &'static [DownloadPin],
    pub total_download_bytes: u64,
    pub estimated_extracted_runtime_bytes: u64,
    pub consent_text: &'static str,
}
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
    revision: "9793ccc07920e0f830e1dba0343efcdf0ef8c903",
    // HF model card for distil-whisper/distil-large-v3.5-ct2 declares `license: mit`.
    license_spdx: "MIT",
    consent_installable: true,
    weights: WEIGHTS,
    python: PYTHON,
    wheels: WHEELS,
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
