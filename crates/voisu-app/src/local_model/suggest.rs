//! Hardware suggestions from the single-host development benchmark.

use super::hardware::{GpuProbe, HostHardware, NvidiaGpu};

pub const DISTIL_MODEL_ID: &str = "distil-large-v3.5-int8";
pub const PARAKEET_MODEL_ID: &str = "parakeet-tdt-0.6b-v3";
pub const DISTIL_MIN_TOTAL_VRAM_MIB: u64 = 3584;

// Rows from docs/research/local-asr-benchmark-2026-10-12.md, development evidence only.
const DISTIL_ENGLISH_WER_PERCENT: f64 = 9.0;
const DISTIL_HINGLISH_WER_PERCENT: f64 = 79.8;
const DISTIL_WARM_P50_MS: u32 = 292;
const DISTIL_COLD_LOAD_SECONDS: f64 = 4.7;
const DISTIL_PEAK_VRAM_GB: f64 = 1.09;
const DISTIL_SILENCE_INSERTED_WORDS: u32 = 18;
const PARAKEET_ENGLISH_WER_PERCENT: f64 = 12.8;
const PARAKEET_HINGLISH_WER_PERCENT: f64 = 80.8;
const PARAKEET_CPU_P50_SECONDS: f64 = 0.61;
const PARAKEET_CPU_P95_SECONDS: f64 = 1.2;
const PARAKEET_SILENCE_INSERTED_WORDS: u32 = 0;
const SILENT_CLIP_COUNT: u32 = 12;
const BENCHMARK_WORKER_CORES: u32 = 4;

const DEVELOPMENT_CAVEAT: &str =
    "Development evidence: one host, one speaker, scripted prompts — not the release gate.";
const HINGLISH_CAVEAT: &str = "Hinglish: no Local model in this suggestion handles it; use Cloud.";

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
        warnings.push(format!("the CPU figures were measured with {BENCHMARK_WORKER_CORES} worker cores; this host allots {quota}"));
    }
    if matches!(&hw.gpu, GpuProbe::Nvidia(gpus) if gpus.len() > 1) {
        warnings
            .push("Voisu uses GPU 0 (PCI bus order); mapping to other GPUs is unverified".into());
    }
    warnings
}

#[must_use]
pub fn suggestion_lines(suggestion: &LocalSuggestion) -> Vec<String> {
    let mut lines = vec!["Suggested for this hardware".into()];
    match suggestion {
        LocalSuggestion::Gpu { model_id, gpu, .. } => {
            lines.push(format!(
                "{model_id} on {} ({} MiB)",
                gpu.name, gpu.total_vram_mib
            ));
            lines.push(format!("English WER {DISTIL_ENGLISH_WER_PERCENT:.1}%; warm p50 {DISTIL_WARM_P50_MS} ms after a {DISTIL_COLD_LOAD_SECONDS:.1} s cold load; peak VRAM {DISTIL_PEAK_VRAM_GB:.2} GB."));
            lines.push(format!("Invented {DISTIL_SILENCE_INSERTED_WORDS} words across {SILENT_CLIP_COUNT} silent clips."));
            lines.push(format!(
                "English only — not for Hinglish ({DISTIL_HINGLISH_WER_PERCENT:.1}% WER)."
            ));
        }
        LocalSuggestion::Cpu { model_id, reason } => {
            lines.push(format!("{model_id} on CPU"));
            lines.push(match reason {
                CpuReason::NoNvidiaGpu => "No NVIDIA GPU detected; no NVIDIA GPU usable by the measured CUDA runtime.".into(),
                CpuReason::GpuTooSmall { name, total_vram_mib } => format!("GPU 0 has {total_vram_mib} MiB, below {DISTIL_MIN_TOTAL_VRAM_MIB} MiB ({name})."),
            });
            lines.extend(cpu_evidence_lines());
        }
        LocalSuggestion::GpuUnknown {
            error,
            cpu_fallback_model_id,
        } => {
            lines.push(format!("GPU status unknown: {error}"));
            lines.push(format!(
                "If this host has no NVIDIA GPU: {cpu_fallback_model_id} on CPU"
            ));
            lines.extend(cpu_evidence_lines());
        }
    }
    lines.push(DEVELOPMENT_CAVEAT.into());
    lines.push(HINGLISH_CAVEAT.into());
    lines
}

