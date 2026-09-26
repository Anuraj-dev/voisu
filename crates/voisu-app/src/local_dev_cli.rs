//! Development-only `voisu local-dev` commands.
//!
//! Register, select, and inspect private development manifests, then replay a
//! pinned WAV through the supervised development worker. Production catalog
//! selection is never written.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde::Serialize;
use voisu_core::{Command, DaemonState, Request, Response, WavScan, scan_wav_pcm};

use crate::local_model::{
    DevManifest, DevModelStatus, DevelopmentError, DevelopmentStore, development_receipt,
    production_selection, shipped_catalog,
};
#[cfg(test)]
use crate::local_worker::FakeWorker;
use crate::local_worker::{
    Correlation, DevWorker, InferenceMetrics, STOP_PROCESSING, TranscribeRequest, WorkerChild,
    WorkerOutcome, WorkerSupervisor, refuse_if_development_worker_loaded,
};

const USAGE: &str = "\
usage: voisu local-dev register --manifest <path>
       voisu local-dev select <model-id>
       voisu local-dev status [--json]
       voisu local-dev replay --json --manifest <path> --audio <wav> [--audio <wav> ...] [--timeout-ms <milliseconds>]";
static TERMINATE_REPLAY: AtomicBool = AtomicBool::new(false);

extern "C" fn request_replay_termination(_: libc::c_int) {
    TERMINATE_REPLAY.store(true, Ordering::Relaxed);
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LocalDevCommand {
    Register {
        manifest: PathBuf,
    },
    Select {
        model_id: String,
    },
    Status {
        json: bool,
    },
    Replay {
        manifest: PathBuf,
        audios: Vec<PathBuf>,
        timeout_ms: u64,
    },
}

#[derive(Clone, Debug, Serialize)]
pub struct ReplayReport {
    pub version: u32,
    pub model_id: String,
    pub language: String,
    pub requested_device: String,
    pub observed_device: String,
    pub resident: bool,
    pub inference_ms: u64,
    pub load_ms: Option<u64>,
    pub phase: &'static str,
    pub model_revision: String,
    pub runtime_revision: String,
    pub receipt_sha256: String,
    pub artifact_hashes: BTreeMap<String, String>,
    pub host_profile: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_profile: Option<&'static str>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peak_host_ram_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peak_gpu_memory_bytes: Option<u64>,
    pub text: String,
    pub status: String,
    pub error: Option<String>,
}

struct ExecuteOpts<'a> {
    store_root: Option<&'a Path>,
    daemon_state: Option<DaemonState>,
    probe_daemon: bool,
    #[cfg(test)]
    replay_worker: Option<FakeWorker>,
}

impl Default for ExecuteOpts<'_> {
    fn default() -> Self {
        Self {
            store_root: None,
            daemon_state: None,
            probe_daemon: true,
            #[cfg(test)]
            replay_worker: None,
        }
    }
}

enum ExecuteError {
    Failed(String),
    Replay(String),
}

/// Parse `voisu local-dev` arguments after the command name.
pub fn parse(args: &[String]) -> Result<LocalDevCommand, String> {
    match args {
        [] => Err(USAGE.to_owned()),
        [command] if command == "status" => Ok(LocalDevCommand::Status { json: false }),
        [command, flag] if command == "status" && flag == "--json" => {
            Ok(LocalDevCommand::Status { json: true })
        }
        [command, model_id] if command == "select" => Ok(LocalDevCommand::Select {
            model_id: model_id.clone(),
        }),
        [command, flag, path] if command == "register" && flag == "--manifest" => {
            let manifest = PathBuf::from(path);
            require_absolute(&manifest, "manifest")?;
            Ok(LocalDevCommand::Register { manifest })
        }
        [command, rest @ ..] if command == "replay" => parse_replay(rest),
        _ => Err(USAGE.to_owned()),
    }
}

/// Run `voisu local-dev` and return a process exit code.
pub fn run(args: &[String]) -> u8 {
    match parse(args) {
        Ok(LocalDevCommand::Replay {
            manifest,
            audios,
            timeout_ms,
        }) => {
            TERMINATE_REPLAY.store(false, Ordering::Relaxed);
            // The handler only sets a flag. The bounded worker exchange finishes,
            // then replay cancels and reaps its unit before returning.
            unsafe {
                libc::signal(
                    libc::SIGTERM,
                    request_replay_termination as *const () as libc::sighandler_t,
                )
            };
            let stdout = std::io::stdout();
            let mut out = stdout.lock();
            let failed = replay_for_opts_stream(
                &manifest,
                &audios,
                Duration::from_millis(timeout_ms),
                ExecuteOpts::default(),
                |report| {
                    let _ = writeln!(out, "{}", encode_replay(&report));
                    let _ = out.flush();
                },
            );
            u8::from(failed)
        }
        Ok(command) => emit(execute(command, ExecuteOpts::default())),
        Err(message) => {
            eprintln!("{message}");
            2
        }
    }
}

