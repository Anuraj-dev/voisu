//! Hardware suggestions from the single-host development benchmark.

use super::hardware::{GpuProbe, HostHardware, NvidiaGpu};

pub const DISTIL_MODEL_ID: &str = "distil-large-v3.5-int8";
pub const PARAKEET_MODEL_ID: &str = "parakeet-tdt-0.6b-v3";
pub const DISTIL_MIN_TOTAL_VRAM_MIB: u64 = 3584;

// The CPU-quota warning refers to the core count the parakeet CPU row in
// docs/research/local-asr-benchmark-2026-10-12.md was measured with.
const BENCHMARK_WORKER_CORES: u32 = 4;

/// Prefix of the one-line engine-screen recommendation.
pub const RECOMMENDATION_PREFIX: &str = "Recommended for this computer:";

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CpuReason {
    NoNvidiaGpu,
    GpuTooSmall { name: String, total_vram_mib: u64 },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LocalSuggestion {
    Gpu {
        model_id: &'static str,
        gpu: NvidiaGpu,
        other_nvidia_gpus: usize,
    },
    Cpu {
        model_id: &'static str,
        reason: CpuReason,
    },
    GpuUnknown {
        error: String,
        cpu_fallback_model_id: &'static str,
    },
}

#[must_use]
pub fn suggest_local(hw: &HostHardware) -> LocalSuggestion {
    match &hw.gpu {
        GpuProbe::Nvidia(gpus) => {
            // The measured worker pins CUDA_VISIBLE_DEVICES=0 in PCI bus order.
            let Some(gpu) = gpus.iter().find(|gpu| gpu.index == 0) else {
                return LocalSuggestion::GpuUnknown {
                    error: "NVIDIA GPU 0 was not listed by the probe".into(),
                    cpu_fallback_model_id: PARAKEET_MODEL_ID,
                };
            };
            if gpu.total_vram_mib >= DISTIL_MIN_TOTAL_VRAM_MIB {
                LocalSuggestion::Gpu {
                    model_id: DISTIL_MODEL_ID,
                    gpu: gpu.clone(),
                    other_nvidia_gpus: gpus.len() - 1,
                }
            } else {
                LocalSuggestion::Cpu {
                    model_id: PARAKEET_MODEL_ID,
                    reason: CpuReason::GpuTooSmall {
                        name: gpu.name.clone(),
                        total_vram_mib: gpu.total_vram_mib,
                    },
                }
            }
        }
        GpuProbe::NoneFound => LocalSuggestion::Cpu {
            model_id: PARAKEET_MODEL_ID,
            reason: CpuReason::NoNvidiaGpu,
        },
        GpuProbe::Failed(error) => LocalSuggestion::GpuUnknown {
            error: error.clone(),
            cpu_fallback_model_id: PARAKEET_MODEL_ID,
        },
    }
}

#[must_use]
pub fn warnings(hw: &HostHardware) -> Vec<String> {
    let mut warnings = Vec::new();
    if hw.ram_bytes.is_none() {
        warnings.push("Host RAM could not be read.".into());
    }
    let quota = crate::local_worker::worker_cpu_quota_cores(hw.cpu_cores);
    if quota < BENCHMARK_WORKER_CORES {
        warnings.push(format!(
            "the Local worker gets {quota} CPU cores here, fewer than the {BENCHMARK_WORKER_CORES} it was tested with; CPU transcription may be slower"
        ));
    }
    if matches!(&hw.gpu, GpuProbe::Nvidia(gpus) if gpus.len() > 1) {
        warnings
            .push("Voisu uses GPU 0 (PCI bus order); mapping to other GPUs is unverified".into());
    }
    warnings
}

/// One line for the setup engine screen. Measured figures stay in
/// docs/research; the consent summary carries the language and GPU limits.
#[must_use]
pub fn suggestion_line(suggestion: &LocalSuggestion) -> String {
    let detail = match suggestion {
        LocalSuggestion::Gpu { model_id, gpu, .. } => format!("{model_id} ({})", gpu.name),
        LocalSuggestion::Cpu {
            model_id,
            reason: CpuReason::NoNvidiaGpu,
        } => format!("{model_id} on CPU (no NVIDIA GPU detected)"),
        LocalSuggestion::Cpu {
            model_id,
            reason:
                CpuReason::GpuTooSmall {
                    name,
                    total_vram_mib,
                },
        } => format!(
            "{model_id} on CPU ({name} has {total_vram_mib} MiB; {DISTIL_MIN_TOTAL_VRAM_MIB} MiB needed for the GPU model)"
        ),
        LocalSuggestion::GpuUnknown {
            error,
            cpu_fallback_model_id,
        } => format!("{cpu_fallback_model_id} on CPU (GPU status unknown: {error})"),
    };
    format!("{RECOMMENDATION_PREFIX} {detail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gpu(index: u32, total_vram_mib: u64) -> NvidiaGpu {
        NvidiaGpu {
            index,
            name: "RTX 5050 Laptop".into(),
            total_vram_mib,
            driver_version: "580.95".into(),
        }
    }

    fn host(gpu: GpuProbe) -> HostHardware {
        HostHardware {
            gpu,
            ram_bytes: Some(16 * 1024 * 1024 * 1024),
            cpu_cores: 16,
        }
    }

    #[test]
    fn gpu_zero_vram_controls_the_model_suggestion() {
        for total_vram_mib in [2048, 3583] {
            assert_eq!(
                suggest_local(&host(GpuProbe::Nvidia(vec![gpu(0, total_vram_mib)]))),
                LocalSuggestion::Cpu {
                    model_id: "parakeet-tdt-0.6b-v3",
                    reason: CpuReason::GpuTooSmall {
                        name: "RTX 5050 Laptop".into(),
                        total_vram_mib
                    },
                }
            );
        }
        for total_vram_mib in [3584, 3911, 4096, 8151] {
            assert_eq!(
                suggest_local(&host(GpuProbe::Nvidia(vec![gpu(0, total_vram_mib)]))),
                LocalSuggestion::Gpu {
                    model_id: "distil-large-v3.5-int8",
                    gpu: gpu(0, total_vram_mib),
                    other_nvidia_gpus: 0,
                }
            );
        }
    }

    #[test]
    fn absent_nvidia_uses_cpu_but_probe_failure_stays_unknown() {
        assert_eq!(
            suggest_local(&host(GpuProbe::NoneFound)),
            LocalSuggestion::Cpu {
                model_id: "parakeet-tdt-0.6b-v3",
                reason: CpuReason::NoNvidiaGpu,
            }
        );
        assert_eq!(
            suggest_local(&host(GpuProbe::Failed("driver failed".into()))),
            LocalSuggestion::GpuUnknown {
                error: "driver failed".into(),
                cpu_fallback_model_id: "parakeet-tdt-0.6b-v3",
            }
        );
    }

    #[test]
    fn a_larger_gpu_one_does_not_override_gpu_zero() {
        let hw = host(GpuProbe::Nvidia(vec![gpu(1, 24564), gpu(0, 2048)]));
        assert_eq!(
            suggest_local(&hw),
            LocalSuggestion::Cpu {
                model_id: "parakeet-tdt-0.6b-v3",
                reason: CpuReason::GpuTooSmall {
                    name: "RTX 5050 Laptop".into(),
                    total_vram_mib: 2048
                },
            }
        );
    }

    #[test]
    fn gpu_suggestion_counts_other_nvidia_gpus() {
        let hw = host(GpuProbe::Nvidia(vec![
            gpu(1, 24564),
            gpu(0, 8151),
            gpu(2, 4096),
        ]));
        assert_eq!(
            suggest_local(&hw),
            LocalSuggestion::Gpu {
                model_id: "distil-large-v3.5-int8",
                gpu: gpu(0, 8151),
                other_nvidia_gpus: 2,
            }
        );
    }

    #[test]
    fn missing_gpu_zero_is_unknown_rather_than_assuming_another_gpu() {
        for gpus in [vec![], vec![gpu(1, 24564)]] {
            assert!(matches!(
                suggest_local(&host(GpuProbe::Nvidia(gpus))),
                LocalSuggestion::GpuUnknown { .. }
            ));
        }
    }

    #[test]
    fn warnings_use_worker_quota_and_do_not_change_the_suggestion() {
        let mut hw = host(GpuProbe::Nvidia(vec![gpu(1, 24564), gpu(0, 2048)]));
        hw.ram_bytes = None;
        hw.cpu_cores = 4;
        let suggestion = suggest_local(&hw);
        assert_eq!(
            warnings(&hw),
            vec![
                "Host RAM could not be read.",
                "the Local worker gets 2 CPU cores here, fewer than the 4 it was tested with; CPU transcription may be slower",
                "Voisu uses GPU 0 (PCI bus order); mapping to other GPUs is unverified",
            ]
        );
        hw.ram_bytes = Some(1);
        hw.cpu_cores = 16;
        assert_eq!(
            warnings(&hw),
            vec!["Voisu uses GPU 0 (PCI bus order); mapping to other GPUs is unverified"]
        );
        assert_eq!(suggest_local(&hw), suggestion);
        hw.gpu = GpuProbe::NoneFound;
        assert!(warnings(&hw).is_empty());
    }

    #[test]
    fn ram_and_core_capacity_do_not_gate_the_gpu_suggestion() {
        for ram_bytes in [None, Some(1)] {
            let mut hw = host(GpuProbe::Nvidia(vec![gpu(0, 8151)]));
            hw.ram_bytes = ram_bytes;
            hw.cpu_cores = 1;
            assert!(matches!(suggest_local(&hw), LocalSuggestion::Gpu { .. }));
        }
    }

    #[test]
    fn suggestion_is_one_compact_line_without_benchmark_figures() {
        let cases = [
            (
                GpuProbe::Nvidia(vec![gpu(0, 8151)]),
                "Recommended for this computer: distil-large-v3.5-int8 (RTX 5050 Laptop)",
            ),
            (
                GpuProbe::Nvidia(vec![gpu(0, 2048)]),
                "Recommended for this computer: parakeet-tdt-0.6b-v3 on CPU (RTX 5050 Laptop has 2048 MiB; 3584 MiB needed for the GPU model)",
            ),
            (
                GpuProbe::NoneFound,
                "Recommended for this computer: parakeet-tdt-0.6b-v3 on CPU (no NVIDIA GPU detected)",
            ),
            (
                GpuProbe::Failed("driver failed".into()),
                "Recommended for this computer: parakeet-tdt-0.6b-v3 on CPU (GPU status unknown: driver failed)",
            ),
        ];
        for (probe, expected) in cases {
            let line = suggestion_line(&suggest_local(&host(probe)));
            assert_eq!(line, expected);
            for removed in ["WER", "Invented", "Hinglish", "Development evidence", "p50"] {
                assert!(!line.contains(removed), "{removed}: {line}");
            }
        }
    }
}
