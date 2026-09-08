//! Runtime feasibility notes. Evaluate process-wrapped whisper.cpp first.
//!
//! This file does **not** select a winner. faster-whisper / Parakeet / Moonshine
//! are recorded only where an exact runtime format and package stack can meet
//! R3–R5. Ollama is not a candidate.

use std::path::{Path, PathBuf};

use super::sandbox::{PackagedUnitRestrictions, packaged_unit_restrictions};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeFamily {
    WhisperCppProcess,
    FasterWhisper,
    Parakeet,
    Moonshine,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Eligibility {
    EvaluateFirst {
        runtime_format: &'static str,
    },
    Blocked {
        reason: &'static str,
    },
    PendingExactFormat {
        required_format: &'static str,
        package_stack: &'static str,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeCandidate {
    pub family: RuntimeFamily,
    pub eligibility: Eligibility,
    pub native_dependencies: &'static [&'static str],
    pub license_recheck_required: bool,
}

/// Ordered evaluation set. Index 0 is always process-wrapped whisper.cpp.
#[must_use]
pub fn evaluation_order(unit: &PackagedUnitRestrictions) -> Vec<RuntimeCandidate> {
    vec![
        RuntimeCandidate {
            family: RuntimeFamily::WhisperCppProcess,
            eligibility: Eligibility::EvaluateFirst {
                runtime_format: "ggml .bin via process-wrapped whisper.cpp (no in-process FFI)",
            },
            native_dependencies: &["whisper.cpp", "ggml"],
            license_recheck_required: true,
        },
        RuntimeCandidate {
            family: RuntimeFamily::FasterWhisper,
            eligibility: faster_whisper_eligibility(unit),
            native_dependencies: &["Python", "CTranslate2", "CUDA 12", "cuDNN 9"],
            license_recheck_required: true,
        },
        RuntimeCandidate {
            family: RuntimeFamily::Parakeet,
            eligibility: Eligibility::PendingExactFormat {
                required_format: "NVIDIA parakeet-tdt-0.6b-v3 as NeMo-Speech.cpp q8_0 GGUF",
                package_stack: "native GGUF worker that inherits the shipped unit restrictions",
            },
            native_dependencies: &["NeMo-Speech.cpp"],
            license_recheck_required: true,
        },
        RuntimeCandidate {
            family: RuntimeFamily::Moonshine,
            eligibility: Eligibility::PendingExactFormat {
                required_format: "Moonshine English ONNX Runtime .ort files",
                package_stack: "ONNX Runtime under MemoryDenyWriteExecute and RestrictNamespaces",
            },
            native_dependencies: &["ONNX Runtime"],
            license_recheck_required: true,
        },
    ]
}

fn faster_whisper_eligibility(unit: &PackagedUnitRestrictions) -> Eligibility {
    if unit.memory_deny_write_execute || unit.restrict_namespaces {
        Eligibility::Blocked {
            reason: "faster-whisper's Python/CTranslate2/CUDA stack is not proven under MemoryDenyWriteExecute and RestrictNamespaces; do not weaken the unit",
        }
    } else {
        Eligibility::PendingExactFormat {
            required_format: "CTranslate2 converted Whisper weights",
            package_stack: "non-JIT packaged worker matching R3-R5",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WhisperCppPaths {
    pub binary: Option<PathBuf>,
    pub model: Option<PathBuf>,
}

/// Explicit env only. Never PATH-search a model. Never download.
#[must_use]
pub fn whisper_cpp_paths() -> WhisperCppPaths {
    WhisperCppPaths {
        binary: std::env::var_os("VOISU_WHISPER_CPP_BIN").map(PathBuf::from),
        model: std::env::var_os("VOISU_WHISPER_CPP_MODEL").map(PathBuf::from),
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RuntimeError {
    ProductionDownloadForbidden,
    OllamaRejected,
    MissingRuntime,
    RelativeProgram,
}

pub fn refuse_production_weight_download() -> Result<(), RuntimeError> {
    Err(RuntimeError::ProductionDownloadForbidden)
}

pub fn reject_forbidden_program(program: &Path) -> Result<(), RuntimeError> {
    let name = program
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    if name.eq_ignore_ascii_case("ollama") {
        return Err(RuntimeError::OllamaRejected);
    }
    if !program.is_absolute() {
        return Err(RuntimeError::RelativeProgram);
    }
    Ok(())
}

#[must_use]
pub fn shipped_unit_candidates() -> Vec<RuntimeCandidate> {
    let unit = include_str!("../../../../packaging/voisu.service");
    evaluation_order(&packaged_unit_restrictions(unit))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whisper_cpp_is_evaluated_first_and_no_winner_is_encoded() {
        let candidates = shipped_unit_candidates();
        assert_eq!(candidates[0].family, RuntimeFamily::WhisperCppProcess);
        assert!(matches!(
            candidates[0].eligibility,
            Eligibility::EvaluateFirst { .. }
        ));
        assert_eq!(candidates.len(), 4);
    }

    #[test]
    fn ollama_is_not_a_runtime_family() {
        let names = shipped_unit_candidates()
            .into_iter()
            .map(|c| format!("{:?}", c.family))
            .collect::<String>();
        assert!(!names.to_ascii_lowercase().contains("ollama"));
        assert_eq!(
            reject_forbidden_program(Path::new("/usr/bin/ollama")),
            Err(RuntimeError::OllamaRejected)
        );
    }

    #[test]
    fn faster_whisper_is_blocked_under_the_shipped_unit() {
        let faster = shipped_unit_candidates()
            .into_iter()
            .find(|c| c.family == RuntimeFamily::FasterWhisper)
            .expect("faster-whisper remains in the evaluation set");
        assert!(matches!(faster.eligibility, Eligibility::Blocked { .. }));
    }

    #[test]
    fn parakeet_and_moonshine_require_exact_format_proof() {
        for family in [RuntimeFamily::Parakeet, RuntimeFamily::Moonshine] {
            let candidate = shipped_unit_candidates()
                .into_iter()
                .find(|c| c.family == family)
                .expect("candidate present");
            assert!(matches!(
                candidate.eligibility,
                Eligibility::PendingExactFormat { .. }
            ));
        }
    }

    #[test]
    fn production_weights_cannot_be_downloaded() {
        assert_eq!(
            refuse_production_weight_download(),
            Err(RuntimeError::ProductionDownloadForbidden)
        );
    }

    #[test]
    fn relative_worker_programs_are_rejected() {
        assert_eq!(
            reject_forbidden_program(Path::new("whisper-cli")),
            Err(RuntimeError::RelativeProgram)
        );
    }
}
