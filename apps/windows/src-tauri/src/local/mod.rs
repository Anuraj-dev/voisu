//! Opt-in Local mode on Windows (ADR 0014): the pinned whisper.cpp model,
//! its download, the private worker that transcribes on this PC, and the
//! commands the Dictation page drives it with.
//!
//! The worker is `voisu-local-asr.exe`, the Linux whisper.cpp worker built
//! with MSVC and installed under `local-asr\` beside the app. One worker runs
//! per Recording; see [`worker`]. The model is never in the installer: the
//! user downloads it from Settings into [`crate::paths::local_model_dir`].
//! macOS compiles this module but never offers it.

pub mod download;
pub mod worker;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};
use voisu_core::{
    BoundaryError, BoundaryKind, DEFAULT_RENDERING_POLICY, IntentObservation, ProviderState,
};

use crate::settings::Mode;

/// One file the Local install pins: where it comes from and what it must be.
#[derive(Clone, Copy, Debug)]
pub struct Pin {
    pub name: &'static str,
    pub url: &'static str,
    /// Lowercase hex SHA-256.
    pub sha256: &'static str,
    pub bytes: u64,
}

/// The Fast tier, the same pin as Linux: small.en at q8_0 decodes 61 s of
/// speech in about 9 s on 4 CPU cores. The Accurate tier (818 MB) takes
/// about 31 s for the same audio, too slow without a GPU, so Windows offers
/// only Fast.
pub const MODEL: Pin = Pin {
    name: "ggml-small.en-q8_0.bin",
    url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/5359861c739e955e79d9a303bcbc70fb988958b1/ggml-small.en-q8_0.bin",
    sha256: "67a179f608ea6114bd3fdb9060e762b588a3fb3bd00c4387971be4d177958067",
    bytes: 264_477_561,
};
pub const MODEL_ID: &str = "whisper-cpp-small-en-q8-0";

/// The clip the worker transcribes at every start to prove the model works.
pub const HEALTH_CLIP: Pin = Pin {
    name: "jfk.wav",
    url: "https://raw.githubusercontent.com/ggml-org/whisper.cpp/c44b60b8053bbf2a5c1e014f11323fb3f2485177/samples/jfk.wav",
    sha256: "59dfb9a4acb36fe2a2affc14bacbee2920ff435cb13cc314a08c13f66ba7860e",
    bytes: 352_078,
};
pub const HEALTH_EXPECTED: &str = "And so my fellow Americans ask not what your country can do for you. Ask what you can do for your country.";

/// Downloaded in this order: the small clip first, so a blocked host fails fast.
pub const PINS: [Pin; 2] = [HEALTH_CLIP, MODEL];
pub const TOTAL_BYTES: u64 = HEALTH_CLIP.bytes + MODEL.bytes;

/// The worker beside the app, under the installer's `local-asr\` resources.
const WORKER_DIR: &str = "local-asr";
const WORKER_EXE: &str = "voisu-local-asr.exe";

/// Every pinned file is in `dir` at its pinned size. The worker re-hashes
/// each one at start, so a file damaged later is still caught.
pub fn installed(dir: &Path) -> bool {
    PINS.iter().all(|pin| {
        std::fs::metadata(dir.join(pin.name))
            .is_ok_and(|meta| meta.is_file() && meta.len() == pin.bytes)
    })
}

/// The installed worker, or `None` in a build without it (and off Windows).
pub fn worker_path(app: &AppHandle) -> Option<PathBuf> {
    if !cfg!(windows) {
        return None;
    }
    let path = app
        .path()
        .resource_dir()
        .ok()?
        .join(WORKER_DIR)
        .join(WORKER_EXE);
    path.is_file().then_some(path)
}

/// Why a Local Recording or download did not produce what it should. Each
/// maps to one plain message, shown in the overlay and on the Dictation page
/// next to "Switch to Cloud".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocalError {
    ModelMissing,
    WorkerMissing,
    /// A pinned file no longer matches its checksum (`manifest_error`).
    Damaged,
    CpuUnsupported,
    /// The model would not load or failed its health check.
    LoadFailed,
    TranscribeFailed,
    TimedOut,
    /// The worker could not start, or broke the protocol.
    WorkerFailed,
}

