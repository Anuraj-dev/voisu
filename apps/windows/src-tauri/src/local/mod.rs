//! Opt-in Local mode on Windows (ADR 0014): the pinned whisper.cpp models,
//! their download, the private worker that transcribes on this PC, and the
//! commands the Dictation page drives it with.
//!
//! The worker is `voisu-local-asr.exe`, the Linux whisper.cpp worker built
//! with MSVC and ggml's Vulkan backend, installed under `local-asr\` beside
//! the app. At startup [`gpu::probe`] asks it which GPU it would use, without
//! loading a model; that picks the `auto` tier as on Linux: Accurate on a
//! GPU, Fast on the CPU. While Local is on, one worker stays loaded
//! ([`worker::Resident`]) and each Recording sends it only `transcribe`.
//! The models are never in the installer: the user downloads one from
//! Settings into [`crate::paths::local_model_dir`]. macOS compiles this
//! module but never offers it.

pub mod download;
pub mod gpu;
pub mod worker;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_notification::NotificationExt as _;
use voisu_core::local_tier::{LocalModel, LocalTier, ModelFile};
use voisu_core::{
    BoundaryError, BoundaryKind, DEFAULT_RENDERING_POLICY, IntentObservation, ProviderState,
};

use crate::settings::Mode;
use gpu::Detected;
use worker::{Lease, Resident, State};

/// The clip the worker transcribes at every start to prove the model works.
pub const HEALTH_CLIP: ModelFile = ModelFile {
    name: "jfk.wav",
    url: "https://raw.githubusercontent.com/ggml-org/whisper.cpp/c44b60b8053bbf2a5c1e014f11323fb3f2485177/samples/jfk.wav",
    sha256: "59dfb9a4acb36fe2a2affc14bacbee2920ff435cb13cc314a08c13f66ba7860e",
    bytes: 352_078,
};
pub const HEALTH_EXPECTED: &str = "And so my fellow Americans ask not what your country can do for you. Ask what you can do for your country.";

/// The worker beside the app, under the installer's `local-asr\` resources.
const WORKER_DIR: &str = "local-asr";
const WORKER_EXE: &str = "voisu-local-asr.exe";
/// The worker's stderr, whisper.cpp's and ggml's own lines, in the log folder.
const WORKER_LOG: &str = "local-asr.log";
/// How long anything that needs the GPU probe's answer waits for it before
/// assuming the CPU. The probe itself gives up after 20 s.
const DETECT_WAIT: Duration = Duration::from_secs(30);
const SETTINGS_NOT_SAVED: &str = "Voisu couldn't save its settings file.";

/// The files `tier` needs, downloaded in this order: the small clip first,
/// so a blocked host fails fast.
pub fn pins(tier: LocalTier) -> [ModelFile; 2] {
    [HEALTH_CLIP, tier.model()]
}

pub fn total_bytes(tier: LocalTier) -> u64 {
    pins(tier).iter().map(|pin| pin.bytes).sum()
}

fn on_disk(dir: &Path, pin: ModelFile) -> bool {
    std::fs::metadata(dir.join(pin.name))
        .is_ok_and(|meta| meta.is_file() && meta.len() == pin.bytes)
}

/// Every file `tier` needs is in `dir` at its pinned size. The worker
/// re-hashes each one at start, so a file damaged later is still caught.
pub fn installed(dir: &Path, tier: LocalTier) -> bool {
    pins(tier).into_iter().all(|pin| on_disk(dir, pin))
}

fn any_installed(dir: &Path) -> bool {
    LocalTier::ALL.into_iter().any(|tier| installed(dir, tier))
}

/// What downloading `tier` would fetch: the size the consent shows.
pub fn missing_bytes(dir: &Path, tier: LocalTier) -> u64 {
    pins(tier)
        .into_iter()
        .filter(|pin| !on_disk(dir, *pin))
        .map(|pin| pin.bytes)
        .sum()
}

/// The tier Local runs. A tier the user picked is that tier. `auto` is the
/// tier for this device or, until that one is downloaded, the other tier if
/// it is: an existing Fast install keeps working on a PC with a GPU, and
/// Accurate downloads only when the user agrees to it.
pub fn active_tier(
    setting: LocalModel,
    on_gpu: bool,
    installed: impl Fn(LocalTier) -> bool,
) -> LocalTier {
    let preferred = LocalTier::resolve(setting, on_gpu);
    if setting != LocalModel::Auto || installed(preferred) {
        return preferred;
    }
    LocalTier::ALL
        .into_iter()
        .find(|tier| *tier != preferred && installed(*tier))
        .unwrap_or(preferred)
}

