//! Development-only `voisu local-dev` commands.
//!
//! Register, select, and inspect private development manifests, then replay a
//! pinned WAV through the supervised development worker. Production catalog
//! selection is never written.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::Serialize;
use voisu_core::{Command, DaemonState, Request, Response, WavScan, scan_wav_pcm};

use crate::local_model::{
    DevManifest, DevModelStatus, DevelopmentError, DevelopmentStore, development_catalog_entry,
    production_selection, shipped_catalog,
};
#[cfg(test)]
use crate::local_worker::FakeWorker;
use crate::local_worker::{
    Correlation, DevWorker, InferenceMetrics, SupervisorError, TranscribeRequest, WorkerChild,
    WorkerOutcome, WorkerSupervisor, refuse_if_development_worker_loaded,
};

const USAGE: &str = "\
usage: voisu local-dev register --manifest <path>
       voisu local-dev select <model-id>
       voisu local-dev status [--json]
       voisu local-dev replay --json --manifest <path> --audio <wav>";

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LocalDevCommand {
    Register { manifest: PathBuf },
    Select { model_id: String },
    Status { json: bool },
    Replay { manifest: PathBuf, audio: PathBuf },
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
    Usage(String),
    Failed(String),
    Replay(ReplayReport),
}

trait ReplayMetrics {
    fn replay_observed_device(&self) -> &str;
    fn replay_resident(&self) -> bool;
    fn replay_inference_ms(&self) -> u64;
    fn replay_prepare_count(&self) -> u32 {
        0
    }
    fn replay_transcribe_count(&self) -> u32 {
        0
    }
}

impl ReplayMetrics for DevWorker {
    fn replay_observed_device(&self) -> &str {
        self.observed_device()
    }

    fn replay_resident(&self) -> bool {
        self.resident()
    }

    fn replay_inference_ms(&self) -> u64 {
        self.last_inference_ms()
    }
}

#[cfg(test)]
impl ReplayMetrics for FakeWorker {
    fn replay_observed_device(&self) -> &str {
        &self.observed_device
    }

    fn replay_resident(&self) -> bool {
        self.transcribe_count >= 2
    }

    fn replay_inference_ms(&self) -> u64 {
        0
    }

    fn replay_prepare_count(&self) -> u32 {
        self.prepare_count
    }

    fn replay_transcribe_count(&self) -> u32 {
        self.transcribe_count
    }
}

struct ReplayExecution {
    report: ReplayReport,
    #[cfg_attr(not(test), allow(dead_code))]
    prepare_count: u32,
    #[cfg_attr(not(test), allow(dead_code))]
    transcribe_count: u32,
}

