//! One `voisu-local-asr.exe` per Recording, on its own thread.
//!
//! The thread writes the manifest, starts the worker and sends `prepare` as
//! soon as the Recording starts, so loading the model and its health check
//! overlap the user speaking. At stop the whole Recording goes over in one
//! `transcribe`, then `shutdown`. Worker protocol v1 (`voisu-local-asr`): a
//! 4-byte little-endian length, then a JSON object; PCM follows a
//! `transcribe` frame as raw s16le bytes. Requests go one at a time.
//!
//! Dropping the [`Session`] kills the worker, so a cancelled Recording or a
//! missed deadline never leaves one running.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::oneshot;
use voisu_core::local_wire::{MAX_JSON_FRAME_BYTES, PROTOCOL_VERSION};

use super::{HEALTH_CLIP, HEALTH_EXPECTED, LocalError, MODEL, MODEL_ID};

/// Same decode settings as the Linux Fast tier.
const BEAM_SIZE: u32 = 5;
const FLASH_ATTN: bool = true;
/// More threads stop helping whisper.cpp small long before this.
const MAX_THREADS: usize = 8;
const MANIFEST: &str = "manifest.json";

/// What a Local Recording starts its worker with.
pub struct Launch {
    pub worker: PathBuf,
    pub models: PathBuf,
    /// The worker's stderr, appended: whisper.cpp's own log lines.
    pub log: PathBuf,
}

/// A Local failure with the diagnostic that goes to the log only.
#[derive(Debug)]
pub struct Failure {
    pub error: LocalError,
    pub diagnostic: String,
}

impl Failure {
    fn new(error: LocalError, diagnostic: impl Into<String>) -> Self {
        Self {
            error,
            diagnostic: diagnostic.into(),
        }
    }
}

/// `Some(text)`, or `None` when the worker heard no words.
pub type Heard = Result<Option<String>, Failure>;

struct Job {
    pcm: Vec<u8>,
    prompt: Option<String>,
}

#[derive(Default)]
struct Slot {
    child: Option<Child>,
    /// The session ended before the worker started: kill it on arrival.
    killed: bool,
}

/// The worker for one Recording.
pub struct Session {
    job: Option<mpsc::SyncSender<Job>>,
    heard: oneshot::Receiver<Heard>,
    slot: Arc<Mutex<Slot>>,
}

impl Session {
    /// Starts the worker and its `prepare` on a new thread; never blocks.
    pub fn start(launch: Launch) -> Self {
        let (job, jobs) = mpsc::sync_channel(1);
        let (done, heard) = oneshot::channel();
        let slot = Arc::new(Mutex::new(Slot::default()));
        let thread_slot = Arc::clone(&slot);
        let spawned = std::thread::Builder::new()
            .name("voisu-local-asr".into())
            .spawn(move || {
                let _ = done.send(run(&launch, &thread_slot, &jobs));
            });
        if let Err(error) = spawned {
            // `done` went down with the closure, so `transcribe` reports it.
            tracing::warn!(%error, "Local worker thread failed to start");
        }
        Self {
            job: Some(job),
            heard,
            slot,
        }
    }

