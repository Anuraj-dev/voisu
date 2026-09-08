//! Versioned, immutable catalog. Packages contain this catalog, not weights.

use sha2::{Digest, Sha256};

use crate::local_worker::{PROTOCOL_VERSION, RuntimeFamily};

pub const SHA256_HEX_LEN: usize = 64;
pub const CATALOG_VERSION: u32 = 1;
const FIXTURE_MODEL: &[u8] = b"voisu-l3-fixture-model";
const FIXTURE_PCM: &[u8] = b"voisu-l3-health-pcm\0";
const FIXTURE_MODEL_SHA: &str = "0baba5aced9189d1fa6b1d464744c379d31c6874564d2d05dc40f7c305a0c96f";
const FIXTURE_PCM_SHA: &str = "1af47a487fa2083c5924dc26e21e16c606458d55bf45d840ca171323838f72bf";

/// Hugging Face git commit that introduced these GGML blobs (not `main`).
const WHISPER_CPP_HF_REV: &str = "80da2d8bfee42b0e836fc3a9890373e5defc00a6";
const GGML_BASE_EN_SHA: &str = "a03779c86df3323075f5e796cb2ce5029f00ec8869eee3fdfb897afe36c6d002";
const GGML_BASE_EN_BYTES: u64 = 147_964_211;
const GGML_SMALL_EN_SHA: &str = "c6138d6d58ecc8322097e0f987c32f1be8bb0a18532a3f88f734d1bbf9c41e5d";
const GGML_SMALL_EN_BYTES: u64 = 487_614_201;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileKind {
    Weights,
    HealthFixture,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeviceSupport {
    Cpu,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeAbi {
    pub family: RuntimeFamily,
    pub abi_id: &'static str,
    pub protocol: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LicenseTerms {
    pub spdx: &'static str,
    pub name: &'static str,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Provenance {
    pub publisher: &'static str,
    pub source: &'static str,
    pub revision: &'static str,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Redistribution {
    pub allowed: bool,
    pub terms: &'static str,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CatalogFile {
    pub name: &'static str,
    pub sha256_hex: &'static str,
    pub bytes: u64,
    pub url: &'static str,
    pub kind: FileKind,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CatalogEntry {
    pub id: &'static str,
    pub revision: &'static str,
    pub language: &'static str,
    pub required_names: &'static [&'static str],
    pub runtime_abi: RuntimeAbi,
    pub devices: &'static [DeviceSupport],
    pub license: LicenseTerms,
    pub provenance: Provenance,
    pub redistribution: Redistribution,
    pub files: &'static [CatalogFile],
    pub allowed_hosts: &'static [&'static str],
    pub production_weights: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Catalog {
    pub version: u32,
    pub entries: &'static [CatalogEntry],
}

#[must_use]
pub fn shipped_catalog() -> Catalog {
    Catalog {
        version: CATALOG_VERSION,
        entries: SHIPPED_ENTRIES,
    }
}

/// Milestone one pins exactly one winner after L2 evidence. None is selected.
#[must_use]
pub fn bakeoff_winner(_catalog: &Catalog) -> Option<&'static CatalogEntry> {
    None
}

#[must_use]
pub fn ci_fixture_entry() -> &'static CatalogEntry {
    &SHIPPED_ENTRIES[0]
}

#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(SHA256_HEX_LEN);
    for byte in digest {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

pub fn verify_file_digest(bytes: &[u8], expected_hex: &str, expected_len: u64) -> bool {
    u64::try_from(bytes.len()).is_ok_and(|len| len == expected_len)
        && expected_hex.len() == SHA256_HEX_LEN
        && expected_hex.bytes().all(|b| b.is_ascii_hexdigit())
        && expected_hex.bytes().any(|b| b != b'0')
        && sha256_hex(bytes) == expected_hex
}

const HEX: &[u8; 16] = b"0123456789abcdef";

const FIXTURE_ABI: RuntimeAbi = RuntimeAbi {
    family: RuntimeFamily::WhisperCppProcess,
    abi_id: "voisu-l3-test-abi",
    protocol: PROTOCOL_VERSION,
};

const WHISPER_ABI: RuntimeAbi = RuntimeAbi {
    family: RuntimeFamily::WhisperCppProcess,
    abi_id: "whisper.cpp-ggml-v1",
    protocol: PROTOCOL_VERSION,
};

const SHIPPED_ENTRIES: &[CatalogEntry] = &[
    CatalogEntry {
        id: "l3-health-fixture",
        revision: "1",
        language: "en",
        required_names: &["model.bin", "health.pcm"],
        runtime_abi: FIXTURE_ABI,
        devices: &[DeviceSupport::Cpu],
        license: LicenseTerms {
            spdx: "MIT",
            name: "Voisu L3 health fixture",
        },
        provenance: Provenance {
            publisher: "voisu",
            source: "in-tree L3 fixture",
            revision: "1",
        },
        redistribution: Redistribution {
            allowed: true,
            terms: "test fixture only; not a production model",
        },
        files: &[
            CatalogFile {
                name: "model.bin",
                sha256_hex: FIXTURE_MODEL_SHA,
                bytes: FIXTURE_MODEL.len() as u64,
                url: "https://fixtures.voisu.test/l3/model.bin",
                kind: FileKind::Weights,
            },
            CatalogFile {
                name: "health.pcm",
                sha256_hex: FIXTURE_PCM_SHA,
                bytes: FIXTURE_PCM.len() as u64,
                url: "https://fixtures.voisu.test/l3/health.pcm",
                kind: FileKind::HealthFixture,
            },
        ],
        allowed_hosts: &["fixtures.voisu.test"],
        production_weights: false,
    },
    CatalogEntry {
        id: "whisper-cpp-ggml-base.en",
        revision: WHISPER_CPP_HF_REV,
        language: "en",
        required_names: &["ggml-base.en.bin"],
        runtime_abi: WHISPER_ABI,
        devices: &[DeviceSupport::Cpu],
        license: LicenseTerms {
            spdx: "MIT",
            name: "whisper.cpp GGML base.en (unelected)",
        },
        provenance: Provenance {
            publisher: "ggerganov/whisper.cpp",
            source: "https://huggingface.co/ggerganov/whisper.cpp",
            revision: WHISPER_CPP_HF_REV,
        },
        redistribution: Redistribution {
            allowed: true,
            terms: "upstream whisper.cpp model card; not selected",
        },
        files: &[CatalogFile {
            name: "ggml-base.en.bin",
            sha256_hex: GGML_BASE_EN_SHA,
            bytes: GGML_BASE_EN_BYTES,
            url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/80da2d8bfee42b0e836fc3a9890373e5defc00a6/ggml-base.en.bin",
            kind: FileKind::Weights,
        }],
        allowed_hosts: &["huggingface.co"],
        production_weights: true,
    },
    CatalogEntry {
        id: "whisper-cpp-ggml-small.en",
        revision: WHISPER_CPP_HF_REV,
        language: "en",
        required_names: &["ggml-small.en.bin"],
        runtime_abi: WHISPER_ABI,
        devices: &[DeviceSupport::Cpu],
        license: LicenseTerms {
            spdx: "MIT",
            name: "whisper.cpp GGML small.en (unelected)",
        },
        provenance: Provenance {
            publisher: "ggerganov/whisper.cpp",
            source: "https://huggingface.co/ggerganov/whisper.cpp",
            revision: WHISPER_CPP_HF_REV,
        },
        redistribution: Redistribution {
            allowed: true,
            terms: "upstream whisper.cpp model card; not selected",
        },
        files: &[CatalogFile {
            name: "ggml-small.en.bin",
            sha256_hex: GGML_SMALL_EN_SHA,
            bytes: GGML_SMALL_EN_BYTES,
            url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/80da2d8bfee42b0e836fc3a9890373e5defc00a6/ggml-small.en.bin",
            kind: FileKind::Weights,
        }],
        allowed_hosts: &["huggingface.co"],
        production_weights: true,
    },
];

impl CatalogEntry {
    pub fn file(&self, name: &str) -> Option<&CatalogFile> {
        self.files.iter().find(|file| file.name == name)
    }

    pub fn total_bytes(&self) -> u64 {
        self.files.iter().map(|file| file.bytes).sum()
    }

    pub fn abi_supported(&self) -> bool {
        !self.runtime_abi.abi_id.is_empty() && self.runtime_abi.protocol == PROTOCOL_VERSION
    }

    #[must_use]
    pub fn fixture_model_bytes() -> &'static [u8] {
        FIXTURE_MODEL
    }

    #[must_use]
    pub fn fixture_pcm_bytes() -> &'static [u8] {
        FIXTURE_PCM
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixture_digests_match_pinned_bytes() {
        let entry = ci_fixture_entry();
        assert!(!entry.production_weights);
        assert_eq!(entry.language, "en");
        assert!(verify_file_digest(
            CatalogEntry::fixture_model_bytes(),
            entry.file("model.bin").unwrap().sha256_hex,
            entry.file("model.bin").unwrap().bytes,
        ));
        assert!(verify_file_digest(
            CatalogEntry::fixture_pcm_bytes(),
            entry.file("health.pcm").unwrap().sha256_hex,
            entry.file("health.pcm").unwrap().bytes,
        ));
    }

    #[test]
    fn production_entries_pin_immutable_revisions_not_main() {
        let catalog = shipped_catalog();
        assert!(bakeoff_winner(&catalog).is_none());
        for entry in catalog.entries {
            assert_ne!(entry.runtime_abi.family, RuntimeFamily::FasterWhisper);
            assert!(!format!("{entry:?}").to_ascii_lowercase().contains("ollama"));
            assert!(entry.abi_supported());
            for file in entry.files {
                assert_eq!(file.sha256_hex.len(), SHA256_HEX_LEN);
                assert!(file.sha256_hex.bytes().any(|b| b != b'0'));
                assert!(file.bytes > 1);
                assert!(!file.url.contains("/resolve/main/"));
            }
        }
        let production = catalog
            .entries
            .iter()
            .filter(|entry| entry.production_weights)
            .collect::<Vec<_>>();
        assert_eq!(production.len(), 2);
        assert!(
            production
                .iter()
                .all(|entry| entry.revision == WHISPER_CPP_HF_REV)
        );
        assert_eq!(
            production[0].file("ggml-base.en.bin").unwrap().bytes,
            GGML_BASE_EN_BYTES
        );
        assert_eq!(
            production[0].file("ggml-base.en.bin").unwrap().sha256_hex,
            GGML_BASE_EN_SHA
        );
        assert_eq!(
            production[1].file("ggml-small.en.bin").unwrap().bytes,
            GGML_SMALL_EN_BYTES
        );
        assert_eq!(
            production[1].file("ggml-small.en.bin").unwrap().sha256_hex,
            GGML_SMALL_EN_SHA
        );
    }

    #[test]
    fn all_zero_digest_is_not_a_pin() {
        assert!(!verify_file_digest(&[0], &"0".repeat(64), 1));
    }
}
