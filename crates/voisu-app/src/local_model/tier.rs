//! Local model tiers: the two whisper.cpp models users choose between by
//! name. Catalog ids appear only in `voisu doctor` and logs.

use super::consented_catalog::{
    ConsentedCatalogEntry, WHISPER_CPP_ENTRY, WHISPER_CPP_SMALL_EN_ENTRY,
};
use crate::config::{LocalDevice, LocalModel};

/// The GPU hint beside the Accurate choice.
pub const ACCURATE_ON_GPU_HINT: &str = "Accurate recommended on your GPU";
/// The warning when Accurate is chosen where it runs on the CPU.
pub const ACCURATE_ON_CPU_WARNING: &str =
    "Accurate is much slower on the CPU; Fast is recommended without a GPU.";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LocalTier {
    /// distil-large-v3.5: the most accurate, best on a GPU.
    Accurate,
    /// small.en: about a third of the size, for the CPU.
    Fast,
}

impl LocalTier {
    pub const ALL: [Self; 2] = [Self::Accurate, Self::Fast];

    /// What `setting` runs where the model runs on a GPU (`on_gpu`) or the
    /// CPU. `auto` picks Accurate on a GPU and Fast on the CPU.
    #[must_use]
    pub fn resolve(setting: LocalModel, on_gpu: bool) -> Self {
        match setting {
            LocalModel::Accurate => Self::Accurate,
            LocalModel::Fast => Self::Fast,
            LocalModel::Auto if on_gpu => Self::Accurate,
            LocalModel::Auto => Self::Fast,
        }
    }

    /// The tier whose catalog entry is `entry_id`, if any.
    #[must_use]
    pub fn of_entry_id(entry_id: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|tier| tier.entry().id == entry_id)
    }

    #[must_use]
    pub fn entry(self) -> &'static ConsentedCatalogEntry {
        match self {
            Self::Accurate => &WHISPER_CPP_ENTRY,
            Self::Fast => &WHISPER_CPP_SMALL_EN_ENTRY,
        }
    }

    /// The name users see.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Accurate => "Accurate",
            Self::Fast => "Fast",
        }
    }

    /// The model behind the name, without quantization or runtime.
    #[must_use]
    pub fn model_name(self) -> &'static str {
        match self {
            Self::Accurate => "distil-large-v3.5",
            Self::Fast => "small.en",
        }
    }

    /// The setting that always picks this tier.
    #[must_use]
    pub fn setting(self) -> LocalModel {
        match self {
            Self::Accurate => LocalModel::Accurate,
            Self::Fast => LocalModel::Fast,
        }
    }

    /// `Accurate (distil-large-v3.5)`.
    #[must_use]
    pub fn title(self) -> String {
        format!("{} ({})", self.name(), self.model_name())
    }

    /// `Accurate (distil-large-v3.5, GPU)`.
    #[must_use]
    pub fn label(self, on_gpu: bool) -> String {
        format!(
            "{} ({}, {})",
            self.name(),
            self.model_name(),
            if on_gpu { "GPU" } else { "CPU" }
        )
    }
}

/// Whether the whisper.cpp worker runs on a GPU under `device`: `auto` and
/// `gpu` both ask it for one and never fall back to the CPU. Setup sets
/// `cpu` when it found no GPU, so after setup this is where Local runs.
#[must_use]
pub fn runs_on_gpu(device: LocalDevice) -> bool {
    device.manifest_device() == "gpu"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_picks_accurate_on_a_gpu_and_fast_on_the_cpu() {
        assert_eq!(
            LocalTier::resolve(LocalModel::Auto, true),
            LocalTier::Accurate
        );
        assert_eq!(LocalTier::resolve(LocalModel::Auto, false), LocalTier::Fast);
        for on_gpu in [true, false] {
            assert_eq!(
                LocalTier::resolve(LocalModel::Accurate, on_gpu),
                LocalTier::Accurate
            );
            assert_eq!(
                LocalTier::resolve(LocalModel::Fast, on_gpu),
                LocalTier::Fast
            );
        }
        // `local-device cpu` is the CPU; auto and gpu are a GPU.
        assert!(runs_on_gpu(LocalDevice::Auto) && runs_on_gpu(LocalDevice::Gpu));
        assert!(!runs_on_gpu(LocalDevice::Cpu));
    }

    #[test]
    fn tiers_map_to_their_catalog_entries_and_friendly_labels() {
        assert_eq!(
            LocalTier::of_entry_id("whisper-cpp-distil-large-v3.5-q8-0"),
            Some(LocalTier::Accurate)
        );
        assert_eq!(
            LocalTier::of_entry_id("whisper-cpp-small-en-q8-0"),
            Some(LocalTier::Fast)
        );
        assert_eq!(LocalTier::of_entry_id("parakeet-tdt-0.6b-v3"), None);
        assert_eq!(
            LocalTier::Accurate.label(true),
            "Accurate (distil-large-v3.5, GPU)"
        );
        assert_eq!(LocalTier::Fast.label(false), "Fast (small.en, CPU)");
        assert_eq!(LocalTier::Fast.title(), "Fast (small.en)");
        for tier in LocalTier::ALL {
            assert!(tier.entry().consent_installable && tier.entry().is_hosted());
            assert_eq!(LocalTier::resolve(tier.setting(), false), tier);
        }
    }
}
