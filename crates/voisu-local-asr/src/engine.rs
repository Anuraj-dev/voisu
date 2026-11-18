//! The speech-to-text engine seam and its whisper.cpp implementation.

use std::ffi::{CStr, CString, c_char, c_void};
use std::path::Path;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Once, OnceLock};

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

    /// The device the engine actually runs on, when it knows better than the
    /// manifest (which GPU, such as `vulkan:0`).
    fn observed_device(&self) -> Option<String> {
        None
    }
}

/// Decoding settings from the manifest. Everything else is fixed in `shim.c`
/// (temperature 0 without fallback, no cross-window context, `suppress_nst`,
/// flash attention on).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DecodeOptions {
    pub n_threads: i32,
    /// 1 decodes greedily; above 1 is beam search of that width.
    pub beam_size: i32,
}

/// Where the model runs, from the manifest's `device`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Device {
    Cpu,
    /// The first discrete GPU, else the first integrated one. There is no
    /// silent CPU fallback: no usable GPU is an error.
    Gpu {
        /// Device memory the model needs; the preflight asks for 25% more free.
        vram_bytes: u64,
    },
}

/// Probability above which whisper.cpp treats a window as no speech.
const NO_SPEECH_THOLD: f32 = 0.6;

/// One loaded whisper.cpp model.
pub struct WhisperCpp {
    ctx: NonNull<sys::WhisperContext>,
    options: DecodeOptions,
    /// `cpu`, or the GPU as `<backend>:<index>` (`vulkan:0`).
    observed_device: String,
}

/// The GPU [`sys::voisu_whisper_pick_gpu`] chose.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Gpu {
    /// whisper.cpp's `gpu_device`, counted over GPU and iGPU devices.
    pub index: i32,
    /// ggml's device name, such as `Vulkan0`.
    pub name: String,
    pub description: String,
    pub free_bytes: u64,
    pub total_bytes: u64,
}

/// What the ggml backend registry offered at start.
#[derive(Debug)]
struct Backends {
    cpu: bool,
    gpu: Option<Gpu>,
}

static ROUTE_LOGS: Once = Once::new();
static BACKENDS: OnceLock<Backends> = OnceLock::new();

/// The backend modules ship beside the worker (`target/<profile>/` or
/// `/usr/libexec/voisu/`). An explicit directory: given none, ggml would also
/// search the working directory.
fn backend_dir() -> Option<CString> {
    let exe = std::env::current_exe().ok()?;
    path_cstring(exe.parent()?)
}

fn backends() -> &'static Backends {
    BACKENDS.get_or_init(|| {
        let Some(dir) = backend_dir() else {
            return Backends {
                cpu: false,
                gpu: None,
            };
        };
        sys::load_backends(&dir);
        // SAFETY: the registry was initialized by the call above.
        let cpu = unsafe { sys::voisu_whisper_cpu_backend_loaded() };
        let mut raw = sys::VoisuGpu::default();
        // SAFETY: `raw` is a writable `struct voisu_gpu` (layout is tested).
        let found = unsafe { sys::voisu_whisper_pick_gpu(&raw mut raw) };
        let gpu = found.then(|| Gpu {
            index: raw.index,
            name: c_text(&raw.name),
            description: c_text(&raw.description),
            free_bytes: raw.free_bytes as u64,
            total_bytes: raw.total_bytes as u64,
        });
        Backends { cpu, gpu }
    })
}

