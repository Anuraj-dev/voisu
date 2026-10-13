//! Consented Install offer during `voisu setup`, and `voisu local remove`.
//!
//! The offer runs after Local is selected and before `run_consented_local_setup`
//! (the closed Bakeoff-catalog repair flow). That older flow must not offer a
//! second download; it stays evidence-gated. This offer is the only setup path
//! that can fetch, and only after an explicit yes.

use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use voisu_core::{AsrMode, KeyDiagnosis};

use crate::asr_mode::LOCAL_UNAVAILABLE;
use crate::local_model::consented::ConsentedRemoveOutcome;
use crate::local_model::{
    CONSENTED_ENTRY, ConsentedConsent, ConsentedError, ConsentedInstallRequest, HostHardware,
    InstallClock, InstallIo, InstallPhase, InstallProgress, ModelStore, OfflineRuntimeBuilder,
    ProductionHttps, models_dir_from_state, remove_consented_install, suggest_local,
    verify_consented_install,
};
use crate::local_setup::SetupMaintenance;
use crate::local_worker::ConsentedWorkerHealth;
use crate::setup::{SETUP_COMPLETE_MESSAGE, WizardIo};
use crate::setup_engine::{ConsentedStatus, ask_yes_no, format_size, suggested_consented_entry};

pub use crate::local_model::{DEVELOPMENT_ROOT_NAME, SystemCommandRunner, probe_hardware};

const INSTALL_PROMPT: &str = "Download and install now?";
const INSTALL_DURATION_HINT: &str = "This can take several minutes.";
#[cfg(test)]
const INSTALL_SUCCESS: &str = "Installed distil-large-v3.5-int8 (Consented Install).";
const DAEMON_RESTARTED: &str = "Restarted the Voisu daemon; Local dictation is ready.";
const CLOUD_PROMPT: &str = "Also set up Cloud?";
const CLOUD_SKIPPED: &str = "Cloud skipped; setup will not ask again. To add Cloud later, run `voisu setup` and choose Cloud, or `voisu auth set groq`.";
/// State file that remembers a No to the Cloud prompt. It lives in the state
/// directory, not the user config: it records what setup already asked, not a
/// setting the user tunes.
pub const CLOUD_DECLINED_MARKER: &str = "setup-cloud-declined";
const BUILDING_RUNTIME: &str = "Building runtime…";
const CHECKING_GPU: &str = "Checking GPU…";
const INSTALL_RETRY: &str =
    "Run `voisu setup` to retry or `voisu mode cloud` to dictate with Cloud.";
const REMOVE_CONFIRM: &str = "Remove the installed Consented model?";
const REMOVE_USAGE: &str = "usage: voisu local remove [--yes]";
pub const REMOVE_REQUIRES_YES: &str = "Non-interactive `voisu local remove` requires `--yes`.";
const REMOVED: &str = "Removed the Consented Install.";
const NOTHING_TO_REMOVE: &str = "No Consented Install is installed.";
const IN_USE: &str =
    "The Consented Install is in use; stop dictation or run `voisu service stop` first.";
const GPU_BUSY: &str = "Another Local worker is using the GPU; stop it (for example `voisu service stop`, or stop voisu-dev.service), then run `voisu setup` again.";

/// Outcome of the setup Consented Install offer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConsentedInstallStep {
    Skipped,
    Declined,
    Installed,
    Failed,
}

/// `voisu local` subcommands.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LocalCommand {
    Remove { yes: bool },
}

/// Result of an injected remove seam.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RemoveOutcome {
    Removed,
    Absent,
}

/// CLI result for `voisu local remove`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LocalRemoveStatus {
    Success,
    Usage(&'static str),
    Failed(String),
}

/// The facts shown before the install prompt, one per line.
///
/// Display only: the receipt still records the full, hashed `CONSENT_TEXT`
/// through [`ConsentedConsent::accepted`], which this summary never replaces.
#[must_use]
pub fn consent_summary() -> [String; 4] {
    [
        format!(
            "Download size: {}",
            format_size(CONSENTED_ENTRY.total_download_bytes)
        ),
        "English only".into(),
        "Needs an NVIDIA GPU".into(),
        "Runs offline after install".into(),
    ]
}

fn consent_summary_for(entry: &crate::local_model::ConsentedCatalogEntry) -> Vec<String> {
    if entry.device == "cpu" {
        vec![
            format!("Download size: {}", format_size(entry.total_download_bytes)),
            "English only".into(),
            format!("Licence: {}", entry.license_spdx),
            "Runs offline on CPU".into(),
            "Development evidence: one host, one speaker — not the release gate.".into(),
        ]
    } else {
        consent_summary().into()
    }
}

/// Shows the consent summary and installs only after an explicit yes. A
/// successful install then restarts the daemon through `restart_daemon`; a
/// failed restart is a warning, never a failed install.
///
/// Non-interactive and an already-verified install never fetch.
pub fn run_consented_install_step(
    io: &mut dyn WizardIo,
    hw: &HostHardware,
    interactive: bool,
    installed: bool,
    now_unix: u64,
    install: impl FnOnce(ConsentedConsent) -> Result<(), ConsentedError>,
    restart_daemon: impl FnOnce() -> Result<(), String>,
) -> ConsentedInstallStep {
    if installed || !interactive {
        return ConsentedInstallStep::Skipped;
    }
    let Some(entry) = suggested_consented_entry(&suggest_local(hw)) else {
        return ConsentedInstallStep::Skipped;
    };
    io.writeln("");
    for fact in consent_summary_for(entry) {
        io.writeln(&fact);
        io.writeln("");
    }
    match ask_yes_no(io, INSTALL_PROMPT, false) {
        Some(true) => {
            io.writeln(INSTALL_DURATION_HINT);
            match install(ConsentedConsent::accepted_entry(entry, now_unix)) {
                Ok(()) => {
                    io.writeln(&format!("Installed {} (Consented Install).", entry.id));
                    match restart_daemon() {
                        Ok(()) => io.writeln(DAEMON_RESTARTED),
                        Err(reason) => io.writeln(&format!(
                            "Warning: could not restart the Voisu daemon ({reason}). Run `voisu service restart` to use Local."
                        )),
                    }
                    ConsentedInstallStep::Installed
                }
                Err(error) => {
                    io.writeln(&format!("Install failed: {}", consented_error_line(&error)));
                    io.writeln(LOCAL_UNAVAILABLE);
                    io.writeln(INSTALL_RETRY);
                    ConsentedInstallStep::Failed
                }
            }
        }
        Some(false) | None => ConsentedInstallStep::Declined,
    }
}

/// After Local setup, asks once whether to also configure Cloud keys.
///
/// Only an interactive user with neither Cloud key configured, and no earlier
/// No saved at `declined_marker`, is asked; the default is No. A No (including
/// Enter) is saved so later Local reruns never ask again. Returns whether the
/// Cloud key prompts should run. The ASR mode is never changed here.
pub fn ask_cloud_after_local(
    io: &mut dyn WizardIo,
    interactive: bool,
    deepgram: &KeyDiagnosis,
    groq: &KeyDiagnosis,
    declined_marker: Option<&Path>,
) -> bool {
    if !interactive
        || deepgram.is_configured()
        || groq.is_configured()
        || declined_marker.is_some_and(Path::exists)
    {
        return false;
    }
    let answer = ask_yes_no(io, CLOUD_PROMPT, false);
    if answer == Some(true) {
        return true;
    }
    io.writeln(CLOUD_SKIPPED);
    // Three invalid answers are not a decision; only a real No is remembered.
    if answer == Some(false)
        && let Some(marker) = declined_marker
        && let Err(error) = remember_cloud_declined(marker)
    {
        io.writeln(&format!(
            "Note: could not save this choice ({error}); setup may ask again."
        ));
    }
    false
}

fn remember_cloud_declined(marker: &Path) -> std::io::Result<()> {
    if let Some(parent) = marker.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(marker, b"declined\n")
}