fn emit(result: Result<String, ExecuteError>) -> u8 {
    match result {
        Ok(output) => {
            println!("{output}");
            0
        }
        Err(ExecuteError::Failed(message)) => {
            eprintln!("{message}");
            1
        }
        Err(ExecuteError::Replay(output)) => {
            println!("{output}");
            1
        }
    }
}

fn execute(command: LocalDevCommand, opts: ExecuteOpts<'_>) -> Result<String, ExecuteError> {
    match command {
        LocalDevCommand::Register { manifest } => {
            let store = open_store(opts.store_root)?;
            let handle = store.register_file(&manifest).map_err(ExecuteError::from)?;
            Ok(format!(
                "registered {} {}",
                handle.model_id, handle.manifest_hash
            ))
        }
        LocalDevCommand::Select { model_id } => {
            let store = open_store(opts.store_root)?;
            let daemon_state = if opts.probe_daemon {
                probe_daemon_state()
            } else {
                opts.daemon_state
            };
            let handle = store
                .select(&model_id, daemon_state)
                .map_err(ExecuteError::from)?;
            Ok(format!(
                "selected {} {}",
                handle.model_id, handle.manifest_hash
            ))
        }
        LocalDevCommand::Status { json } => {
            let store = open_store(opts.store_root)?;
            Ok(format_status(&store.statuses(), json))
        }
        LocalDevCommand::Replay {
            manifest,
            audios,
            timeout_ms,
        } => {
            let reports =
                replay_for_opts(&manifest, &audios, Duration::from_millis(timeout_ms), opts);
            let failed = reports.iter().any(|report| report.status == "error");
            let output = reports
                .iter()
                .map(encode_replay)
                .collect::<Vec<_>>()
                .join("\n");
            if failed {
                Err(ExecuteError::Replay(output))
            } else {
                Ok(output)
            }
        }
    }
}

fn open_store(root: Option<&Path>) -> Result<DevelopmentStore, ExecuteError> {
    match root {
        Some(root) => DevelopmentStore::open(root.to_path_buf()),
        None => DevelopmentStore::open_default(),
    }
    .map_err(ExecuteError::from)
}

fn parse_replay(args: &[String]) -> Result<LocalDevCommand, String> {
    let mut json = false;
    let mut manifest = None;
    let mut audios = Vec::new();
    let mut timeout_ms = STOP_PROCESSING.as_millis() as u64;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--json" => json = true,
            "--manifest" => {
                index += 1;
                let path = args
                    .get(index)
                    .ok_or_else(|| "--manifest needs a path".to_owned())?;
                manifest = Some(PathBuf::from(path));
            }
            "--audio" => {
                index += 1;
                let path = args
                    .get(index)
                    .ok_or_else(|| "--audio needs a path".to_owned())?;
                audios.push(PathBuf::from(path));
            }
            "--timeout-ms" => {
                index += 1;
                timeout_ms = args
                    .get(index)
                    .and_then(|value| value.parse::<u64>().ok())
                    .filter(|value| *value > 0)
                    .ok_or_else(|| "--timeout-ms needs a positive integer".to_owned())?;
            }
            _ => return Err(USAGE.to_owned()),
        }
        index += 1;
    }
    let Some(manifest) = manifest else {
        return Err(USAGE.to_owned());
    };
    if !json || audios.is_empty() {
        return Err(USAGE.to_owned());
    }
    require_absolute(&manifest, "manifest")?;
    for audio in &audios {
        require_absolute(audio, "audio")?;
    }
    Ok(LocalDevCommand::Replay {
        manifest,
        audios,
        timeout_ms,
    })
}

fn require_absolute(path: &Path, label: &str) -> Result<(), String> {
    if path.is_absolute() {
        Ok(())
    } else {
        Err(format!("{label} path must be absolute"))
    }
}