    /// Transcribes the stopped Recording. Bound it with a timeout: dropping
    /// the future drops the session and kills the worker.
    pub async fn transcribe(mut self, pcm: Vec<u8>, prompt: Option<String>) -> Heard {
        if let Some(job) = self.job.take() {
            // A worker that already failed has dropped its receiver; its
            // failure is waiting on `heard`.
            let _ = job.send(Job { pcm, prompt });
        }
        match (&mut self.heard).await {
            Ok(heard) => heard,
            Err(_) => Err(Failure::new(
                LocalError::WorkerFailed,
                "Local worker thread ended without a result",
            )),
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let child = {
            let mut slot = self.slot.lock().unwrap();
            slot.killed = true;
            slot.child.take()
        };
        if let Some(mut child) = child {
            // Already exited after a clean shutdown, or killed here; either
            // way the wait is immediate.
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn run(launch: &Launch, slot: &Mutex<Slot>, jobs: &mpsc::Receiver<Job>) -> Heard {
    let manifest = write_manifest(launch).map_err(|error| {
        Failure::new(
            LocalError::WorkerFailed,
            format!("manifest write failed: {error}"),
        )
    })?;
    let (mut stdin, mut stdout) = spawn(launch, &manifest, slot)?;
    match request(&mut stdin, &mut stdout, &control("prepare"), None)? {
        Reply::Ready => {}
        other => return Err(unexpected("prepare", other)),
    }
    // The Sender is dropped without a job when the Recording is cancelled.
    let Ok(job) = jobs.recv() else {
        let _ = request(&mut stdin, &mut stdout, &control("shutdown"), None);
        return Ok(None);
    };
    let mut frame = json!({
        "v": PROTOCOL_VERSION,
        "id": "transcribe",
        "op": "transcribe",
        "pcm_bytes": job.pcm.len(),
    });
    if let Some(prompt) = job.prompt {
        frame["prompt"] = Value::String(prompt);
    }
    let heard = match request(&mut stdin, &mut stdout, &frame, Some(&job.pcm))? {
        Reply::Transcript(text) => Some(text),
        Reply::NoText => None,
        other => return Err(unexpected("transcribe", other)),
    };
    // Best effort: the session's Drop kills a worker that lingers.
    let _ = request(&mut stdin, &mut stdout, &control("shutdown"), None);
    Ok(heard)
}

fn spawn(
    launch: &Launch,
    manifest: &Path,
    slot: &Mutex<Slot>,
) -> Result<(ChildStdin, ChildStdout), Failure> {
    let stderr = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&launch.log)
        .map_or_else(|_| Stdio::null(), Stdio::from);
    let mut command = Command::new(&launch.worker);
    command
        .arg("--manifest")
        .arg(manifest)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(stderr);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        /// No console window flashes up for the worker.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = command.spawn().map_err(|error| {
        Failure::new(
            LocalError::WorkerFailed,
            format!("worker spawn failed: {error}"),
        )
    })?;
    let pipes = child.stdin.take().zip(child.stdout.take());
    let mut slot = slot.lock().unwrap();
    if slot.killed {
        let _ = child.kill();
        let _ = child.wait();
        return Err(Failure::new(LocalError::WorkerFailed, "Recording ended"));
    }
    slot.child = Some(child);
    pipes.ok_or_else(|| Failure::new(LocalError::WorkerFailed, "worker pipes missing"))
}

fn control(op: &str) -> Value {
    json!({ "v": PROTOCOL_VERSION, "id": op, "op": op })
}

/// Sends one request and reads its reply. A worker that refused its
/// manifest writes its error and exits before reading anything, so a failed
/// write still reads the reply waiting on stdout.
fn request(
    stdin: &mut impl Write,
    stdout: &mut impl Read,
    frame: &Value,
    pcm: Option<&[u8]>,
) -> Result<Reply, Failure> {
    let sent = write_frame(stdin, frame).and_then(|()| match pcm {
        Some(pcm) => stdin.write_all(pcm).and_then(|()| stdin.flush()),
        None => Ok(()),
    });
    match read_frame(stdout) {
        Ok(reply) => match parse_reply(&reply) {
            Reply::Error { code, message } => Err(Failure::new(
                LocalError::from_code(&code),
                format!("worker error {code}: {message}"),
            )),
            reply => Ok(reply),
        },
        Err(error) => Err(Failure::new(
            LocalError::WorkerFailed,
            format!("worker reply unreadable: {error}; send: {sent:?}"),
        )),
    }
}

/// Names the reply without its text: diagnostics never carry a Transcript.
fn unexpected(op: &str, reply: Reply) -> Failure {
    let status = match reply {
        Reply::Ready => "ready",
        Reply::Transcript(_) => "transcript",
        Reply::NoText => "no_text",
        Reply::Shutdown => "shutdown",
        Reply::Error { .. } => "error",
    };
    Failure::new(
        LocalError::WorkerFailed,
        format!("unexpected reply to {op}: {status}"),
    )
}

/// The reply statuses the app acts on.
#[derive(Debug, PartialEq, Eq)]
pub enum Reply {
    Ready,
    Transcript(String),
    NoText,
    Shutdown,
    Error { code: String, message: String },
}

pub fn parse_reply(value: &Value) -> Reply {
    let field = |name: &str| value.get(name).and_then(Value::as_str);
    match field("status") {
        Some("ready") => Reply::Ready,
        Some("transcript") => match field("text") {
            Some(text) => Reply::Transcript(text.to_owned()),
            None => Reply::NoText,
        },
        Some("no_text") => Reply::NoText,
        Some("shutdown") => Reply::Shutdown,
        Some("error") => Reply::Error {
            code: field("code").unwrap_or("unknown").to_owned(),
            message: field("message").unwrap_or_default().to_owned(),
        },
        _ => Reply::Error {
            code: "invalid_reply".to_owned(),
            message: "reply has no known status".to_owned(),
        },
    }
}

pub fn write_frame(writer: &mut impl Write, value: &Value) -> io::Result<()> {
    let payload = serde_json::to_vec(value)?;
    if payload.len() > MAX_JSON_FRAME_BYTES {
        return Err(io::Error::other("frame exceeds 64 KiB"));
    }
    let size = u32::try_from(payload.len()).map_err(io::Error::other)?;
    writer.write_all(&size.to_le_bytes())?;
    writer.write_all(&payload)?;
    writer.flush()
}

pub fn read_frame(reader: &mut impl Read) -> io::Result<Value> {
    let mut header = [0u8; 4];
    reader.read_exact(&mut header)?;
    let size = u32::from_le_bytes(header) as usize;
    if size == 0 || size > MAX_JSON_FRAME_BYTES {
        return Err(io::Error::other("reply frame size out of range"));
    }
    let mut payload = vec![0u8; size];
    reader.read_exact(&mut payload)?;
    serde_json::from_slice(&payload).map_err(io::Error::other)
}

/// Writes `models\manifest.json`, pinning the worker, the model and the
/// health clip. The worker refuses to start (`manifest_error`) if any pinned
/// file has changed since.
fn write_manifest(launch: &Launch) -> io::Result<PathBuf> {
    let model = launch.models.join(MODEL.name);
    let clip = launch.models.join(HEALTH_CLIP.name);
    let worker_bytes = fs::metadata(&launch.worker)?.len();
    let cores = std::thread::available_parallelism()
        .map_or(4, usize::from)
        .min(MAX_THREADS);
    let document = manifest(
        &launch.worker,
        &sha256_file(&launch.worker)?,
        worker_bytes,
        &model,
        &clip,
        cores,
    );
    let path = launch.models.join(MANIFEST);
    let temp = path.with_extension("json.tmp");
    fs::write(&temp, serde_json::to_vec_pretty(&document)?)?;
    fs::rename(&temp, &path)?;
    Ok(path)
}

fn manifest(
    worker: &Path,
    worker_sha256: &str,
    worker_bytes: u64,
    model: &Path,
    clip: &Path,
    cores: usize,
) -> Value {
    json!({
        "version": 1,
        "model_id": MODEL_ID,
        "device": "cpu",
        "language": "en",
        "limits": { "cpu_cores": cores, "vram_bytes": 0 },
        "runtime": { "worker": worker },
        "model_path": model,
        "files": [
            { "path": worker, "sha256": worker_sha256, "bytes": worker_bytes },
            { "path": model, "sha256": MODEL.sha256, "bytes": MODEL.bytes },
            { "path": clip, "sha256": HEALTH_CLIP.sha256, "bytes": HEALTH_CLIP.bytes },
        ],
        "options": {
            "health_audio": clip,
            "health_expected": HEALTH_EXPECTED,
            "beam_size": BEAM_SIZE,
            "flash_attn": FLASH_ATTN,
            "n_threads": cores,
        },
    })
}

pub(super) fn sha256_file(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1 << 16];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex(&hasher.finalize()))
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(64), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replies_parse_into_what_the_session_acts_on() {
        let cases = [
            (json!({"status": "ready", "model_id": "m"}), Reply::Ready),
            (
                json!({"status": "transcript", "text": "hi"}),
                Reply::Transcript("hi".into()),
            ),
            (json!({"status": "no_text"}), Reply::NoText),
            (json!({"status": "shutdown"}), Reply::Shutdown),
            (
                json!({"id": "", "status": "error", "code": "manifest_error", "message": "pinned artifact changed"}),
                Reply::Error {
                    code: "manifest_error".into(),
                    message: "pinned artifact changed".into(),
                },
            ),
        ];
        for (value, reply) in cases {
            assert_eq!(parse_reply(&value), reply, "{value}");
        }
        assert!(matches!(
            parse_reply(&json!({"status": "warming"})),
            Reply::Error { code, .. } if code == "invalid_reply"
        ));
    }

    #[test]
    fn a_request_round_trips_and_a_refused_manifest_reads_as_damaged() {
        let mut sent = Vec::new();
        let mut reply = Vec::new();
        write_frame(
            &mut reply,
            &json!({"status": "transcript", "text": "hello"}),
        )
        .unwrap();
        let heard = request(
            &mut sent,
            &mut reply.as_slice(),
            &control("x"),
            Some(&[1, 2]),
        )
        .unwrap();
        assert_eq!(heard, Reply::Transcript("hello".into()));
        let mut written = sent.as_slice();
        assert_eq!(read_frame(&mut written).unwrap()["op"], "x");
        assert_eq!(written, [1, 2]);

        let mut refusal = Vec::new();
        write_frame(
            &mut refusal,
            &json!({"id": "", "status": "error", "code": "manifest_error", "message": "m"}),
        )
        .unwrap();
        let failure = request(
            &mut Vec::new(),
            &mut refusal.as_slice(),
            &control("prepare"),
            None,
        )
        .unwrap_err();
        assert_eq!(failure.error, LocalError::Damaged);
        // A worker that died without a reply.
        let mut nothing: &[u8] = &[];
        let failure =
            request(&mut Vec::new(), &mut nothing, &control("prepare"), None).unwrap_err();
        assert_eq!(failure.error, LocalError::WorkerFailed);
    }

    #[test]
    fn the_manifest_pins_the_worker_the_model_and_the_clip() {
        let dir = tempfile::tempdir().unwrap();
        let worker = dir.path().join("voisu-local-asr.exe");
        fs::write(&worker, b"worker").unwrap();
        let document = manifest(
            &worker,
            &sha256_file(&worker).unwrap(),
            6,
            &dir.path().join(MODEL.name),
            &dir.path().join(HEALTH_CLIP.name),
            4,
        );
        assert_eq!(document["runtime"]["worker"], document["files"][0]["path"]);
        assert_eq!(document["model_path"], document["files"][1]["path"]);
        assert_eq!(
            document["options"]["health_audio"],
            document["files"][2]["path"]
        );
        assert_eq!(
            document["files"][0]["sha256"],
            // sha256("worker")
            "87eba76e7f3164534045ba922e7770fb58bbd14ad732bbf5ba6f11cc56989e6e"
        );
        assert_eq!(document["files"][1]["bytes"], MODEL.bytes);
        assert_eq!(document["limits"]["vram_bytes"], 0);
    }
}