impl LocalError {
    /// The worker's error `code`, as `voisu-local-asr` reports it.
    pub fn from_code(code: &str) -> Self {
        match code {
            "manifest_error" => Self::Damaged,
            "cpu_unsupported" => Self::CpuUnsupported,
            "model_load_failed" | "health_empty" | "health_mismatch" => Self::LoadFailed,
            "inference_failed" | "too_many_windows" | "transcript_too_large" => {
                Self::TranscribeFailed
            }
            _ => Self::WorkerFailed,
        }
    }

    pub fn message(self) -> &'static str {
        match self {
            Self::ModelMissing => {
                "The Local model isn't downloaded. Download it in Settings, or switch to Cloud."
            }
            Self::WorkerMissing => "Local mode isn't available in this build. Switch to Cloud.",
            Self::Damaged => {
                "The Local model file is damaged. Remove it and download it again, or switch to Cloud."
            }
            Self::CpuUnsupported => {
                "This PC's processor can't run Local mode (it needs AVX2). Switch to Cloud."
            }
            Self::LoadFailed => {
                "The Local model couldn't start. Download it again, or switch to Cloud."
            }
            Self::TranscribeFailed => {
                "Local transcription failed, so this Recording wasn't transcribed."
            }
            Self::TimedOut => {
                "Local transcription took too long, so this Recording wasn't transcribed."
            }
            Self::WorkerFailed => {
                "Voisu's Local engine stopped unexpectedly, so this Recording wasn't transcribed."
            }
        }
    }

    /// Whether the user has to act before Local can work again, so Settings
    /// opens on the Dictation page's error. A failed or slow Recording alone
    /// does not open it.
    pub fn needs_settings(self) -> bool {
        matches!(
            self,
            Self::ModelMissing
                | Self::WorkerMissing
                | Self::Damaged
                | Self::CpuUnsupported
                | Self::LoadFailed
        )
    }

    pub fn into_boundary(self, diagnostic: impl Into<String>) -> BoundaryError {
        BoundaryError::new(BoundaryKind::Provider, diagnostic).with_public_message(self.message())
    }
}

/// Room for the worker to load the model and pass its health check, plus at
/// worst real time for the Recording itself. Fast decodes about 7x faster
/// than real time on 4 cores, so only a stuck worker reaches this.
pub fn decode_deadline(pcm_bytes: usize) -> Duration {
    const BYTES_PER_SECOND: u64 = 16_000 * 2;
    let audio_ms = (pcm_bytes as u64).saturating_mul(1000) / BYTES_PER_SECOND;
    Duration::from_secs(30) + Duration::from_millis(audio_ms)
}

/// The Local Transcript as Linux delivers it (`local_tail::decide_transcript`
/// with DPR on): trimmed, a subtitle dash dropped, outro hallucinations
/// removed, the user's Dictionary applied, then spoken marks, line breaks
/// and lists organized locally. `None` is no text: nothing was heard.
pub fn finish_transcript(text: &str, dictionary: &[String]) -> Option<String> {
    let trimmed = without_leading_dialogue_dash(text.trim());
    let sanitized = voisu_core::sanitize_source_transcript_text(trimmed);
    if sanitized.trim().is_empty() || voisu_core::is_whisper_silence_phrase(&sanitized) {
        return None;
    }
    let exact = voisu_core::apply_user_vocabulary(&sanitized, dictionary, &[]);
    let replaced = voisu_core::apply_phonetic_dictionary(&exact, dictionary);
    let (_, baseline) = voisu_cloud::dpr_local::route_and_organize_local(&IntentObservation {
        policy: DEFAULT_RENDERING_POLICY,
        primary_text: replaced,
        provider_state: ProviderState::SingleProvider,
        surface_hint: None,
        process_hint: None,
        timing: None,
    });
    // Untrimmed, as Linux and Cloud deliver it: a spoken "new line" at either
    // end stays a line break.
    let formatted = baseline.rendered().to_owned();
    (!formatted.trim().is_empty()).then_some(formatted)
}

/// whisper.cpp small.en learned a leading dialogue dash from subtitles; a
/// dash glued to the next word ("-5") is kept.
fn without_leading_dialogue_dash(text: &str) -> &str {
    let Some(rest) = text.strip_prefix(['-', '\u{2013}', '\u{2014}']) else {
        return text;
    };
    if rest.is_empty() || rest.starts_with(char::is_whitespace) {
        rest.trim_start()
    } else {
        text
    }
}

/// Download progress and the latest Local error, in memory only.
#[derive(Default)]
pub struct LocalState {
    status: Mutex<Status>,
    cancel: Arc<AtomicBool>,
}