fn probe_daemon_state() -> Option<DaemonState> {
    let path = voisu_core::socket_path().ok()?;
    if !path.exists() {
        return Some(DaemonState::Idle);
    }
    let mut stream = UnixStream::connect(&path).ok()?;
    let _ = stream.set_write_timeout(Some(Duration::from_secs(1)));
    let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
    serde_json::to_writer(&mut stream, &Request::new(Command::Status)).ok()?;
    stream.write_all(b"\n").ok()?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).ok()?;
    serde_json::from_str::<Response>(&line)
        .ok()
        .and_then(|response| response.state)
}

fn format_status(statuses: &[DevModelStatus], json: bool) -> String {
    if json {
        let models: Vec<serde_json::Value> = statuses
            .iter()
            .map(|status| {
                serde_json::json!({
                    "model_id": status.model_id,
                    "runtime": status.runtime.as_str(),
                    "languages": status.languages,
                    "devices": status.devices,
                    "memory_bytes": status.memory_bytes,
                    "vram_bytes": status.vram_bytes,
                    "selected": status.selected,
                    "ready": status.ready,
                    "manifest_hash": status.manifest.as_ref().map(|handle| &handle.manifest_hash),
                    "error": status.error,
                })
            })
            .collect();
        return serde_json::to_string_pretty(&serde_json::json!({
            "production_selection": production_selection(&shipped_catalog()).map(|entry| entry.id),
            "models": models,
        }))
        .unwrap_or_else(|_| "{}".into());
    }
    let mut lines = Vec::new();
    for status in statuses {
        let state = if status.selected {
            "selected"
        } else if status.manifest.is_some() {
            "registered"
        } else {
            "unregistered"
        };
        let error = status.error.as_deref().unwrap_or("-");
        lines.push(format!(
            "{} runtime={} {} ready={} error={}",
            status.model_id,
            status.runtime.as_str(),
            state,
            status.ready,
            error
        ));
    }
    lines.join("\n")
}

fn replay_for_opts(
    manifest_path: &Path,
    audios: &[PathBuf],
    timeout: Duration,
    opts: ExecuteOpts<'_>,
) -> Vec<ReplayReport> {
    let mut reports = Vec::with_capacity(audios.len());
    replay_for_opts_stream(manifest_path, audios, timeout, opts, |report| {
        reports.push(report)
    });
    reports
}

fn replay_for_opts_stream(
    manifest_path: &Path,
    audios: &[PathBuf],
    timeout: Duration,
    opts: ExecuteOpts<'_>,
    mut emit: impl FnMut(ReplayReport),
) -> bool {
    let (manifest, hash, snapshot) = match load_replay_manifest(manifest_path, opts.store_root) {
        Ok(loaded) => loaded,
        Err(error) => {
            for _ in audios {
                emit(ReplayReport::unknown().failed(error.to_string()));
            }
            return true;
        }
    };
    #[cfg(test)]
    if let Some(worker) = opts.replay_worker {
        return replay_batch_with_stream(&manifest, &hash, audios, timeout, worker, emit);
    }
    if let Err(error) = refuse_if_development_worker_loaded() {
        for _ in audios {
            emit(ReplayReport::from_manifest(&manifest).failed(format!(
                "another development worker is still loaded: {error}"
            )));
        }
        return true;
    }
    match DevWorker::launch(manifest.clone(), hash.clone(), &snapshot) {
        Ok(worker) => replay_batch_with_stream(&manifest, &hash, audios, timeout, worker, emit),
        Err(error) => {
            for _ in audios {
                emit(
                    ReplayReport::from_manifest(&manifest)
                        .failed(format!("failed to launch development worker: {error}")),
                );
            }
            true
        }
    }
}

fn load_replay_manifest(
    manifest_path: &Path,
    store_root: Option<&Path>,
) -> Result<(DevManifest, String, PathBuf), DevelopmentError> {
    let text = std::fs::read_to_string(manifest_path)
        .map_err(|error| DevelopmentError::Io(format!("cannot read manifest: {error}")))?;
    let requested = DevManifest::from_json(&text)?;
    let store = match store_root {
        Some(root) => DevelopmentStore::open(root.to_path_buf()),
        None => DevelopmentStore::open_default(),
    }?;
    let (manifest, handle) = store.load(&requested.model_id)?;
    if requested != manifest {
        return Err(DevelopmentError::Invalid(
            "manifest differs from the registered snapshot".into(),
        ));
    }
    Ok((manifest, handle.manifest_hash, handle.path))
}

