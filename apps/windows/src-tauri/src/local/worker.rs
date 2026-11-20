//! One `voisu-local-asr.exe` per Recording, on its own thread.
//!
//! The thread writes the manifest, starts the worker and sends `prepare` as
//! soon as the Recording starts, so loading the model and its health check
//! overlap the user speaking. At stop the whole Recording goes over in one
//! `transcribe`; its result is handed back at once, then the worker gets
//! `shutdown` and [`SHUTDOWN_GRACE`] to exit before it is killed. Worker
//! protocol v1 (`voisu-local-asr`): a 4-byte little-endian length, then a
//! JSON object; PCM follows a `transcribe` frame as raw s16le bytes.
//! Requests go one at a time, and each reply must answer the request sent.
//!
//! Dropping the [`Session`] before its result kills the worker, so a
//! cancelled Recording or a missed deadline never leaves one running. On
//! Windows each worker is also in a kill-on-close Job Object, so it dies
//! with Voisu even when Voisu exits or is killed mid-Recording.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::oneshot;
use voisu_core::local_wire::{MAX_JSON_FRAME_BYTES, MAX_TRANSCRIPT_BYTES, PROTOCOL_VERSION};

use super::{HEALTH_CLIP, HEALTH_EXPECTED, LocalError, MODEL, MODEL_ID};

/// Same decode settings as the Linux Fast tier.
const BEAM_SIZE: u32 = 5;
const FLASH_ATTN: bool = true;
/// More threads stop helping whisper.cpp small long before this.
const MAX_THREADS: usize = 8;
/// How long a worker has to exit after `shutdown` before it is killed.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);
/// Manifests are `manifest-<pid>-<n>.json`, one per launch, so a quick retry
/// never races a cancelled launch still writing its own.
pub(super) const MANIFEST_PREFIX: &str = "manifest";

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

/// A running worker, and on Windows the Job Object that kills it when this
/// is dropped.
struct Worker {
    child: Child,
    #[cfg(windows)]
    _job: Option<job::Job>,
}

impl Worker {
    /// Kills the worker and waits for it. Blocks briefly: never call it on
    /// the async executor.
    fn reap(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[derive(Default)]
struct Slot {
    worker: Option<Worker>,
    /// The session ended before the worker started: kill it on arrival.
    killed: bool,
    /// The result is delivered and the worker thread is shutting it down.
    closing: bool,
}

fn lock(slot: &Mutex<Slot>) -> MutexGuard<'_, Slot> {
    slot.lock().unwrap_or_else(PoisonError::into_inner)
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
            .spawn(move || serve(&launch, &thread_slot, &jobs, done));
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
        let worker = {
            let mut slot = lock(&self.slot);
            slot.killed = true;
            // Once the result is out, the worker thread shuts it down.
            if slot.closing {
                None
            } else {
                slot.worker.take()
            }
        };
        if let Some(mut worker) = worker {
            // `kill` only asks Windows to end it; the wait happens off the
            // async executor. Should the thread not start, dropping the
            // worker closes its Job Object, which ends it too.
            let _ = worker.child.kill();
            let _ = std::thread::Builder::new()
                .name("voisu-local-asr-reap".into())
                .spawn(move || worker.reap());
        }
    }
}

/// The worker thread: one launch, from manifest to exit.
fn serve(
    launch: &Launch,
    slot: &Mutex<Slot>,
    jobs: &mpsc::Receiver<Job>,
    done: oneshot::Sender<Heard>,
) {
    let manifest = match ManifestFile::write(launch) {
        Ok(manifest) => manifest,
        Err(error) => {
            let _ = done.send(Err(Failure::new(
                LocalError::WorkerFailed,
                format!("manifest write failed: {error}"),
            )));
            return;
        }
    };
    let (mut stdin, mut stdout) = match spawn(launch, &manifest.0, slot) {
        Ok(pipes) => pipes,
        Err(failure) => {
            let _ = done.send(Err(failure));
            return;
        }
    };
    let heard = converse(&mut stdin, &mut stdout, jobs);
    lock(slot).closing = true;
    // The result stands whatever the shutdown below does.
    let _ = done.send(heard);
    close(stdin, stdout, slot);
    drop(manifest);
}

