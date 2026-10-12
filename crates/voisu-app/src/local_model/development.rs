//! Development-only model manifests and selection.
//!
//! Development manifests are deliberately separate from the production
//! catalog and receipt format.  A manifest is a private, host-local snapshot
//! of the runtime and artifact pins used by one supervised worker.  It is
//! never consulted by production selection or written as a production receipt.

use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use super::receipt::{ActiveReceipt, FilePin, encode as encode_receipt};
use super::sha256_hex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const DEVELOPMENT_MANIFEST_VERSION: u32 = 1;
pub const DEVELOPMENT_ROOT_NAME: &str = "local-dev";
pub const DEVELOPMENT_MANIFEST_MAX_BYTES: usize = 128 * 1024;
pub const DEVELOPMENT_ID_MAX_BYTES: usize = 96;
pub const DEVELOPMENT_RUNTIME_REVISION_MAX_BYTES: usize = 256;

/// IDs are fixed here so a malformed or unreviewed manifest cannot introduce
/// an arbitrary executable model into the selector.
pub const DEV_MODEL_IDS: [&str; 13] = [
    "nemotron-3.5-0.6b",
    "parakeet-tdt-0.6b-v3",
    "qwen3-asr-0.6b",
    "qwen3-asr-1.7b",
    "fun-asr-nano-2512",
    "moonshine-streaming-medium",
    "cohere-transcribe-03-2026",
    "whisper-cpp-large-v3-turbo-q5-0",
    "whisper-cpp-small-q8-0",
    "faster-whisper-large-v3-turbo-int8",
    "whisper-hindi2hinglish-apex-int8",
    "distil-large-v3.5-int8",
    "granite-speech-5.0-470m-turboctc",
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DevRuntimeKind {
    Native,
    Python,
}

impl DevRuntimeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::Python => "python",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DevCatalogEntry {
    pub model_id: &'static str,
    pub runtime: DevRuntimeKind,
    pub languages: &'static [&'static str],
    pub devices: &'static [&'static str],
    pub memory_bytes: u64,
    pub vram_bytes: u64,
}

const EN: &[&str] = &["en"];
const EN_HI_AUTO: &[&str] = &["en", "hi", "auto"];
const EN_AUTO: &[&str] = &["en", "auto"];
/// `hi` is Latin-script Hinglish, which this model writes under Whisper's `en` token.
const EN_HINGLISH: &[&str] = &["en", "hi"];
// Candidate limits below are the measured RTX 5050 peaks (2026-09-26, clips up
// to 62 s) plus margin; see the registered manifests.
const CPU_CUDA: &[&str] = &["cpu", "cuda:0"];
const CPU_ONLY: &[&str] = &["cpu"];

static DEVELOPMENT_CATALOG: &[DevCatalogEntry] = &[
    DevCatalogEntry {
        model_id: "nemotron-3.5-0.6b",
        runtime: DevRuntimeKind::Native,
        languages: EN_HI_AUTO,
        devices: CPU_CUDA,
        memory_bytes: 4 * 1024 * 1024 * 1024,
        vram_bytes: 2 * 1024 * 1024 * 1024,
    },
    DevCatalogEntry {
        model_id: "parakeet-tdt-0.6b-v3",
        runtime: DevRuntimeKind::Native,
        languages: EN,
        devices: CPU_CUDA,
        memory_bytes: 4 * 1024 * 1024 * 1024,
        vram_bytes: 2 * 1024 * 1024 * 1024,
    },
    DevCatalogEntry {
        model_id: "qwen3-asr-0.6b",
        runtime: DevRuntimeKind::Python,
        languages: EN_HI_AUTO,
        devices: CPU_CUDA,
        memory_bytes: 4 * 1024 * 1024 * 1024,
        vram_bytes: 2 * 1024 * 1024 * 1024,
    },
    DevCatalogEntry {
        model_id: "qwen3-asr-1.7b",
        runtime: DevRuntimeKind::Python,
        languages: EN_HI_AUTO,
        devices: CPU_CUDA,
        memory_bytes: 8 * 1024 * 1024 * 1024,
        vram_bytes: 5 * 1024 * 1024 * 1024,
    },
    DevCatalogEntry {
        model_id: "fun-asr-nano-2512",
        runtime: DevRuntimeKind::Native,
        languages: EN,
        devices: CPU_CUDA,
        memory_bytes: 6 * 1024 * 1024 * 1024,
        vram_bytes: 3 * 1024 * 1024 * 1024,
    },
    DevCatalogEntry {
        model_id: "moonshine-streaming-medium",
        runtime: DevRuntimeKind::Python,
        languages: EN,
        devices: CPU_ONLY,
        memory_bytes: 2 * 1024 * 1024 * 1024,
        vram_bytes: 0,
    },
    DevCatalogEntry {
        model_id: "cohere-transcribe-03-2026",
        runtime: DevRuntimeKind::Python,
        languages: EN,
        devices: CPU_CUDA,
        memory_bytes: 8 * 1024 * 1024 * 1024,
        vram_bytes: 4 * 1024 * 1024 * 1024,
    },
    DevCatalogEntry {
        model_id: "whisper-cpp-large-v3-turbo-q5-0",
        runtime: DevRuntimeKind::Native,
        languages: EN_AUTO,
        devices: CPU_CUDA,
        memory_bytes: 2048 * 1024 * 1024,
        vram_bytes: 1536 * 1024 * 1024,
    },
    DevCatalogEntry {
        model_id: "whisper-cpp-small-q8-0",
        runtime: DevRuntimeKind::Native,
        languages: EN_AUTO,
        devices: CPU_CUDA,
        memory_bytes: 2048 * 1024 * 1024,
        vram_bytes: 1280 * 1024 * 1024,
    },
    DevCatalogEntry {
        model_id: "faster-whisper-large-v3-turbo-int8",
        runtime: DevRuntimeKind::Native,
        languages: EN_AUTO,
        devices: CPU_CUDA,
        memory_bytes: 3072 * 1024 * 1024,
        vram_bytes: 2048 * 1024 * 1024,
    },
    DevCatalogEntry {
        model_id: "whisper-hindi2hinglish-apex-int8",
        runtime: DevRuntimeKind::Native,
        languages: EN_HINGLISH,
        devices: CPU_CUDA,
        memory_bytes: 3072 * 1024 * 1024,
        vram_bytes: 2048 * 1024 * 1024,
    },
    DevCatalogEntry {
        model_id: "distil-large-v3.5-int8",
        runtime: DevRuntimeKind::Native,
        languages: EN,
        devices: CPU_CUDA,
        memory_bytes: 3072 * 1024 * 1024,
        vram_bytes: 2048 * 1024 * 1024,
    },
    DevCatalogEntry {
        model_id: "granite-speech-5.0-470m-turboctc",
        runtime: DevRuntimeKind::Python,
        languages: EN,
        devices: CPU_CUDA,
        memory_bytes: 4096 * 1024 * 1024,
        vram_bytes: 1792 * 1024 * 1024,
    },
];

/// Requested Fun-ASR Nano checkpoint. Fun-ASR-MLT-Nano is a different model.
pub const FUN_ASR_NANO_CHECKPOINT: &str = "Fun-ASR-Nano-2512";
pub const FUN_ASR_ENCODER_NAME: &str = "funasr-encoder-f16.gguf";
pub const FUN_ASR_ENCODER_SHA: &str =
    "f92f91d01a24fbed6c863495b2ee8c6a6788144a02858b75743f0946668de8a2";
pub const FUN_ASR_ENCODER_BYTES: u64 = 469_331_008;
pub const FUN_ASR_DECODER_Q8_NAME: &str = "qwen3-0.6b-q8_0.gguf";
pub const FUN_ASR_DECODER_Q8_SHA: &str =
    "819f385dc0e035dccc3d9e7edaf6b7b044b8ba7ace63cbcbf84c7e397eecbf27";
pub const FUN_ASR_DECODER_Q8_BYTES: u64 = 804_753_280;
pub const FUN_ASR_VAD_NAME: &str = "fsmn-vad.gguf";
pub const FUN_ASR_VAD_SHA: &str =
    "1270f2559c495f4e7b6e739541151027d360761a3fda43fc147034f5719f5479";
pub const FUN_ASR_VAD_BYTES: u64 = 1_720_512;
pub const FUN_ASR_HF_REV: &str = "d93b302ee7fd505e1b3576120fc142fc6f7820e1";
pub const FUN_ASR_HF_WEIGHTS_NAME: &str = "model.safetensors";
pub const FUN_ASR_HF_WEIGHTS_SHA: &str =
    "1bbb6dcc5d8b75084a399d48c4d4b0f3aa1d3f09f2616ae40ed2c4fba03d89c9";
pub const FUN_ASR_HF_WEIGHTS_BYTES: u64 = 1_659_773_320;
pub const QWEN_REQUIRED_FILES: [&str; 6] = [
    "config.json",
    "generation_config.json",
    "model.safetensors",
    "processor_config.json",
    "tokenizer.json",
    "tokenizer_config.json",
];
pub const MOONSHINE_MEDIUM_ARCH: &str = "MEDIUM_STREAMING";