#[cfg(test)]
fn replay_batch_with<C: WorkerChild>(
    manifest: &DevManifest,
    hash: &str,
    audios: &[PathBuf],
    worker: C,
) -> Vec<ReplayReport> {
    let mut reports = Vec::with_capacity(audios.len());
    replay_batch_with_stream(manifest, hash, audios, STOP_PROCESSING, worker, |report| {
        reports.push(report)
    });
    reports
}

fn replay_batch_with_stream<C: WorkerChild>(
    manifest: &DevManifest,
    hash: &str,
    audios: &[PathBuf],
    timeout: Duration,
    worker: C,
    mut emit: impl FnMut(ReplayReport),
) -> bool {
    let mut supervisor = WorkerSupervisor::absent();
    let prepared = supervisor
        .attach_pending(worker, Instant::now())
        .and_then(|()| supervisor.prepare(correlation(hash, "prepare"), Instant::now()));
    let load_ms = match prepared {
        Ok(duration) => u64::try_from(duration.as_millis()).unwrap_or(u64::MAX),
        Err(error) => {
            let _ = supervisor.cancel();
            for _ in audios {
                emit(ReplayReport::from_manifest(manifest).failed(error.to_string()));
            }
            return true;
        }
    };
    let mut failed = None;
    let mut any_failed = false;
    for (index, audio) in audios.iter().enumerate() {
        let mut report = ReplayReport::from_manifest(manifest);
        report.receipt_sha256 = development_receipt(manifest, hash).receipt_hash;
        report.model_revision = manifest
            .options
            .get("revision")
            .and_then(|value| value.as_str())
            .unwrap_or(hash)
            .to_owned();
        report.artifact_hashes = manifest
            .files
            .iter()
            .map(|pin| (pin.path.clone(), pin.sha256.clone()))
            .collect();
        report.phase = "cold";
        if index == 0 {
            report.load_ms = Some(load_ms);
        }
        if TERMINATE_REPLAY.load(Ordering::Relaxed) && failed.is_none() {
            failed = Some("replay terminated".into());
        }
        if let Some(error) = &failed {
            report.status = "unavailable".into();
            report.error = Some(format!("worker unavailable after previous clip: {error}"));
            any_failed = true;
            emit(report);
            continue;
        }
        let pcm = match read_replay_pcm(audio) {
            Ok(pcm) => pcm,
            Err(error) => {
                failed = Some(error.clone());
                any_failed = true;
                emit(report.failed(error));
                continue;
            }
        };
        report.duration_profile = Some(if pcm.len() <= 10 * 32_000 {
            "short"
        } else {
            "long"
        });
        let started = Instant::now();
        let outcome = supervisor.transcribe_until(
            TranscribeRequest {
                correlation: correlation(hash, &format!("transcribe-{index}")),
                pcm,
            },
            started + timeout,
        );
        report.observed_device = supervisor.observed_device().to_owned();
        report.inference_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        match outcome {
            Ok(WorkerOutcome::Transcript {
                text,
                observed_device,
                metrics,
            }) => {
                report.observed_device = observed_device;
                report.text = text;
                report.status = "transcript".into();
                apply_outcome_metrics(&mut report, metrics, index);
            }
            Ok(WorkerOutcome::NoText {
                reason,
                observed_device,
                metrics,
            }) => {
                report.observed_device = observed_device;
                report.status = "no_text".into();
                report.error = Some(reason);
                apply_outcome_metrics(&mut report, metrics, index);
            }
            Err(error) => {
                let message = error.to_string();
                failed = Some(message.clone());
                any_failed = true;
                report = report.failed(message);
            }
        }
        emit(report);
    }
    let _ = supervisor.cancel();
    any_failed
}

fn apply_outcome_metrics(report: &mut ReplayReport, metrics: InferenceMetrics, index: usize) {
    if let Some(backend) = metrics.backend {
        report.backend = Some(backend);
    }
    report.resident = metrics.resident == Some(true);
    if index > 0 && report.resident {
        report.phase = "warm";
    }
    if let Some(ms) = metrics.inference_ms {
        report.inference_ms = ms;
    }
    report.peak_host_ram_bytes = metrics.peak_host_ram_bytes;
    report.peak_gpu_memory_bytes = metrics.peak_gpu_memory_bytes;
}

fn correlation(hash: &str, request_id: &str) -> Correlation {
    Correlation {
        daemon_nonce: "local-dev".into(),
        generation: 1,
        request_id: request_id.to_owned(),
        recording_id: "replay".into(),
        model_receipt_hash: hash.to_owned(),
    }
}