/// `prepare`, then the Recording's `transcribe`.
fn converse(stdin: &mut ChildStdin, stdout: &mut ChildStdout, jobs: &mpsc::Receiver<Job>) -> Heard {
    match request(stdin, stdout, &control(PREPARE), None)? {
        Reply::Ready => {}
        other => return Err(unexpected(PREPARE, &other)),
    }
    // The Sender is dropped without a job when the Recording is cancelled.
    let Ok(job) = jobs.recv() else {
        return Ok(None);
    };
    let mut frame = json!({
        "v": PROTOCOL_VERSION,
        "id": TRANSCRIBE,
        "op": TRANSCRIBE,
        "pcm_bytes": job.pcm.len(),
    });
    if let Some(prompt) = job.prompt {
        frame["prompt"] = Value::String(prompt);
    }
    match request(stdin, stdout, &frame, Some(&job.pcm))? {
        Reply::Transcript(text) => Ok(Some(text)),
        Reply::NoText => Ok(None),
        other => Err(unexpected(TRANSCRIBE, &other)),
    }
}

/// Asks the worker to exit and gives it [`SHUTDOWN_GRACE`]; kills it after.
/// The reply is not read: a stalled worker must not hold this thread.
fn close(mut stdin: ChildStdin, stdout: ChildStdout, slot: &Mutex<Slot>) {
    let _ = write_frame(&mut stdin, &control(SHUTDOWN));
    drop(stdin);
    drop(stdout);
    let deadline = Instant::now() + SHUTDOWN_GRACE;
    loop {
        let mut guard = lock(slot);
        let Some(worker) = guard.worker.as_mut() else {
            // The session killed it first.
            return;
        };
        let exited = !matches!(worker.child.try_wait(), Ok(None));
        if exited || Instant::now() >= deadline {
            let worker = guard.worker.take();
            drop(guard);
            if let Some(worker) = worker {
                if !exited {
                    tracing::warn!("Local worker ignored shutdown; killed");
                }
                worker.reap();
            }
            return;
        }
        drop(guard);
        std::thread::sleep(Duration::from_millis(50));
    }
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
    let worker = Worker {
        #[cfg(windows)]
        _job: job::Job::assign(&child)
            .inspect_err(|error| {
                tracing::warn!(%error, "Local worker not in a Job Object; it may outlive Voisu");
            })
            .ok(),
        child,
    };
    let mut guard = lock(slot);
    if guard.killed {
        drop(guard);
        worker.reap();
        return Err(Failure::new(LocalError::WorkerFailed, "Recording ended"));
    }
    guard.worker = Some(worker);
    pipes.ok_or_else(|| Failure::new(LocalError::WorkerFailed, "worker pipes missing"))
}

