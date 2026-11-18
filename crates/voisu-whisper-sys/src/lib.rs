//! Raw bindings to the vendored whisper.cpp (tag in `whisper.cpp/VENDORED`)
//! through `shim.c`. Decoding parameters are set in C; Rust only passes
//! [`VoisuWhisperOptions`]. The safe wrapper lives in `voisu-local-asr`.

use std::ffi::{c_char, c_int, c_void};

/// The whisper.cpp release this crate vendors.
pub const WHISPER_CPP_VERSION: &str = "1.9.5";

/// Opaque `struct whisper_context`.
#[repr(C)]
pub struct WhisperContext {
    _private: [u8; 0],
}

/// `ggml_abort_callback`: return true to stop the running computation.
pub type AbortCallback = Option<unsafe extern "C" fn(data: *mut c_void) -> bool>;

/// Mirror of `struct voisu_whisper_options` in `shim.c`.
#[repr(C)]
#[derive(Debug)]
pub struct VoisuWhisperOptions {
    pub n_threads: i32,
    /// 1 is greedy decoding; above 1 is beam search of that width.
    pub beam_size: i32,
    pub no_speech_thold: f32,
    pub language: *const c_char,
    /// Null for no prompt.
    pub initial_prompt: *const c_char,
    pub abort_callback: AbortCallback,
    pub abort_callback_user_data: *mut c_void,
}

/// Mirror of `struct voisu_gpu` in `shim.c`.
#[repr(C)]
#[derive(Debug)]
pub struct VoisuGpu {
    /// The `gpu_device` index whisper.cpp counts over GPU and iGPU devices.
    pub index: i32,
    pub discrete: bool,
    pub free_bytes: usize,
    pub total_bytes: usize,
    pub name: [c_char; 128],
    pub description: [c_char; 256],
}

impl Default for VoisuGpu {
    fn default() -> Self {
        Self {
            index: 0,
            discrete: false,
            free_bytes: 0,
            total_bytes: 0,
            name: [0; 128],
            description: [0; 256],
        }
    }
}

unsafe extern "C" {
    pub fn voisu_whisper_options_size() -> usize;
    pub fn voisu_gpu_size() -> usize;
    pub fn voisu_whisper_route_logs_to_stderr();
    /// Loads the ggml backend modules from `dir` (NUL-terminated); returns
    /// the number of registered devices. Use [`load_backends`], which calls it
    /// once.
    pub fn voisu_whisper_load_backends(dir: *const c_char) -> usize;
    pub fn voisu_whisper_cpu_backend_loaded() -> bool;
    /// The first discrete GPU, else the first integrated one.
    pub fn voisu_whisper_pick_gpu(out: *mut VoisuGpu) -> bool;
    pub fn voisu_whisper_load(
        model_path: *const c_char,
        use_gpu: bool,
        gpu_device: i32,
        flash_attn: bool,
    ) -> *mut WhisperContext;
    pub fn voisu_whisper_transcribe(
        ctx: *mut WhisperContext,
        options: *const VoisuWhisperOptions,
        samples: *const f32,
        n_samples: i32,
    ) -> i32;
    pub fn whisper_full_n_segments(ctx: *mut WhisperContext) -> c_int;
    pub fn whisper_full_get_segment_text(
        ctx: *mut WhisperContext,
        i_segment: c_int,
    ) -> *const c_char;
    pub fn whisper_free(ctx: *mut WhisperContext);
    pub fn whisper_version() -> *const c_char;
    pub fn whisper_print_system_info() -> *const c_char;
}

/// Load the ggml backend modules from `dir` once per process and return the
/// number of registered devices. ggml's registry takes no lock, so two
/// threads loading at once corrupt it; every later call returns the first
/// call's count and ignores its `dir`.
pub fn load_backends(dir: &std::ffi::CStr) -> usize {
    static LOADED: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    // SAFETY: `dir` is NUL-terminated for the call; ggml copies what it keeps.
    *LOADED.get_or_init(|| unsafe { voisu_whisper_load_backends(dir.as_ptr()) })
}