fn read_replay_pcm(path: &Path) -> Result<Vec<u8>, String> {
    let bytes = std::fs::read(path).map_err(|error| format!("cannot read audio: {error}"))?;
    match scan_wav_pcm(&bytes) {
        WavScan::DataAt(offset) => {
            let pcm = bytes.get(offset..).unwrap_or(&[]).to_vec();
            if pcm.len() % 2 != 0 {
                return Err("audio PCM has an odd byte count".into());
            }
            Ok(pcm)
        }
        WavScan::Incomplete => Err("audio WAV is incomplete".into()),
        WavScan::Invalid(reason) => Err(reason.to_owned()),
    }
}

fn encode_replay(report: &ReplayReport) -> String {
    serde_json::to_string(report).unwrap_or_else(|_| {
        r#"{"version":1,"model_id":"","language":"","requested_device":"","observed_device":"unknown","resident":false,"inference_ms":0,"text":"","status":"error","error":"cannot encode replay JSON"}"#.into()
    })
}

impl ReplayReport {
    fn unknown() -> Self {
        Self {
            version: 1,
            model_id: String::new(),
            language: String::new(),
            requested_device: String::new(),
            observed_device: "unknown".into(),
            resident: false,
            inference_ms: 0,
            load_ms: None,
            phase: "cold",
            model_revision: String::new(),
            runtime_revision: String::new(),
            receipt_sha256: String::new(),
            artifact_hashes: BTreeMap::new(),
            host_profile: String::new(),
            duration_profile: None,
            backend: None,
            peak_host_ram_bytes: None,
            peak_gpu_memory_bytes: None,
            text: String::new(),
            status: "error".into(),
            error: Some("replay failed".into()),
        }
    }

    fn from_manifest(manifest: &DevManifest) -> Self {
        Self {
            version: 1,
            model_id: manifest.model_id.clone(),
            language: manifest.language.clone(),
            requested_device: manifest.device.clone(),
            observed_device: "unknown".into(),
            resident: false,
            inference_ms: 0,
            load_ms: None,
            phase: "cold",
            model_revision: String::new(),
            runtime_revision: manifest.runtime.revision.clone(),
            receipt_sha256: String::new(),
            artifact_hashes: BTreeMap::new(),
            host_profile: std::env::var("VOISU_DEV_HOST_PROFILE")
                .unwrap_or_else(|_| "unprofiled".into()),
            duration_profile: None,
            backend: None,
            peak_host_ram_bytes: None,
            peak_gpu_memory_bytes: None,
            text: String::new(),
            status: "error".into(),
            error: None,
        }
    }

    fn failed(mut self, error: String) -> Self {
        self.status = "error".into();
        self.error = Some(error);
        self
    }
}