fn cpu_evidence_lines() -> [String; 3] {
    [
        format!(
            "English WER {PARAKEET_ENGLISH_WER_PERCENT:.1}%; p50 {PARAKEET_CPU_P50_SECONDS:.2} s, p95 {PARAKEET_CPU_P95_SECONDS:.1} s on {BENCHMARK_WORKER_CORES} cores."
        ),
        format!(
            "No invented words on silence ({PARAKEET_SILENCE_INSERTED_WORDS} across {SILENT_CLIP_COUNT} silent clips)."
        ),
        format!("English only ({PARAKEET_HINGLISH_WER_PERCENT:.1}% Hinglish WER)."),
    ]
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
                "the CPU figures were measured with 4 worker cores; this host allots 2",
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
    fn benchmark_constants_pin_the_measured_rows() {
        assert_eq!(DISTIL_ENGLISH_WER_PERCENT, 9.0);
        assert_eq!(DISTIL_HINGLISH_WER_PERCENT, 79.8);
        assert_eq!(DISTIL_WARM_P50_MS, 292);
        assert_eq!(DISTIL_COLD_LOAD_SECONDS, 4.7);
        assert_eq!(DISTIL_PEAK_VRAM_GB, 1.09);
        assert_eq!(DISTIL_SILENCE_INSERTED_WORDS, 18);
        assert_eq!(PARAKEET_ENGLISH_WER_PERCENT, 12.8);
        assert_eq!(PARAKEET_HINGLISH_WER_PERCENT, 80.8);
        assert_eq!(PARAKEET_CPU_P50_SECONDS, 0.61);
        assert_eq!(PARAKEET_CPU_P95_SECONDS, 1.2);
        assert_eq!(PARAKEET_SILENCE_INSERTED_WORDS, 0);
        assert_eq!(SILENT_CLIP_COUNT, 12);
        assert_eq!(BENCHMARK_WORKER_CORES, 4);
        assert_eq!(DISTIL_MIN_TOTAL_VRAM_MIB, 3584);
    }

    #[test]
    fn suggestion_text_preserves_evidence_and_language_limits() {
        for suggestion in [
            suggest_local(&host(GpuProbe::Nvidia(vec![gpu(0, 8151)]))),
            suggest_local(&host(GpuProbe::Nvidia(vec![gpu(0, 2048)]))),
            suggest_local(&host(GpuProbe::NoneFound)),
            suggest_local(&host(GpuProbe::Failed("driver failed".into()))),
        ] {
            let lines = suggestion_lines(&suggestion);
            let text = lines.join("\n");
            assert!(text.contains("Suggested for this hardware"));
            assert!(!text.contains("Recommended"));
            assert_eq!(
                lines[lines.len() - 2],
                "Development evidence: one host, one speaker, scripted prompts — not the release gate."
            );
            assert_eq!(
                lines.last().map(String::as_str),
                Some("Hinglish: no Local model in this suggestion handles it; use Cloud.")
            );
            match &suggestion {
                LocalSuggestion::Gpu { .. } => {
                    assert!(text.contains("distil-large-v3.5-int8 on RTX 5050 Laptop (8151 MiB)"));
                    assert!(text.contains("English WER 9.0%; warm p50 292 ms after a 4.7 s cold load; peak VRAM 1.09 GB"));
                    assert!(text.contains("Invented 18 words across 12 silent clips"));
                    assert!(text.contains("English only — not for Hinglish (79.8% WER)"));
                }
                LocalSuggestion::Cpu { .. } | LocalSuggestion::GpuUnknown { .. } => {
                    assert!(text.contains("parakeet-tdt-0.6b-v3 on CPU"));
                    assert!(text.contains("English WER 12.8%; p50 0.61 s, p95 1.2 s on 4 cores"));
                    assert!(text.contains("No invented words on silence"));
                    assert!(text.contains("English only (80.8% Hinglish WER)"));
                }
            }
            match &suggestion {
                LocalSuggestion::Cpu {
                    reason: CpuReason::NoNvidiaGpu,
                    ..
                } => {
                    assert!(text.contains("No NVIDIA GPU detected"));
                    assert!(text.contains("no NVIDIA GPU usable by the measured CUDA runtime"));
                    assert!(!text.to_lowercase().contains("no gpu"));
                }
                LocalSuggestion::Cpu {
                    reason: CpuReason::GpuTooSmall { .. },
                    ..
                } => {
                    assert!(text.contains("GPU 0 has 2048 MiB, below 3584 MiB"));
                }
                LocalSuggestion::GpuUnknown { .. } => {
                    assert!(text.contains("GPU status unknown: driver failed"));
                    assert!(
                        text.contains(
                            "If this host has no NVIDIA GPU: parakeet-tdt-0.6b-v3 on CPU"
                        )
                    );
                }
                LocalSuggestion::Gpu { .. } => {}
            }
        }
    }
}
