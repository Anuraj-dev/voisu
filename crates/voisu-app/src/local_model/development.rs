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
pub const DEV_MODEL_IDS: [&str; 7] = [
    "nemotron-3.5-0.6b",
    "parakeet-tdt-0.6b-v3",
    "qwen3-asr-0.6b",
    "qwen3-asr-1.7b",
    "fun-asr-nano-2512",
    "moonshine-streaming-medium",
    "cohere-transcribe-03-2026",
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DevDevice {
    Cuda0,
    Cpu,
}

impl DevDevice {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cuda0 => "cuda:0",
            Self::Cpu => "cpu",
        }
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "cuda:0" => Some(Self::Cuda0),
            "cpu" => Some(Self::Cpu),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DevLanguage {
    English,
    Hindi,
    Auto,
}

impl DevLanguage {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::English => "en",
            Self::Hindi => "hi",
            Self::Auto => "auto",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "en" => Some(Self::English),
            "hi" => Some(Self::Hindi),
            "auto" => Some(Self::Auto),
            _ => None,
        }
    }
}

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
    pub note: &'static str,
}

const EN: &[&str] = &["en"];
const EN_HI_AUTO: &[&str] = &["en", "hi", "auto"];
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
        note: "NeMo-Speech.cpp native development adapter",
    },
    DevCatalogEntry {
        model_id: "parakeet-tdt-0.6b-v3",
        runtime: DevRuntimeKind::Native,
        languages: EN,
        devices: CPU_CUDA,
        memory_bytes: 4 * 1024 * 1024 * 1024,
        vram_bytes: 2 * 1024 * 1024 * 1024,
        note: "NeMo-Speech.cpp native English adapter",
    },
    DevCatalogEntry {
        model_id: "qwen3-asr-0.6b",
        runtime: DevRuntimeKind::Python,
        languages: EN_HI_AUTO,
        devices: CPU_CUDA,
        memory_bytes: 4 * 1024 * 1024 * 1024,
        vram_bytes: 2 * 1024 * 1024 * 1024,
        note: "Measured Transformers CUDA on RTX 5050 8151 MiB: peak RAM 2465 MiB, peak VRAM 1804 MiB; observed cuda:0; English and Hinglish selectable",
    },
    DevCatalogEntry {
        model_id: "qwen3-asr-1.7b",
        runtime: DevRuntimeKind::Python,
        languages: EN_HI_AUTO,
        devices: CPU_CUDA,
        memory_bytes: 8 * 1024 * 1024 * 1024,
        vram_bytes: 5 * 1024 * 1024 * 1024,
        note: "Measured Transformers CUDA on RTX 5050 8151 MiB: peak RAM 4869 MiB, peak VRAM 4196 MiB; fits 75% admission 6113 MiB plus 2038 MiB desktop reserve; observed cuda:0; English and Hinglish selectable",
    },
    DevCatalogEntry {
        model_id: "fun-asr-nano-2512",
        runtime: DevRuntimeKind::Native,
        languages: EN,
        devices: CPU_CUDA,
        memory_bytes: 6 * 1024 * 1024 * 1024,
        vram_bytes: 3 * 1024 * 1024 * 1024,
        note: "Fun-ASR-Nano-2512 only (not Fun-ASR-MLT-Nano); llama.cpp CPU JFK 1.67s; Linux llama.cpp has no Nano CUDA package; Transformers CUDA on RTX 5050 observed cuda:0 peak RAM 2576 MiB / VRAM 1625 MiB; English comparison, no Hindi",
    },
    DevCatalogEntry {
        model_id: "moonshine-streaming-medium",
        runtime: DevRuntimeKind::Python,
        languages: EN,
        devices: CPU_ONLY,
        memory_bytes: 2 * 1024 * 1024 * 1024,
        vram_bytes: 0,
        note: "Official moonshine-voice==0.1.5 Medium Streaming CPU-first English adapter; JFK load 0.274s cold 1.994s warm 1.934s; GPU is not a supported runtime profile",
    },
    DevCatalogEntry {
        model_id: "cohere-transcribe-03-2026",
        runtime: DevRuntimeKind::Python,
        languages: EN,
        devices: CPU_CUDA,
        memory_bytes: 8 * 1024 * 1024 * 1024,
        vram_bytes: 4 * 1024 * 1024 * 1024,
        note: "Cohere Transcribe 03-2026 English-only Transformers adapter; Hugging Face gated HTTP 401 GatedRepo on this host; Hindi is not in the advertised language set",
    },
];

/// Requested Fun-ASR Nano checkpoint. Fun-ASR-MLT-Nano is a different model.
pub const FUN_ASR_NANO_CHECKPOINT: &str = "Fun-ASR-Nano-2512";
pub const FUN_ASR_NANO_GGUF_SOURCE: &str = "https://huggingface.co/FunAudioLLM/Fun-ASR-Nano-GGUF";
pub const FUN_ASR_NANO_GGUF_REV: &str = "46e849502a867080d66d351b8dfb1018b607e509";
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
pub const FUN_ASR_LLAMA_RUNTIME: &str = "runtime-llamacpp-v0.2.6";
pub const FUN_ASR_HF_SOURCE: &str = "https://huggingface.co/FunAudioLLM/Fun-ASR-Nano-2512-hf";
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
pub const MOONSHINE_VOICE_SOURCE: &str =
    "https://download.moonshine.ai/model/medium-streaming-en/quantized";