impl From<DevelopmentError> for ExecuteError {
    fn from(error: DevelopmentError) -> Self {
        Self::Failed(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local_model::{
        DEV_MODEL_IDS, DevFilePin, DevLimits, DevRuntimeManifest, sha256_hex,
    };
    use crate::local_worker::{ControlFrame, ReapOutcome, SupervisorError, WorkerFrame};
    use std::fs;
    use std::sync::{Arc, Mutex};
    use tempfile::TempDir;

    const REPLAY_FIELDS: [&str; 10] = [
        "version",
        "model_id",
        "language",
        "requested_device",
        "observed_device",
        "resident",
        "inference_ms",
        "text",
        "status",
        "error",
    ];

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    fn pin(path: &Path) -> DevFilePin {
        DevFilePin {
            path: path.to_string_lossy().into_owned(),
            sha256: sha256_hex(&fs::read(path).unwrap()),
            bytes: fs::metadata(path).unwrap().len(),
        }
    }

    fn write_manifest(dir: &Path, model_id: &str) -> (PathBuf, DevManifest) {
        let python = dir.join("python");
        let worker = dir.join("worker.py");
        let model = dir.join("model.bin");
        fs::write(&python, b"python").unwrap();
        fs::write(&worker, b"worker").unwrap();
        fs::write(&model, b"model").unwrap();
        let manifest = DevManifest {
            version: 1,
            model_id: model_id.into(),
            device: "cpu".into(),
            language: "en".into(),
            runtime: DevRuntimeManifest {
                python: python.to_string_lossy().into_owned(),
                worker: worker.to_string_lossy().into_owned(),
                revision: "test-runtime".into(),
            },
            model_path: model.to_string_lossy().into_owned(),
            files: vec![pin(&python), pin(&worker), pin(&model)],
            options: serde_json::Map::new(),
            limits: DevLimits {
                memory_bytes: 1024,
                tasks: 4,
                cpu_cores: 1,
                cache_bytes: 1024,
                vram_bytes: 0,
            },
        };
        let path = dir.join("manifest.json");
        fs::write(&path, manifest.to_json().unwrap()).unwrap();
        (path, manifest)
    }

    fn pcm_wav(pcm: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let data_len = pcm.len() as u32;
        out.extend(b"RIFF");
        out.extend((36 + data_len).to_le_bytes());
        out.extend(b"WAVE");
        out.extend(b"fmt ");
        out.extend(16u32.to_le_bytes());
        out.extend(1u16.to_le_bytes());
        out.extend(1u16.to_le_bytes());
        out.extend(16_000u32.to_le_bytes());
        out.extend(32_000u32.to_le_bytes());
        out.extend(2u16.to_le_bytes());
        out.extend(16u16.to_le_bytes());
        out.extend(b"data");
        out.extend(data_len.to_le_bytes());
        out.extend(pcm);
        out
    }

    fn run_test(
        argv: &[String],
        store_root: &Path,
        daemon_state: Option<DaemonState>,
        replay_worker: Option<FakeWorker>,
    ) -> (u8, String) {
        let parsed = parse(argv).expect("parse");
        match execute(
            parsed,
            ExecuteOpts {
                store_root: Some(store_root),
                daemon_state,
                probe_daemon: false,
                replay_worker,
            },
        ) {
            Ok(output) => (0, output),
            Err(ExecuteError::Failed(message)) => (1, message),
            Err(ExecuteError::Replay(output)) => (1, output),
        }
    }

    #[test]
    fn parse_register_select_status_and_replay() {
        assert_eq!(
            parse(&args(&["register", "--manifest", "/tmp/model.json"])).unwrap(),
            LocalDevCommand::Register {
                manifest: PathBuf::from("/tmp/model.json")
            }
        );
        assert_eq!(
            parse(&args(&["select", DEV_MODEL_IDS[0]])).unwrap(),
            LocalDevCommand::Select {
                model_id: DEV_MODEL_IDS[0].into()
            }
        );
        assert_eq!(
            parse(&args(&["status", "--json"])).unwrap(),
            LocalDevCommand::Status { json: true }
        );
        assert_eq!(
            parse(&args(&[
                "replay",
                "--audio",
                "/tmp/a.wav",
                "--json",
                "--manifest",
                "/tmp/m.json"
            ]))
            .unwrap(),
            LocalDevCommand::Replay {
                manifest: PathBuf::from("/tmp/m.json"),
                audios: vec![PathBuf::from("/tmp/a.wav")],
                timeout_ms: STOP_PROCESSING.as_millis() as u64,
            }
        );
        assert!(parse(&args(&["register", "--manifest", "relative.json"])).is_err());
        assert!(parse(&args(&["replay", "--manifest", "/tmp/m.json"])).is_err());
        assert_eq!(
            parse(&args(&[
                "replay",
                "--json",
                "--manifest",
                "/tmp/m.json",
                "--audio",
                "/tmp/a.wav",
                "--timeout-ms",
                "7"
            ]))
            .unwrap(),
            LocalDevCommand::Replay {
                manifest: PathBuf::from("/tmp/m.json"),
                audios: vec![PathBuf::from("/tmp/a.wav")],
                timeout_ms: 7,
            }
        );
    }

    #[test]
    fn register_select_status_busy_and_production_stay_closed() {
        let dir = TempDir::new().unwrap();
        let (manifest_path, _) = write_manifest(dir.path(), DEV_MODEL_IDS[0]);
        let store_root = dir.path().join("local-dev");
        let (code, output) = run_test(
            &args(&["register", "--manifest", manifest_path.to_str().unwrap()]),
            &store_root,
            None,
            None,
        );
        assert_eq!(code, 0, "{output}");
        let (code, output) = run_test(
            &args(&["select", DEV_MODEL_IDS[0]]),
            &store_root,
            Some(DaemonState::Recording),
            None,
        );
        assert_eq!(code, 1);
        assert!(output.contains("Recording"), "{output}");
        let (code, output) = run_test(
            &args(&["select", DEV_MODEL_IDS[0]]),
            &store_root,
            None,
            None,
        );
        assert_eq!(code, 1);
        assert!(output.contains("idle state is confirmed"), "{output}");
        let (code, output) = run_test(
            &args(&["select", DEV_MODEL_IDS[0]]),
            &store_root,
            Some(DaemonState::Idle),
            None,
        );
        assert_eq!(code, 0, "{output}");
        let (code, output) = run_test(&args(&["status", "--json"]), &store_root, None, None);
        assert_eq!(code, 0, "{output}");
        let value: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert!(value["production_selection"].is_null());
        assert_eq!(
            value["models"].as_array().unwrap().len(),
            DEV_MODEL_IDS.len()
        );
        assert_eq!(
            value["models"]
                .as_array()
                .unwrap()
                .iter()
                .find(|row| row["model_id"] == DEV_MODEL_IDS[0])
                .unwrap()["selected"],
            true
        );
        assert!(
            crate::local_model::production_selection(&crate::local_model::shipped_catalog())
                .is_none()
        );
    }

    #[test]
    fn replay_json_shape_uses_fake_worker() {
        let dir = TempDir::new().unwrap();
        let (manifest_path, manifest) = write_manifest(dir.path(), DEV_MODEL_IDS[0]);
        let audio = dir.path().join("audio.wav");
        fs::write(&audio, pcm_wav(&[1, 0, 2, 0])).unwrap();
        let store_root = dir.path().join("local-dev");
        let hash = DevelopmentStore::open(store_root.clone())
            .unwrap()
            .register(&manifest)
            .unwrap()
            .manifest_hash;
        let worker = FakeWorker {
            scripted_text: Some("hello raja".into()),
            model_receipt_hash: hash,
            observed_device: "cpu".into(),
            ..FakeWorker::default()
        };
        let (code, output) = run_test(
            &args(&[
                "replay",
                "--json",
                "--manifest",
                manifest_path.to_str().unwrap(),
                "--audio",
                audio.to_str().unwrap(),
            ]),
            &store_root,
            None,
            Some(worker),
        );
        assert_eq!(code, 0, "{output}");
        let value: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert!(value.is_object());
        for field in REPLAY_FIELDS {
            assert!(value.get(field).is_some(), "missing {field} in {value}");
        }
        assert_eq!(value["version"], 1);
        assert_eq!(value["model_id"], DEV_MODEL_IDS[0]);
        assert_eq!(value["language"], "en");
        assert_eq!(value["requested_device"], "cpu");
        assert_eq!(value["observed_device"], "cpu");
        assert_eq!(value["resident"], false);
        assert_eq!(value["phase"], "cold");
        assert!(value["load_ms"].as_u64().is_some());
        assert!(value["inference_ms"].as_u64().is_some());
        assert_eq!(value["text"], "hello raja");
        assert_eq!(value["status"], "transcript");
        assert!(value["error"].is_null());
        assert!(value.get("receipt_sha256").is_some());
    }

    #[test]
    fn replay_batch_prepares_once_and_transcribes_each_clip_once() {
        let dir = TempDir::new().unwrap();
        let (manifest_path, manifest) = write_manifest(dir.path(), DEV_MODEL_IDS[1]);
        let audio = dir.path().join("audio.wav");
        fs::write(&audio, pcm_wav(&[1, 0, 2, 0])).unwrap();
        let store_root = dir.path().join("local-dev");
        let hash = DevelopmentStore::open(store_root.clone())
            .unwrap()
            .register(&manifest)
            .unwrap()
            .manifest_hash;
        let worker = FakeWorker {
            scripted_text: Some("ask not what your country can do".into()),
            model_receipt_hash: hash,
            observed_device: "cpu".into(),
            ..FakeWorker::default()
        };
        let (loaded, loaded_hash, _) =
            load_replay_manifest(&manifest_path, Some(&store_root)).unwrap();
        struct TrackingWorker {
            inner: FakeWorker,
            counts: Arc<Mutex<(usize, usize)>>,
            fail_on_transcribe: Option<usize>,
            timeout_on_transcribe: Option<usize>,
            resident: Option<bool>,
            reaps: Arc<Mutex<usize>>,
            emitted_count: Option<Arc<Mutex<usize>>>,
        }
        impl WorkerChild for TrackingWorker {
            fn exchange(
                &mut self,
                control: ControlFrame,
                pcm: Option<&[u8]>,
                deadline: Instant,
            ) -> Result<WorkerFrame, SupervisorError> {
                let mut counts = self.counts.lock().unwrap();
                match &control {
                    ControlFrame::Prepare(_) => counts.0 += 1,
                    ControlFrame::Transcribe { .. } => counts.1 += 1,
                    ControlFrame::Cancel { .. } => {}
                }
                let fail = self.fail_on_transcribe == Some(counts.1)
                    && matches!(&control, ControlFrame::Transcribe { .. });
                let timeout = self.timeout_on_transcribe == Some(counts.1)
                    && matches!(&control, ControlFrame::Transcribe { .. });
                if counts.1 == 2
                    && matches!(&control, ControlFrame::Transcribe { .. })
                    && let Some(emitted) = &self.emitted_count
                {
                    assert_eq!(*emitted.lock().unwrap(), 1);
                }
                drop(counts);
                if fail {
                    return Err(SupervisorError::Crashed);
                }
                if timeout {
                    self.inner.block_for = Some(Duration::from_secs(1));
                }
                self.inner.exchange(control, pcm, deadline)
            }
            fn cancel_and_reap(&mut self) -> Result<ReapOutcome, SupervisorError> {
                *self.reaps.lock().unwrap() += 1;
                self.inner.cancel_and_reap()
            }
            fn last_inference_metrics(&self) -> InferenceMetrics {
                InferenceMetrics {
                    resident: self.resident,
                    ..InferenceMetrics::default()
                }
            }
        }
        let counts = Arc::new(Mutex::new((0, 0)));
        let reports = replay_batch_with(
            &loaded,
            &loaded_hash,
            &[audio.clone(), audio.clone()],
            TrackingWorker {
                inner: worker.clone(),
                counts: counts.clone(),
                fail_on_transcribe: None,
                timeout_on_transcribe: None,
                resident: Some(true),
                reaps: Arc::new(Mutex::new(0)),
                emitted_count: None,
            },
        );
        assert_eq!(*counts.lock().unwrap(), (1, 2));
        assert_eq!(reports.len(), 2);
        assert_eq!(reports[0].phase, "cold");
        assert_eq!(reports[1].phase, "warm");
        assert_eq!(reports[0].text, "ask not what your country can do");
        assert_eq!(reports[1].text, reports[0].text);
        assert!(reports[0].load_ms.is_some());
        assert!(reports[1].load_ms.is_none());
        let counts = Arc::new(Mutex::new((0, 0)));
        let failed = replay_batch_with(
            &loaded,
            &loaded_hash,
            &[audio.clone(), audio.clone(), audio.clone()],
            TrackingWorker {
                inner: worker,
                counts: counts.clone(),
                fail_on_transcribe: Some(2),
                timeout_on_transcribe: None,
                resident: Some(true),
                reaps: Arc::new(Mutex::new(0)),
                emitted_count: None,
            },
        );
        assert_eq!(*counts.lock().unwrap(), (1, 2));
        assert_eq!(failed[0].status, "transcript");
        assert_eq!(failed[1].status, "error");
        assert_eq!(failed[2].status, "unavailable");
        let missing = replay_batch_with(
            &loaded,
            &loaded_hash,
            &[audio.clone(), audio.clone()],
            TrackingWorker {
                inner: FakeWorker {
                    model_receipt_hash: loaded_hash.clone(),
                    scripted_text: Some("words".into()),
                    ..FakeWorker::default()
                },
                counts: Arc::new(Mutex::new((0, 0))),
                fail_on_transcribe: None,
                timeout_on_transcribe: None,
                resident: None,
                reaps: Arc::new(Mutex::new(0)),
                emitted_count: None,
            },
        );
        assert_eq!(missing[1].phase, "cold");
        assert!(!missing[1].resident);
        let reaps = Arc::new(Mutex::new(0));
        let emitted_count = Arc::new(Mutex::new(0));
        let mut emitted = Vec::new();
        let failed = replay_batch_with_stream(
            &loaded,
            &loaded_hash,
            &[audio.clone(), audio.clone(), audio],
            Duration::from_millis(100),
            TrackingWorker {
                inner: FakeWorker {
                    model_receipt_hash: loaded_hash.clone(),
                    scripted_text: Some("words".into()),
                    ..FakeWorker::default()
                },
                counts: Arc::new(Mutex::new((0, 0))),
                fail_on_transcribe: None,
                timeout_on_transcribe: Some(2),
                resident: Some(true),
                reaps: reaps.clone(),
                emitted_count: Some(emitted_count.clone()),
            },
            |report| {
                emitted.push(report);
                *emitted_count.lock().unwrap() += 1;
            },
        );
        assert!(failed);
        assert_eq!(
            emitted
                .iter()
                .map(|report| report.status.as_str())
                .collect::<Vec<_>>(),
            ["transcript", "error", "unavailable"]
        );
        assert!(*reaps.lock().unwrap() >= 1);
    }
}