/// Production location of [`CLOUD_DECLINED_MARKER`].
#[must_use]
pub fn production_cloud_declined_marker() -> Option<std::path::PathBuf> {
    voisu_core::state_dir()
        .ok()
        .map(|state| state.join(CLOUD_DECLINED_MARKER))
}

/// Whether the engine screen shows the Developer pilot line for `model_id`.
/// Dev-only: requires the developer Local pilot flag and a development
/// manifest, so normal setup never mentions the pilot.
#[must_use]
pub fn developer_pilot_manifest_exists(model_id: &str) -> bool {
    crate::local_model::development_local_enabled()
        && voisu_core::state_dir().is_ok_and(|state| {
            state
                .join(DEVELOPMENT_ROOT_NAME)
                .join("manifests")
                .join(format!("{model_id}.json"))
                .is_file()
        })
}

/// Production daemon restart after a successful install.
pub fn production_restart_daemon() -> Result<(), String> {
    let report = crate::service::manage_user_service(crate::service::UserServiceAction::Restart)?;
    if report.exit_code == 0 {
        Ok(())
    } else {
        Err(report
            .message
            .lines()
            .next()
            .unwrap_or("the restart did not complete")
            .to_owned())
    }
}

/// `1.2 GiB / 2.8 GiB (43%) at 25.3 MiB/s, ETA 1m 05s`, or without a total
/// `1.2 GiB at 25.3 MiB/s`. Speed and ETA read `--` until bytes have moved;
/// an ETA beyond 99 hours reads `>99h`. Never panics on any input.
#[must_use]
pub fn format_download_progress(done: u64, total: Option<u64>, elapsed: Duration) -> String {
    let speed = (done > 0 && !elapsed.is_zero()).then(|| done as f64 / elapsed.as_secs_f64());
    let rate = speed.map_or_else(
        || "-- /s".to_owned(),
        // `as u64` saturates, so an absurd rate still formats.
        |bytes_per_second| format!("{}/s", format_size(bytes_per_second as u64)),
    );
    let Some(total) = total.filter(|total| *total > 0) else {
        return format!("Downloading {} at {rate}", format_size(done));
    };
    let remaining = total.saturating_sub(done);
    let eta = match speed {
        _ if remaining == 0 => "0s".to_owned(),
        Some(bytes_per_second) => format_eta(remaining as f64 / bytes_per_second),
        None => "--".to_owned(),
    };
    format!(
        "Downloading {} / {} ({}%) at {rate}, ETA {eta}",
        format_size(done),
        format_size(total),
        percent(done, total)
    )
}

/// Longest ETA shown in full; anything longer is `>99h`.
const MAX_ETA_SECONDS: u64 = 100 * 3600 - 1;

fn format_eta(seconds: f64) -> String {
    // Rounds up so a running download never shows `0s`. Checked against the
    // bound before any integer math, so a huge or non-finite value cannot
    // overflow.
    let seconds = seconds.ceil();
    if !seconds.is_finite() || seconds > MAX_ETA_SECONDS as f64 {
        return ">99h".to_owned();
    }
    let seconds = seconds.max(0.0) as u64;
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3600 => format!("{}m {:02}s", seconds / 60, seconds % 60),
        _ => format!("{}h {:02}m", seconds / 3600, (seconds % 3600) / 60),
    }
}

fn percent(done: u64, total: u64) -> u64 {
    (u128::from(done) * 100 / u128::from(total)).min(100) as u64
}

const MIB: u64 = 1024 * 1024;
/// Width assumed when the terminal size cannot be read.
const FALLBACK_COLUMNS: usize = 80;
/// How often the render thread samples the byte counter.
const RENDER_TICK: Duration = Duration::from_millis(200);

/// Cuts `line` so a `\r` redraw never wraps: one column is left free because
/// writing the last column makes some terminals wrap early.
#[must_use]
pub fn fit_to_columns(line: &str, columns: usize) -> String {
    line.chars()
        .take(columns.saturating_sub(1).max(1))
        .collect()
}

/// Columns of the terminal on stdout, if it is one.
fn stdout_columns() -> Option<usize> {
    // SAFETY: TIOCGWINSZ only writes one `winsize` into the struct passed by
    // pointer, which lives for the whole call.
    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    let result = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut size) };
    (result == 0 && size.ws_col > 0).then_some(usize::from(size.ws_col))
}

/// Turns byte counts and phases into terminal lines. Pure and single
/// threaded, so tests drive it directly. On a TTY it redraws one line in place
/// at every whole percent, fitted to the terminal width; otherwise it prints a
/// plain line every 10%. Output is keyed to bytes, never to wall time.
struct ProgressRenderer<W: Write> {
    out: W,
    tty: bool,
    total: Option<u64>,
    columns: fn() -> Option<usize>,
    last_step: Option<u64>,
    line_open: bool,
}

impl<W: Write> ProgressRenderer<W> {
    fn new(out: W, tty: bool, total: Option<u64>, columns: fn() -> Option<usize>) -> Self {
        Self {
            out,
            tty,
            total,
            columns,
            last_step: None,
            line_open: false,
        }
    }

    fn step(&self, done: u64) -> u64 {
        match (self.total.filter(|total| *total > 0), self.tty) {
            (Some(total), true) => percent(done, total),
            (Some(total), false) => percent(done, total) / 10,
            (None, true) => done / (16 * MIB),
            (None, false) => done / (256 * MIB),
        }
    }

    // Terminal output is best effort; a closed stdout must not fail the install.
    fn bytes(&mut self, done: u64, elapsed: Duration) {
        if done == 0 {
            return;
        }
        let step = self.step(done);
        if self.last_step.is_some_and(|last| step <= last) {
            return;
        }
        self.last_step = Some(step);
        let line = format_download_progress(done, self.total, elapsed);
        if self.tty {
            let columns = (self.columns)().unwrap_or(FALLBACK_COLUMNS);
            let _ = write!(self.out, "\r{}\x1b[K", fit_to_columns(&line, columns));
            self.line_open = true;
        } else {
            let _ = writeln!(self.out, "{line}");
        }
        let _ = self.out.flush();
    }

    fn phase(&mut self, phase: InstallPhase) {
        self.finish();
        let _ = writeln!(
            self.out,
            "{}",
            match phase {
                InstallPhase::BuildingRuntime => BUILDING_RUNTIME,
                InstallPhase::CheckingGpu => CHECKING_GPU,
                InstallPhase::CheckingCpu => "Checking CPU…",
            }
        );
        let _ = self.out.flush();
    }

    /// Ends an open redraw line so the next message starts on its own line.
    fn finish(&mut self) {
        if std::mem::take(&mut self.line_open) {
            let _ = writeln!(self.out);
            let _ = self.out.flush();
        }
    }
}

/// Terminal observer for the Consented Install.
///
/// The fetch sink calls [`InstallProgress::downloaded`] on the download path,
/// so it only adds to an atomic counter; phases go over a channel. A separate
/// render thread samples both and writes to the terminal. A paused or slow
/// terminal therefore blocks only that thread and can never stall the
/// download or trip the fetch's no-progress timeout. Dropping the observer
/// stops the thread and waits for its last line.
pub struct TerminalProgress {
    done: Arc<AtomicU64>,
    phases: Option<mpsc::Sender<InstallPhase>>,
    renderer: Option<thread::JoinHandle<()>>,
}

