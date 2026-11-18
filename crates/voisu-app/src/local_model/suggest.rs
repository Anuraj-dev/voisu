//! Setup's Local suggestion: Accurate on a GPU, Fast on the CPU.
//!
//! Which tier runs follows where the model runs. That is the
//! `local_device` setting and the worker's own GPU probe at install, not this
//! hardware probe, which sees NVIDIA GPUs only.

use super::consented_catalog::WHISPER_CPP_ENTRY;
use super::hardware::{GpuProbe, HostHardware};
use super::tier::LocalTier;

// The CPU-quota warning refers to the core count the Local CPU row in
// docs/research/local-asr-benchmark-2026-10-12.md was measured with.
const BENCHMARK_WORKER_CORES: u32 = 4;

/// Prefix of the one-line engine-screen recommendation.
pub const RECOMMENDATION_PREFIX: &str = "Recommended for this computer:";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalSuggestion {
    pub model_id: &'static str,
    /// The first NVIDIA GPU the hardware probe listed, shown as a hint only.
    pub nvidia_gpu: Option<String>,
}

#[must_use]
pub fn suggest_local(hw: &HostHardware) -> LocalSuggestion {
    let nvidia_gpu = match &hw.gpu {
        GpuProbe::Nvidia(gpus) => gpus
            .iter()
            .min_by_key(|gpu| gpu.index)
            .map(|gpu| gpu.name.clone()),
        GpuProbe::NoneFound | GpuProbe::Failed(_) => None,
    };
    LocalSuggestion {
        model_id: WHISPER_CPP_ENTRY.id,
        nvidia_gpu,
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
    warnings
}

/// One line for the setup engine screen. The consent summary says where the
/// model will actually run.
#[must_use]
pub fn suggestion_line(suggestion: &LocalSuggestion) -> String {
    let accurate = LocalTier::Accurate.title();
    match &suggestion.nvidia_gpu {
        Some(name) => format!("{RECOMMENDATION_PREFIX} {accurate} on your GPU ({name})"),
        None => format!(
            "{RECOMMENDATION_PREFIX} {accurate} on a GPU, or {} on the CPU without one",
            LocalTier::Fast.title()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local_model::NvidiaGpu;

    fn gpu(index: u32, name: &str) -> NvidiaGpu {
        NvidiaGpu {
            index,
            name: name.into(),
            total_vram_mib: 2048,
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
    fn every_host_is_offered_the_whisper_cpp_model() {
        for probe in [
            GpuProbe::Nvidia(vec![gpu(0, "RTX 5050 Laptop")]),
            GpuProbe::Nvidia(vec![]),
            GpuProbe::NoneFound,
            GpuProbe::Failed("driver failed".into()),
        ] {
            assert_eq!(
                suggest_local(&host(probe)).model_id,
                "whisper-cpp-distil-large-v3.5-q8-0"
            );
        }
    }

    #[test]
    fn the_line_names_the_first_nvidia_gpu_or_says_where_it_runs() {
        let line = |probe| suggestion_line(&suggest_local(&host(probe)));
        assert_eq!(
            line(GpuProbe::Nvidia(vec![
                gpu(1, "RTX 4090"),
                gpu(0, "RTX 5050 Laptop")
            ])),
            "Recommended for this computer: Accurate (distil-large-v3.5) on your GPU (RTX 5050 Laptop)"
        );
        for probe in [
            GpuProbe::NoneFound,
            GpuProbe::Failed("driver failed".into()),
        ] {
            assert_eq!(
                line(probe),
                "Recommended for this computer: Accurate (distil-large-v3.5) on a GPU, or Fast (small.en) on the CPU without one"
            );
        }
    }

    #[test]
    fn warnings_use_worker_quota_and_do_not_change_the_suggestion() {
        let mut hw = host(GpuProbe::Nvidia(vec![gpu(1, "a"), gpu(0, "b")]));
        hw.ram_bytes = None;
        hw.cpu_cores = 4;
        let suggestion = suggest_local(&hw);
        assert_eq!(
            warnings(&hw),
            vec![
                "Host RAM could not be read.",
                "the Local worker gets 2 CPU cores here, fewer than the 4 it was tested with; CPU transcription may be slower",
            ]
        );
        hw.ram_bytes = Some(1);
        hw.cpu_cores = 16;
        assert!(warnings(&hw).is_empty());
        assert_eq!(suggest_local(&hw), suggestion);
    }
}