/// The tier a setting always picks, or `None` for `auto`.
fn pinned_tier(model: LocalModel) -> Option<LocalTier> {
    match model {
        LocalModel::Auto => None,
        LocalModel::Accurate => Some(LocalTier::Accurate),
        LocalModel::Fast => Some(LocalTier::Fast),
    }
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

fn worker_log() -> PathBuf {
    crate::paths::log_dir().join(WORKER_LOG)
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
    /// The worker crashed more often than the resident restarts it.
    KeepsStopping,
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
            Self::KeepsStopping => {
                "Voisu's Local engine keeps stopping, so Local is paused. Choose Local again to retry, or switch to Cloud."
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
                | Self::KeepsStopping
        )
    }

    pub fn into_boundary(self, diagnostic: impl Into<String>) -> BoundaryError {
        BoundaryError::new(BoundaryKind::Provider, diagnostic).with_public_message(self.message())
    }
}

/// At worst real time for the Recording, plus room for a stuck request to
/// be noticed. A worker still loading its model (`ready` false) also gets
/// [`worker::PREPARE_DEADLINE`], the most its load and health check may
/// take. Only a stuck worker reaches either.
pub fn decode_deadline(pcm_bytes: usize, ready: bool) -> Duration {
    const BYTES_PER_SECOND: u64 = 16_000 * 2;
    let audio_ms = (pcm_bytes as u64).saturating_mul(1000) / BYTES_PER_SECOND;
    let decode = Duration::from_secs(30) + Duration::from_millis(audio_ms);
    if ready {
        decode
    } else {
        decode + worker::PREPARE_DEADLINE
    }
}