#[derive(Default)]
struct Status {
    downloading: bool,
    downloaded: u64,
    error: Option<&'static str>,
}

/// What the Dictation page shows about Local mode.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalUi {
    /// The worker is installed, so Local can be chosen once downloaded.
    available: bool,
    installed: bool,
    downloading: bool,
    downloaded: u64,
    total: u64,
    /// The latest download or Recording error, until the user acts on it.
    error: Option<&'static str>,
}

/// `None` off Windows, where the page shows nothing about Local.
pub fn ui(app: &AppHandle) -> Option<LocalUi> {
    if !cfg!(windows) {
        return None;
    }
    let state = app.state::<LocalState>();
    let status = state.status.lock().unwrap();
    Some(LocalUi {
        available: worker_path(app).is_some(),
        installed: installed(&crate::paths::local_model_dir()),
        downloading: status.downloading,
        downloaded: status.downloaded,
        total: TOTAL_BYTES,
        error: status.error,
    })
}

/// Remembers a Local failure for the Dictation page and opens Settings when
/// the user has to act on it.
pub fn report(app: &AppHandle, error: LocalError) {
    app.state::<LocalState>().status.lock().unwrap().error = Some(error.message());
    let _ = app.emit(crate::STATE_CHANGED, ());
    if error.needs_settings() {
        crate::show_settings(app);
    }
}

/// Whether a Recording can start without the Cloud keys.
pub fn dictation_ready(app: &AppHandle) -> bool {
    let mode = app.state::<crate::AppState>().settings.lock().unwrap().mode;
    mode.effective() == Mode::Local || crate::keys_ready()
}

/// What a Local Recording needs before it starts: the worker and the model.
pub fn launch(app: &AppHandle) -> Result<worker::Launch, LocalError> {
    let worker = worker_path(app).ok_or(LocalError::WorkerMissing)?;
    let models = crate::paths::local_model_dir();
    if !installed(&models) {
        return Err(LocalError::ModelMissing);
    }
    Ok(worker::Launch {
        worker,
        models,
        log: crate::paths::log_dir().join("local-asr.log"),
    })
}

/// Chooses Cloud or Local. Local is refused until the worker and the model
/// are both here; Cloud always works. An error is the message to show.
#[tauri::command]
pub fn set_mode(app: AppHandle, mode: Mode) -> Result<Mode, String> {
    if mode == Mode::Local {
        if mode.effective() != Mode::Local || worker_path(&app).is_none() {
            return Err(LocalError::WorkerMissing.message().into());
        }
        if !installed(&crate::paths::local_model_dir()) {
            return Err("Download the Local model first.".into());
        }
    }
    crate::try_update_settings(&app, |settings| settings.mode = mode)?;
    tracing::info!(?mode, "transcription mode");
    app.state::<LocalState>().status.lock().unwrap().error = None;
    crate::refresh_tray(&app);
    let _ = app.emit(crate::STATE_CHANGED, ());
    Ok(mode)
}

/// Downloads the clip and the model with progress as `STATE_CHANGED`. A
/// cancelled download keeps its `.part` file so the next one resumes. An
/// error is the message to show; cancelling is not an error.
#[tauri::command]
pub async fn download_local_model(app: AppHandle) -> Result<(), String> {
    if !cfg!(windows) {
        return Err(LocalError::WorkerMissing.message().into());
    }
    let state = app.state::<LocalState>();
    {
        let mut status = state.status.lock().unwrap();
        if status.downloading {
            return Ok(());
        }
        *status = Status {
            downloading: true,
            ..Status::default()
        };
    }
    state.cancel.store(false, Ordering::SeqCst);
    let _ = app.emit(crate::STATE_CHANGED, ());
    tracing::info!("Local model download started");
    let progress_app = app.clone();
    let mut last = std::time::Instant::now();
    let result = download::install(
        &crate::paths::local_model_dir(),
        &state.cancel,
        move |downloaded| {
            progress_app
                .state::<LocalState>()
                .status
                .lock()
                .unwrap()
                .downloaded = downloaded;
            // A few updates a second are plenty for a progress bar.
            if last.elapsed() >= Duration::from_millis(250) || downloaded == TOTAL_BYTES {
                last = std::time::Instant::now();
                let _ = progress_app.emit(crate::STATE_CHANGED, ());
            }
        },
    )
    .await;
    let message = match result {
        Ok(()) => {
            tracing::info!("Local model installed");
            None
        }
        Err(download::DownloadError::Cancelled) => {
            tracing::info!("Local model download cancelled");
            None
        }
        Err(error) => {
            tracing::warn!(?error, "Local model download failed");
            Some(error.message())
        }
    };
    {
        let mut status = state.status.lock().unwrap();
        status.downloading = false;
        status.error = message;
    }
    let _ = app.emit(crate::STATE_CHANGED, ());
    match message {
        Some(message) => Err(message.to_owned()),
        None => Ok(()),
    }
}