pub const MOONSHINE_MEDIUM_FILES: [&str; 7] = [
    "encoder.ort",
    "adapter.ort",
    "cross_kv.ort",
    "decoder_kv.ort",
    "frontend.ort",
    "streaming_config.json",
    "tokenizer.bin",
];
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
pub const FUN_ASR_LLAMA_LINUX_X64_AVX2_SHA: &str =
    "aaebc5470f846ce915200b35d6e9f9bd0a0d3ed399d39e49bdeb7a1f1782bc70";
pub const COHERE_HF_REV: &str = "b1eacc2686a3d08ceaae5f24a88b1d519620bc09";
pub const COHERE_HF_SOURCE: &str = "https://huggingface.co/CohereLabs/cohere-transcribe-03-2026";

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
        let device = DevDevice::parse(&self.device)
            .ok_or_else(|| DevelopmentError::Invalid("device must be cuda:0 or cpu".into()))?;
        let language = DevLanguage::parse(&self.language)
            .ok_or_else(|| DevelopmentError::Invalid("language must be en, hi, or auto".into()))?;
        if !catalog.devices.contains(&device.as_str()) {
            return Err(DevelopmentError::Invalid(format!(
                "{} does not support {}",
                self.model_id,
                device.as_str()
            )));
        }
        if !catalog.languages.contains(&language.as_str()) {
            return Err(DevelopmentError::Invalid(format!(
                "{} does not advertise {}",
                self.model_id,
                language.as_str()
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
        if device == DevDevice::Cpu && self.limits.vram_bytes != 0 {
            return Err(DevelopmentError::Invalid(
                "cpu manifests must set vram_bytes=0".into(),
            ));
        }
        if device == DevDevice::Cuda0 && self.limits.vram_bytes == 0 {
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
                {
                    if Path::new(encoder).file_name() != Some(FUN_ASR_ENCODER_NAME.as_ref()) {
                        return Err(DevelopmentError::Invalid(
                            "fun-asr-nano-2512 encoder must be funasr-encoder-f16.gguf".into(),
                        ));
                    }
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
                {
                    if revision != FUN_ASR_HF_REV {
                        return Err(DevelopmentError::Invalid(
                            "fun-asr-nano-2512 Transformers revision must be the pinned Fun-ASR-Nano-2512-hf commit".into(),
                        ));
                    }
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
                    if let Some(path) = self.options.get(key).and_then(serde_json::Value::as_str) {
                        if !paths.contains_key(path) {
                            return Err(DevelopmentError::Invalid(format!(
                                "{key} must be an absolute pinned file"
                            )));
                        }
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
                {
                    if !pin.sha256.eq_ignore_ascii_case(FUN_ASR_HF_WEIGHTS_SHA)
                        || pin.bytes != FUN_ASR_HF_WEIGHTS_BYTES
                    {
                        return Err(DevelopmentError::Invalid(
                            "Fun-ASR-Nano-2512-hf weights do not match the published snapshot"
                                .into(),
                        ));
                    }
                }
            }
            "moonshine-streaming-medium" => {
                let root = model_artifact_root(&self.model_path);
                for (name, sha, bytes) in MOONSHINE_MEDIUM_PINS {
                    self.require_published_path_pin(&root.join(name), sha, bytes, name)?;
                }
            }
            _ => {}
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

    fn identity_hash(&self) -> Result<String, DevelopmentError> {
        self.validate()?;
        self.verify_pin_sizes()?;
        let encoded =
            serde_json::to_vec(self).map_err(|error| DevelopmentError::Json(error.to_string()))?;
        Ok(sha256_hex(&encoded))
    }

    /// Validate the manifest and every pin against the current filesystem.
    /// This is called immediately before a worker is launched and again after
    /// a manifest is copied into the private store.
    pub fn verify_pins(&self) -> Result<String, DevelopmentError> {
        self.verify_pin_sizes()?;
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
        self.identity_hash()
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

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn register(&self, manifest: &DevManifest) -> Result<DevManifestHandle, DevelopmentError> {
        let hash = manifest.verify_pins()?;
        let path = self
            .root
            .join("manifests")
            .join(format!("{}.json", manifest.model_id));
        let encoded = manifest.to_json()?;
        atomic_private_write(&path, encoded.as_bytes())?;
        Ok(DevManifestHandle {
            model_id: manifest.model_id.clone(),
            path,
            manifest_hash: hash,
        })
    }

    pub fn register_file(&self, path: &Path) -> Result<DevManifestHandle, DevelopmentError> {
        if !path.is_absolute() {
            return Err(DevelopmentError::Invalid(
                "manifest path must be absolute".into(),
            ));
        }
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
            .map(|entry| match self.load(entry.model_id) {
                Ok((_manifest, handle)) => DevModelStatus {
                    model_id: entry.model_id.into(),
                    runtime: entry.runtime,
                    languages: entry.languages.iter().map(|v| (*v).into()).collect(),
                    devices: entry.devices.iter().map(|v| (*v).into()).collect(),
                    memory_bytes: entry.memory_bytes,
                    vram_bytes: entry.vram_bytes,
                    manifest: Some(handle),
                    selected: selected.as_deref() == Some(entry.model_id),
                    ready: false,
                    error: None,
                },
                Err(error) => DevModelStatus {
                    model_id: entry.model_id.into(),
                    runtime: entry.runtime,
                    languages: entry.languages.iter().map(|v| (*v).into()).collect(),
                    devices: entry.devices.iter().map(|v| (*v).into()).collect(),
                    memory_bytes: entry.memory_bytes,
                    vram_bytes: entry.vram_bytes,
                    manifest: None,
                    selected: selected.as_deref() == Some(entry.model_id),
                    ready: false,
                    error: Some(error.to_string()),
                },
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
    fn catalog_contains_exactly_the_seven_requested_models() {
        assert_eq!(development_catalog().len(), 7);
        assert_eq!(
            development_catalog()
                .iter()
                .map(|e| e.model_id)
                .collect::<Vec<_>>(),
            DEV_MODEL_IDS
        );
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
        let small = development_catalog_entry("qwen3-asr-0.6b").unwrap();
        let large = development_catalog_entry("qwen3-asr-1.7b").unwrap();
        assert!(small.note.contains("1804 MiB"), "{}", small.note);
        assert!(large.note.contains("4196 MiB"), "{}", large.note);
        assert!(!large.note.to_ascii_lowercase().contains("blocked"));
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
            assert!(!entry.note.to_ascii_lowercase().contains("hindi candidate"));
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
        assert!(moonshine.note.contains("1.994s"));
        assert!(!moonshine.note.to_ascii_lowercase().contains("hindi"));
        let fun = development_catalog_entry("fun-asr-nano-2512").unwrap();
        assert_eq!(fun.runtime, DevRuntimeKind::Native);
        assert!(fun.note.contains(FUN_ASR_NANO_CHECKPOINT));
        assert!(fun.note.contains("Fun-ASR-MLT-Nano"));
        assert!(fun.note.contains("1625 MiB"));
        assert!(!fun.note.to_ascii_lowercase().contains("hindi candidate"));
        let cohere = development_catalog_entry("cohere-transcribe-03-2026").unwrap();
        assert!(cohere.note.to_ascii_lowercase().contains("gated"));
        assert!(cohere.note.contains("401"));
        assert_eq!(FUN_ASR_ENCODER_SHA.len(), 64);
        assert_eq!(FUN_ASR_DECODER_Q8_SHA.len(), 64);
        assert_eq!(FUN_ASR_ENCODER_BYTES, 469_331_008);
        assert_eq!(FUN_ASR_DECODER_Q8_BYTES, 804_753_280);
        assert_eq!(FUN_ASR_VAD_NAME, "fsmn-vad.gguf");
        assert_eq!(FUN_ASR_VAD_SHA.len(), 64);
        assert_eq!(FUN_ASR_VAD_BYTES, 1_720_512);
        assert_eq!(FUN_ASR_NANO_GGUF_REV.len(), 40);
        assert!(FUN_ASR_NANO_GGUF_SOURCE.contains("Fun-ASR-Nano-GGUF"));
        assert_eq!(FUN_ASR_LLAMA_RUNTIME, "runtime-llamacpp-v0.2.6");
        assert_eq!(MOONSHINE_MEDIUM_FILES.len(), 7);
        assert_eq!(MOONSHINE_MEDIUM_PINS.len(), 7);
        assert!(MOONSHINE_MEDIUM_FILES.contains(&"encoder.ort"));
        assert!(!MOONSHINE_MEDIUM_FILES.contains(&"encoder_model.ort"));
        assert_eq!(
            FUN_ASR_LLAMA_LINUX_X64_AVX2_SHA,
            "aaebc5470f846ce915200b35d6e9f9bd0a0d3ed399d39e49bdeb7a1f1782bc70"
        );
        assert!(MOONSHINE_VOICE_SOURCE.contains("download.moonshine.ai"));
        assert!(!MOONSHINE_VOICE_SOURCE.contains("huggingface.co"));
        assert_eq!(COHERE_HF_REV, "b1eacc2686a3d08ceaae5f24a88b1d519620bc09");
        assert_eq!(FUN_ASR_HF_REV, "d93b302ee7fd505e1b3576120fc142fc6f7820e1");
        assert_eq!(FUN_ASR_HF_WEIGHTS_BYTES, 1_659_773_320);
        assert!(FUN_ASR_HF_SOURCE.contains("Fun-ASR-Nano-2512-hf"));
        assert!(!FUN_ASR_HF_SOURCE.to_ascii_lowercase().contains("mlt"));
        assert!(COHERE_HF_SOURCE.contains("CohereLabs/cohere-transcribe-03-2026"));
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