/// Whether the Recording holds speech, by the gate Linux applies before
/// Local transcription; silence never reaches the worker.
pub fn has_speech(pcm_s16le: &[u8]) -> bool {
    let samples: Vec<i16> = pcm_s16le
        .as_chunks::<2>()
        .0
        .iter()
        .map(|sample| i16::from_le_bytes(*sample))
        .collect();
    voisu_core::detect_speech(&samples, 16_000).has_speech
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

/// The GPU probe's answer, once it has one.
#[derive(Default)]
struct Detection {
    found: Mutex<Option<Detected>>,
    answered: Condvar,
}

impl Detection {
    fn get(&self) -> Option<Detected> {
        self.found.lock().unwrap().clone()
    }

    fn set(&self, detected: Detected) {
        *self.found.lock().unwrap() = Some(detected);
        self.answered.notify_all();
    }

    /// Blocks until the probe answers, or [`DETECT_WAIT`] passes and the
    /// CPU is assumed. Never call it on the main thread or the executor.
    fn wait(&self) -> Detected {
        let found = self.found.lock().unwrap();
        let (found, _) = self
            .answered
            .wait_timeout_while(found, DETECT_WAIT, |found| found.is_none())
            .unwrap();
        found.clone().unwrap_or(Detected::Cpu)
    }
}

/// Local mode's state, in memory only: the GPU probe's answer, the resident
/// worker, download progress and the latest error.
#[derive(Default)]
pub struct LocalState {
    /// Locked before the settings, never after: every change to the mode,
    /// the tier or the model files saves its setting under this lock, and a
    /// worker starts under it only while that setting still asks for it.
    status: Mutex<Status>,
    detection: Detection,
}

struct Download {
    /// This download's own cancel switch, so cancelling one never touches
    /// the next.
    cancel: Arc<download::Cancel>,
    tier: LocalTier,
}

#[derive(Default)]
struct Status {
    download: Option<Download>,
    downloaded: u64,
    /// The tier setting to apply when the running download installs. A tier
    /// chosen meanwhile clears it.
    after_model: Option<LocalModel>,
    /// Turn Local on when the running download installs. Choosing Cloud
    /// meanwhile clears it, whichever page asked for Local.
    after_local: bool,
    /// The "Using Cloud" notification went out for this download.
    cloud_notice_shown: bool,
    error: Option<&'static str>,
    /// The resident worker for the tier Local runs, while Local is on.
    worker: Option<Arc<Resident>>,
    /// The last resident replaced, which the next one waits for.
    retired: Option<Arc<Resident>>,
    /// Local Recordings admitted and not yet finished.
    recordings: usize,
    /// Remove is deleting the models: nothing starts meanwhile.
    removing: bool,
    /// Voisu is quitting: no worker starts again.
    closed: bool,
}

impl Status {
    /// Takes the resident out of the slot. It serves the Recordings bound
    /// to it, then stops, and the next resident waits for it to exit.
    fn retire(&mut self) {
        if let Some(resident) = self.worker.take() {
            resident.retire();
            self.retired = Some(resident);
        }
    }
}

/// Starts the GPU probe off the main thread, then warms the worker if Local
/// is on.
pub fn start(app: &AppHandle) {
    let probe_app = app.clone();
    let spawned = std::thread::Builder::new()
        .name("voisu-local-probe".into())
        .spawn(move || {
            let app = probe_app;
            let detected = match worker_path(&app) {
                Some(worker) => gpu::probe(&worker, &worker_log()),
                None => Detected::Cpu,
            };
            app.state::<LocalState>().detection.set(detected);
            let _ = app.emit(crate::STATE_CHANGED, ());
            if local_on(&app) {
                warm(&app);
            }
        });
    if let Err(error) = spawned {
        tracing::warn!(%error, "Local GPU probe not started; using the CPU");
        app.state::<LocalState>().detection.set(Detected::Cpu);
    }
}

fn local_on(app: &AppHandle) -> bool {
    let mode = app.state::<crate::AppState>().settings.lock().unwrap().mode;
    mode.effective() == Mode::Local
}

fn local_model_setting(app: &AppHandle) -> LocalModel {
    app.state::<crate::AppState>()
        .settings
        .lock()
        .unwrap()
        .local_model
}

fn changed(app: &AppHandle) -> worker::Changed {
    let app = app.clone();
    Arc::new(move || {
        let _ = app.emit(crate::STATE_CHANGED, ());
    })
}

/// The resident worker for the tier Local runs now, started if none runs
/// or it runs another tier. Called with the status lock held, so it sees
/// the latest mode and tier: `None` when Local is off, Remove is deleting
/// the models or Voisu is quitting.
fn resident_locked(
    app: &AppHandle,
    status: &mut Status,
    worker: PathBuf,
    detected: &Detected,
) -> Result<Option<Arc<Resident>>, LocalError> {
    if status.closed || status.removing || !local_on(app) {
        return Ok(None);
    }
    let models = crate::paths::local_model_dir();
    let setting = local_model_setting(app);
    let is_installed = |tier| installed(&models, tier);
    let tier = active_tier(setting, detected.on_gpu(), is_installed);
    if !installed(&models, tier) {
        return Err(LocalError::ModelMissing);
    }
    if let Some(resident) = status.worker.as_ref()
        && resident.tier() == tier
        && resident.state() != State::Stopped
    {
        return Ok(Some(Arc::clone(resident)));
    }
    status.retire();
    let cpu_tier = active_tier(setting, false, is_installed);
    let resident = Arc::new(Resident::start(
        worker::Launch {
            worker,
            models,
            log: worker_log(),
            tier,
            cpu_tier,
            gpu: detected.on_gpu(),
        },
        status.retired.take(),
        changed(app),
    ));
    status.worker = Some(Arc::clone(&resident));
    Ok(Some(resident))
}

/// [`resident_locked`] once the GPU probe has answered, leased before the
/// lock is released so nothing retires it unseen. Blocks while the probe
/// has not answered: never call it on the main thread or the executor.
fn resident(app: &AppHandle) -> Result<Option<Lease>, LocalError> {
    let worker = worker_path(app).ok_or(LocalError::WorkerMissing)?;
    let state = app.state::<LocalState>();
    let detected = state.detection.wait();
    let mut status = state.status.lock().unwrap();
    let lease = resident_locked(app, &mut status, worker, &detected)?.map(|found| found.lease());
    drop(status);
    Ok(lease)
}

/// Starts the resident worker in the background, so the model is loaded
/// before the next Recording stops. A failure here is only logged: the
/// Recording that needs the worker reports it.
pub fn warm(app: &AppHandle) {
    let app = app.clone();
    let spawned = std::thread::Builder::new()
        .name("voisu-local-warm".into())
        .spawn(move || {
            if let Err(error) = resident(&app) {
                tracing::info!(?error, "Local worker not started");
            }
        });
    if let Err(error) = spawned {
        tracing::warn!(%error, "Local worker warm-up not started");
    }
}

/// At exit: no worker starts again, and the one running (with any it
/// replaced) shuts down cleanly within `bound`, or is killed and reaped.
pub fn shutdown(app: &AppHandle, bound: Duration) {
    let resident = {
        let local = app.state::<LocalState>();
        let mut status = local.status.lock().unwrap();
        status.closed = true;
        status.retire();
        status.retired.take()
    };
    let Some(resident) = resident else {
        return;
    };
    resident.halt();
    if !resident.wait_finished(Some(Instant::now() + bound)) {
        tracing::warn!("Local worker still running at exit; killed");
        resident.reap();
    }
}

/// A Local Recording, admitted before its mic opens. It holds the resident
/// that transcribes it, so a tier or Cloud chosen meanwhile lets it finish
/// on the model it started with, and Remove waits until none is held.
pub struct LocalRecording {
    app: AppHandle,
    /// `None` when the GPU probe had not answered at admission: bound at
    /// stop instead.
    lease: Option<Lease>,
}

impl LocalRecording {
    /// The resident that transcribes this Recording.
    pub async fn resident(&mut self) -> Result<&Resident, LocalError> {
        if self.lease.is_none() {
            let app = self.app.clone();
            let lease = tokio::task::spawn_blocking(move || resident(&app))
                .await
                .unwrap_or(Err(LocalError::WorkerFailed))?
                // Local was turned off, or Voisu is quitting.
                .ok_or(LocalError::WorkerFailed)?;
            self.lease = Some(lease);
        }
        self.lease.as_deref().ok_or(LocalError::WorkerFailed)
    }
}

impl Drop for LocalRecording {
    fn drop(&mut self) {
        let state = self.app.state::<LocalState>();
        let mut status = state.status.lock().unwrap();
        status.recordings = status.recordings.saturating_sub(1);
    }
}

/// Admits a Local Recording: the worker and a model are here, the resident
/// hasn't given up, and Local is still on. Binds the resident for the tier
/// Local runs now, starting it if needed. Never blocks.
pub fn admit(app: &AppHandle) -> Result<LocalRecording, LocalError> {
    let worker = worker_path(app).ok_or(LocalError::WorkerMissing)?;
    if !any_installed(&crate::paths::local_model_dir()) {
        return Err(LocalError::ModelMissing);
    }
    let state = app.state::<LocalState>();
    let detected = state.detection.get();
    let mut status = state.status.lock().unwrap();
    // Cloud was chosen as the hotkey went down, or Remove or Quit runs.
    if status.closed || status.removing || !local_on(app) {
        return Err(LocalError::WorkerFailed);
    }
    if let Some(State::Failed(error)) = status.worker.as_ref().map(|resident| resident.state()) {
        return Err(error);
    }
    let lease = match &detected {
        Some(detected) => Some(
            resident_locked(app, &mut status, worker, detected)?
                .ok_or(LocalError::WorkerFailed)?
                .lease(),
        ),
        None => None,
    };
    status.recordings += 1;
    drop(status);
    if lease.is_none() {
        warm(app);
    }
    Ok(LocalRecording {
        app: app.clone(),
        lease,
    })
}

/// Local was chosen while its model downloads, so Recordings use Cloud.
pub fn cloud_until_downloaded(app: &AppHandle) -> bool {
    let state = app.state::<LocalState>();
    let status = state.status.lock().unwrap();
    status.download.is_some() && status.after_local
}

/// Called when a Recording uses Cloud: while the model Local was chosen
/// with downloads, says so, once per download.
pub fn note_cloud_fallback(app: &AppHandle) {
    let state = app.state::<LocalState>();
    let first = {
        let mut status = state.status.lock().unwrap();
        if status.download.is_none() || !status.after_local {
            return;
        }
        !std::mem::replace(&mut status.cloud_notice_shown, true)
    };
    tracing::info!("Local model still downloading; this Recording uses Cloud");
    if first {
        let shown = app
            .notification()
            .builder()
            .title("Using Cloud until the model finishes")
            .body("Voisu switches to Local when the Local model has downloaded.")
            .show();
        if let Err(error) = shown {
            tracing::warn!(%error, "Cloud fallback notification not shown");
        }
    }
}

/// One Local tier as the Dictation page offers it.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TierUi {
    /// The setting value: `accurate` or `fast`.
    id: &'static str,
    name: &'static str,
    model_name: &'static str,
    installed: bool,
    /// What choosing it would download: the size the consent shows.
    bytes: u64,
    /// The GPU hint or the CPU warning for this PC.
    note: Option<&'static str>,
}