/// Parse `voisu local-dev` arguments after the command name.
pub fn parse(args: &[String]) -> Result<LocalDevCommand, String> {
    match args {
        [] => Err(USAGE.to_owned()),
        [command] if command == "status" => Ok(LocalDevCommand::Status { json: false }),
        [command, flag] if command == "status" && flag == "--json" => {
            Ok(LocalDevCommand::Status { json: true })
        }
        [command, model_id] if command == "select" => {
            if development_catalog_entry(model_id).is_none() {
                return Err(format!("unknown development model: {model_id}"));
            }
            Ok(LocalDevCommand::Select {
                model_id: model_id.clone(),
            })
        }
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
        Err(ExecuteError::Usage(message)) => {
            eprintln!("{message}");
            2
        }
        Err(ExecuteError::Failed(message)) => {
            eprintln!("{message}");
            1
        }
        Err(ExecuteError::Replay(report)) => {
            println!("{}", encode_replay(&report));
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
        LocalDevCommand::Replay { manifest, audio } => {
            require_absolute(&manifest, "manifest").map_err(ExecuteError::Usage)?;
            require_absolute(&audio, "audio").map_err(ExecuteError::Usage)?;
            let report = replay_for_opts(&manifest, &audio, opts);
            if report.status == "error" {
                return Err(ExecuteError::Replay(report));
            }
            Ok(encode_replay(&report))
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
    let mut audio = None;
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
                audio = Some(PathBuf::from(path));
            }
            _ => return Err(USAGE.to_owned()),
        }
        index += 1;
    }
    let Some(manifest) = manifest else {
        return Err(USAGE.to_owned());
    };
    let Some(audio) = audio else {
        return Err(USAGE.to_owned());
    };
    if !json {
        return Err(USAGE.to_owned());
    }
    require_absolute(&manifest, "manifest")?;
    require_absolute(&audio, "audio")?;
    Ok(LocalDevCommand::Replay { manifest, audio })
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

fn replay_for_opts(manifest_path: &Path, audio_path: &Path, opts: ExecuteOpts<'_>) -> ReplayReport {
    #[cfg(test)]
    if let Some(worker) = opts.replay_worker {
        return replay_manifest_with_worker(manifest_path, audio_path, worker);
    }
    let _ = opts;
    replay_manifest(manifest_path, audio_path)
}

fn replay_manifest(manifest_path: &Path, audio_path: &Path) -> ReplayReport {
    match load_replay_inputs(manifest_path, audio_path) {
        Ok((manifest, hash, pcm)) => {
            if let Err(error) = refuse_if_development_worker_loaded() {
                return ReplayReport::from_manifest(&manifest).failed(format!(
                    "another development worker is still loaded: {error}"
                ));
            }
            match DevWorker::launch(manifest.clone(), hash.clone(), manifest_path) {
                Ok(worker) => replay_with(&manifest, &hash, pcm, worker),
                Err(error) => ReplayReport::from_manifest(&manifest)
                    .failed(format!("failed to launch development worker: {error}")),
            }
        }
        Err(report) => report,
    }
}

#[cfg(test)]
fn replay_manifest_with_worker(
    manifest_path: &Path,
    audio_path: &Path,
    worker: FakeWorker,
) -> ReplayReport {
    match load_replay_inputs(manifest_path, audio_path) {
        Ok((manifest, hash, pcm)) => replay_with(&manifest, &hash, pcm, worker),
        Err(report) => report,
    }
}

fn load_replay_inputs(
    manifest_path: &Path,
    audio_path: &Path,
) -> Result<(DevManifest, String, Vec<u8>), ReplayReport> {
    let text = std::fs::read_to_string(manifest_path).map_err(|error| {
        ReplayReport::unknown().failed(format!("cannot read manifest: {error}"))
    })?;
    let manifest = DevManifest::from_json(&text)
        .map_err(|error| ReplayReport::unknown().failed(error.to_string()))?;
    let hash = manifest
        .verify_pins()
        .map_err(|error| ReplayReport::from_manifest(&manifest).failed(error.to_string()))?;
    let pcm = read_replay_pcm(audio_path)
        .map_err(|error| ReplayReport::from_manifest(&manifest).failed(error))?;
    Ok((manifest, hash, pcm))
}

fn replay_with<C: WorkerChild + ReplayMetrics>(
    manifest: &DevManifest,
    hash: &str,
    pcm: Vec<u8>,
    worker: C,
) -> ReplayReport {
    replay_execute(manifest, hash, pcm, worker).report
}

fn replay_execute<C: WorkerChild + ReplayMetrics>(
    manifest: &DevManifest,
    hash: &str,
    pcm: Vec<u8>,
    worker: C,
) -> ReplayExecution {
    let mut report = ReplayReport::from_manifest(manifest);
    let mut supervisor = WorkerSupervisor::absent();
    if let Err(error) = supervisor.attach_pending(worker, Instant::now()) {
        return ReplayExecution {
            report: report.failed(error.to_string()),
            prepare_count: 0,
            transcribe_count: 0,
        };
    }
    let prepare = correlation(hash, "prepare");
    if let Err(error) = supervisor.prepare(prepare, Instant::now()) {
        let _ = supervisor.cancel();
        return ReplayExecution {
            report: report.failed(error.to_string()),
            prepare_count: 0,
            transcribe_count: 0,
        };
    }
    let started = Instant::now();
    let first = supervisor.transcribe(TranscribeRequest {
        correlation: correlation(hash, "transcribe"),
        pcm: pcm.clone(),
    });
    if let Err(error) = first {
        let metrics = capture_replay_metrics(&mut supervisor, &mut report, started);
        let _ = supervisor.cancel();
        return ReplayExecution {
            report: report.failed(format_supervisor_error(&error)),
            prepare_count: metrics.0,
            transcribe_count: metrics.1,
        };
    }
    // Same worker, same audio, no reload: the second inference is the
    // residency proof. Resident stays false until this warm pass.
    let outcome = supervisor.transcribe(TranscribeRequest {
        correlation: correlation(hash, "transcribe-warm"),
        pcm,
    });
    let metrics = capture_replay_metrics(&mut supervisor, &mut report, started);
    let _ = supervisor.cancel();
    let report = match outcome {
        Ok(WorkerOutcome::Transcript {
            text,
            observed_device,
            metrics,
        }) => {
            report.observed_device = observed_device;
            report.text = text;
            report.status = "transcript".into();
            report.error = None;
            apply_outcome_metrics(&mut report, metrics);
            report
        }
        Ok(WorkerOutcome::NoText {
            reason,
            observed_device,
            metrics,
        }) => {
            report.observed_device = observed_device;
            report.status = "no_text".into();
            report.error = Some(reason);
            apply_outcome_metrics(&mut report, metrics);
            report
        }
        Err(error) => report.failed(format_supervisor_error(&error)),
    };
    ReplayExecution {
        report,
        prepare_count: metrics.0,
        transcribe_count: metrics.1,
    }
}

fn apply_outcome_metrics(report: &mut ReplayReport, metrics: InferenceMetrics) {
    if let Some(backend) = metrics.backend {
        report.backend = Some(backend);
    }
    if let Some(resident) = metrics.resident {
        report.resident = resident;
    }
    if let Some(ms) = metrics.inference_ms {
        report.inference_ms = ms;
    }
    report.peak_host_ram_bytes = metrics.peak_host_ram_bytes;
    report.peak_gpu_memory_bytes = metrics.peak_gpu_memory_bytes;
}

fn capture_replay_metrics<C: WorkerChild + ReplayMetrics>(
    supervisor: &mut WorkerSupervisor<C>,
    report: &mut ReplayReport,
    started: Instant,
) -> (u32, u32) {
    let wall_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    if let Some(child) = supervisor.child_mut() {
        report.observed_device = child.replay_observed_device().to_owned();
        report.resident = child.replay_resident();
        let reported = child.replay_inference_ms();
        report.inference_ms = if reported == 0 { wall_ms } else { reported };
        (
            child.replay_prepare_count(),
            child.replay_transcribe_count(),
        )
    } else {
        (0, 0)
    }
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

fn format_supervisor_error(error: &SupervisorError) -> String {
    error.to_string()
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
    use std::fs;
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
            Err(ExecuteError::Usage(message)) | Err(ExecuteError::Failed(message)) => (1, message),
            Err(ExecuteError::Replay(report)) => (1, encode_replay(&report)),
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
                audio: PathBuf::from("/tmp/a.wav"),
            }
        );
        assert!(parse(&args(&["select", "not-a-model"])).is_err());
        assert!(parse(&args(&["register", "--manifest", "relative.json"])).is_err());
        assert!(parse(&args(&["replay", "--manifest", "/tmp/m.json"])).is_err());
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
        let hash = manifest.verify_pins().unwrap();
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
            &dir.path().join("local-dev"),
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
        assert_eq!(value["resident"], true);
        assert!(value["inference_ms"].as_u64().is_some());
        assert_eq!(value["text"], "hello raja");
        assert_eq!(value["status"], "transcript");
        assert!(value["error"].is_null());
        assert_eq!(value.as_object().unwrap().len(), REPLAY_FIELDS.len());
    }

    #[test]
    fn replay_warm_second_inference_does_not_reload() {
        let dir = TempDir::new().unwrap();
        let (manifest_path, manifest) = write_manifest(dir.path(), DEV_MODEL_IDS[1]);
        let audio = dir.path().join("audio.wav");
        fs::write(&audio, pcm_wav(&[1, 0, 2, 0])).unwrap();
        let hash = manifest.verify_pins().unwrap();
        let worker = FakeWorker {
            scripted_text: Some("ask not what your country can do".into()),
            model_receipt_hash: hash,
            observed_device: "cpu".into(),
            ..FakeWorker::default()
        };
        let (loaded, loaded_hash, pcm) = load_replay_inputs(&manifest_path, &audio).unwrap();
        let run = replay_execute(&loaded, &loaded_hash, pcm, worker);
        assert_eq!(run.prepare_count, 1);
        assert_eq!(run.transcribe_count, 2);
        assert!(run.report.resident);
        assert_eq!(run.report.model_id, DEV_MODEL_IDS[1]);
        assert_eq!(run.report.language, "en");
        assert_eq!(run.report.status, "transcript");
        assert_eq!(run.report.text, "ask not what your country can do");
    }
}