/// The CPU floor for decoding on the CPU device: x86-64-v3 (Haswell / Zen 1),
/// ggml's `haswell` variant. ggml is built with every CPU variant down to
/// plain x86-64 (`x64`), so a GPU device runs on any x86-64 CPU; only the CPU
/// device refuses below this floor, because the Phase 0 speed (RTF 0.5) was
/// measured at this level and the older variants are several times slower.
#[cfg(target_arch = "x86_64")]
pub const REQUIRED_CPU_FEATURES: &[&str] = &["sse4.2", "avx", "avx2", "fma", "f16c", "bmi2"];

/// Other architectures build with ggml's portable defaults.
#[cfg(not(target_arch = "x86_64"))]
pub const REQUIRED_CPU_FEATURES: &[&str] = &[];

/// The [`REQUIRED_CPU_FEATURES`] this CPU lacks.
#[must_use]
pub fn missing_cpu_features() -> Vec<&'static str> {
    #[cfg(target_arch = "x86_64")]
    let detected = [
        ("sse4.2", std::arch::is_x86_feature_detected!("sse4.2")),
        ("avx", std::arch::is_x86_feature_detected!("avx")),
        ("avx2", std::arch::is_x86_feature_detected!("avx2")),
        ("fma", std::arch::is_x86_feature_detected!("fma")),
        ("f16c", std::arch::is_x86_feature_detected!("f16c")),
        ("bmi2", std::arch::is_x86_feature_detected!("bmi2")),
    ];
    #[cfg(not(target_arch = "x86_64"))]
    let detected: [(&'static str, bool); 0] = [];
    detected
        .into_iter()
        .filter(|(_, present)| !present)
        .map(|(name, _)| name)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CStr;

    #[test]
    fn rust_options_match_the_shim_layout() {
        assert_eq!(
            unsafe { voisu_whisper_options_size() },
            std::mem::size_of::<VoisuWhisperOptions>()
        );
    }

    #[test]
    fn linked_library_is_the_vendored_tag() {
        let version = unsafe { CStr::from_ptr(whisper_version()) };
        assert_eq!(version.to_str().unwrap(), WHISPER_CPP_VERSION);
    }

    fn backend_dir() -> std::path::PathBuf {
        // build.rs copies the backend modules next to every test binary.
        std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf()
    }

    fn load_backends() -> usize {
        let dir = std::ffi::CString::new(backend_dir().to_str().unwrap()).unwrap();
        super::load_backends(&dir)
    }

    #[test]
    fn rust_gpu_matches_the_shim_layout() {
        assert_eq!(unsafe { voisu_gpu_size() }, std::mem::size_of::<VoisuGpu>());
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn every_cpu_variant_is_built_and_one_loads_on_this_host() {
        let dir = backend_dir();
        for variant in ["x64", "sse42", "haswell", "skylakex", "alderlake"] {
            let module = dir.join(format!("libggml-cpu-{variant}.so"));
            assert!(module.is_file(), "{} missing", module.display());
        }
        assert!(load_backends() >= 1);
        assert!(unsafe { voisu_whisper_cpu_backend_loaded() });
        // ggml loaded the best variant this CPU runs, not the build baseline.
        let info = unsafe { CStr::from_ptr(whisper_print_system_info()) }
            .to_str()
            .unwrap()
            .to_owned();
        if missing_cpu_features().is_empty() {
            assert!(info.contains("AVX2 = 1"), "{info}");
        }
    }

    #[test]
    fn missing_features_are_named_from_the_baseline_list() {
        let missing = missing_cpu_features();
        assert!(
            missing
                .iter()
                .all(|name| REQUIRED_CPU_FEATURES.contains(name))
        );
    }

    #[test]
    fn a_missing_model_fails_to_load_without_crashing() {
        unsafe { voisu_whisper_route_logs_to_stderr() };
        load_backends();
        let ctx = unsafe { voisu_whisper_load(c"/nonexistent/model.bin".as_ptr(), false, 0, true) };
        assert!(ctx.is_null());
    }
}