#[tauri::command]
pub fn cancel_local_download(app: AppHandle) {
    app.state::<LocalState>()
        .cancel
        .store(true, Ordering::SeqCst);
}

/// Deletes the downloaded model, switching to Cloud first so no Recording
/// starts against a missing file.
#[tauri::command]
pub fn remove_local_model(app: AppHandle) -> Result<(), String> {
    if app.state::<LocalState>().status.lock().unwrap().downloading {
        return Err("Cancel the download first.".into());
    }
    crate::try_update_settings(&app, |settings| settings.mode = Mode::Cloud)?;
    let dir = crate::paths::local_model_dir();
    let removed = download::remove(&dir);
    app.state::<LocalState>().status.lock().unwrap().error = None;
    crate::refresh_tray(&app);
    let _ = app.emit(crate::STATE_CHANGED, ());
    removed.map_err(|error| {
        tracing::warn!(%error, "Local model removal failed");
        "Voisu couldn't delete the Local model. Close anything using it and try again.".to_owned()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_error_codes_map_to_what_the_user_can_do() {
        let cases = [
            ("manifest_error", LocalError::Damaged, true),
            ("cpu_unsupported", LocalError::CpuUnsupported, true),
            ("model_load_failed", LocalError::LoadFailed, true),
            ("health_mismatch", LocalError::LoadFailed, true),
            ("inference_failed", LocalError::TranscribeFailed, false),
            ("not_ready", LocalError::WorkerFailed, false),
            ("anything_new", LocalError::WorkerFailed, false),
        ];
        for (code, error, settings) in cases {
            assert_eq!(LocalError::from_code(code), error, "{code}");
            assert_eq!(error.needs_settings(), settings, "{code}");
        }
        // Every message offers a way forward or says the Recording is lost,
        // and reaches the overlay as the public message.
        let boundary = LocalError::ModelMissing.into_boundary("model missing");
        assert_eq!(
            boundary.public_message(),
            LocalError::ModelMissing.message()
        );
        assert!(LocalError::ModelMissing.needs_settings());
        assert!(!LocalError::TimedOut.needs_settings());
    }

    #[test]
    fn the_decode_deadline_grows_with_the_recording() {
        assert_eq!(decode_deadline(0), Duration::from_secs(30));
        // 60 s of 16 kHz mono s16le.
        assert_eq!(decode_deadline(1_920_000), Duration::from_secs(90));
        assert_eq!(
            decode_deadline(voisu_core::local_wire::MAX_PCM_BYTES),
            Duration::from_secs(630)
        );
    }

    #[test]
    fn a_local_transcript_is_cleaned_and_formatted_as_on_linux() {
        assert_eq!(finish_transcript("  ", &[]), None);
        assert_eq!(finish_transcript("- ", &[]), None);
        // The Linux `local_tail` expectations, DPR on.
        for (heard, expected) in [
            (
                "i said quote hello unquote to him",
                "I said \"hello\" to him.",
            ),
            ("- hello new line", "Hello.\n"),
            ("hello new paragraph", "Hello.\n\n"),
        ] {
            assert_eq!(
                finish_transcript(heard, &[]).as_deref(),
                Some(expected),
                "{heard}"
            );
        }
    }

    #[test]
    fn the_install_counts_only_when_every_pinned_file_has_its_size() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!installed(dir.path()));
        for pin in PINS {
            let file = std::fs::File::create(dir.path().join(pin.name)).unwrap();
            file.set_len(pin.bytes).unwrap();
        }
        assert!(installed(dir.path()));
        std::fs::File::create(dir.path().join(MODEL.name))
            .unwrap()
            .set_len(MODEL.bytes - 1)
            .unwrap();
        assert!(!installed(dir.path()));
        assert_eq!(TOTAL_BYTES, 264_829_639);
    }
}