/// Kill-on-close Job Objects: a worker in one ends when the last handle to
/// the job closes, which Windows does for Voisu however it exits.
#[cfg(windows)]
mod job {
    use std::os::windows::io::AsRawHandle;
    use std::process::Child;

    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_BASIC_LIMIT_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JobObjectExtendedLimitInformation, SetInformationJobObject,
    };
    use windows::core::PCWSTR;

    pub struct Job(HANDLE);

    // SAFETY: a job handle is a kernel handle, valid from any thread.
    unsafe impl Send for Job {}

    impl Job {
        /// A new job holding `child`. The worker starts no processes of its
        /// own, so assigning it just after spawn leaves nothing outside.
        pub fn assign(child: &Child) -> windows::core::Result<Self> {
            // SAFETY: the job handle is owned here (closed on drop), the
            // limits struct outlives the call, and the child's process
            // handle is open for as long as `child` lives.
            unsafe {
                let job = Self(CreateJobObjectW(None, PCWSTR::null())?);
                let limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION {
                    BasicLimitInformation: JOBOBJECT_BASIC_LIMIT_INFORMATION {
                        LimitFlags: JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                        ..Default::default()
                    },
                    ..Default::default()
                };
                SetInformationJobObject(
                    job.0,
                    JobObjectExtendedLimitInformation,
                    std::ptr::from_ref(&limits).cast(),
                    u32::try_from(size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>())
                        .unwrap_or(u32::MAX),
                )?;
                AssignProcessToJobObject(job.0, HANDLE(child.as_raw_handle()))?;
                Ok(job)
            }
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            // SAFETY: the handle came from CreateJobObjectW and closes once.
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

/// Request ids: each op is sent at most once per worker.
const PREPARE: &str = "prepare";
const TRANSCRIBE: &str = "transcribe";
const SHUTDOWN: &str = "shutdown";

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
    let id = frame["id"].as_str().unwrap_or_default();
    let sent = write_frame(stdin, frame).and_then(|()| match pcm {
        Some(pcm) => stdin.write_all(pcm).and_then(|()| stdin.flush()),
        None => Ok(()),
    });
    match read_frame(stdout) {
        Ok(reply) => match parse_reply(&reply, id) {
            // The worker's message is not logged: it can quote input.
            Reply::Error { code } => Err(Failure::new(
                LocalError::from_code(&code),
                format!("worker error {code}"),
            )),
            Reply::Invalid(reason) => Err(Failure::new(
                LocalError::WorkerFailed,
                format!("invalid reply to {id}: {reason}"),
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
fn unexpected(op: &str, reply: &Reply) -> Failure {
    let status = match reply {
        Reply::Ready => "ready",
        Reply::Transcript(_) => "transcript",
        Reply::NoText => "no_text",
        Reply::Shutdown => "shutdown",
        Reply::Error { .. } => "error",
        Reply::Invalid(_) => "invalid",
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
    /// `code` is a worker error code, or `unrecognized`; never its message.
    Error {
        code: String,
    },
    /// Not a protocol v1 reply to the request sent.
    Invalid(&'static str),
}

/// Checks `value` against the request `id` it must answer. An error may also
/// carry the empty id: the worker sends that when it refused its manifest or
/// could not read the request.
pub fn parse_reply(value: &Value, id: &str) -> Reply {
    let field = |name: &str| value.get(name).and_then(Value::as_str);
    if value.get("v").and_then(Value::as_u64) != Some(u64::from(PROTOCOL_VERSION)) {
        return Reply::Invalid("wrong protocol version");
    }
    let status = field("status");
    let answers = match field("id") {
        Some(reply_id) => reply_id == id || (reply_id.is_empty() && status == Some("error")),
        None => false,
    };
    if !answers {
        return Reply::Invalid("answers another request");
    }
    match status {
        Some("ready") => Reply::Ready,
        Some("transcript") => match field("text") {
            Some(text) if !text.is_empty() && text.len() <= MAX_TRANSCRIPT_BYTES => {
                Reply::Transcript(text.to_owned())
            }
            _ => Reply::Invalid("transcript text missing or too long"),
        },
        Some("no_text") => Reply::NoText,
        Some("shutdown") => Reply::Shutdown,
        Some("error") => Reply::Error {
            code: error_code(field("code")),
        },
        _ => Reply::Invalid("unknown status"),
    }
}

/// A worker error code as it is safe to log: a short snake_case token.
fn error_code(code: Option<&str>) -> String {
    match code {
        Some(code)
            if (1..=40).contains(&code.len())
                && code.bytes().all(|byte| {
                    byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_'
                }) =>
        {
            code.to_owned()
        }
        _ => "unrecognized".to_owned(),
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

/// This launch's manifest in `models\`, pinning the worker, the model and
/// the health clip. The worker refuses to start (`manifest_error`) if any
/// pinned file has changed since. Deleted with the launch.
struct ManifestFile(PathBuf);

impl ManifestFile {
    fn write(launch: &Launch) -> io::Result<Self> {
        static LAUNCHES: AtomicU64 = AtomicU64::new(0);
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
        let launch_number = LAUNCHES.fetch_add(1, Ordering::Relaxed);
        let path = launch.models.join(format!(
            "{MANIFEST_PREFIX}-{}-{launch_number}.json",
            std::process::id()
        ));
        // Written whole before the worker is started, so no temp file.
        fs::write(&path, serde_json::to_vec_pretty(&document)?)?;
        Ok(Self(path))
    }
}

impl Drop for ManifestFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
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

    fn reply(id: &str, rest: Value) -> Value {
        let mut value = json!({"v": PROTOCOL_VERSION, "id": id});
        value
            .as_object_mut()
            .unwrap()
            .extend(rest.as_object().unwrap().clone());
        value
    }

    #[test]
    fn replies_parse_into_what_the_session_acts_on() {
        let cases = [
            (
                reply("prepare", json!({"status": "ready", "model_id": "m"})),
                Reply::Ready,
            ),
            (
                reply("prepare", json!({"status": "transcript", "text": "hi"})),
                Reply::Transcript("hi".into()),
            ),
            (
                reply("prepare", json!({"status": "no_text"})),
                Reply::NoText,
            ),
            (
                reply("prepare", json!({"status": "shutdown"})),
                Reply::Shutdown,
            ),
            (
                reply(
                    "",
                    json!({"status": "error", "code": "manifest_error", "message": "pinned artifact changed"}),
                ),
                Reply::Error {
                    code: "manifest_error".into(),
                },
            ),
            (
                reply(
                    "prepare",
                    json!({"status": "error", "code": "Bad code: see text"}),
                ),
                Reply::Error {
                    code: "unrecognized".into(),
                },
            ),
        ];
        for (value, expected) in cases {
            assert_eq!(parse_reply(&value, "prepare"), expected, "{value}");
        }
    }

    #[test]
    fn a_reply_must_answer_the_request_sent() {
        let invalid = |value: Value| matches!(parse_reply(&value, "transcribe"), Reply::Invalid(_));
        // Another request's id, or the empty id on anything but an error.
        assert!(invalid(reply("prepare", json!({"status": "no_text"}))));
        assert!(invalid(reply("", json!({"status": "no_text"}))));
        assert!(invalid(json!({"v": PROTOCOL_VERSION, "status": "no_text"})));
        // Another protocol version, or none.
        assert!(invalid(
            json!({"v": 2, "id": "transcribe", "status": "no_text"})
        ));
        assert!(invalid(json!({"id": "transcribe", "status": "no_text"})));
        // A Transcript that is empty, missing or past the protocol's bound.
        assert!(invalid(reply(
            "transcribe",
            json!({"status": "transcript", "text": ""})
        )));
        assert!(invalid(reply(
            "transcribe",
            json!({"status": "transcript"})
        )));
        let long = "a".repeat(MAX_TRANSCRIPT_BYTES + 1);
        assert!(invalid(reply(
            "transcribe",
            json!({"status": "transcript", "text": long})
        )));
        let longest = "a".repeat(MAX_TRANSCRIPT_BYTES);
        assert_eq!(
            parse_reply(
                &reply(
                    "transcribe",
                    json!({"status": "transcript", "text": longest.clone()})
                ),
                "transcribe"
            ),
            Reply::Transcript(longest)
        );
        assert!(invalid(reply("transcribe", json!({"status": "warming"}))));
    }

    #[test]
    fn a_request_round_trips_and_a_refused_manifest_reads_as_damaged() {
        let mut sent = Vec::new();
        let mut answer = Vec::new();
        write_frame(
            &mut answer,
            &reply("x", json!({"status": "transcript", "text": "hello"})),
        )
        .unwrap();
        let heard = request(
            &mut sent,
            &mut answer.as_slice(),
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
            &reply(
                "",
                json!({"status": "error", "code": "manifest_error", "message": "m"}),
            ),
        )
        .unwrap();
        let failure = request(
            &mut Vec::new(),
            &mut refusal.as_slice(),
            &control(PREPARE),
            None,
        )
        .unwrap_err();
        assert_eq!(failure.error, LocalError::Damaged);
        // The worker's message stays out of the log.
        assert_eq!(failure.diagnostic, "worker error manifest_error");
        // A worker that died without a reply.
        let mut nothing: &[u8] = &[];
        let failure = request(&mut Vec::new(), &mut nothing, &control(PREPARE), None).unwrap_err();
        assert_eq!(failure.error, LocalError::WorkerFailed);
    }

    #[test]
    fn each_launch_writes_its_own_manifest_and_deletes_it() {
        let dir = tempfile::tempdir().unwrap();
        let worker = dir.path().join("voisu-local-asr.exe");
        fs::write(&worker, b"worker").unwrap();
        let launch = Launch {
            worker,
            models: dir.path().to_owned(),
            log: dir.path().join("worker.log"),
        };
        let first = ManifestFile::write(&launch).unwrap();
        let second = ManifestFile::write(&launch).unwrap();
        assert_ne!(first.0, second.0);
        assert!(first.0.exists() && second.0.exists());
        let kept = second.0.clone();
        drop(second);
        assert!(!kept.exists());
        assert!(first.0.exists());
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