impl TerminalProgress {
    /// Starts the render thread. `columns` is read before every TTY redraw so
    /// a resized terminal still gets a fitting line.
    pub fn spawn<W: Write + Send + 'static>(
        out: W,
        tty: bool,
        total: Option<u64>,
        columns: fn() -> Option<usize>,
    ) -> Self {
        let done = Arc::new(AtomicU64::new(0));
        let (phases, received) = mpsc::channel();
        let counter = Arc::clone(&done);
        let started = Instant::now();
        let renderer = thread::Builder::new()
            .name("voisu-progress".into())
            .spawn(move || {
                let mut renderer = ProgressRenderer::new(out, tty, total, columns);
                let sample = |renderer: &mut ProgressRenderer<W>| {
                    renderer.bytes(counter.load(Ordering::Relaxed), started.elapsed());
                };
                loop {
                    match received.recv_timeout(RENDER_TICK) {
                        Ok(phase) => {
                            sample(&mut renderer);
                            renderer.phase(phase);
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => sample(&mut renderer),
                        Err(mpsc::RecvTimeoutError::Disconnected) => {
                            sample(&mut renderer);
                            renderer.finish();
                            return;
                        }
                    }
                }
            })
            // Without a render thread the install still runs, just silently.
            .ok();
        Self {
            done,
            phases: Some(phases),
            renderer,
        }
    }

    /// Production observer on stdout.
    #[must_use]
    pub fn stdout(total: u64) -> Self {
        let stdout = std::io::stdout();
        let tty = std::io::IsTerminal::is_terminal(&stdout);
        Self::spawn(stdout, tty, Some(total), stdout_columns)
    }
}

impl InstallProgress for TerminalProgress {
    fn downloaded(&self, bytes: u64) {
        self.done.fetch_add(bytes, Ordering::Relaxed);
    }

    fn phase(&self, phase: InstallPhase) {
        if let Some(phases) = &self.phases {
            let _ = phases.send(phase);
        }
    }
}

impl Drop for TerminalProgress {
    fn drop(&mut self) {
        // Closing the channel tells the thread to draw the final count, end an
        // open line, and exit.
        drop(self.phases.take());
        if let Some(renderer) = self.renderer.take() {
            let _ = renderer.join();
        }
    }
}

/// Bakeoff-catalog repair after the Consented Install offer. Skip it when a
/// verified install already exists or was just installed.
#[must_use]
pub fn runs_catalog_repair(step: ConsentedInstallStep, already_installed: bool) -> bool {
    !already_installed && step != ConsentedInstallStep::Installed
}

pub fn run_catalog_repair_if_needed(
    io: &mut dyn WizardIo,
    step: ConsentedInstallStep,
    already_installed: bool,
    catalog: impl FnOnce(&mut dyn WizardIo) -> Result<(), String>,
) -> Result<(), String> {
    if runs_catalog_repair(step, already_installed) {
        catalog(io)
    } else {
        Ok(())
    }
}

#[must_use]
pub fn local_complete_message(verified_consented: bool, development_pilot: bool) -> String {
    if verified_consented || development_pilot {
        SETUP_COMPLETE_MESSAGE.to_owned()
    } else {
        format!("{LOCAL_UNAVAILABLE}\nRun `voisu mode cloud` to dictate with Cloud.")
    }
}

pub fn parse_local_command(args: &[String]) -> Result<LocalCommand, String> {
    match args {
        [action] if action == "remove" => Ok(LocalCommand::Remove { yes: false }),
        [action, flag] if action == "remove" && flag == "--yes" => {
            Ok(LocalCommand::Remove { yes: true })
        }
        _ => Err(REMOVE_USAGE.to_owned()),
    }
}

pub fn run_local_remove(
    io: &mut dyn WizardIo,
    interactive: bool,
    command: LocalCommand,
    mode: AsrMode,
    remove: impl FnOnce() -> Result<RemoveOutcome, ConsentedError>,
) -> LocalRemoveStatus {
    let LocalCommand::Remove { yes } = command;
    if !yes {
        if !interactive {
            return LocalRemoveStatus::Usage(REMOVE_REQUIRES_YES);
        }
        match ask_yes_no(io, REMOVE_CONFIRM, false) {
            Some(true) => {}
            Some(false) | None => return LocalRemoveStatus::Success,
        }
    }
    match remove() {
        Ok(RemoveOutcome::Removed) => {
            io.writeln(REMOVED);
            if mode == AsrMode::Local {
                io.writeln(LOCAL_UNAVAILABLE);
            }
            LocalRemoveStatus::Success
        }
        Ok(RemoveOutcome::Absent) => {
            io.writeln(NOTHING_TO_REMOVE);
            if mode == AsrMode::Local {
                io.writeln(LOCAL_UNAVAILABLE);
            }
            LocalRemoveStatus::Success
        }
        Err(ConsentedError::Leased) | Err(ConsentedError::Maintenance) => {
            LocalRemoveStatus::Failed(IN_USE.to_owned())
        }
        Err(error) => LocalRemoveStatus::Failed(consented_error_line(&error)),
    }
}

#[must_use]
pub fn consented_error_line(error: &ConsentedError) -> String {
    match error {
        ConsentedError::NoSpace => "Not enough free disk space.".into(),
        ConsentedError::Maintenance => IN_USE.into(),
        ConsentedError::Health(_) => "The installed model failed its health check.".into(),
        ConsentedError::GpuBusy => GPU_BUSY.into(),
        ConsentedError::HealthUnreaped => {
            "The health-check worker did not stop; its files were kept.".into()
        }
        ConsentedError::Fetch(_) | ConsentedError::Deadline => {
            "The download failed (network or deadline).".into()
        }
        ConsentedError::Leased => IN_USE.into(),
        ConsentedError::DifferentEntry(id) => {
            format!("{id} is already installed; run `voisu local remove` first.")
        }
        ConsentedError::PendingRemoval(id) => {
            format!("A removal of {id} is still in progress; re-run `voisu local remove`.")
        }
        ConsentedError::Consent => "Consent did not match the catalog.".into(),
        ConsentedError::Missing => NOTHING_TO_REMOVE.into(),
        ConsentedError::Receipt => "The Consented Install receipt is invalid.".into(),
        ConsentedError::Digest(_) => "A downloaded file failed its checksum.".into(),
        ConsentedError::Tree(_) => "The install tree is invalid.".into(),
        ConsentedError::Build(_) => "The pinned runtime failed to build.".into(),
        ConsentedError::Store(_) => "The model store is busy or unavailable.".into(),
        ConsentedError::Fs(_) | ConsentedError::Io(_) => {
            "A filesystem error stopped the operation.".into()
        }
        ConsentedError::Aborted => "The install was aborted.".into(),
    }
}

/// Real-store probe for the setup binary. Does not create a models directory
/// when none exists.
#[must_use]
pub fn production_consented_status() -> ConsentedStatus {
    let dir =
        match models_dir_from_state(std::env::var_os("XDG_STATE_HOME"), std::env::var_os("HOME")) {
            Ok(dir) => dir,
            Err(_) => {
                return ConsentedStatus::Unknown("the model store path is unavailable".into());
            }
        };
    if !dir.exists() {
        return ConsentedStatus::NotInstalled;
    }
    let store = match ModelStore::open(dir) {
        Ok(store) => store,
        Err(_) => return ConsentedStatus::Unknown("the model store is unavailable".into()),
    };
    match verify_consented_install(&store) {
        Ok(install) => ConsentedStatus::Installed {
            entry_id: install.receipt().entry_id.clone(),
            device: install.receipt().device.clone(),
        },
        Err(ConsentedError::Missing) => ConsentedStatus::NotInstalled,
        Err(error) => ConsentedStatus::Unknown(consented_error_line(&error)),
    }
}

/// Completes Local setup from the real store and the developer-pilot flag.
#[must_use]
pub fn production_local_complete_message() -> String {
    local_complete_message(
        matches!(
            production_consented_status(),
            ConsentedStatus::Installed { .. }
        ),
        crate::local_model::development_local_enabled(),
    )
}

