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

unsafe extern "C" {
    pub fn voisu_whisper_options_size() -> usize;
    pub fn voisu_whisper_route_logs_to_stderr();
    pub fn voisu_whisper_load(
        model_path: *const c_char,
        use_gpu: bool,
        gpu_device: i32,
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

    #[test]
    fn a_missing_model_fails_to_load_without_crashing() {
        unsafe { voisu_whisper_route_logs_to_stderr() };
        let ctx = unsafe { voisu_whisper_load(c"/nonexistent/model.bin".as_ptr(), false, 0) };
        assert!(ctx.is_null());
    }
}