/// What the Dictation and Home pages show about Local mode.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalUi {
    /// The worker is installed, so Local can be chosen once downloaded.
    available: bool,
    /// The GPU probe hasn't answered yet.
    detecting: bool,
    /// The GPU's name, or `CPU`.
    device: String,
    on_gpu: bool,
    /// The setting: `auto`, `accurate` or `fast`.
    model: &'static str,
    /// The tier `auto` picks on this PC.
    auto_tier: &'static str,
    /// The tier Local runs, once one is downloaded.
    active_tier: Option<&'static str>,
    tiers: Vec<TierUi>,
    /// The resident worker: `off`, `starting`, `ready` or `failed`.
    engine: &'static str,
    /// The tier the ready worker runs: `active_tier`, or after a GPU
    /// failure the CPU's.
    engine_tier: Option<&'static str>,
    /// Where the ready worker runs, as it reports it (`cpu`, `vulkan:0`).
    engine_device: Option<String>,
    /// Some tier is downloaded, so Local can be chosen.
    installed: bool,
    /// The tier downloading, if any.
    downloading: Option<&'static str>,
    downloaded: u64,
    total: u64,
    /// The latest download or Recording error, until the user acts on it.
    error: Option<&'static str>,
    /// Local was chosen while its model downloads; Cloud is used meanwhile.
    cloud_until_downloaded: bool,
}

