//! The two whisper.cpp Local model tiers users choose between by name, the
//! `auto` / `accurate` / `fast` setting that picks one, and the pinned model
//! file behind each. Linux and Windows both read them from here, so the two
//! platforms resolve `auto` the same way and download the same bytes.

/// The GPU hint beside the Accurate choice.
pub const ACCURATE_ON_GPU_HINT: &str = "Accurate recommended on your GPU";
/// The warning when Accurate is chosen where it runs on the CPU.
pub const ACCURATE_ON_CPU_WARNING: &str =
    "Accurate is much slower on the CPU; Fast is recommended without a GPU.";

/// Which Local model tier the user asked for. `auto` picks Accurate on a GPU
/// and Fast on the CPU; see [`LocalTier::resolve`].
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum LocalModel {
    #[default]
    Auto,
    Accurate,
    Fast,
}

impl LocalModel {
    pub const ALL: [Self; 3] = [Self::Auto, Self::Accurate, Self::Fast];

    /// The config value and CLI word.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Accurate => "accurate",
            Self::Fast => "fast",
        }
    }

    /// `auto`, `accurate` or `fast`, in any case and with spaces around.
    #[must_use]
    pub fn from_value(value: &str) -> Option<Self> {
        let value = value.trim().to_ascii_lowercase();
        Self::ALL.into_iter().find(|model| model.as_str() == value)
    }
}

/// One pinned model file: where it comes from and what it must be.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ModelFile {
    pub name: &'static str,
    pub url: &'static str,
    /// Lowercase hex SHA-256.
    pub sha256: &'static str,
    pub bytes: u64,
}

/// distil-large-v3.5 at q8_0, converted for whisper.cpp and published at a
/// pinned revision.
const ACCURATE_MODEL: ModelFile = ModelFile {
    name: "ggml-distil-large-v3.5-q8_0.bin",
    url: "https://huggingface.co/Anurajjit/voisu-distil-large-v3.5-ggml/resolve/33b03a8479479b1ad96d4b7b51ebdeecbd1d9b71/ggml-distil-large-v3.5-q8_0.bin",
    sha256: "7e570abdf13b681354a2ecc93802e25bf204dd6f8c0dd9f6ecb9478b71b231d7",
    bytes: 818_305_955,
};

/// small.en at q8_0 from whisper.cpp's own model repository.
const FAST_MODEL: ModelFile = ModelFile {
    name: "ggml-small.en-q8_0.bin",
    url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/5359861c739e955e79d9a303bcbc70fb988958b1/ggml-small.en-q8_0.bin",
    sha256: "67a179f608ea6114bd3fdb9060e762b588a3fb3bd00c4387971be4d177958067",
    bytes: 264_477_561,
};

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

    /// The pinned model file.
    #[must_use]
    pub const fn model(self) -> ModelFile {
        match self {
            Self::Accurate => ACCURATE_MODEL,
            Self::Fast => FAST_MODEL,
        }
    }

    /// The catalog id the worker reports back as its `model_id`.
    #[must_use]
    pub const fn model_id(self) -> &'static str {
        match self {
            Self::Accurate => "whisper-cpp-distil-large-v3.5-q8-0",
            Self::Fast => "whisper-cpp-small-en-q8-0",
        }
    }

    /// The GPU memory the worker asks to have free before it loads this
    /// tier there: an estimate for q8_0 with beam 5 and flash attention, not
    /// a measured peak.
    #[must_use]
    pub const fn vram_bytes(self) -> u64 {
        match self {
            Self::Accurate => 1536 * 1024 * 1024,
            Self::Fast => 768 * 1024 * 1024,
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

    /// The warning to show beside this tier where it runs on the CPU, or the
    /// hint where it runs on a GPU; `None` when there is nothing to say.
    #[must_use]
    pub fn device_note(self, on_gpu: bool) -> Option<&'static str> {
        match (self, on_gpu) {
            (Self::Accurate, true) => Some(ACCURATE_ON_GPU_HINT),
            (Self::Accurate, false) => Some(ACCURATE_ON_CPU_WARNING),
            (Self::Fast, _) => None,
        }
    }
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
        for tier in LocalTier::ALL {
            assert_eq!(LocalTier::resolve(tier.setting(), false), tier);
        }
    }

    #[test]
    fn settings_parse_leniently_and_round_trip() {
        for model in LocalModel::ALL {
            assert_eq!(LocalModel::from_value(model.as_str()), Some(model));
        }
        assert_eq!(
            LocalModel::from_value(" Accurate "),
            Some(LocalModel::Accurate)
        );
        assert_eq!(LocalModel::from_value("large"), None);
        assert_eq!(LocalModel::default(), LocalModel::Auto);
    }

    #[test]
    fn tiers_name_their_pinned_models_and_device_notes() {
        assert_eq!(
            LocalTier::Accurate.label(true),
            "Accurate (distil-large-v3.5, GPU)"
        );
        assert_eq!(LocalTier::Fast.label(false), "Fast (small.en, CPU)");
        assert_eq!(LocalTier::Fast.title(), "Fast (small.en)");
        assert_eq!(LocalTier::Accurate.model().bytes, 818_305_955);
        assert_eq!(LocalTier::Fast.model().bytes, 264_477_561);
        for tier in LocalTier::ALL {
            let model = tier.model();
            assert!(model.url.starts_with("https://huggingface.co/"));
            assert!(model.url.ends_with(model.name));
            assert_eq!(model.sha256.len(), 64);
        }
        assert_eq!(
            LocalTier::Accurate.device_note(true),
            Some(ACCURATE_ON_GPU_HINT)
        );
        assert_eq!(
            LocalTier::Accurate.device_note(false),
            Some(ACCURATE_ON_CPU_WARNING)
        );
        assert_eq!(LocalTier::Fast.device_note(false), None);
    }
}