/// Low-VRAM candidate identity: the upstream revision, every file the loader
/// reads (name under the model root, SHA-256, size), the peak VRAM measured on
/// the RTX 5050 (a CUDA manifest may not admit less), and the precision option
/// it was measured at. Mirrors `tools/local-asr/voisu_asr/candidates.py`.
#[derive(Clone, Copy, Debug)]
pub struct CandidatePin {
    pub model_id: &'static str,
    pub revision: &'static str,
    /// The file the runtime loads as the model; whisper.cpp names it as model_path.
    pub weights: &'static str,
    pub files: &'static [(&'static str, &'static str, u64)],
    pub min_vram_bytes: u64,
    /// (option key, CUDA value, CPU value); any other value was never measured.
    pub precision: Option<(&'static str, &'static str, &'static str)>,
}

/// Whisper-Hindi2Hinglish-Apex has no published CTranslate2 build; its files
/// are the reproducible float16 `ct2-transformers-converter` output of the
/// pinned revision (see `tools/local-asr/runtimes/faster-whisper`).
pub static CANDIDATE_PINS: &[CandidatePin] = &[
    CandidatePin {
        model_id: "whisper-cpp-large-v3-turbo-q5-0",
        revision: "5359861c739e955e79d9a303bcbc70fb988958b1",
        weights: "ggml-large-v3-turbo-q5_0.bin",
        files: &[(
            "ggml-large-v3-turbo-q5_0.bin",
            "394221709cd5ad1f40c46e6031ca61bce88931e6e088c188294c6d5a55ffa7e2",
            574_041_195,
        )],
        min_vram_bytes: 1186 * 1024 * 1024,
        precision: None,
    },
    CandidatePin {
        model_id: "whisper-cpp-small-q8-0",
        revision: "5359861c739e955e79d9a303bcbc70fb988958b1",
        weights: "ggml-small-q8_0.bin",
        files: &[(
            "ggml-small-q8_0.bin",
            "49c8fb02b65e6049d5fa6c04f81f53b867b5ec9540406812c643f177317f779f",
            264_464_607,
        )],
        min_vram_bytes: 786 * 1024 * 1024,
        precision: None,
    },
    CandidatePin {
        model_id: "faster-whisper-large-v3-turbo-int8",
        revision: "0a363e9161cbc7ed1431c9597a8ceaf0c4f78fcf",
        weights: "model.bin",
        files: &[
            (
                "config.json",
                "b0253ea6c0d3bea6b1e19e91a02acfd3b53f4467362efcb5a3e6b16c9b3a9b7e",
                2_263,
            ),
            (
                "model.bin",
                "e76620f83d5f5b69efd3d87e3dc180c1bd21df9fbebacfd4335e5e1efcc018da",
                1_617_884_929,
            ),
            (
                "preprocessor_config.json",
                "7ccc62c6f2765af1f3b46c00c9b5894426835a05021c8b9c01eecb6dfb542711",
                340,
            ),
            (
                "tokenizer.json",
                "297b13372ac43916285644fb9687add3cc62ee2a1adb60da3dc25cc94c1871fd",
                2_710_337,
            ),
            (
                "vocabulary.json",
                "c69260f2ab26d659b7c398f9a2b2b48ed0df16c3b47d7326782fd9cba71690c1",
                1_068_114,
            ),
        ],
        min_vram_bytes: 1280 * 1024 * 1024,
        precision: Some(("compute_type", "int8_float16", "int8")),
    },
    CandidatePin {
        model_id: "whisper-hindi2hinglish-apex-int8",
        revision: "f3214eed20b4e4d4144e739982d911f87b9cb223",
        weights: "model.bin",
        files: &[
            (
                "config.json",
                "a0feddc18de0c285ed147e5483c8d1bc911bd45e23104ae0726d79594e7a6b1d",
                3_300,
            ),
            (
                "model.bin",
                "1a47e83dbb61709cb703159c4ca16dae4f2d33bffab1f21d7615c4d0b46786b2",
                1_617_884_929,
            ),
            (
                "preprocessor_config.json",
                "654cf18d3e163b948ceaf9766da56ce0b52de265d58673cf61c9376f126bd499",
                357,
            ),
            (
                "tokenizer.json",
                "5c1bf30c9e716e1477bedef846b01be0013daecb89e9e3ef7ab89b23c178df1b",
                3_930_645,
            ),
            (
                "vocabulary.json",
                "c69260f2ab26d659b7c398f9a2b2b48ed0df16c3b47d7326782fd9cba71690c1",
                1_068_114,
            ),
        ],
        min_vram_bytes: 1314 * 1024 * 1024,
        precision: Some(("compute_type", "int8_float16", "int8")),
    },
    CandidatePin {
        model_id: "distil-large-v3.5-int8",
        revision: "9793ccc07920e0f830e1dba0343efcdf0ef8c903",
        weights: "model.bin",
        files: &[
            (
                "config.json",
                "90c55f775cc4e0bb17293d0bf12f96557a486f20dea886fabd8e6075a3588b21",
                2_690,
            ),
            (
                "model.bin",
                "c58b88b8585ffcd2135fddaaf421ce72cb223b32edea70d156aed1dea319a119",
                1_512_927_867,
            ),
            (
                "preprocessor_config.json",
                "7ccc62c6f2765af1f3b46c00c9b5894426835a05021c8b9c01eecb6dfb542711",
                340,
            ),
            (
                "tokenizer.json",
                "b3c8202bbf06d8ee4232c5984baa563784ac4737e2e7fdc42fa180200d3cfcdb",
                2_480_645,
            ),
            (
                "vocabulary.json",
                "c69260f2ab26d659b7c398f9a2b2b48ed0df16c3b47d7326782fd9cba71690c1",
                1_068_114,
            ),
        ],
        min_vram_bytes: 1248 * 1024 * 1024,
        precision: Some(("compute_type", "int8_float16", "int8")),
    },
    CandidatePin {
        model_id: "granite-speech-5.0-470m-turboctc",
        revision: "286456107c8ba1161f5c22dfe85466402c88333b",
        weights: "model.safetensors",
        files: &[
            (
                "config.json",
                "41b72a7596bebe5106573f9e0f64c6b0acdac145373386946e7d1ef277fe0c9f",
                926,
            ),
            (
                "generation_config.json",
                "e9eef52b79e24b329889157f75e02aea236c429552a4a986dd5fffb349809208",
                158,
            ),
            (
                "model.safetensors",
                "8b98a8c34fd5fcb081caef719638eded31bb6d197d62053eefc5c1703aaf1ad4",
                946_180_704,
            ),
            (
                "preprocessor_config.json",
                "4ff826570a74cd33092d467b16eb1c5c22c46c6f1b78a35aed1decdf90f4a3fe",
                327,
            ),
            (
                "processor_config.json",
                "3d7c14d3ed7a5218b2fd6797eaa5b3aa615c681938bd1734a510d7a6f8c1b9fb",
                430,
            ),
            (
                "tokenizer.json",
                "3ee80b02f0119a040a70eb909c20fac8271c173d7e71d195a3b35f77780061e6",
                1_137_492,
            ),
            (
                "tokenizer_config.json",
                "5c7ddc8d2a6b0f6b009e0a7629af67f5d29a98d5e722fe435cf1be2da84c778f",
                222,
            ),
        ],
        min_vram_bytes: 1114 * 1024 * 1024,
        precision: Some(("dtype", "bfloat16", "float32")),
    },
];

#[cfg(test)]
thread_local! {
    /// Lets a test stand in a candidate identity whose digests are real bytes.
    static CANDIDATE_PIN_OVERRIDE: std::cell::Cell<Option<CandidatePin>> =
        const { std::cell::Cell::new(None) };
}

fn candidate_pin(model_id: &str) -> Option<CandidatePin> {
    #[cfg(test)]
    if let Some(pin) = CANDIDATE_PIN_OVERRIDE.get()
        && pin.model_id == model_id
    {
        return Some(pin);
    }
    CANDIDATE_PINS
        .iter()
        .find(|pin| pin.model_id == model_id)
        .copied()
}
pub const MOONSHINE_MEDIUM_PINS: [(&str, &str, u64); 7] = [
    (
        "encoder.ort",
        "a5f11167a62eef61787fe8410453257d6ddb8eba90af461a9604e5f2e93d5322",
        94_202_872,
    ),
    (
        "adapter.ort",
        "16307442b7f4229f2f1511fc51b545cec9616e55872c588f3a297bbc6f4762ea",
        3_647_712,
    ),
    (
        "cross_kv.ort",
        "354b9a955caeb768b528f447f0a36ce4b850ca7b4531900165df304d97904fba",
        11_544_952,
    ),
    (
        "decoder_kv.ort",
        "fa67aa87521247f5bf44d3e44d4e4978e58c1f114249c3c6909c882624056715",
        146_216_448,
    ),
    (
        "frontend.ort",
        "378fe8a5d7090a1b9ab88bbb1fc95bde010cdd64ec23419350d2d23c675636e9",
        47_467_256,
    ),
    (
        "streaming_config.json",
        "28e83b7a28e91472692a035e0dae3116422ae43aeb2bef5ed822c44ce89b88af",
        513,
    ),
    (
        "tokenizer.bin",
        "6884b35fd6377d4c4d32336a0bc152f36b64d1e45b6503683cdc238250a8472d",
        249_974,
    ),
];
pub const COHERE_HF_REV: &str = "b1eacc2686a3d08ceaae5f24a88b1d519620bc09";

#[must_use]
pub fn development_catalog() -> &'static [DevCatalogEntry] {
    DEVELOPMENT_CATALOG
}

#[must_use]
pub fn development_catalog_entry(model_id: &str) -> Option<&'static DevCatalogEntry> {
    DEVELOPMENT_CATALOG
        .iter()
        .find(|entry| entry.model_id == model_id)
}