/// `None` off Windows, where the pages show nothing about Local.
pub fn ui(app: &AppHandle) -> Option<LocalUi> {
    if !cfg!(windows) {
        return None;
    }
    let state = app.state::<LocalState>();
    let detected = state.detection.get();
    let on_gpu = detected.as_ref().is_some_and(Detected::on_gpu);
    let setting = local_model_setting(app);
    let models = crate::paths::local_model_dir();
    let is_installed = |tier| installed(&models, tier);
    let tiers = LocalTier::ALL
        .into_iter()
        .map(|tier| TierUi {
            id: tier.setting().as_str(),
            name: tier.name(),
            model_name: tier.model_name(),
            installed: is_installed(tier),
            bytes: missing_bytes(&models, tier),
            note: detected.as_ref().and_then(|_| tier.device_note(on_gpu)),
        })
        .collect();
    let any = any_installed(&models);
    let active = any.then(|| active_tier(setting, on_gpu, is_installed));
    let status = state.status.lock().unwrap();
    let engine = status
        .worker
        .as_ref()
        .map(|resident| (resident.tier(), resident.state()));
    let (engine, engine_tier, engine_device, failed) = match engine {
        None | Some((_, State::Stopped)) => ("off", None, None, None),
        Some((_, State::Failed(error))) => ("failed", None, None, Some(error.message())),
        // A new tier was chosen and its worker is not up yet.
        Some((tier, _)) if Some(tier) != active => ("starting", None, None, None),
        Some((_, State::Starting)) => ("starting", None, None, None),
        Some((_, State::Ready { tier, device })) => {
            ("ready", Some(tier.setting().as_str()), Some(device), None)
        }
    };
    let downloading = status.download.as_ref().map(|download| download.tier);
    Some(LocalUi {
        available: worker_path(app).is_some(),
        detecting: detected.is_none(),
        device: detected.as_ref().map_or("CPU", Detected::label).to_owned(),
        on_gpu,
        model: setting.as_str(),
        auto_tier: LocalTier::resolve(LocalModel::Auto, on_gpu)
            .setting()
            .as_str(),
        active_tier: active.map(|tier| tier.setting().as_str()),
        tiers,
        engine,
        engine_tier,
        engine_device,
        installed: any,
        downloading: downloading.map(|tier| tier.setting().as_str()),
        downloaded: status.downloaded,
        total: downloading.map_or(0, total_bytes),
        error: status.error.or(failed),
        cloud_until_downloaded: status.download.is_some() && status.after_local,
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
    local_on(app) || crate::keys_ready()
}

fn emit_changed(app: &AppHandle) {
    crate::refresh_tray(app);
    let _ = app.emit(crate::STATE_CHANGED, ());
}

/// Chooses Cloud or Local. Local needs the worker and a downloaded model;
/// chosen while the first model downloads, it turns on when that finishes
/// and Cloud is used until then. Cloud always works and stops the worker.
/// An error is the message to show.
#[tauri::command]
pub fn set_mode(app: AppHandle, mode: Mode) -> Result<Mode, String> {
    if mode == Mode::Local && (mode.effective() != Mode::Local || worker_path(&app).is_none()) {
        return Err(LocalError::WorkerMissing.message().into());
    }
    let state = app.state::<LocalState>();
    let mut status = state.status.lock().unwrap();
    if mode == Mode::Local {
        if status.removing {
            return Err("Wait for the Local model to be removed.".into());
        }
        if !any_installed(&crate::paths::local_model_dir()) {
            if status.download.is_none() {
                return Err("Download the Local model first.".into());
            }
            status.after_local = true;
            drop(status);
            tracing::info!("Local chosen; it turns on when the model has downloaded");
            emit_changed(&app);
            return Ok(Mode::Cloud);
        }
    }
    crate::try_update_settings(&app, |settings| settings.mode = mode)?;
    tracing::info!(?mode, "transcription mode");
    status.error = None;
    match mode {
        Mode::Cloud => {
            status.after_local = false;
            status.retire();
        }
        // Choosing Local again retries a worker that gave up.
        Mode::Local => {
            if status
                .worker
                .as_ref()
                .is_some_and(|resident| matches!(resident.state(), State::Failed(_)))
            {
                status.retire();
            }
        }
    }
    drop(status);
    if mode == Mode::Local {
        warm(&app);
    }
    emit_changed(&app);
    Ok(mode)
}

/// Chooses the Local tier: `auto`, `accurate` or `fast`. A tier must be
/// downloaded before it can be chosen; `auto` always can. While Local is
/// on, a different tier restarts the worker with it. Returns the setting.
#[tauri::command]
pub fn set_local_model(app: AppHandle, model: String) -> Result<&'static str, String> {
    let model = LocalModel::from_value(&model).ok_or("Voisu doesn't know that Local model.")?;
    if let Some(tier) = pinned_tier(model)
        && !installed(&crate::paths::local_model_dir(), tier)
    {
        return Err(format!("Download {} first.", tier.name()));
    }
    {
        let state = app.state::<LocalState>();
        let mut status = state.status.lock().unwrap();
        crate::try_update_settings(&app, |settings| settings.local_model = model)?;
        status.after_model = None;
        status.error = None;
    }
    tracing::info!(model = model.as_str(), "Local model setting");
    if local_on(&app) {
        warm(&app);
    }
    emit_changed(&app);
    Ok(model.as_str())
}

/// How a download that did not fail ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DownloadOutcome {
    Installed,
    Cancelled,
}