fn c_text(raw: &[c_char]) -> String {
    let bytes: Vec<u8> = raw
        .iter()
        .take_while(|&&byte| byte != 0)
        .map(|&byte| byte as u8)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

impl WhisperCpp {
    /// Load a GGML model file on `device`. ggml and whisper.cpp logs go to
    /// stderr.
    ///
    /// # Errors
    ///
    /// - `cpu_unsupported`: no ggml CPU variant runs on this CPU, or the CPU
    ///   device is asked for below its speed floor
    ///   ([`sys::REQUIRED_CPU_FEATURES`]);
    /// - `gpu_unavailable`: the GPU device is asked for and none loaded;
    /// - `gpu_memory`: the GPU has too little free memory, or the load on it
    ///   failed;
    /// - `model_load_failed`: the path is unusable or whisper.cpp refuses it.
    pub fn load(
        model_path: &Path,
        device: Device,
        options: DecodeOptions,
    ) -> Result<Self, EngineError> {
        ROUTE_LOGS.call_once(|| unsafe { sys::voisu_whisper_route_logs_to_stderr() });
        let backends = backends();
        if !backends.cpu {
            return Err(EngineError::new(
                "cpu_unsupported",
                "no ggml CPU backend loads on this CPU",
            ));
        }
        let gpu = match device {
            Device::Cpu => {
                cpu_support(&sys::missing_cpu_features())?;
                None
            }
            Device::Gpu { vram_bytes } => Some(gpu_support(backends.gpu.as_ref(), vram_bytes)?),
        };
        let path = path_cstring(model_path)
            .ok_or_else(|| EngineError::new("model_load_failed", "model path is not a C string"))?;
        let (use_gpu, index) = gpu.map_or((false, 0), |gpu| (true, gpu.index));
        // SAFETY: `path` is a valid NUL-terminated string for the call.
        let ctx = unsafe { sys::voisu_whisper_load(path.as_ptr(), use_gpu, index, true) };
        let ctx = NonNull::new(ctx).ok_or_else(|| match gpu {
            // whisper.cpp logs the cause; a device allocation is the usual one.
            Some(gpu) => EngineError::new(
                "gpu_memory",
                format!(
                    "whisper.cpp could not load the model on {}",
                    gpu.description
                ),
            ),
            None => EngineError::new("model_load_failed", "whisper.cpp could not load the model"),
        })?;
        Ok(Self {
            ctx,
            options,
            observed_device: gpu.map_or_else(|| "cpu".into(), |gpu| device_label(&gpu.name)),
        })
    }
}

/// `Vulkan0` -> `vulkan:0`, `CUDA1` -> `cuda:1`: the daemon's device names.
#[must_use]
pub fn device_label(ggml_name: &str) -> String {
    let split = ggml_name
        .trim_end_matches(|c: char| c.is_ascii_digit())
        .len();
    let (backend, index) = ggml_name.split_at(split);
    if index.is_empty() {
        backend.to_ascii_lowercase()
    } else {
        format!("{}:{index}", backend.to_ascii_lowercase())
    }
}

/// The GPU to load on, or why there is none. Free memory must cover the
/// model's need plus 25%.
///
/// # Errors
///
/// `gpu_unavailable` without a GPU, `gpu_memory` when too little is free.
pub fn gpu_support(gpu: Option<&Gpu>, vram_bytes: u64) -> Result<&Gpu, EngineError> {
    let Some(gpu) = gpu else {
        return Err(EngineError::new(
            "gpu_unavailable",
            "no GPU backend device loaded (Vulkan driver missing or unsupported)",
        ));
    };
    let need = vram_bytes.saturating_add(vram_bytes / 4);
    // A driver that reports no memory figures is not refused here; the load
    // itself still fails with gpu_memory.
    if gpu.total_bytes > 0 && gpu.free_bytes < need {
        return Err(EngineError::new(
            "gpu_memory",
            format!(
                "{} has {} MiB free; the model needs {} MiB",
                gpu.description,
                gpu.free_bytes >> 20,
                need >> 20
            ),
        ));
    }
    Ok(gpu)
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

    fn observed_device(&self) -> Option<String> {
        Some(self.observed_device.clone())
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

/// Refuse the CPU device on a CPU below its speed floor (`missing`, from
/// [`sys::missing_cpu_features`]). ggml would still run there on a plainer
/// variant, but too slowly for a Recording.
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
            "this CPU lacks {}, which Local on the CPU needs (x86-64-v3); use a GPU",
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

    fn gpu(free_mib: u64, total_mib: u64) -> Gpu {
        Gpu {
            index: 0,
            name: "Vulkan0".into(),
            description: "Test GPU".into(),
            free_bytes: free_mib << 20,
            total_bytes: total_mib << 20,
        }
    }

    #[test]
    fn a_gpu_needs_a_device_and_a_quarter_more_free_memory_than_the_model() {
        assert_eq!(gpu_support(None, 1).unwrap_err().code, "gpu_unavailable");
        let need = 1024 << 20;
        assert!(gpu_support(Some(&gpu(1280, 8192)), need).is_ok());
        let error = gpu_support(Some(&gpu(1279, 8192)), need).unwrap_err();
        assert_eq!(error.code, "gpu_memory");
        assert!(error.message.contains("1279 MiB free"), "{}", error.message);
        // No memory figures: the load decides.
        assert!(gpu_support(Some(&gpu(0, 0)), need).is_ok());
    }

    #[test]
    fn ggml_device_names_become_daemon_device_labels() {
        assert_eq!(device_label("Vulkan0"), "vulkan:0");
        assert_eq!(device_label("CUDA1"), "cuda:1");
        assert_eq!(device_label("Metal"), "metal");
    }
}
