//! The speech-to-text engine seam and its whisper.cpp implementation.

use std::ffi::{CStr, CString, c_void};
use std::path::Path;
use std::ptr::NonNull;
use std::sync::Once;
use std::sync::atomic::{AtomicBool, Ordering};

use voisu_whisper_sys as sys;

/// An engine failure, reported to the daemon as an Error frame.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EngineError {
    pub code: &'static str,
    pub message: String,
}

impl EngineError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// Transcribes one window at a time. The server owns windowing, the prompt
/// echo filter and the join, so an engine sees no state from other windows.
pub trait Engine {
    /// Transcribe one window of 16 kHz mono samples in `[-1, 1)`.
    ///
    /// `abort` turns true when the request is cancelled; a long-running
    /// engine should stop as soon as it can and return any error.
    ///
    /// # Errors
    ///
    /// The window failed; the whole request fails with it.
    fn transcribe(
        &mut self,
        samples: &[f32],
        prompt: Option<&str>,
        abort: &AtomicBool,
    ) -> Result<String, EngineError>;
}

/// Decoding settings from the manifest. Everything else is fixed in `shim.c`
/// (temperature 0 without fallback, no cross-window context, `suppress_nst`,
/// flash attention off).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DecodeOptions {
    pub n_threads: i32,
    /// 1 decodes greedily; above 1 is beam search of that width.
    pub beam_size: i32,
}

/// Probability above which whisper.cpp treats a window as no speech.
const NO_SPEECH_THOLD: f32 = 0.6;

/// One loaded whisper.cpp model on the CPU.
pub struct WhisperCpp {
    ctx: NonNull<sys::WhisperContext>,
    options: DecodeOptions,
}

static ROUTE_LOGS: Once = Once::new();

impl WhisperCpp {
    /// Load a GGML model file. ggml and whisper.cpp logs go to stderr.
    ///
    /// # Errors
    ///
    /// `cpu_unsupported` when the CPU lacks the instruction sets ggml was
    /// built for (checked before any ggml code runs), and `model_load_failed`
    /// when the path is unusable or whisper.cpp refuses it.
    pub fn load(model_path: &Path, options: DecodeOptions) -> Result<Self, EngineError> {
        cpu_support(&sys::missing_cpu_features())?;
        ROUTE_LOGS.call_once(|| unsafe { sys::voisu_whisper_route_logs_to_stderr() });
        let path = path_cstring(model_path)
            .ok_or_else(|| EngineError::new("model_load_failed", "model path is not a C string"))?;
        // SAFETY: `path` is a valid NUL-terminated string for the call.
        let ctx = unsafe { sys::voisu_whisper_load(path.as_ptr(), false, 0) };
        let ctx = NonNull::new(ctx).ok_or_else(|| {
            EngineError::new("model_load_failed", "whisper.cpp could not load the model")
        })?;
        Ok(Self { ctx, options })
    }
}

impl Drop for WhisperCpp {
    fn drop(&mut self) {
        // SAFETY: `ctx` came from `voisu_whisper_load` and is freed once.
        unsafe { sys::whisper_free(self.ctx.as_ptr()) };
    }
}

unsafe extern "C" fn abort_requested(data: *mut c_void) -> bool {
    // SAFETY: `data` is the `&AtomicBool` passed for this `transcribe` call,
    // which outlives the call.
    unsafe { (*data.cast::<AtomicBool>()).load(Ordering::Relaxed) }
}

impl Engine for WhisperCpp {
    fn transcribe(
        &mut self,
        samples: &[f32],
        prompt: Option<&str>,
        abort: &AtomicBool,
    ) -> Result<String, EngineError> {
        let n_samples = i32::try_from(samples.len())
            .map_err(|_| EngineError::new("inference_failed", "window is too long"))?;
        let prompt = prompt
            .map(CString::new)
            .transpose()
            .map_err(|_| EngineError::new("invalid_request", "prompt contains NUL"))?;
        let options = sys::VoisuWhisperOptions {
            n_threads: self.options.n_threads,
            beam_size: self.options.beam_size,
            no_speech_thold: NO_SPEECH_THOLD,
            language: c"en".as_ptr(),
            initial_prompt: prompt
                .as_ref()
                .map_or(std::ptr::null(), |prompt| prompt.as_ptr()),
            abort_callback: Some(abort_requested),
            abort_callback_user_data: std::ptr::from_ref(abort).cast_mut().cast(),
        };
        // SAFETY: `ctx` is live; `options`, its strings, `abort` and `samples`
        // all outlive this synchronous call.
        let status = unsafe {
            sys::voisu_whisper_transcribe(self.ctx.as_ptr(), &options, samples.as_ptr(), n_samples)
        };
        if status != 0 {
            return Err(EngineError::new(
                "inference_failed",
                format!("whisper.cpp inference failed ({status})"),
            ));
        }
        // SAFETY: `ctx` is live and holds the result of the call above.
        let segments = unsafe { sys::whisper_full_n_segments(self.ctx.as_ptr()) };
        let mut texts = Vec::new();
        for index in 0..segments {
            // SAFETY: `index` is in range; the string lives until the next run.
            let text = unsafe { sys::whisper_full_get_segment_text(self.ctx.as_ptr(), index) };
            if text.is_null() {
                continue;
            }
            let text = unsafe { CStr::from_ptr(text) }.to_string_lossy();
            let text = text.trim();
            if !text.is_empty() {
                texts.push(text.to_owned());
            }
        }
        Ok(texts.join(" "))
    }
}

#[cfg(unix)]
fn path_cstring(path: &Path) -> Option<CString> {
    use std::os::unix::ffi::OsStrExt;
    CString::new(path.as_os_str().as_bytes()).ok()
}

#[cfg(not(unix))]
fn path_cstring(path: &Path) -> Option<CString> {
    CString::new(path.to_str()?).ok()
}

/// Refuse a CPU that lacks part of the ggml build baseline (`missing`, from
/// [`sys::missing_cpu_features`]) with a protocol error instead of SIGILL.
///
/// # Errors
///
/// `cpu_unsupported`, naming the missing instruction sets.
pub fn cpu_support(missing: &[&str]) -> Result<(), EngineError> {
    if missing.is_empty() {
        return Ok(());
    }
    Err(EngineError::new(
        "cpu_unsupported",
        format!(
            "this CPU lacks {}, which the whisper.cpp worker is built for (x86-64-v3)",
            missing.join(", ")
        ),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cpu_without_the_baseline_is_refused_by_name() {
        assert_eq!(cpu_support(&[]), Ok(()));
        let error = cpu_support(&["avx2", "fma"]).unwrap_err();
        assert_eq!(error.code, "cpu_unsupported");
        assert!(error.message.contains("avx2, fma"), "{}", error.message);
    }
}