/// What a download asks for: the tier to fetch, exactly the one the consent
/// showed, and the setting to apply once it installs. `auto` stays `auto`
/// whichever tier it fetches; a named tier fetches only itself.
fn download_request(tier: &str, model: &str) -> Option<(LocalTier, LocalModel)> {
    let tier = pinned_tier(LocalModel::from_value(tier)?)?;
    let model = LocalModel::from_value(model)?;
    pinned_tier(model)
        .is_none_or(|pinned| pinned == tier)
        .then_some((tier, model))
}

/// Downloads `tier`, which must still fetch the `bytes` the consent showed,
/// with progress as `STATE_CHANGED`. Once it installs, `model` becomes the
/// tier setting and, with `switch_to_local`, Local turns on, unless the user
/// chose otherwise meanwhile. A cancelled download keeps its `.part` file so
/// the next one resumes. An error is the message to show; cancelling is not
/// an error.
#[tauri::command]
pub async fn download_local_model(
    app: AppHandle,
    tier: String,
    bytes: u64,
    model: String,
    switch_to_local: bool,
) -> Result<DownloadOutcome, String> {
    if worker_path(&app).is_none() {
        return Err(LocalError::WorkerMissing.message().into());
    }
    let (tier, model) =
        download_request(&tier, &model).ok_or("Voisu doesn't know that Local model.")?;
    let dir = crate::paths::local_model_dir();
    let total = total_bytes(tier);
    let state = app.state::<LocalState>();
    let (cancel, missing) = {
        let mut status = state.status.lock().unwrap();
        if status.download.is_some() {
            return Err("A Local model is already downloading.".into());
        }
        if status.removing {
            return Err("Wait for the Local model to be removed.".into());
        }
        // Never more than the user agreed to.
        let missing = missing_bytes(&dir, tier);
        if missing != bytes {
            return Err("The download size changed. Check it and try again.".into());
        }
        let cancel = Arc::new(download::Cancel::default());
        status.download = Some(Download {
            cancel: Arc::clone(&cancel),
            tier,
        });
        status.downloaded = total - missing;
        status.after_model = Some(model);
        status.after_local = switch_to_local;
        status.cloud_notice_shown = false;
        status.error = None;
        (cancel, missing)
    };
    let _ = app.emit(crate::STATE_CHANGED, ());
    tracing::info!(
        tier = tier.name(),
        bytes = missing,
        "Local model download started"
    );
    let progress_app = app.clone();
    let mut last = Instant::now();
    let result = download::install(&dir, tier, &cancel, move |downloaded| {
        progress_app
            .state::<LocalState>()
            .status
            .lock()
            .unwrap()
            .downloaded = downloaded;
        // A few updates a second are plenty for a progress bar.
        if last.elapsed() >= Duration::from_millis(250) || downloaded == total {
            last = Instant::now();
            let _ = progress_app.emit(crate::STATE_CHANGED, ());
        }
    })
    .await;
    // Still registered until its settings are saved: Remove refuses while it
    // is, and Cloud or a tier chosen meanwhile has already cleared what it
    // would apply.
    let outcome = {
        let mut status = state.status.lock().unwrap();
        let after_model = status.after_model.take();
        let after_local = std::mem::take(&mut status.after_local);
        let outcome = match result {
            Ok(()) => {
                tracing::info!(tier = tier.name(), "Local model installed");
                apply_installed(&app, after_model, after_local)
            }
            Err(download::DownloadError::Cancelled) => {
                tracing::info!("Local model download cancelled");
                Ok(DownloadOutcome::Cancelled)
            }
            Err(error) => {
                tracing::warn!(%error, "Local model download failed");
                Err(error.message())
            }
        };
        status.download = None;
        if let Err(message) = outcome {
            status.error = Some(message);
        }
        outcome
    };
    if outcome == Ok(DownloadOutcome::Installed) && local_on(&app) {
        warm(&app);
    }
    emit_changed(&app);
    outcome.map_err(str::to_owned)
}