/// Fetches and installs only when the setup offer calls this after an explicit yes.
pub fn production_install(consent: ConsentedConsent) -> Result<(), ConsentedError> {
    let models =
        models_dir_from_state(std::env::var_os("XDG_STATE_HOME"), std::env::var_os("HOME"))
            .map_err(ConsentedError::Store)?;
    let store = ModelStore::open(models).map_err(ConsentedError::Store)?;
    let state = voisu_core::state_dir()
        .map_err(|message| ConsentedError::Io(std::io::Error::other(message)))?;
    let fetcher = ProductionHttps;
    let builder = OfflineRuntimeBuilder;
    let entry = [&CONSENTED_ENTRY, &crate::local_model::PARAKEET_ENTRY]
        .into_iter()
        .find(|entry| {
            entry.consent_text == consent.text && entry.license_spdx == consent.license_spdx
        })
        .ok_or(ConsentedError::Consent)?;
    let health = ConsentedWorkerHealth::for_entry(state, entry);
    let maintenance = SetupMaintenance::default();
    let progress = TerminalProgress::stdout(entry.total_download_bytes);
    crate::local_model::install_consented_install(ConsentedInstallRequest {
        store: &store,
        consent,
        fetcher: &fetcher,
        builder: &builder,
        health: &health,
        maintenance: &maintenance,
        io: InstallIo::default(),
        clock: InstallClock::default(),
        progress: &progress,
    })
    .map(|_| ())
}

pub fn production_remove() -> Result<RemoveOutcome, ConsentedError> {
    let dir = models_dir_from_state(std::env::var_os("XDG_STATE_HOME"), std::env::var_os("HOME"))
        .map_err(ConsentedError::Store)?;
    if !dir.exists() {
        return Ok(RemoveOutcome::Absent);
    }
    let store = ModelStore::open(dir).map_err(ConsentedError::Store)?;
    remove_consented_from_store(&store)
}

fn remove_consented_from_store(store: &ModelStore) -> Result<RemoveOutcome, ConsentedError> {
    Ok(match remove_consented_install(store)? {
        ConsentedRemoveOutcome::Removed => RemoveOutcome::Removed,
        ConsentedRemoveOutcome::Absent => RemoveOutcome::Absent,
    })
}

