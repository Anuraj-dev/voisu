//! Local model tiers: the two whisper.cpp models users choose between by
//! name. Catalog ids appear only in `voisu doctor` and logs.

use super::consented_catalog::{
    ConsentedCatalogEntry, WHISPER_CPP_ENTRY, WHISPER_CPP_SMALL_EN_ENTRY,
};
use crate::config::LocalDevice;
#[cfg(test)]
use crate::config::LocalModel;
pub use voisu_core::local_tier::{ACCURATE_ON_CPU_WARNING, ACCURATE_ON_GPU_HINT, LocalTier};

/// A tier's consented catalog entry. The tiers themselves live in
/// `voisu_core` so Windows resolves and pins them the same way.
pub trait TierEntry {
    fn entry(self) -> &'static ConsentedCatalogEntry;
}

impl TierEntry for LocalTier {
    fn entry(self) -> &'static ConsentedCatalogEntry {
        match self {
            Self::Accurate => &WHISPER_CPP_ENTRY,
            Self::Fast => &WHISPER_CPP_SMALL_EN_ENTRY,
        }
    }
}

/// The tier whose catalog entry is `entry_id`, if any.
#[must_use]
pub fn tier_of_entry_id(entry_id: &str) -> Option<LocalTier> {
    LocalTier::ALL
        .into_iter()
        .find(|tier| tier.entry().id == entry_id)
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
            tier_of_entry_id("whisper-cpp-distil-large-v3.5-q8-0"),
            Some(LocalTier::Accurate)
        );
        assert_eq!(
            tier_of_entry_id("whisper-cpp-small-en-q8-0"),
            Some(LocalTier::Fast)
        );
        assert_eq!(tier_of_entry_id("parakeet-tdt-0.6b-v3"), None);
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