/// Saves what the user asked for when the download started, if it still
/// stands. Called with the status lock held.
fn apply_installed(
    app: &AppHandle,
    model: Option<LocalModel>,
    switch_to_local: bool,
) -> Result<DownloadOutcome, &'static str> {
    if model.is_some() || switch_to_local {
        crate::try_update_settings(app, |settings| {
            if let Some(model) = model {
                settings.local_model = model;
            }
            if switch_to_local {
                settings.mode = Mode::Local;
            }
        })
        .map_err(|_| SETTINGS_NOT_SAVED)?;
        if switch_to_local {
            tracing::info!(mode = ?Mode::Local, "transcription mode");
        }
    }
    Ok(DownloadOutcome::Installed)
}

#[tauri::command]
pub fn cancel_local_download(app: AppHandle) {
    if let Some(download) = &app.state::<LocalState>().status.lock().unwrap().download {
        download.cancel.cancel();
    }
}

/// Deletes both tiers' models, switching to Cloud and stopping the worker
/// first so nothing holds the files open. Refused while a download or a
/// Local Recording runs; while it deletes, no worker, download or Local
/// Recording starts.
#[tauri::command]
pub async fn remove_local_model(app: AppHandle) -> Result<(), String> {
    const RECORDING: &str = "Wait for the dictation to finish, then remove the model.";
    if crate::engine::current_phase(&app).is_some() {
        return Err(RECORDING.into());
    }
    let state = app.state::<LocalState>();
    let resident = {
        let mut status = state.status.lock().unwrap();
        if status.download.is_some() {
            return Err("Cancel the download first.".into());
        }
        if status.removing {
            return Err("The Local model is already being removed.".into());
        }
        if status.recordings > 0 {
            return Err(RECORDING.into());
        }
        crate::try_update_settings(&app, |settings| settings.mode = Mode::Cloud)?;
        status.removing = true;
        status.error = None;
        status.after_local = false;
        status.after_model = None;
        status.retire();
        status.retired.take()
    };
    emit_changed(&app);
    if let Some(resident) = resident {
        resident.halt();
        let _ = tokio::task::spawn_blocking(move || resident.wait_finished(None)).await;
    }
    let removed = {
        let mut status = state.status.lock().unwrap();
        status.removing = false;
        download::remove(&crate::paths::local_model_dir())
    };
    emit_changed(&app);
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
            ("gpu_memory", LocalError::WorkerFailed, false),
            ("not_ready", LocalError::WorkerFailed, false),
            ("unrecognized", LocalError::WorkerFailed, false),
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
        assert!(LocalError::KeepsStopping.needs_settings());
        assert!(!LocalError::TimedOut.needs_settings());
    }

    #[test]
    fn the_decode_deadline_grows_with_the_recording_and_a_cold_worker() {
        assert_eq!(decode_deadline(0, true), Duration::from_secs(30));
        // 60 s of 16 kHz mono s16le.
        assert_eq!(decode_deadline(1_920_000, true), Duration::from_secs(90));
        assert_eq!(
            decode_deadline(voisu_core::local_wire::MAX_PCM_BYTES, true),
            Duration::from_secs(630)
        );
        assert_eq!(decode_deadline(1_920_000, false), Duration::from_secs(180));
    }

    #[test]
    fn silence_never_reaches_the_worker() {
        assert!(!has_speech(&[]));
        assert!(!has_speech(&vec![0u8; 32_000 * 2]));
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
    fn a_tier_counts_as_installed_only_when_each_of_its_files_has_its_size() {
        let dir = tempfile::tempdir().unwrap();
        let fill = |pin: ModelFile, bytes: u64| {
            std::fs::File::create(dir.path().join(pin.name))
                .unwrap()
                .set_len(bytes)
                .unwrap();
        };
        assert!(!installed(dir.path(), LocalTier::Fast));
        assert_eq!(
            missing_bytes(dir.path(), LocalTier::Fast),
            total_bytes(LocalTier::Fast)
        );
        for pin in pins(LocalTier::Fast) {
            fill(pin, pin.bytes);
        }
        assert!(installed(dir.path(), LocalTier::Fast));
        assert_eq!(missing_bytes(dir.path(), LocalTier::Fast), 0);
        // The clip is shared, so Accurate needs only its model.
        assert!(!installed(dir.path(), LocalTier::Accurate));
        assert_eq!(
            missing_bytes(dir.path(), LocalTier::Accurate),
            LocalTier::Accurate.model().bytes
        );
        let fast = LocalTier::Fast.model();
        fill(fast, fast.bytes - 1);
        assert!(!installed(dir.path(), LocalTier::Fast));
        assert_eq!(total_bytes(LocalTier::Fast), 264_829_639);
        assert_eq!(total_bytes(LocalTier::Accurate), 818_658_033);
    }

    #[test]
    fn auto_runs_the_tier_for_this_device_or_else_the_one_downloaded() {
        use LocalModel::{Accurate, Auto, Fast};
        use LocalTier as T;
        let only_fast = |tier: T| tier == T::Fast;
        let only_accurate = |tier: T| tier == T::Accurate;
        let both = |_: T| true;
        let none = |_: T| false;
        // An existing Fast install keeps working on a GPU until the user
        // agrees to download Accurate.
        assert_eq!(active_tier(Auto, true, only_fast), T::Fast);
        assert_eq!(active_tier(Auto, true, both), T::Accurate);
        assert_eq!(active_tier(Auto, true, none), T::Accurate);
        assert_eq!(active_tier(Auto, false, only_accurate), T::Accurate);
        assert_eq!(active_tier(Auto, false, both), T::Fast);
        assert_eq!(active_tier(Auto, false, none), T::Fast);
        // A tier the user picked is never swapped for another.
        assert_eq!(active_tier(Accurate, false, only_fast), T::Accurate);
        assert_eq!(active_tier(Fast, true, only_accurate), T::Fast);
        assert_eq!(pinned_tier(Auto), None);
        assert_eq!(pinned_tier(Fast), Some(T::Fast));
    }

    #[test]
    fn a_download_fetches_the_tier_shown_and_keeps_auto() {
        use LocalModel::{Accurate, Auto, Fast};
        use LocalTier as T;
        assert_eq!(download_request("fast", "auto"), Some((T::Fast, Auto)));
        assert_eq!(
            download_request("accurate", "auto"),
            Some((T::Accurate, Auto))
        );
        assert_eq!(
            download_request("accurate", "accurate"),
            Some((T::Accurate, Accurate))
        );
        assert_eq!(download_request("fast", "fast"), Some((T::Fast, Fast)));
        // Never resolved here: the target is always concrete.
        assert_eq!(download_request("auto", "auto"), None);
        // A named tier fetches only itself.
        assert_eq!(download_request("accurate", "fast"), None);
        assert_eq!(download_request("large", "auto"), None);
    }
}