const _: () = assert!(CONSENTED_ENTRY.consent_installable);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local_model::{
        CONSENT_TEXT, DISTIL_MODEL_ID, FetchError, GpuProbe, NvidiaGpu, ProgressSink,
    };
    use crate::setup_engine::{
        ConsentedStatus, EngineChoice, INSTALLED_AVAILABILITY, run_engine_screen,
    };
    use voisu_core::KeyDiagnosis;

    struct FakeIo {
        lines: Vec<Option<String>>,
        output: Vec<String>,
        prompts: usize,
        install_calls: Vec<ConsentedConsent>,
    }

    impl FakeIo {
        fn new(lines: Vec<Option<&str>>) -> Self {
            Self {
                lines: lines
                    .into_iter()
                    .map(|line| line.map(str::to_owned))
                    .collect(),
                output: Vec::new(),
                prompts: 0,
                install_calls: Vec::new(),
            }
        }

        fn transcript(&self) -> String {
            self.output.join("\n")
        }
    }

    impl WizardIo for FakeIo {
        fn writeln(&mut self, line: &str) {
            self.output.push(line.to_owned());
        }

        fn prompt_line(&mut self, prompt: &str) -> Option<String> {
            self.prompts += 1;
            self.output.push(format!("PROMPT {prompt}"));
            if self.lines.is_empty() {
                None
            } else {
                self.lines.remove(0)
            }
        }

        fn prompt_secret(&mut self, _prompt: &str) -> Option<String> {
            panic!("Consented Install must not prompt for Cloud keys");
        }
    }

    fn gpu(index: u32, total_vram_mib: u64) -> NvidiaGpu {
        NvidiaGpu {
            index,
            name: "RTX 5050 Laptop".into(),
            total_vram_mib,
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

    const NOW: u64 = 1_700_000_000;

    fn offer(
        hw: &HostHardware,
        interactive: bool,
        installed: bool,
        answers: Vec<Option<&str>>,
        result: Result<(), ConsentedError>,
    ) -> (ConsentedInstallStep, FakeIo) {
        let (step, io, _) = offer_with_restart(hw, interactive, installed, answers, result, Ok(()));
        (step, io)
    }

    fn offer_with_restart(
        hw: &HostHardware,
        interactive: bool,
        installed: bool,
        answers: Vec<Option<&str>>,
        result: Result<(), ConsentedError>,
        restart: Result<(), String>,
    ) -> (ConsentedInstallStep, FakeIo, usize) {
        let mut io = FakeIo::new(answers);
        let mut recorded = Vec::new();
        let mut restarts = 0;
        let step = run_consented_install_step(
            &mut io,
            hw,
            interactive,
            installed,
            NOW,
            |consent| {
                recorded.push(consent);
                match &result {
                    Ok(()) => Ok(()),
                    Err(ConsentedError::NoSpace) => Err(ConsentedError::NoSpace),
                    Err(ConsentedError::Maintenance) => Err(ConsentedError::Maintenance),
                    Err(ConsentedError::Health(message)) => {
                        Err(ConsentedError::Health(message.clone()))
                    }
                    Err(ConsentedError::GpuBusy) => Err(ConsentedError::GpuBusy),
                    Err(ConsentedError::Fetch(error)) => Err(ConsentedError::Fetch(error.clone())),
                    Err(_) => Err(ConsentedError::NoSpace),
                }
            },
            || {
                restarts += 1;
                restart
            },
        );
        io.install_calls = recorded;
        (step, io, restarts)
    }

    const SUMMARY_BLOCK: &str = "\nDownload size: 2.8 GiB\n\nEnglish only\n\nNeeds an NVIDIA GPU\n\nRuns offline after install\n\nPROMPT Download and install now? [y/N] ";

    #[test]
    fn rtx_5050_offers_install_and_decline_never_fetches() {
        let hw = host(GpuProbe::Nvidia(vec![gpu(0, 8151)]));
        let (step, io) = offer(&hw, true, false, vec![Some("n")], Ok(()));
        assert_eq!(step, ConsentedInstallStep::Declined);
        assert!(
            io.transcript().starts_with(SUMMARY_BLOCK),
            "{}",
            io.transcript()
        );
        assert!(
            !io.transcript().contains(CONSENT_TEXT),
            "{}",
            io.transcript()
        );
        assert!(io.install_calls.is_empty(), "{:?}", io.install_calls);
        assert!(
            !io.transcript().contains(INSTALL_DURATION_HINT),
            "{}",
            io.transcript()
        );
    }

    #[test]
    fn rtx_5050_yes_installs_once_with_exact_consent() {
        let hw = host(GpuProbe::Nvidia(vec![gpu(0, 8151)]));
        let (step, io) = offer(&hw, true, false, vec![Some("y")], Ok(()));
        assert_eq!(step, ConsentedInstallStep::Installed);
        assert_eq!(io.install_calls, vec![ConsentedConsent::accepted(NOW)]);
        // The hashed consent record keeps the full catalog text even though
        // the terminal shows only the short summary.
        assert_eq!(io.install_calls[0].text, CONSENT_TEXT);
        assert_eq!(
            io.install_calls[0].license_spdx,
            CONSENTED_ENTRY.license_spdx
        );
        assert!(
            io.transcript().contains(INSTALL_DURATION_HINT),
            "{}",
            io.transcript()
        );
        assert!(
            io.transcript().contains(INSTALL_SUCCESS),
            "{}",
            io.transcript()
        );
    }

    #[test]
    fn cpu_suggestion_offers_exact_parakeet_consent_and_decline_does_not_fetch() {
        for hw in [
            host(GpuProbe::Nvidia(vec![gpu(0, 2048)])),
            host(GpuProbe::NoneFound),
        ] {
            let (step, io) = offer(&hw, true, false, vec![Some("n")], Ok(()));
            assert_eq!(step, ConsentedInstallStep::Declined);
            assert!(io.install_calls.is_empty());
            let text = io.transcript();
            for expected in [
                "734.1 MiB",
                "English only",
                "CC-BY-4.0",
                "Runs offline on CPU",
                "Development evidence",
                "not the release gate",
            ] {
                assert!(text.contains(expected), "{text}");
            }
            let (step, io) = offer(&hw, true, false, vec![Some("y")], Ok(()));
            assert_eq!(step, ConsentedInstallStep::Installed);
            assert_eq!(
                io.install_calls,
                vec![ConsentedConsent::accepted_entry(
                    &crate::local_model::PARAKEET_ENTRY,
                    NOW
                )]
            );
            assert!(io.transcript().contains("Installed parakeet-tdt-0.6b-v3"));
        }
    }

    #[test]
    fn verified_install_does_not_offer_again() {
        let hw = host(GpuProbe::Nvidia(vec![gpu(0, 8151)]));
        let mut engine = FakeIo::new(vec![]);
        let choice = run_engine_screen(
            &mut engine,
            AsrMode::Cloud,
            &hw,
            &KeyDiagnosis::Absent,
            &KeyDiagnosis::Absent,
            false,
            |_| false,
            ConsentedStatus::Installed {
                entry_id: CONSENTED_ENTRY.id.into(),
                device: "cuda:0".into(),
            },
        );
        assert_eq!(choice, EngineChoice::Keep(AsrMode::Cloud));
        assert!(
            engine.transcript().contains(INSTALLED_AVAILABILITY),
            "{}",
            engine.transcript()
        );
        let (step, io) = offer(&hw, true, true, vec![Some("y")], Ok(()));
        assert_eq!(step, ConsentedInstallStep::Skipped);
        assert!(io.install_calls.is_empty());
        assert_eq!(io.prompts, 0);
    }

    #[test]
    fn install_errors_print_the_failure_lines() {
        let hw = host(GpuProbe::Nvidia(vec![gpu(0, 8151)]));
        let cases = [
            (
                ConsentedError::NoSpace,
                "Install failed: Not enough free disk space.",
            ),
            (
                ConsentedError::Maintenance,
                "Install failed: The Consented Install is in use; stop dictation or run `voisu service stop` first.",
            ),
            (
                ConsentedError::Fetch(FetchError::Io("reset".into())),
                "Install failed: The download failed (network or deadline).",
            ),
            (
                ConsentedError::Health("cuda".into()),
                "Install failed: The installed model failed its health check.",
            ),
            (
                ConsentedError::GpuBusy,
                "Install failed: Another Local worker is using the GPU; stop it (for example `voisu service stop`, or stop voisu-dev.service), then run `voisu setup` again.",
            ),
        ];
        for (error, line) in cases {
            let (step, io) = offer(&hw, true, false, vec![Some("yes")], Err(error));
            assert_eq!(step, ConsentedInstallStep::Failed);
            let text = io.transcript();
            assert!(text.contains(line), "{line}: {text}");
            assert!(text.contains(LOCAL_UNAVAILABLE), "{text}");
            assert!(text.contains(INSTALL_RETRY), "{text}");
            assert_eq!(io.install_calls, vec![ConsentedConsent::accepted(NOW)]);
        }
    }

    #[test]
    fn non_interactive_never_prompts_or_installs() {
        let hw = host(GpuProbe::Nvidia(vec![gpu(0, 8151)]));
        let leftover = vec![Some("y")];
        let (step, io) = offer(&hw, false, false, leftover, Ok(()));
        assert_eq!(step, ConsentedInstallStep::Skipped);
        assert_eq!(io.prompts, 0);
        assert!(io.install_calls.is_empty());
        assert!(
            !io.transcript().contains("Download and install"),
            "{}",
            io.transcript()
        );
        assert!(
            !io.transcript().contains("Download size"),
            "{}",
            io.transcript()
        );
    }

    #[test]
    fn eof_and_enter_are_declines() {
        let hw = host(GpuProbe::Nvidia(vec![gpu(0, 8151)]));
        for answers in [vec![], vec![Some("")]] {
            let (step, io, restarts) =
                offer_with_restart(&hw, true, false, answers, Ok(()), Ok(()));
            assert_eq!(step, ConsentedInstallStep::Declined);
            assert!(io.install_calls.is_empty());
            assert_eq!(restarts, 0);
            assert!(
                io.transcript().contains("Download and install now? [y/N] "),
                "{}",
                io.transcript()
            );
        }
    }

    #[test]
    fn successful_install_restarts_the_daemon_once() {
        let hw = host(GpuProbe::Nvidia(vec![gpu(0, 8151)]));
        let (step, io, restarts) =
            offer_with_restart(&hw, true, false, vec![Some("y")], Ok(()), Ok(()));
        assert_eq!(step, ConsentedInstallStep::Installed);
        assert_eq!(restarts, 1);
        assert_eq!(io.output.last().map(String::as_str), Some(DAEMON_RESTARTED));
    }

    #[test]
    fn failed_restart_warns_with_the_manual_command_and_keeps_the_install() {
        let hw = host(GpuProbe::Nvidia(vec![gpu(0, 8151)]));
        let (step, io, restarts) = offer_with_restart(
            &hw,
            true,
            false,
            vec![Some("y")],
            Ok(()),
            Err("voisu.service is not installed".into()),
        );
        assert_eq!(step, ConsentedInstallStep::Installed);
        assert_eq!(restarts, 1);
        assert_eq!(
            io.output.last().map(String::as_str),
            Some(
                "Warning: could not restart the Voisu daemon (voisu.service is not installed). Run `voisu service restart` to use Local."
            )
        );
        assert!(
            io.transcript().contains(INSTALL_SUCCESS),
            "{}",
            io.transcript()
        );
    }

    #[test]
    fn failed_install_never_restarts_the_daemon() {
        let hw = host(GpuProbe::Nvidia(vec![gpu(0, 8151)]));
        let (step, _, restarts) = offer_with_restart(
            &hw,
            true,
            false,
            vec![Some("y")],
            Err(ConsentedError::NoSpace),
            Ok(()),
        );
        assert_eq!(step, ConsentedInstallStep::Failed);
        assert_eq!(restarts, 0);
    }

    fn ask_cloud(
        interactive: bool,
        deepgram: KeyDiagnosis,
        groq: KeyDiagnosis,
        marker: &Path,
        answers: Vec<Option<&str>>,
    ) -> (bool, FakeIo) {
        let mut io = FakeIo::new(answers);
        let wants_cloud =
            ask_cloud_after_local(&mut io, interactive, &deepgram, &groq, Some(marker));
        (wants_cloud, io)
    }

    fn marker() -> (tempfile::TempDir, std::path::PathBuf) {
        let state = tempfile::tempdir().unwrap();
        let marker = state.path().join("voisu").join(CLOUD_DECLINED_MARKER);
        (state, marker)
    }

    #[test]
    fn cloud_question_no_is_remembered_and_never_asked_again() {
        for answers in [vec![Some("n")], vec![Some("")], vec![]] {
            let (_state, marker) = marker();
            let (wants_cloud, io) = ask_cloud(
                true,
                KeyDiagnosis::Absent,
                KeyDiagnosis::Absent,
                &marker,
                answers,
            );
            assert!(!wants_cloud);
            assert_eq!(
                io.output,
                vec![
                    "PROMPT Also set up Cloud? [y/N] ".to_owned(),
                    CLOUD_SKIPPED.to_owned()
                ]
            );
            assert!(marker.is_file());
            // A later Local rerun, still without keys, does not ask.
            let (wants_cloud, rerun) = ask_cloud(
                true,
                KeyDiagnosis::Absent,
                KeyDiagnosis::Absent,
                &marker,
                vec![Some("y")],
            );
            assert!(!wants_cloud);
            assert_eq!(rerun.prompts, 0);
            assert!(rerun.output.is_empty(), "{:?}", rerun.output);
        }
    }

    #[test]
    fn cloud_question_yes_continues_to_the_key_prompts_and_is_not_remembered() {
        let (_state, marker) = marker();
        for _ in 0..2 {
            let (wants_cloud, io) = ask_cloud(
                true,
                KeyDiagnosis::Absent,
                KeyDiagnosis::Absent,
                &marker,
                vec![Some("y")],
            );
            assert!(wants_cloud);
            assert_eq!(io.prompts, 1);
            assert!(
                !io.transcript().contains(CLOUD_SKIPPED),
                "{}",
                io.transcript()
            );
        }
        assert!(!marker.exists());
    }

    #[test]
    fn cloud_question_without_a_valid_answer_is_not_remembered() {
        let (_state, marker) = marker();
        let (wants_cloud, _) = ask_cloud(
            true,
            KeyDiagnosis::Absent,
            KeyDiagnosis::Absent,
            &marker,
            vec![Some("maybe"), Some("later"), Some("?")],
        );
        assert!(!wants_cloud);
        assert!(!marker.exists());
    }

    #[test]
    fn cloud_decline_that_cannot_be_saved_says_setup_may_ask_again() {
        let state = tempfile::tempdir().unwrap();
        let file = state.path().join("not-a-dir");
        std::fs::write(&file, b"").unwrap();
        let marker = file.join(CLOUD_DECLINED_MARKER);
        let (wants_cloud, io) = ask_cloud(
            true,
            KeyDiagnosis::Absent,
            KeyDiagnosis::Absent,
            &marker,
            vec![Some("n")],
        );
        assert!(!wants_cloud);
        assert!(
            io.output
                .last()
                .is_some_and(|line| line.ends_with("setup may ask again.")),
            "{:?}",
            io.output
        );
    }

    #[test]
    fn cloud_question_is_not_asked_with_a_key_or_without_a_terminal() {
        let key = || KeyDiagnosis::Found {
            location: voisu_core::KeyLocation::Keyring,
            credential: voisu_core::Credential::new("test-key".into()).unwrap(),
        };
        let (_state, marker) = marker();
        for (interactive, deepgram, groq) in [
            (true, key(), KeyDiagnosis::Absent),
            (true, KeyDiagnosis::Absent, key()),
            (false, KeyDiagnosis::Absent, KeyDiagnosis::Absent),
        ] {
            let (wants_cloud, io) =
                ask_cloud(interactive, deepgram, groq, &marker, vec![Some("y")]);
            assert!(!wants_cloud);
            assert_eq!(io.prompts, 0);
            assert!(io.output.is_empty(), "{:?}", io.output);
        }
        assert!(!marker.exists());
    }

    #[test]
    fn download_progress_formats_percent_speed_and_eta() {
        const GIB: u64 = 1024 * 1024 * 1024;
        assert_eq!(
            format_download_progress(GIB, Some(3 * GIB), Duration::from_secs(64)),
            "Downloading 1.0 GiB / 3.0 GiB (33%) at 16.0 MiB/s, ETA 2m 08s"
        );
        assert_eq!(
            format_download_progress(3 * GIB, Some(3 * GIB), Duration::from_secs(100)),
            "Downloading 3.0 GiB / 3.0 GiB (100%) at 30.7 MiB/s, ETA 0s"
        );
        assert_eq!(
            format_download_progress(MIB, Some(GIB), Duration::from_secs(1)),
            "Downloading 1.0 MiB / 1.0 GiB (0%) at 1.0 MiB/s, ETA 17m 03s"
        );
        assert_eq!(
            format_download_progress(MIB, Some(4 * GIB), Duration::from_secs(1)),
            "Downloading 1.0 MiB / 4.0 GiB (0%) at 1.0 MiB/s, ETA 1h 08m"
        );
    }

    #[test]
    fn download_progress_without_movement_or_total() {
        assert_eq!(
            format_download_progress(0, Some(10 * MIB), Duration::ZERO),
            "Downloading 0 B / 10.0 MiB (0%) at -- /s, ETA --"
        );
        assert_eq!(
            format_download_progress(5 * MIB, None, Duration::from_secs(2)),
            "Downloading 5.0 MiB at 2.5 MiB/s"
        );
        assert_eq!(
            format_download_progress(5 * MIB, Some(0), Duration::ZERO),
            "Downloading 5.0 MiB at -- /s"
        );
    }

    #[test]
    fn download_progress_boundaries_never_panic() {
        const GIB: u64 = 1024 * 1024 * 1024;
        assert_eq!(
            format_download_progress(1, Some(u64::MAX), Duration::from_secs(2)),
            "Downloading 1 B / 17179869184.0 GiB (0%) at 0 B/s, ETA >99h"
        );
        assert_eq!(
            format_download_progress(u64::MAX, Some(u64::MAX), Duration::from_nanos(1)),
            "Downloading 17179869184.0 GiB / 17179869184.0 GiB (100%) at 17179869184.0 GiB/s, ETA 0s"
        );
        assert_eq!(
            format_download_progress(2 * GIB, Some(GIB), Duration::from_secs(1)),
            "Downloading 2.0 GiB / 1.0 GiB (100%) at 2.0 GiB/s, ETA 0s"
        );
        assert_eq!(
            format_download_progress(0, Some(GIB), Duration::from_secs(5)),
            "Downloading 0 B / 1.0 GiB (0%) at -- /s, ETA --"
        );
        assert_eq!(
            format_download_progress(u64::MAX, None, Duration::ZERO),
            "Downloading 17179869184.0 GiB at -- /s"
        );
    }

    #[test]
    fn eta_rounds_up_and_caps_at_ninety_nine_hours() {
        assert_eq!(format_eta(0.2), "1s");
        assert_eq!(format_eta(59.0), "59s");
        assert_eq!(format_eta(60.0), "1m 00s");
        assert_eq!(format_eta(359_999.0), "99h 59m");
        assert_eq!(format_eta(359_999.5), ">99h");
        assert_eq!(format_eta(f64::INFINITY), ">99h");
        assert_eq!(format_eta(f64::MAX), ">99h");
    }

    #[test]
    fn progress_line_is_cut_to_the_terminal_width() {
        assert_eq!(fit_to_columns("abcdef", 4), "abc");
        assert_eq!(fit_to_columns("abc", 80), "abc");
        assert_eq!(fit_to_columns("abc", 0), "a");
        let mut renderer = ProgressRenderer::new(Vec::new(), true, Some(4 * MIB), || Some(30));
        for done in 1..=4 {
            renderer.bytes(done * MIB, Duration::from_secs(1));
        }
        renderer.finish();
        let text = String::from_utf8(renderer.out).unwrap();
        let redraws: Vec<_> = text
            .split('\r')
            .skip(1)
            .map(|redraw| redraw.split("\x1b[K").next().unwrap())
            .collect();
        assert_eq!(redraws.len(), 4, "{text:?}");
        for redraw in redraws {
            assert_eq!(redraw.chars().count(), 29, "{redraw:?}");
            assert!(redraw.starts_with("Downloading "), "{redraw:?}");
        }
    }

    /// Feeds cumulative byte counts at a fixed one-second elapsed time.
    fn render(
        tty: bool,
        total: Option<u64>,
        drive: impl FnOnce(&mut ProgressRenderer<Vec<u8>>),
    ) -> String {
        let mut renderer = ProgressRenderer::new(Vec::new(), tty, total, || None);
        drive(&mut renderer);
        String::from_utf8(renderer.out).unwrap()
    }

    #[test]
    fn non_tty_progress_prints_a_plain_line_every_ten_percent_then_phases() {
        let text = render(false, Some(100 * MIB), |renderer| {
            for done in 1..=100 {
                renderer.bytes(done * MIB, Duration::from_secs(1));
            }
            renderer.phase(InstallPhase::BuildingRuntime);
            renderer.phase(InstallPhase::CheckingGpu);
        });
        assert!(!text.contains('\r'), "{text:?}");
        let lines: Vec<_> = text.lines().collect();
        assert_eq!(lines.len(), 11 + 2, "{text}");
        assert!(lines[0].contains("(1%)"), "{text}");
        assert!(lines[1].contains("(10%)"), "{text}");
        assert!(lines[10].contains("(100%)"), "{text}");
        assert_eq!(lines[11..], [BUILDING_RUNTIME, CHECKING_GPU]);
    }

    #[test]
    fn tty_progress_redraws_in_place_and_ends_the_line_before_a_phase() {
        let text = render(true, Some(4 * MIB), |renderer| {
            for done in 1..=4 {
                renderer.bytes(done * MIB, Duration::from_secs(done));
            }
            renderer.phase(InstallPhase::BuildingRuntime);
        });
        assert_eq!(text.matches('\r').count(), 4, "{text:?}");
        assert!(
            text.ends_with("(100%) at 1.0 MiB/s, ETA 0s\x1b[K\nBuilding runtime…\n"),
            "{text:?}"
        );
    }

    #[test]
    fn tty_progress_finish_closes_an_interrupted_line_once() {
        let text = render(true, Some(4 * MIB), |renderer| {
            renderer.bytes(MIB, Duration::from_secs(1));
            renderer.finish();
            renderer.finish();
        });
        assert!(
            text.ends_with("(25%) at 1.0 MiB/s, ETA 3s\x1b[K\n"),
            "{text:?}"
        );
    }

    /// A terminal writer that blocks every write until released, like a
    /// terminal paused with Ctrl-S.
    #[derive(Clone, Default)]
    struct PausedTerminal {
        open: Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>,
        bytes: Arc<std::sync::Mutex<Vec<u8>>>,
    }

    impl PausedTerminal {
        fn release(&self) {
            let (open, changed) = &*self.open;
            *open.lock().unwrap() = true;
            changed.notify_all();
        }

        fn text(&self) -> String {
            String::from_utf8(self.bytes.lock().unwrap().clone()).unwrap()
        }
    }

    impl Write for PausedTerminal {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let (open, changed) = &*self.open;
            let mut released = open.lock().unwrap();
            while !*released {
                released = changed.wait(released).unwrap();
            }
            drop(released);
            self.bytes.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn paused_terminal_never_stalls_the_download_sink() {
        struct CountingFile(usize);
        impl Write for CountingFile {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0 += buf.len();
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let terminal = PausedTerminal::default();
        let reporter = TerminalProgress::spawn(terminal.clone(), false, Some(100 * MIB), || None);
        let chunk = vec![0_u8; 64 * 1024];
        let (finished, sink_done) = mpsc::channel();
        thread::scope(|scope| {
            scope.spawn(|| {
                let mut file = CountingFile(0);
                let mut sink = ProgressSink::new(&mut file, &reporter);
                let written = (0..1600).try_for_each(|_| sink.write_all(&chunk));
                reporter.phase(InstallPhase::BuildingRuntime);
                reporter.phase(InstallPhase::CheckingGpu);
                let _ = finished.send((written, file.0));
            });
            // Generous bound: the sink only touches an atomic, so this returns
            // in milliseconds unless the terminal can block it.
            let outcome: Result<(std::io::Result<()>, usize), _> =
                sink_done.recv_timeout(Duration::from_secs(60));
            let printed_while_paused = terminal.text();
            terminal.release();
            let (written, bytes) = outcome.expect("a paused terminal stalled the download sink");
            written.expect("the sink reported every write as successful");
            assert_eq!(bytes as u64, 100 * MIB);
            assert_eq!(printed_while_paused, "");
        });
        drop(reporter);
        let text = terminal.text();
        assert!(text.contains("(100%)"), "{text}");
        assert!(
            text.ends_with("Building runtime…\nChecking GPU…\n"),
            "{text}"
        );
    }

    #[test]
    fn terminal_progress_totals_bytes_across_files() {
        let terminal = PausedTerminal::default();
        terminal.release();
        let reporter = TerminalProgress::spawn(terminal.clone(), false, None, || None);
        reporter.downloaded(200 * MIB);
        reporter.downloaded(100 * MIB);
        drop(reporter);
        let text = terminal.text();
        assert!(
            text.lines()
                .last()
                .is_some_and(|line| line.starts_with("Downloading 300.0 MiB at")),
            "{text}"
        );
    }

    #[test]
    fn rtx_5050_setup_transcript_decline_and_accept() {
        let hw = host(GpuProbe::Nvidia(vec![gpu(0, 8151)]));
        // The consent prompt is the only confirmation of the Cloud -> Local switch.
        let mut decline = FakeIo::new(vec![Some("2"), Some("n")]);
        let choice = run_engine_screen(
            &mut decline,
            AsrMode::Cloud,
            &hw,
            &KeyDiagnosis::Absent,
            &KeyDiagnosis::Absent,
            true,
            |_| false,
            ConsentedStatus::NotInstalled,
        );
        assert_eq!(choice, EngineChoice::Switch(AsrMode::Local));
        let step = run_consented_install_step(
            &mut decline,
            &hw,
            true,
            false,
            NOW,
            |_| panic!("decline must not install"),
            || panic!("decline must not restart the daemon"),
        );
        assert_eq!(step, ConsentedInstallStep::Declined);
        let declined = decline.transcript();
        assert!(
            declined.contains(&format!(
                "Recommended for this computer: {DISTIL_MODEL_ID} (RTX 5050 Laptop)"
            )),
            "{declined}"
        );
        assert!(declined.contains(SUMMARY_BLOCK), "{declined}");
        assert!(!declined.contains(CONSENT_TEXT), "{declined}");
        assert!(!declined.contains(INSTALL_SUCCESS), "{declined}");
        assert!(!declined.contains("Switch to Local anyway?"), "{declined}");
        assert_eq!(decline.prompts, 2, "engine choice, then consent");

        let mut accept = FakeIo::new(vec![Some("2"), Some("y")]);
        let choice = run_engine_screen(
            &mut accept,
            AsrMode::Cloud,
            &hw,
            &KeyDiagnosis::Absent,
            &KeyDiagnosis::Absent,
            true,
            |_| false,
            ConsentedStatus::NotInstalled,
        );
        assert_eq!(choice, EngineChoice::Switch(AsrMode::Local));
        let mut consents = Vec::new();
        let step = run_consented_install_step(
            &mut accept,
            &hw,
            true,
            false,
            NOW,
            |consent| {
                consents.push(consent);
                Ok(())
            },
            || Ok(()),
        );
        assert_eq!(step, ConsentedInstallStep::Installed);
        assert_eq!(consents, vec![ConsentedConsent::accepted(NOW)]);
        let accepted = accept.transcript();
        assert!(accepted.contains(SUMMARY_BLOCK), "{accepted}");
        assert!(accepted.contains(DAEMON_RESTARTED), "{accepted}");
        assert!(accepted.contains(INSTALL_DURATION_HINT), "{accepted}");
        assert!(accepted.contains(INSTALL_SUCCESS), "{accepted}");
    }

    #[test]
    fn complete_message_treats_a_verified_install_as_setup_complete() {
        assert_eq!(local_complete_message(true, false), SETUP_COMPLETE_MESSAGE);
        assert_eq!(local_complete_message(false, true), SETUP_COMPLETE_MESSAGE);
        assert_eq!(
            local_complete_message(false, false),
            format!("{LOCAL_UNAVAILABLE}\nRun `voisu mode cloud` to dictate with Cloud.")
        );
    }

    #[test]
    fn parse_local_remove_and_yes() {
        assert_eq!(
            parse_local_command(&["remove".into()]).unwrap(),
            LocalCommand::Remove { yes: false }
        );
        assert_eq!(
            parse_local_command(&["remove".into(), "--yes".into()]).unwrap(),
            LocalCommand::Remove { yes: true }
        );
        assert_eq!(parse_local_command(&[]).unwrap_err(), REMOVE_USAGE);
        assert_eq!(
            parse_local_command(&["remove".into(), "--yes".into(), "extra".into()]).unwrap_err(),
            REMOVE_USAGE
        );
        assert_eq!(
            parse_local_command(&["install".into()]).unwrap_err(),
            REMOVE_USAGE
        );
    }

    #[test]
    fn local_remove_interactive_confirm_defaults_to_no() {
        let mut io = FakeIo::new(vec![Some("")]);
        let mut removed = false;
        let status = run_local_remove(
            &mut io,
            true,
            LocalCommand::Remove { yes: false },
            AsrMode::Local,
            || {
                removed = true;
                Ok(RemoveOutcome::Removed)
            },
        );
        assert_eq!(status, LocalRemoveStatus::Success);
        assert!(!removed);
        assert!(
            io.transcript()
                .contains("Remove the installed Consented model? [y/N]"),
            "{}",
            io.transcript()
        );
        assert!(!io.transcript().contains(REMOVED), "{}", io.transcript());
    }

    #[test]
    fn local_remove_non_interactive_requires_yes() {
        let mut io = FakeIo::new(vec![]);
        let mut removed = false;
        let status = run_local_remove(
            &mut io,
            false,
            LocalCommand::Remove { yes: false },
            AsrMode::Cloud,
            || {
                removed = true;
                Ok(RemoveOutcome::Removed)
            },
        );
        assert_eq!(status, LocalRemoveStatus::Usage(REMOVE_REQUIRES_YES));
        assert!(!removed);
        assert_eq!(io.prompts, 0);
    }

    #[test]
    fn local_remove_yes_prints_what_was_removed() {
        let mut io = FakeIo::new(vec![]);
        let status = run_local_remove(
            &mut io,
            false,
            LocalCommand::Remove { yes: true },
            AsrMode::Local,
            || Ok(RemoveOutcome::Removed),
        );
        assert_eq!(status, LocalRemoveStatus::Success);
        let text = io.transcript();
        assert!(text.contains(REMOVED), "{text}");
        assert!(text.contains(LOCAL_UNAVAILABLE), "{text}");
        assert_eq!(io.prompts, 0);
    }

    #[test]
    fn gpu_busy_prints_the_setup_line() {
        assert_eq!(consented_error_line(&ConsentedError::GpuBusy), GPU_BUSY);
    }

    #[test]
    fn catalog_repair_runs_only_without_a_verified_consented_install() {
        assert!(!runs_catalog_repair(ConsentedInstallStep::Installed, false));
        assert!(!runs_catalog_repair(ConsentedInstallStep::Skipped, true));
        assert!(runs_catalog_repair(ConsentedInstallStep::Declined, false));
        assert!(runs_catalog_repair(ConsentedInstallStep::Skipped, false));
    }

    #[test]
    fn after_installed_catalog_repair_is_not_run() {
        use crate::local_setup::{LocalSetupActions, run_with};

        struct EmptyCatalog;
        impl LocalSetupActions for EmptyCatalog {
            fn catalog_lines(&self) -> Vec<String> {
                vec!["whisper-cpp-ggml-base.en".into()]
            }
            fn active_identity(&self) -> Option<String> {
                None
            }
            fn retained_identity(&self) -> Option<String> {
                None
            }
            fn restore_retained(&mut self) -> Result<String, String> {
                panic!("catalog repair must not restore after Consented Install")
            }
            fn install_fixture(
                &mut self,
                _consent: crate::local_model::InstallConsent,
            ) -> Result<String, String> {
                panic!("catalog repair must not install after Consented Install")
            }
        }

        let hw = host(GpuProbe::Nvidia(vec![gpu(0, 8151)]));
        let mut io = FakeIo::new(vec![Some("y")]);
        let step =
            run_consented_install_step(&mut io, &hw, true, false, NOW, |_| Ok(()), || Ok(()));
        assert_eq!(step, ConsentedInstallStep::Installed);
        let mut catalog_ran = false;
        run_catalog_repair_if_needed(&mut io, step, false, |io| {
            catalog_ran = true;
            run_with(io, &mut EmptyCatalog).map(|_| ())
        })
        .unwrap();
        assert!(!catalog_ran);
        let text = io.transcript();
        assert!(text.contains(INSTALL_SUCCESS), "{text}");
        assert!(!text.contains("Active model: none"), "{text}");
    }

    fn test_store() -> (tempfile::TempDir, ModelStore) {
        let temp = tempfile::tempdir().unwrap();
        let root = models_dir_from_state(Some(temp.path().as_os_str().to_owned()), None).unwrap();
        let store = ModelStore::open(root).unwrap();
        (temp, store)
    }

    fn removal_receipt() -> crate::local_model::ConsentedReceipt {
        let mut receipt = crate::local_model::ConsentedReceipt {
            version: 1,
            entry_id: CONSENTED_ENTRY.id.into(),
            revision: CONSENTED_ENTRY.revision.into(),
            content_hash: "a".repeat(64),
            consent: ConsentedConsent {
                text: String::new(),
                license_spdx: String::new(),
                unix_time: 0,
            },
            consent_text_sha256: String::new(),
            files: std::collections::BTreeMap::new(),
            runtime_inputs: std::collections::BTreeMap::new(),
            runtime_manifest: std::collections::BTreeMap::new(),
            python_path: std::path::PathBuf::from("runtime/python"),
            venv_path: std::path::PathBuf::from("runtime/venv"),
            device: "cuda:0".into(),
            compute_type: "int8_float16".into(),
            receipt_hash: String::new(),
        };
        receipt.receipt_hash =
            crate::local_model::sha256_hex(&serde_json::to_vec(&receipt).unwrap());
        receipt
    }

    #[test]
    fn remove_without_receipt_is_absent() {
        let (_temp, store) = test_store();
        assert_eq!(
            remove_consented_from_store(&store).unwrap(),
            RemoveOutcome::Absent
        );
    }

    #[test]
    fn remove_with_receipt_is_removed_without_verifying() {
        let (_temp, store) = test_store();
        let receipt = removal_receipt();
        std::fs::write(
            store.root().join("private/consented.receipt.json"),
            serde_json::to_vec(&receipt).unwrap(),
        )
        .unwrap();
        assert_eq!(
            remove_consented_from_store(&store).unwrap(),
            RemoveOutcome::Removed
        );
        assert!(!store.root().join("private/consented.receipt.json").exists());
    }

    #[test]
    fn remove_resumes_journaled_tree_without_receipt_and_reports_removed() {
        let (_temp, store) = test_store();
        let receipt = removal_receipt();
        let root = store
            .root()
            .join("consented")
            .join(&receipt.entry_id)
            .join(&receipt.content_hash);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("remaining-file"), b"partial removal").unwrap();
        let journal = store.root().join("private/consented.removal.json");
        std::fs::write(&journal, serde_json::to_vec(&receipt).unwrap()).unwrap();
        assert!(!store.root().join("private/consented.receipt.json").exists());

        assert_eq!(
            remove_consented_from_store(&store).unwrap(),
            RemoveOutcome::Removed
        );
        assert!(!root.exists());
        assert!(!journal.exists());
    }

    #[test]
    fn remove_rejects_a_malformed_receipt() {
        let (_temp, store) = test_store();
        std::fs::write(
            store.root().join("private/consented.receipt.json"),
            b"not-a-receipt",
        )
        .unwrap();
        let error = remove_consented_from_store(&store).unwrap_err();
        assert!(matches!(error, ConsentedError::Receipt));
        assert_eq!(
            consented_error_line(&error),
            "The Consented Install receipt is invalid."
        );
        assert!(store.root().join("private/consented.receipt.json").exists());
    }

    #[test]
    fn local_remove_leased_tells_the_user_to_stop_the_service() {
        let mut io = FakeIo::new(vec![]);
        let status = run_local_remove(
            &mut io,
            false,
            LocalCommand::Remove { yes: true },
            AsrMode::Local,
            || Err(ConsentedError::Leased),
        );
        assert_eq!(status, LocalRemoveStatus::Failed(IN_USE.to_owned()));
        assert!(!io.transcript().contains(REMOVED), "{}", io.transcript());
    }
}