/// Construct a private synthetic receipt for the selected development
/// manifest.  It has the same correlation shape as a production receipt, but
/// its model ID and runtime identity cannot be mistaken for a shipped
/// production catalog entry.
pub fn development_receipt(manifest: &DevManifest, manifest_hash: &str) -> ActiveReceipt {
    let files = manifest
        .files
        .iter()
        .map(|pin| {
            (
                pin.path.clone(),
                FilePin {
                    sha256_hex: pin.sha256.clone(),
                    bytes: pin.bytes,
                },
            )
        })
        .collect();
    let mut receipt = ActiveReceipt {
        catalog_id: manifest.model_id.clone(),
        catalog_revision: manifest.runtime.revision.clone(),
        runtime_abi: format!("voisu-dev:{}", manifest.runtime.revision),
        protocol: crate::local_worker::PROTOCOL_VERSION,
        language: manifest.language.clone(),
        artifact_id: manifest_hash.to_owned(),
        files,
        receipt_hash: String::new(),
    };
    receipt.receipt_hash = sha256_hex(encode_receipt(&receipt).as_bytes());
    receipt
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DevRuntimeManifest {
    pub python: String,
    pub worker: String,
    pub revision: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DevFilePin {
    pub path: String,
    pub sha256: String,
    pub bytes: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DevLimits {
    pub memory_bytes: u64,
    pub tasks: u64,
    pub cpu_cores: u32,
    pub cache_bytes: u64,
    pub vram_bytes: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DevManifest {
    pub version: u32,
    pub model_id: String,
    pub device: String,
    pub language: String,
    pub runtime: DevRuntimeManifest,
    pub model_path: String,
    pub files: Vec<DevFilePin>,
    #[serde(default)]
    pub options: serde_json::Map<String, serde_json::Value>,
    pub limits: DevLimits,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DevManifestHandle {
    pub model_id: String,
    pub path: PathBuf,
    pub manifest_hash: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DevModelStatus {
    pub model_id: String,
    pub runtime: DevRuntimeKind,
    pub languages: Vec<String>,
    pub devices: Vec<String>,
    pub memory_bytes: u64,
    pub vram_bytes: u64,
    pub manifest: Option<DevManifestHandle>,
    pub selected: bool,
    pub ready: bool,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DevelopmentError {
    Io(String),
    Json(String),
    Invalid(String),
    Busy,
    MissingSelection,
}

impl fmt::Display for DevelopmentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(message) | Self::Json(message) | Self::Invalid(message) => {
                f.write_str(message)
            }
            Self::Busy => {
                f.write_str("development model selection refused while Recording is active")
            }
            Self::MissingSelection => f.write_str("no development model is selected"),
        }
    }
}

impl std::error::Error for DevelopmentError {}

impl DevManifest {
    pub fn from_json(text: &str) -> Result<Self, DevelopmentError> {
        if text.len() > DEVELOPMENT_MANIFEST_MAX_BYTES {
            return Err(DevelopmentError::Invalid(
                "development manifest is too large".into(),
            ));
        }
        let manifest: Self = serde_json::from_str(text).map_err(|error| {
            DevelopmentError::Json(format!("invalid development manifest: {error}"))
        })?;
        Ok(manifest)
    }

    pub fn to_json(&self) -> Result<String, DevelopmentError> {
        serde_json::to_string_pretty(self).map_err(|error| {
            DevelopmentError::Json(format!("cannot encode development manifest: {error}"))
        })
    }

    pub fn validate(&self) -> Result<(), DevelopmentError> {
        if self.version != DEVELOPMENT_MANIFEST_VERSION {
            return Err(DevelopmentError::Invalid(format!(
                "unsupported development manifest version {}",
                self.version
            )));
        }
        if self.model_id.is_empty()
            || self.model_id.len() > DEVELOPMENT_ID_MAX_BYTES
            || !self.model_id.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-' || byte == b'.'
            })
        {
            return Err(DevelopmentError::Invalid(
                "invalid development model_id".into(),
            ));
        }
        let Some(catalog) = development_catalog_entry(&self.model_id) else {
            return Err(DevelopmentError::Invalid(format!(
                "model {} is not in the development catalog",
                self.model_id
            )));
        };
        if !catalog.devices.contains(&self.device.as_str()) {
            return Err(DevelopmentError::Invalid(format!(
                "{} does not support {}",
                self.model_id, self.device
            )));
        }
        if !catalog.languages.contains(&self.language.as_str()) {
            return Err(DevelopmentError::Invalid(format!(
                "{} does not advertise {}",
                self.model_id, self.language
            )));
        }
        if self.model_id == "parakeet-tdt-0.6b-v3" {
            match self
                .options
                .get("language_code")
                .and_then(|value| value.as_str())
            {
                None | Some("en") | Some("en-US") => {}
                _ => {
                    return Err(DevelopmentError::Invalid(
                        "parakeet-tdt-0.6b-v3 language_code must be en or en-US".into(),
                    ));
                }
            }
        }
        self.validate_remaining_model_identity()?;
        if self.device == "cpu" && self.limits.vram_bytes != 0 {
            return Err(DevelopmentError::Invalid(
                "cpu manifests must set vram_bytes=0".into(),
            ));
        }
        if self.device == "cuda:0" && self.limits.vram_bytes == 0 {
            return Err(DevelopmentError::Invalid(
                "cuda manifests must set a vram_bytes limit".into(),
            ));
        }
        if self.runtime.python.is_empty()
            || self.runtime.worker.is_empty()
            || self.runtime.revision.is_empty()
            || self.runtime.revision.len() > DEVELOPMENT_RUNTIME_REVISION_MAX_BYTES
        {
            return Err(DevelopmentError::Invalid(
                "runtime identity is incomplete".into(),
            ));
        }
        for (label, path) in [
            ("runtime.python", self.runtime.python.as_str()),
            ("runtime.worker", self.runtime.worker.as_str()),
            ("model_path", self.model_path.as_str()),
        ] {
            let path = Path::new(path);
            if !path.is_absolute() || path.to_string_lossy().contains('\0') {
                return Err(DevelopmentError::Invalid(format!(
                    "{label} must be absolute"
                )));
            }
        }
        if self.files.is_empty() {
            return Err(DevelopmentError::Invalid(
                "development manifest has no file pins".into(),
            ));
        }
        let mut paths = BTreeMap::new();
        for pin in &self.files {
            let path = Path::new(&pin.path);
            if !path.is_absolute()
                || pin.sha256.len() != 64
                || !pin.sha256.bytes().all(|b| b.is_ascii_hexdigit())
            {
                return Err(DevelopmentError::Invalid(format!(
                    "invalid file pin for {}",
                    pin.path
                )));
            }
            if paths.insert(pin.path.clone(), ()).is_some() {
                return Err(DevelopmentError::Invalid(format!(
                    "duplicate file pin for {}",
                    pin.path
                )));
            }
            if pin.bytes == 0 {
                return Err(DevelopmentError::Invalid(format!(
                    "empty file pin for {}",
                    pin.path
                )));
            }
        }
        for path in [&self.runtime.python, &self.runtime.worker] {
            if !paths.contains_key(path) {
                return Err(DevelopmentError::Invalid(format!(
                    "runtime path is not pinned: {path}"
                )));
            }
        }
        if Path::new(&self.model_path).is_file() && !paths.contains_key(&self.model_path) {
            return Err(DevelopmentError::Invalid(
                "model_path file is not pinned".into(),
            ));
        }
        if let Some(health) = self
            .options
            .get("health_audio")
            .and_then(serde_json::Value::as_str)
        {
            if !Path::new(health).is_absolute() || !paths.contains_key(health) {
                return Err(DevelopmentError::Invalid(
                    "health_audio must be an absolute pinned file".into(),
                ));
            }
            if self
                .options
                .get("health_expected")
                .and_then(serde_json::Value::as_str)
                .is_none()
            {
                return Err(DevelopmentError::Invalid(
                    "health_expected is required with health_audio".into(),
                ));
            }
        }
        self.validate_remaining_model_pins(&paths)?;
        self.validate_qwen_model_pins()?;
        if let Some(cache) = self.options.get("cache_dir") {
            match cache.as_str() {
                Some(path) if Path::new(path).is_absolute() && !path.contains('\0') => {}
                _ => {
                    return Err(DevelopmentError::Invalid(
                        "cache_dir must be an absolute path".into(),
                    ));
                }
            }
        }
        if self.limits.memory_bytes == 0
            || self.limits.tasks == 0
            || self.limits.cpu_cores == 0
            || self.limits.cache_bytes == 0
        {
            return Err(DevelopmentError::Invalid(
                "resource limits must be non-zero".into(),
            ));
        }
        Ok(())
    }

    fn validate_remaining_model_identity(&self) -> Result<(), DevelopmentError> {
        match self.model_id.as_str() {
            "fun-asr-nano-2512" => {
                reject_fun_asr_mlt(&self.model_path)?;
                for value in self.options.values() {
                    if let Some(text) = value.as_str() {
                        reject_fun_asr_mlt(text)?;
                    }
                }
                match self
                    .options
                    .get("checkpoint")
                    .and_then(serde_json::Value::as_str)
                {
                    Some(FUN_ASR_NANO_CHECKPOINT) => {}
                    _ => {
                        return Err(DevelopmentError::Invalid(
                            "fun-asr-nano-2512 requires checkpoint Fun-ASR-Nano-2512; Fun-ASR-MLT-Nano is not a substitute".into(),
                        ));
                    }
                }
                let native = ["encoder", "decoder", "llama_cli"]
                    .iter()
                    .all(|key| self.options.contains_key(*key));
                let python = self.options.contains_key("remote_code");
                let transformers = self
                    .options
                    .get("revision")
                    .and_then(serde_json::Value::as_str)
                    == Some(FUN_ASR_HF_REV);
                if !native && !python && !transformers {
                    return Err(DevelopmentError::Invalid(
                        "fun-asr-nano-2512 requires the llama.cpp encoder/decoder/llama_cli bundle, pinned remote_code, or Fun-ASR-Nano-2512-hf revision".into(),
                    ));
                }
                for key in ["encoder", "decoder", "llama_cli", "vad", "remote_code"] {
                    if let Some(path) = self.options.get(key).and_then(serde_json::Value::as_str) {
                        require_absolute_option(key, path)?;
                    }
                }
                if let Some(encoder) = self
                    .options
                    .get("encoder")
                    .and_then(serde_json::Value::as_str)
                    && Path::new(encoder).file_name() != Some(FUN_ASR_ENCODER_NAME.as_ref())
                {
                    return Err(DevelopmentError::Invalid(
                        "fun-asr-nano-2512 encoder must be funasr-encoder-f16.gguf".into(),
                    ));
                }
                if let Some(decoder) = self
                    .options
                    .get("decoder")
                    .and_then(serde_json::Value::as_str)
                {
                    let name = Path::new(decoder)
                        .file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or_default();
                    if name != FUN_ASR_DECODER_Q8_NAME {
                        return Err(DevelopmentError::Invalid(
                            "fun-asr-nano-2512 decoder must be qwen3-0.6b-q8_0.gguf".into(),
                        ));
                    }
                }
                if let Some(revision) = self
                    .options
                    .get("revision")
                    .and_then(serde_json::Value::as_str)
                    && revision != FUN_ASR_HF_REV
                {
                    return Err(DevelopmentError::Invalid(
                        "fun-asr-nano-2512 Transformers revision must be the pinned Fun-ASR-Nano-2512-hf commit".into(),
                    ));
                }
            }
            "moonshine-streaming-medium" => {
                match self
                    .options
                    .get("model_arch")
                    .and_then(serde_json::Value::as_str)
                {
                    Some(MOONSHINE_MEDIUM_ARCH) => {}
                    _ => {
                        return Err(DevelopmentError::Invalid(
                            "moonshine-streaming-medium requires model_arch=MEDIUM_STREAMING"
                                .into(),
                        ));
                    }
                }
                if !Path::new(&self.model_path).is_dir() {
                    return Err(DevelopmentError::Invalid(
                        "moonshine-streaming-medium model_path must be a directory".into(),
                    ));
                }
            }
            model_id if candidate_pin(model_id).is_some() => {
                if let Some(pin) = candidate_pin(model_id) {
                    self.validate_candidate_identity(&pin)?;
                }
            }
            "cohere-transcribe-03-2026" => {
                match self
                    .options
                    .get("revision")
                    .and_then(serde_json::Value::as_str)
                {
                    Some(revision)
                        if revision == COHERE_HF_REV
                            && revision.len() == 40
                            && revision.bytes().all(|byte| byte.is_ascii_hexdigit())
                            && !revision.starts_with("refs/") => {}
                    _ => {
                        return Err(DevelopmentError::Invalid(
                            "cohere-transcribe-03-2026 requires revision b1eacc2686a3d08ceaae5f24a88b1d519620bc09"
                                .into(),
                        ));
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn validate_candidate_identity(&self, pin: &CandidatePin) -> Result<(), DevelopmentError> {
        let model_id = pin.model_id;
        let option = |key: &str| self.options.get(key).and_then(serde_json::Value::as_str);
        if option("revision") != Some(pin.revision) {
            return Err(DevelopmentError::Invalid(format!(
                "{model_id} requires revision {}",
                pin.revision
            )));
        }
        let path = Path::new(&self.model_path);
        if path.is_file() && path.file_name() != Some(pin.weights.as_ref()) {
            return Err(DevelopmentError::Invalid(format!(
                "{model_id} model_path must be {}",
                pin.weights
            )));
        }
        let cuda = self.device.starts_with("cuda:");
        if let Some((key, on_cuda, on_cpu)) = pin.precision {
            let measured = if cuda { on_cuda } else { on_cpu };
            if self.options.contains_key(key) && option(key) != Some(measured) {
                return Err(DevelopmentError::Invalid(format!(
                    "{model_id} on {} was measured only at {key}={measured}",
                    self.device
                )));
            }
        }
        if cuda && self.limits.vram_bytes < pin.min_vram_bytes {
            return Err(DevelopmentError::Invalid(format!(
                "{model_id} vram_bytes {} is below its measured peak {}",
                self.limits.vram_bytes, pin.min_vram_bytes
            )));
        }
        Ok(())
    }

    fn require_published_fun_asr_pin(
        &self,
        key: &str,
        name: &str,
        sha: &str,
        bytes: u64,
    ) -> Result<(), DevelopmentError> {
        let Some(path) = self.options.get(key).and_then(serde_json::Value::as_str) else {
            return Ok(());
        };
        let Some(pin) = self.files.iter().find(|pin| pin.path == path) else {
            return Err(DevelopmentError::Invalid(format!(
                "{key} must be an absolute pinned file"
            )));
        };
        if Path::new(path).file_name() != Some(name.as_ref())
            || !pin.sha256.eq_ignore_ascii_case(sha)
            || pin.bytes != bytes
        {
            return Err(DevelopmentError::Invalid(format!(
                "{key} does not match the published Fun-ASR-Nano artifact"
            )));
        }
        Ok(())
    }

    fn validate_remaining_model_pins(
        &self,
        paths: &BTreeMap<String, ()>,
    ) -> Result<(), DevelopmentError> {
        match self.model_id.as_str() {
            "fun-asr-nano-2512" => {
                for key in ["encoder", "decoder", "llama_cli", "vad", "remote_code"] {
                    if let Some(path) = self.options.get(key).and_then(serde_json::Value::as_str)
                        && !paths.contains_key(path)
                    {
                        return Err(DevelopmentError::Invalid(format!(
                            "{key} must be an absolute pinned file"
                        )));
                    }
                }
                self.require_published_fun_asr_pin(
                    "encoder",
                    FUN_ASR_ENCODER_NAME,
                    FUN_ASR_ENCODER_SHA,
                    FUN_ASR_ENCODER_BYTES,
                )?;
                self.require_published_fun_asr_pin(
                    "decoder",
                    FUN_ASR_DECODER_Q8_NAME,
                    FUN_ASR_DECODER_Q8_SHA,
                    FUN_ASR_DECODER_Q8_BYTES,
                )?;
                if self.options.contains_key("vad") {
                    self.require_published_fun_asr_pin(
                        "vad",
                        FUN_ASR_VAD_NAME,
                        FUN_ASR_VAD_SHA,
                        FUN_ASR_VAD_BYTES,
                    )?;
                }
                let transformers = self
                    .options
                    .get("revision")
                    .and_then(serde_json::Value::as_str)
                    == Some(FUN_ASR_HF_REV);
                let weights = model_artifact_root(&self.model_path).join(FUN_ASR_HF_WEIGHTS_NAME);
                if transformers {
                    self.require_published_path_pin(
                        &weights,
                        FUN_ASR_HF_WEIGHTS_SHA,
                        FUN_ASR_HF_WEIGHTS_BYTES,
                        "Fun-ASR-Nano-2512-hf weights",
                    )?;
                } else if let Some(pin) = self
                    .files
                    .iter()
                    .find(|pin| Path::new(&pin.path) == weights.as_path())
                    && (!pin.sha256.eq_ignore_ascii_case(FUN_ASR_HF_WEIGHTS_SHA)
                        || pin.bytes != FUN_ASR_HF_WEIGHTS_BYTES)
                {
                    return Err(DevelopmentError::Invalid(
                        "Fun-ASR-Nano-2512-hf weights do not match the published snapshot".into(),
                    ));
                }
            }
            "moonshine-streaming-medium" => {
                let root = model_artifact_root(&self.model_path);
                for (name, sha, bytes) in MOONSHINE_MEDIUM_PINS {
                    self.require_published_path_pin(&root.join(name), sha, bytes, name)?;
                }
            }
            model_id => {
                if let Some(pin) = candidate_pin(model_id) {
                    let root = model_artifact_root(&self.model_path);
                    for &(name, sha, bytes) in pin.files {
                        self.require_published_path_pin(&root.join(name), sha, bytes, name)?;
                    }
                }
            }
        }
        Ok(())
    }

    fn validate_qwen_model_pins(&self) -> Result<(), DevelopmentError> {
        if self.model_id != "qwen3-asr-0.6b" && self.model_id != "qwen3-asr-1.7b" {
            return Ok(());
        }
        let root = model_artifact_root(&self.model_path);
        if !root.is_dir() {
            return Err(DevelopmentError::Invalid(
                "Qwen model_path must be a directory of pinned Transformers files".into(),
            ));
        }
        for name in QWEN_REQUIRED_FILES {
            let path = root.join(name);
            if !self
                .files
                .iter()
                .any(|pin| Path::new(&pin.path) == path.as_path())
            {
                return Err(DevelopmentError::Invalid(format!(
                    "Qwen Transformers file is not pinned: {name}"
                )));
            }
        }
        Ok(())
    }

    fn require_published_path_pin(
        &self,
        expected: &Path,
        sha: &str,
        bytes: u64,
        label: &str,
    ) -> Result<(), DevelopmentError> {
        let Some(pin) = self
            .files
            .iter()
            .find(|pin| Path::new(&pin.path) == expected)
        else {
            return Err(DevelopmentError::Invalid(format!(
                "{label} must be pinned at {}",
                expected.display()
            )));
        };
        if !pin.sha256.eq_ignore_ascii_case(sha) || pin.bytes != bytes {
            return Err(DevelopmentError::Invalid(format!(
                "{label} do not match the published snapshot"
            )));
        }
        Ok(())
    }

    fn verify_pin_sizes(&self) -> Result<(), DevelopmentError> {
        for pin in &self.files {
            let path = Path::new(&pin.path);
            let metadata = fs::metadata(path).map_err(|error| {
                DevelopmentError::Io(format!("cannot inspect {}: {error}", pin.path))
            })?;
            if !metadata.is_file() || metadata.len() != pin.bytes {
                return Err(DevelopmentError::Invalid(format!(
                    "file pin size/type changed: {}",
                    pin.path
                )));
            }
        }
        Ok(())
    }

    pub fn identity_hash(&self) -> Result<String, DevelopmentError> {
        self.validate()?;
        self.verify_pin_sizes()?;
        let encoded =
            serde_json::to_vec(self).map_err(|error| DevelopmentError::Json(error.to_string()))?;
        Ok(sha256_hex(&encoded))
    }

    /// Hash every pinned artifact at registration. Later loads check the
    /// private snapshot's metadata fingerprints without rehashing weights.
    pub fn verify_pins(&self) -> Result<String, DevelopmentError> {
        let hash = self.identity_hash()?;
        for pin in &self.files {
            let path = Path::new(&pin.path);
            let actual = sha256_file(path).map_err(|error| {
                DevelopmentError::Io(format!("cannot read {}: {error}", pin.path))
            })?;
            if !actual.eq_ignore_ascii_case(&pin.sha256) {
                return Err(DevelopmentError::Invalid(format!(
                    "file pin digest changed: {}",
                    pin.path
                )));
            }
        }
        Ok(hash)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DevSelection {
    version: u32,
    model_id: String,
    manifest: String,
    manifest_sha256: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ArtifactFingerprint {
    size: u64,
    mtime_seconds: i64,
    mtime_nanoseconds: i64,
    inode: u64,
    /// `st_dev` is not a stable identity: btrfs hands each mount an anonymous
    /// device number that changes across reboots. Older snapshots carry it, so
    /// it is still accepted on read, but never written or compared
    /// (`IgnoredAny` values are always equal).
    #[serde(default, skip_serializing)]
    device: serde::de::IgnoredAny,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ArtifactSnapshot {
    manifest_sha256: String,
    fingerprints: BTreeMap<String, ArtifactFingerprint>,
}

impl ArtifactFingerprint {
    fn read(path: &str) -> Result<Self, DevelopmentError> {
        let metadata = fs::metadata(path)
            .map_err(|error| DevelopmentError::Io(format!("cannot inspect {path}: {error}")))?;
        if !metadata.is_file() {
            return Err(DevelopmentError::Invalid(format!(
                "file pin type changed: {path}"
            )));
        }
        Ok(Self {
            size: metadata.len(),
            mtime_seconds: metadata.mtime(),
            mtime_nanoseconds: metadata.mtime_nsec(),
            inode: metadata.ino(),
            device: serde::de::IgnoredAny,
        })
    }
}

pub struct DevelopmentStore {
    root: PathBuf,
}

impl DevelopmentStore {
    pub fn open(root: PathBuf) -> Result<Self, DevelopmentError> {
        ensure_private_dir(&root)?;
        ensure_private_dir(&root.join("manifests"))?;
        Ok(Self { root })
    }

    pub fn open_default() -> Result<Self, DevelopmentError> {
        let state = voisu_core::state_dir().map_err(DevelopmentError::Io)?;
        Self::open(state.join(DEVELOPMENT_ROOT_NAME))
    }

    pub fn register(&self, manifest: &DevManifest) -> Result<DevManifestHandle, DevelopmentError> {
        let before: BTreeMap<_, _> = manifest
            .files
            .iter()
            .map(|pin| Ok((pin.path.clone(), ArtifactFingerprint::read(&pin.path)?)))
            .collect::<Result<_, DevelopmentError>>()?;
        let hash = manifest.verify_pins()?;
        let fingerprints: BTreeMap<_, _> = manifest
            .files
            .iter()
            .map(|pin| Ok((pin.path.clone(), ArtifactFingerprint::read(&pin.path)?)))
            .collect::<Result<_, DevelopmentError>>()?;
        if before != fingerprints {
            return Err(DevelopmentError::Invalid(
                "artifact changed during registration".into(),
            ));
        }
        let path = self
            .root
            .join("manifests")
            .join(format!("{}.json", manifest.model_id));
        let encoded = manifest.to_json()?;
        let fingerprint_path = path.with_extension("fingerprints.json");
        let fingerprints_encoded = serde_json::to_vec(&ArtifactSnapshot {
            manifest_sha256: hash.clone(),
            fingerprints,
        })
        .map_err(|error| DevelopmentError::Json(error.to_string()))?;
        atomic_private_write(&fingerprint_path, &fingerprints_encoded)?;
        atomic_private_write(&path, encoded.as_bytes())?;
        Ok(DevManifestHandle {
            model_id: manifest.model_id.clone(),
            path,
            manifest_hash: hash,
        })
    }

    pub fn register_file(&self, path: &Path) -> Result<DevManifestHandle, DevelopmentError> {
        let text =
            fs::read_to_string(path).map_err(|error| DevelopmentError::Io(error.to_string()))?;
        let manifest = DevManifest::from_json(&text)?;
        self.register(&manifest)
    }

    pub fn load(
        &self,
        model_id: &str,
    ) -> Result<(DevManifest, DevManifestHandle), DevelopmentError> {
        if development_catalog_entry(model_id).is_none() {
            return Err(DevelopmentError::Invalid(format!(
                "unknown development model: {model_id}"
            )));
        }
        let path = self.root.join("manifests").join(format!("{model_id}.json"));
        let text =
            fs::read_to_string(&path).map_err(|error| DevelopmentError::Io(error.to_string()))?;
        let manifest = DevManifest::from_json(&text)?;
        let hash = manifest.identity_hash()?;
        let fingerprint_path = path.with_extension("fingerprints.json");
        let snapshot: ArtifactSnapshot =
            serde_json::from_slice(&fs::read(&fingerprint_path).map_err(|error| {
                DevelopmentError::Io(format!("cannot read artifact fingerprints: {error}"))
            })?)
            .map_err(|error| DevelopmentError::Json(error.to_string()))?;
        if snapshot.manifest_sha256 != hash || snapshot.fingerprints.len() != manifest.files.len() {
            return Err(DevelopmentError::Invalid(
                "artifact fingerprints changed".into(),
            ));
        }
        for pin in &manifest.files {
            if snapshot.fingerprints.get(&pin.path) != Some(&ArtifactFingerprint::read(&pin.path)?)
            {
                return Err(DevelopmentError::Invalid(format!(
                    "registered artifact changed: {}",
                    pin.path
                )));
            }
        }
        if manifest.model_id != model_id {
            return Err(DevelopmentError::Invalid(
                "manifest filename/model mismatch".into(),
            ));
        }
        Ok((
            manifest,
            DevManifestHandle {
                model_id: model_id.into(),
                path,
                manifest_hash: hash,
            },
        ))
    }

    pub fn select(
        &self,
        model_id: &str,
        daemon_state: Option<voisu_core::DaemonState>,
    ) -> Result<DevManifestHandle, DevelopmentError> {
        match daemon_state {
            Some(voisu_core::DaemonState::Idle) => {}
            Some(voisu_core::DaemonState::Recording) => return Err(DevelopmentError::Busy),
            Some(voisu_core::DaemonState::Processing) => {
                return Err(DevelopmentError::Invalid(
                    "development model selection refused while the daemon is not idle".into(),
                ));
            }
            None => {
                return Err(DevelopmentError::Invalid(
                    "development model selection refused until daemon idle state is confirmed"
                        .into(),
                ));
            }
        }
        let (_manifest, handle) = self.load(model_id)?;
        let selection = DevSelection {
            version: DEVELOPMENT_MANIFEST_VERSION,
            model_id: model_id.into(),
            manifest: handle.path.to_string_lossy().into_owned(),
            manifest_sha256: handle.manifest_hash.clone(),
        };
        let encoded = serde_json::to_vec_pretty(&selection)
            .map_err(|error| DevelopmentError::Json(error.to_string()))?;
        atomic_private_write(&self.root.join("selection.json"), &encoded)?;
        Ok(handle)
    }

    pub fn selected(&self) -> Result<(DevManifest, DevManifestHandle), DevelopmentError> {
        let path = self.root.join("selection.json");
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(DevelopmentError::MissingSelection);
            }
            Err(error) => return Err(DevelopmentError::Io(error.to_string())),
        };
        let selection: DevSelection = serde_json::from_str(&text)
            .map_err(|error| DevelopmentError::Json(error.to_string()))?;
        if selection.version != DEVELOPMENT_MANIFEST_VERSION {
            return Err(DevelopmentError::Invalid(
                "unsupported selection version".into(),
            ));
        }
        let (manifest, handle) = self.load(&selection.model_id)?;
        if selection.manifest != handle.path.to_string_lossy()
            || selection.manifest_sha256 != handle.manifest_hash
        {
            return Err(DevelopmentError::Invalid(
                "selected manifest identity changed".into(),
            ));
        }
        Ok((manifest, handle))
    }

    pub fn statuses(&self) -> Vec<DevModelStatus> {
        let selected = self.selected().ok().map(|(_, handle)| handle.model_id);
        DEVELOPMENT_CATALOG
            .iter()
            .map(|entry| {
                let (manifest, error) = match self.load(entry.model_id) {
                    Ok((_, handle)) => (Some(handle), None),
                    Err(error) => (None, Some(error.to_string())),
                };
                DevModelStatus {
                    model_id: entry.model_id.into(),
                    runtime: entry.runtime,
                    languages: entry.languages.iter().map(|v| (*v).into()).collect(),
                    devices: entry.devices.iter().map(|v| (*v).into()).collect(),
                    memory_bytes: entry.memory_bytes,
                    vram_bytes: entry.vram_bytes,
                    manifest,
                    selected: selected.as_deref() == Some(entry.model_id),
                    ready: false,
                    error,
                }
            })
            .collect()
    }
}

fn sha256_file(path: &Path) -> Result<String, std::io::Error> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 128 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn model_artifact_root(model_path: &str) -> PathBuf {
    let path = Path::new(model_path);
    if path.is_dir() {
        path.to_path_buf()
    } else {
        path.parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or(path)
            .to_path_buf()
    }
}

fn reject_fun_asr_mlt(value: &str) -> Result<(), DevelopmentError> {
    if value.to_ascii_lowercase().contains("fun-asr-mlt-nano") {
        return Err(DevelopmentError::Invalid(
            "Fun-ASR-MLT-Nano is not a substitute for Fun-ASR-Nano-2512".into(),
        ));
    }
    Ok(())
}

fn require_absolute_option(key: &str, path: &str) -> Result<(), DevelopmentError> {
    if !Path::new(path).is_absolute() || path.contains('\0') {
        return Err(DevelopmentError::Invalid(format!(
            "options.{key} must be an absolute path"
        )));
    }
    Ok(())
}

fn ensure_private_dir(path: &Path) -> Result<(), DevelopmentError> {
    fs::create_dir_all(path).map_err(|error| DevelopmentError::Io(error.to_string()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|error| DevelopmentError::Io(error.to_string()))?;
    let metadata = fs::metadata(path).map_err(|error| DevelopmentError::Io(error.to_string()))?;
    if !metadata.is_dir() || metadata.uid() != unsafe { libc::geteuid() } {
        return Err(DevelopmentError::Invalid(format!(
            "development directory is not private: {}",
            path.display()
        )));
    }
    Ok(())
}

fn atomic_private_write(path: &Path, bytes: &[u8]) -> Result<(), DevelopmentError> {
    let parent = path
        .parent()
        .ok_or_else(|| DevelopmentError::Io("development file has no parent".into()))?;
    ensure_private_dir(parent)?;
    let mut staged = tempfile::Builder::new()
        .prefix(".dev-")
        .tempfile_in(parent)
        .map_err(|error| DevelopmentError::Io(error.to_string()))?;
    staged
        .as_file_mut()
        .set_permissions(fs::Permissions::from_mode(0o600))
        .map_err(|error| DevelopmentError::Io(error.to_string()))?;
    staged
        .write_all(bytes)
        .and_then(|_| staged.as_file().sync_all())
        .map_err(|error| DevelopmentError::Io(error.to_string()))?;
    staged
        .persist(path)
        .map_err(|error| DevelopmentError::Io(error.error.to_string()))?;
    File::open(parent)
        .and_then(|file| file.sync_all())
        .map_err(|error| DevelopmentError::Io(error.to_string()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    fn manifest(dir: &Path, model_id: &str) -> DevManifest {
        let python = dir.join("python");
        let worker = dir.join("worker.py");
        let model = dir.join("model.bin");
        for (path, bytes) in [
            (&python, b"python".as_slice()),
            (&worker, b"worker".as_slice()),
            (&model, b"model".as_slice()),
        ] {
            fs::write(path, bytes).unwrap();
        }
        let pin = |path: &Path| DevFilePin {
            path: path.to_string_lossy().into_owned(),
            sha256: sha256_hex(&fs::read(path).unwrap()),
            bytes: fs::metadata(path).unwrap().len(),
        };
        DevManifest {
            version: 1,
            model_id: model_id.into(),
            device: "cpu".into(),
            language: "en".into(),
            runtime: DevRuntimeManifest {
                python: python.to_string_lossy().into_owned(),
                worker: worker.to_string_lossy().into_owned(),
                revision: "test-runtime".into(),
            },
            model_path: model.to_string_lossy().into_owned(),
            files: vec![pin(&python), pin(&worker), pin(&model)],
            options: serde_json::Map::new(),
            limits: DevLimits {
                memory_bytes: 1024,
                tasks: 4,
                cpu_cores: 1,
                cache_bytes: 1024,
                vram_bytes: 0,
            },
        }
    }

    #[test]
    fn streaming_file_digest_matches_in_memory_digest() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("blob.bin");
        let bytes = vec![7u8; 200_000];
        fs::write(&path, &bytes).unwrap();
        assert_eq!(sha256_file(&path).unwrap(), sha256_hex(&bytes));
    }

    #[test]
    fn catalog_contains_exactly_the_requested_models() {
        assert_eq!(development_catalog().len(), DEV_MODEL_IDS.len());
        assert_eq!(
            development_catalog()
                .iter()
                .map(|e| e.model_id)
                .collect::<Vec<_>>(),
            DEV_MODEL_IDS
        );
    }

    #[test]
    fn candidate_published_pins_are_well_formed() {
        let hex = |text: &str, len| {
            text.len() == len
                && text
                    .bytes()
                    .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
        };
        for pin in CANDIDATE_PINS {
            let entry = development_catalog_entry(pin.model_id).unwrap();
            assert!(hex(pin.revision, 40), "{}", pin.model_id);
            assert!(pin.files.iter().any(|file| file.0 == pin.weights));
            let mut names: Vec<_> = pin.files.iter().map(|file| file.0).collect();
            names.sort_unstable();
            names.dedup();
            assert_eq!(names.len(), pin.files.len(), "{}", pin.model_id);
            for &(name, sha, bytes) in pin.files {
                assert!(hex(sha, 64) && bytes > 0, "{} {name}", pin.model_id);
            }
            assert!(pin.min_vram_bytes > 0);
            assert!(pin.min_vram_bytes <= entry.vram_bytes, "{}", pin.model_id);
            assert!(entry.vram_bytes < 8151u64 * 1024 * 1024 * 75 / 100);
        }
        let ct2 = candidate_pin("distil-large-v3.5-int8").unwrap();
        assert_eq!(
            ct2.precision,
            Some(("compute_type", "int8_float16", "int8"))
        );
        let languages = |id| development_catalog_entry(id).unwrap().languages;
        assert_eq!(languages("whisper-hindi2hinglish-apex-int8"), &["en", "hi"]);
        assert_eq!(
            languages("whisper-cpp-large-v3-turbo-q5-0"),
            &["en", "auto"]
        );
        assert_eq!(languages("distil-large-v3.5-int8"), &["en"]);
        assert_eq!(languages("granite-speech-5.0-470m-turboctc"), &["en"]);
    }

    /// Writes real files for every name the published pin lists, pins their
    /// real digests, and stands in an identity with those digests for this
    /// test thread.
    fn real_candidate(dir: &Path, model_id: &str, device: &str) -> DevManifest {
        let published = CANDIDATE_PINS
            .iter()
            .find(|pin| pin.model_id == model_id)
            .unwrap();
        let mut value = manifest(dir, model_id);
        let model_dir = dir.join("candidate");
        fs::create_dir_all(&model_dir).unwrap();
        let mut files = Vec::new();
        for &(name, _, _) in published.files {
            let path = model_dir.join(name);
            let bytes = format!("{model_id}/{name}").into_bytes();
            fs::write(&path, &bytes).unwrap();
            let sha: &'static str = Box::leak(sha256_hex(&bytes).into_boxed_str());
            files.push((name, sha, bytes.len() as u64));
            value.files.push(DevFilePin {
                path: path.to_string_lossy().into_owned(),
                sha256: sha.into(),
                bytes: bytes.len() as u64,
            });
        }
        CANDIDATE_PIN_OVERRIDE.set(Some(CandidatePin {
            files: Box::leak(files.into_boxed_slice()),
            ..*published
        }));
        value.model_path = model_dir.to_string_lossy().into_owned();
        value.device = device.into();
        if device == "cuda:0" {
            value.limits.vram_bytes = published.min_vram_bytes;
        }
        value.options.insert(
            "revision".into(),
            serde_json::Value::from(published.revision),
        );
        value
    }

    #[test]
    fn candidate_with_real_bytes_registers_and_detects_mutation() {
        for pin in CANDIDATE_PINS {
            let dir = TempDir::new().unwrap();
            let value = real_candidate(dir.path(), pin.model_id, "cuda:0");
            assert!(value.validate().is_ok(), "{}", pin.model_id);
            let store = DevelopmentStore::open(dir.path().join("local-dev")).unwrap();
            store.register(&value).unwrap();
            assert!(value.verify_pins().is_ok());
            let weights = Path::new(&value.model_path).join(pin.weights);
            fs::write(&weights, b"changed weights").unwrap();
            assert!(value.verify_pins().is_err(), "{}", pin.model_id);
        }
        CANDIDATE_PIN_OVERRIDE.set(None);
    }

    #[test]
    fn candidate_rejects_unmeasured_identity() {
        let dir = TempDir::new().unwrap();
        let value = real_candidate(dir.path(), "distil-large-v3.5-int8", "cuda:0");
        assert!(value.validate().is_ok());
        for index in 3..value.files.len() {
            let mut wrong = value.clone();
            wrong.files[index].sha256 = "0".repeat(64);
            assert!(wrong.validate().is_err(), "file {index} must match");
            let mut wrong = value.clone();
            wrong.files.remove(index);
            assert!(wrong.validate().is_err(), "file {index} must be pinned");
        }
        let with_option = |key: &str, text: &str| {
            let mut changed = value.clone();
            changed
                .options
                .insert(key.into(), serde_json::Value::from(text));
            changed
        };
        assert!(with_option("revision", "main").validate().is_err());
        assert!(
            with_option("compute_type", "int8_float16")
                .validate()
                .is_ok()
        );
        assert!(with_option("compute_type", "int8").validate().is_err());
        assert!(with_option("compute_type", "float16").validate().is_err());
        let mut low = value.clone();
        low.limits.vram_bytes -= 1;
        assert!(low.validate().is_err(), "VRAM below the measured peak");
        let mut hindi = value.clone();
        hindi.language = "hi".into();
        assert!(
            hindi.validate().is_err(),
            "distil-large-v3.5 is English only"
        );

        let cpu = real_candidate(dir.path(), "distil-large-v3.5-int8", "cpu");
        assert!(cpu.validate().is_ok());
        let mut cpu_float = cpu.clone();
        cpu_float.options.insert(
            "compute_type".into(),
            serde_json::Value::from("int8_float16"),
        );
        assert!(cpu_float.validate().is_err());
        CANDIDATE_PIN_OVERRIDE.set(None);
    }

    #[test]
    fn manifest_rejects_unpinned_runtime_and_detects_mutation() {
        let dir = TempDir::new().unwrap();
        let mut value = manifest(dir.path(), DEV_MODEL_IDS[0]);
        value.runtime.worker.push_str("-changed");
        assert!(value.validate().is_err());
        let value = manifest(dir.path(), DEV_MODEL_IDS[0]);
        let hash = value.verify_pins().unwrap();
        assert_eq!(hash.len(), 64);
        fs::write(dir.path().join("model.bin"), b"mutated").unwrap();
        assert!(value.verify_pins().is_err());
        let store = DevelopmentStore::open(dir.path().join("local-dev")).unwrap();
        assert!(store.register(&value).is_err());
    }

    #[test]
    fn selection_refuses_recording_and_persists_private_identity() {
        let dir = TempDir::new().unwrap();
        let store = DevelopmentStore::open(dir.path().join("local-dev")).unwrap();
        let value = manifest(dir.path(), DEV_MODEL_IDS[0]);
        store.register(&value).unwrap();
        assert_eq!(
            store.select(DEV_MODEL_IDS[0], Some(voisu_core::DaemonState::Recording)),
            Err(DevelopmentError::Busy)
        );
        assert!(matches!(
            store.select(DEV_MODEL_IDS[0], None),
            Err(DevelopmentError::Invalid(_))
        ));
        let selected = store
            .select(DEV_MODEL_IDS[0], Some(voisu_core::DaemonState::Idle))
            .unwrap();
        assert_eq!(
            store.selected().unwrap().1.manifest_hash,
            selected.manifest_hash
        );
        assert_eq!(
            fs::metadata(dir.path().join("local-dev/selection.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn mutated_pin_after_register_invalidates_selection() {
        let dir = TempDir::new().unwrap();
        let store = DevelopmentStore::open(dir.path().join("local-dev")).unwrap();
        let value = manifest(dir.path(), DEV_MODEL_IDS[0]);
        store.register(&value).unwrap();
        store
            .select(DEV_MODEL_IDS[0], Some(voisu_core::DaemonState::Idle))
            .unwrap();
        fs::write(dir.path().join("model.bin"), b"mutated-pin").unwrap();
        assert!(store.selected().is_err());
        assert!(store.load(DEV_MODEL_IDS[0]).is_err());
    }

    #[test]
    fn registered_snapshot_rejects_same_size_artifact_after_registration() {
        let dir = TempDir::new().unwrap();
        let store = DevelopmentStore::open(dir.path().join("local-dev")).unwrap();
        let value = manifest(dir.path(), DEV_MODEL_IDS[0]);
        store.register(&value).unwrap();
        let artifact = dir.path().join("model.bin");
        fs::write(&artifact, b"other").unwrap();
        File::open(&artifact)
            .unwrap()
            .set_times(
                fs::FileTimes::new()
                    .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1)),
            )
            .unwrap();
        assert!(value.verify_pins().is_err());
        assert!(store.load(DEV_MODEL_IDS[0]).is_err());
    }

    /// btrfs gives a mount a new anonymous `st_dev` after reboot; a snapshot
    /// written before the reboot must still load, while any change to size,
    /// mtime, or inode still fails.
    #[test]
    fn registered_snapshot_ignores_device_number_but_not_file_identity() {
        let dir = TempDir::new().unwrap();
        let store = DevelopmentStore::open(dir.path().join("local-dev")).unwrap();
        let value = manifest(dir.path(), DEV_MODEL_IDS[0]);
        store.register(&value).unwrap();
        let snapshot_path = dir.path().join(format!(
            "local-dev/manifests/{}.fingerprints.json",
            DEV_MODEL_IDS[0]
        ));
        let written: serde_json::Value =
            serde_json::from_slice(&fs::read(&snapshot_path).unwrap()).unwrap();
        let rewrite = |field: &str, change: &dyn Fn(u64) -> u64| {
            let mut snapshot = written.clone();
            for fingerprint in snapshot["fingerprints"]
                .as_object_mut()
                .unwrap()
                .values_mut()
            {
                let fingerprint = fingerprint.as_object_mut().unwrap();
                assert!(!fingerprint.contains_key("device"), "device is not written");
                let current = fingerprint
                    .get(field)
                    .and_then(|v| v.as_u64())
                    .unwrap_or(57);
                fingerprint.insert(field.into(), serde_json::Value::from(change(current)));
            }
            fs::write(&snapshot_path, serde_json::to_vec(&snapshot).unwrap()).unwrap();
        };

        // A legacy snapshot recorded before a reboot, with another device number.
        rewrite("device", &|_| 57);
        assert!(store.load(DEV_MODEL_IDS[0]).is_ok());
        for field in ["size", "mtime_seconds", "mtime_nanoseconds", "inode"] {
            rewrite(field, &|current| current + 1);
            assert!(
                store.load(DEV_MODEL_IDS[0]).is_err(),
                "a changed {field} must fail"
            );
        }
    }

    #[test]
    fn development_selection_does_not_change_production_catalog() {
        let dir = TempDir::new().unwrap();
        let store = DevelopmentStore::open(dir.path().join("local-dev")).unwrap();
        let value = manifest(dir.path(), DEV_MODEL_IDS[0]);
        let handle = store.register(&value).unwrap();
        store
            .select(DEV_MODEL_IDS[0], Some(voisu_core::DaemonState::Idle))
            .unwrap();
        let catalog = crate::local_model::shipped_catalog();
        assert!(crate::local_model::production_selection(&catalog).is_none());
        assert!(
            catalog
                .entries
                .iter()
                .all(|entry| entry.id != DEV_MODEL_IDS[0])
        );
        let receipt = development_receipt(&value, &handle.manifest_hash);
        assert_eq!(receipt.catalog_id, DEV_MODEL_IDS[0]);
        assert!(
            catalog
                .entries
                .iter()
                .all(|entry| entry.id != receipt.catalog_id)
        );
        assert!(!dir.path().join("active.json").exists());
        assert!(!dir.path().join("receipt.json").exists());
        assert!(dir.path().join("local-dev/selection.json").is_file());
    }

    #[test]
    fn qwen_family_keeps_english_and_hinglish_selectable() {
        for model_id in ["qwen3-asr-0.6b", "qwen3-asr-1.7b"] {
            let entry = development_catalog_entry(model_id).unwrap();
            assert_eq!(entry.languages, &["en", "hi", "auto"]);
            assert_eq!(entry.runtime, DevRuntimeKind::Python);
            assert!(entry.devices.contains(&"cuda:0"));
            let dir = TempDir::new().unwrap();
            let mut value = manifest(dir.path(), model_id);
            let model_dir = dir.path().join("qwen");
            fs::create_dir_all(&model_dir).unwrap();
            value.model_path = model_dir.to_string_lossy().into_owned();
            for name in QWEN_REQUIRED_FILES {
                let path = model_dir.join(name);
                fs::write(&path, name.as_bytes()).unwrap();
                let bytes = fs::read(&path).unwrap();
                value.files.push(DevFilePin {
                    path: path.to_string_lossy().into_owned(),
                    sha256: sha256_hex(&bytes),
                    bytes: bytes.len() as u64,
                });
            }
            value.language = "en".into();
            assert!(value.validate().is_ok(), "{model_id} en");
            value.language = "hi".into();
            assert!(value.validate().is_ok(), "{model_id} hi");
            value.language = "auto".into();
            assert!(value.validate().is_ok(), "{model_id} auto");
            value
                .files
                .retain(|pin| Path::new(&pin.path).file_name() != Some("tokenizer.json".as_ref()));
            assert!(
                value.validate().is_err(),
                "{model_id} must pin tokenizer.json"
            );
        }
        let large = development_catalog_entry("qwen3-asr-1.7b").unwrap();
        assert!(large.vram_bytes < 8151u64 * 1024 * 1024 * 75 / 100);
        let catalog = crate::local_model::shipped_catalog();
        assert!(crate::local_model::production_selection(&catalog).is_none());
        assert!(
            catalog
                .entries
                .iter()
                .all(|entry| entry.id != "qwen3-asr-0.6b" && entry.id != "qwen3-asr-1.7b")
        );
    }

    #[test]
    fn parakeet_is_english_only() {
        let entry = development_catalog_entry("parakeet-tdt-0.6b-v3").unwrap();
        assert_eq!(entry.languages, &["en"]);
        assert_eq!(entry.runtime, DevRuntimeKind::Native);
        let dir = TempDir::new().unwrap();
        let mut value = manifest(dir.path(), "parakeet-tdt-0.6b-v3");
        assert!(value.validate().is_ok());
        value.language = "hi".into();
        assert!(value.validate().is_err());
        value.language = "auto".into();
        assert!(value.validate().is_err());
        value.language = "en".into();
        assert!(value.validate().is_ok());
        value.options.insert(
            "language_code".into(),
            serde_json::Value::String("hi-IN".into()),
        );
        assert!(value.validate().is_err());
        value.options.insert(
            "language_code".into(),
            serde_json::Value::String("en-US".into()),
        );
        assert!(value.validate().is_ok());
    }

    fn remaining_manifest(dir: &Path, model_id: &str) -> DevManifest {
        let mut value = manifest(dir, model_id);
        match model_id {
            "fun-asr-nano-2512" => {
                let encoder = dir.join(FUN_ASR_ENCODER_NAME);
                let decoder = dir.join(FUN_ASR_DECODER_Q8_NAME);
                let cli = dir.join("llama-funasr-cli");
                for path in [&encoder, &decoder, &cli] {
                    fs::write(path, b"blob").unwrap();
                }
                value.options.insert(
                    "checkpoint".into(),
                    serde_json::Value::String(FUN_ASR_NANO_CHECKPOINT.into()),
                );
                value.options.insert(
                    "encoder".into(),
                    serde_json::Value::String(encoder.to_string_lossy().into_owned()),
                );
                value.options.insert(
                    "decoder".into(),
                    serde_json::Value::String(decoder.to_string_lossy().into_owned()),
                );
                value.options.insert(
                    "llama_cli".into(),
                    serde_json::Value::String(cli.to_string_lossy().into_owned()),
                );
                value.files.push(DevFilePin {
                    path: encoder.to_string_lossy().into_owned(),
                    sha256: FUN_ASR_ENCODER_SHA.into(),
                    bytes: FUN_ASR_ENCODER_BYTES,
                });
                value.files.push(DevFilePin {
                    path: decoder.to_string_lossy().into_owned(),
                    sha256: FUN_ASR_DECODER_Q8_SHA.into(),
                    bytes: FUN_ASR_DECODER_Q8_BYTES,
                });
                let cli_bytes = fs::read(&cli).unwrap();
                value.files.push(DevFilePin {
                    path: cli.to_string_lossy().into_owned(),
                    sha256: sha256_hex(&cli_bytes),
                    bytes: cli_bytes.len() as u64,
                });
            }
            "moonshine-streaming-medium" => {
                let model_dir = dir.join("medium-streaming");
                fs::create_dir_all(&model_dir).unwrap();
                value.model_path = model_dir.to_string_lossy().into_owned();
                value.options.insert(
                    "model_arch".into(),
                    serde_json::Value::String(MOONSHINE_MEDIUM_ARCH.into()),
                );
                for (name, sha, bytes) in MOONSHINE_MEDIUM_PINS {
                    let path = model_dir.join(name);
                    fs::write(&path, b"ort").unwrap();
                    value.files.push(DevFilePin {
                        path: path.to_string_lossy().into_owned(),
                        sha256: sha.into(),
                        bytes,
                    });
                }
            }
            "cohere-transcribe-03-2026" => {
                value.options.insert(
                    "revision".into(),
                    serde_json::Value::String(COHERE_HF_REV.into()),
                );
            }
            _ => {}
        }
        value
    }

    #[test]
    fn remaining_models_are_english_only_and_keep_exact_identity() {
        for model_id in [
            "fun-asr-nano-2512",
            "moonshine-streaming-medium",
            "cohere-transcribe-03-2026",
        ] {
            let entry = development_catalog_entry(model_id).unwrap();
            assert_eq!(entry.languages, &["en"]);
            let dir = TempDir::new().unwrap();
            let mut value = remaining_manifest(dir.path(), model_id);
            assert!(value.validate().is_ok(), "{model_id}");
            value.language = "hi".into();
            assert!(value.validate().is_err(), "{model_id} must reject Hindi");
            value.language = "auto".into();
            assert!(value.validate().is_err(), "{model_id} must reject auto");
        }
        let moonshine = development_catalog_entry("moonshine-streaming-medium").unwrap();
        assert_eq!(moonshine.devices, &["cpu"]);
        assert_eq!(moonshine.vram_bytes, 0);
        assert_eq!(moonshine.runtime, DevRuntimeKind::Python);
        let fun = development_catalog_entry("fun-asr-nano-2512").unwrap();
        assert_eq!(fun.runtime, DevRuntimeKind::Native);
        assert_eq!(FUN_ASR_ENCODER_SHA.len(), 64);
        assert_eq!(FUN_ASR_DECODER_Q8_SHA.len(), 64);
        assert_eq!(FUN_ASR_ENCODER_BYTES, 469_331_008);
        assert_eq!(FUN_ASR_DECODER_Q8_BYTES, 804_753_280);
        assert_eq!(FUN_ASR_VAD_NAME, "fsmn-vad.gguf");
        assert_eq!(FUN_ASR_VAD_SHA.len(), 64);
        assert_eq!(FUN_ASR_VAD_BYTES, 1_720_512);
        assert_eq!(MOONSHINE_MEDIUM_PINS.len(), 7);
        assert_eq!(COHERE_HF_REV, "b1eacc2686a3d08ceaae5f24a88b1d519620bc09");
        assert_eq!(FUN_ASR_HF_REV, "d93b302ee7fd505e1b3576120fc142fc6f7820e1");
        assert_eq!(FUN_ASR_HF_WEIGHTS_BYTES, 1_659_773_320);
    }

    #[test]
    fn fun_asr_rejects_mlt_and_requires_nano_bundle() {
        let dir = TempDir::new().unwrap();
        let mut value = remaining_manifest(dir.path(), "fun-asr-nano-2512");
        assert!(value.validate().is_ok());
        value.options.insert(
            "checkpoint".into(),
            serde_json::Value::String("Fun-ASR-MLT-Nano-2512".into()),
        );
        assert!(value.validate().is_err());
        value = remaining_manifest(dir.path(), "fun-asr-nano-2512");
        value.model_path = dir
            .path()
            .join("Fun-ASR-MLT-Nano-2512")
            .to_string_lossy()
            .into_owned();
        assert!(value.validate().is_err());
        value = remaining_manifest(dir.path(), "fun-asr-nano-2512");
        value.options.remove("encoder");
        value.options.remove("decoder");
        value.options.remove("llama_cli");
        assert!(value.validate().is_err());
        let remote = dir.path().join("model.py");
        fs::write(&remote, b"remote").unwrap();
        value.options.insert(
            "remote_code".into(),
            serde_json::Value::String(remote.to_string_lossy().into_owned()),
        );
        value.files.push(DevFilePin {
            path: remote.to_string_lossy().into_owned(),
            sha256: sha256_hex(b"remote"),
            bytes: 6,
        });
        assert!(value.validate().is_ok());
        value = remaining_manifest(dir.path(), "fun-asr-nano-2512");
        value
            .files
            .retain(|pin| Path::new(&pin.path).file_name() != Some(FUN_ASR_ENCODER_NAME.as_ref()));
        assert!(value.validate().is_err());
        value = remaining_manifest(dir.path(), "fun-asr-nano-2512");
        if let Some(pin) = value
            .files
            .iter_mut()
            .find(|pin| Path::new(&pin.path).file_name() == Some(FUN_ASR_ENCODER_NAME.as_ref()))
        {
            pin.sha256 = "0".repeat(64);
            pin.bytes = 4;
        }
        assert!(value.validate().is_err());
        value = remaining_manifest(dir.path(), "fun-asr-nano-2512");
        value.options.remove("encoder");
        value.options.remove("decoder");
        value.options.remove("llama_cli");
        value.options.insert(
            "revision".into(),
            serde_json::Value::String(FUN_ASR_HF_REV.into()),
        );
        value.device = "cuda:0".into();
        value.limits.vram_bytes = 3 * 1024 * 1024 * 1024;
        assert!(value.validate().is_err());
        let weights = dir.path().join(FUN_ASR_HF_WEIGHTS_NAME);
        value.files.push(DevFilePin {
            path: weights.to_string_lossy().into_owned(),
            sha256: FUN_ASR_HF_WEIGHTS_SHA.into(),
            bytes: FUN_ASR_HF_WEIGHTS_BYTES,
        });
        assert!(value.validate().is_ok());
        if let Some(pin) = value
            .files
            .iter_mut()
            .find(|pin| Path::new(&pin.path) == weights.as_path())
        {
            pin.sha256 = "0".repeat(64);
        }
        assert!(value.validate().is_err());
        value = remaining_manifest(dir.path(), "fun-asr-nano-2512");
        value.options.remove("encoder");
        value.options.remove("decoder");
        value.options.remove("llama_cli");
        value.options.insert(
            "revision".into(),
            serde_json::Value::String(FUN_ASR_HF_REV.into()),
        );
        value.device = "cuda:0".into();
        value.limits.vram_bytes = 3 * 1024 * 1024 * 1024;
        value.files.push(DevFilePin {
            path: dir
                .path()
                .join("other")
                .join(FUN_ASR_HF_WEIGHTS_NAME)
                .to_string_lossy()
                .into_owned(),
            sha256: FUN_ASR_HF_WEIGHTS_SHA.into(),
            bytes: FUN_ASR_HF_WEIGHTS_BYTES,
        });
        assert!(value.validate().is_err());
        value
            .options
            .insert("revision".into(), serde_json::Value::String("0".repeat(40)));
        assert!(value.validate().is_err());
    }

    #[test]
    fn moonshine_is_cpu_first_medium_streaming() {
        let dir = TempDir::new().unwrap();
        let mut value = remaining_manifest(dir.path(), "moonshine-streaming-medium");
        assert!(value.validate().is_ok());
        value.device = "cuda:0".into();
        value.limits.vram_bytes = 1;
        assert!(value.validate().is_err());
        value = remaining_manifest(dir.path(), "moonshine-streaming-medium");
        value.options.insert(
            "model_arch".into(),
            serde_json::Value::String("SMALL_STREAMING".into()),
        );
        assert!(value.validate().is_err());
        value = remaining_manifest(dir.path(), "moonshine-streaming-medium");
        value.model_path = dir.path().join("model.bin").to_string_lossy().into_owned();
        assert!(value.validate().is_err());
        value = remaining_manifest(dir.path(), "moonshine-streaming-medium");
        value
            .files
            .retain(|pin| Path::new(&pin.path).file_name() != Some("encoder.ort".as_ref()));
        assert!(value.validate().is_err());
        value = remaining_manifest(dir.path(), "moonshine-streaming-medium");
        let outside = dir.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        if let Some(pin) = value
            .files
            .iter_mut()
            .find(|pin| Path::new(&pin.path).file_name() == Some("encoder.ort".as_ref()))
        {
            pin.path = outside.join("encoder.ort").to_string_lossy().into_owned();
        }
        assert!(value.validate().is_err());
        value = remaining_manifest(dir.path(), "moonshine-streaming-medium");
        assert!(value.validate().is_ok());
        let error = value.verify_pins().unwrap_err();
        let message = error.to_string();
        assert!(
            message.contains("file pin size/type changed")
                || message.contains("file pin digest changed"),
            "{message}"
        );
    }

    #[test]
    fn cohere_requires_pinned_git_revision_not_a_pr_ref() {
        let dir = TempDir::new().unwrap();
        let mut value = remaining_manifest(dir.path(), "cohere-transcribe-03-2026");
        assert!(value.validate().is_ok());
        value.options.insert(
            "revision".into(),
            serde_json::Value::String("refs/pr/6".into()),
        );
        assert!(value.validate().is_err());
        value.options.remove("revision");
        assert!(value.validate().is_err());
    }

    #[test]
    fn cache_dir_option_is_absolute_and_unpinned() {
        let dir = TempDir::new().unwrap();
        let mut value = manifest(dir.path(), DEV_MODEL_IDS[0]);
        value.options.insert(
            "cache_dir".into(),
            serde_json::Value::String(dir.path().join("cache").to_string_lossy().into_owned()),
        );
        assert!(value.validate().is_ok());
        value.options.insert(
            "cache_dir".into(),
            serde_json::Value::String("relative-cache".into()),
        );
        assert!(value.validate().is_err());
    }
}
