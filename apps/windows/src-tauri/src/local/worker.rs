//! One resident `voisu-local-asr.exe` while Local mode is on, as on Linux.
//!
//! A [`Resident`] owns one thread that starts the worker, sends `prepare`
//! once (the model load and the health check), then serves each Recording
//! with a single `transcribe`. Worker protocol v1 (`voisu-local-asr`): a
//! 4-byte little-endian length, then a JSON object; PCM follows a
//! `transcribe` frame as raw s16le bytes. Requests go one at a time, each id
//! is used once per worker, and each reply must answer the request sent.
//!
//! A worker asked for the GPU that fails to start there is started again on
//! the CPU. A worker that crashes is restarted, up to [`Restarts`]; past that
//! the resident fails with [`LocalError::KeepsStopping`] until the user acts.
//! Stopping sends `shutdown` and reads its reply before closing the pipes, so
//! the worker never writes into a closed pipe; it is killed if it has not
//! exited within [`SHUTDOWN_GRACE`].
//!
//! On Windows each worker starts suspended, goes into a kill-on-close Job
//! Object, and only then runs, so it can never outlive Voisu: not when Voisu
//! exits, crashes or is killed.

use std::collections::VecDeque;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::oneshot;
use voisu_core::local_tier::LocalTier;
use voisu_core::local_wire::{MAX_JSON_FRAME_BYTES, MAX_TRANSCRIPT_BYTES, PROTOCOL_VERSION};

use super::{HEALTH_CLIP, HEALTH_EXPECTED, LocalError};

/// Same decode settings as both Linux tiers.
const BEAM_SIZE: u32 = 5;
const FLASH_ATTN: bool = true;
/// More threads stop helping whisper.cpp long before this.
const MAX_THREADS: usize = 8;
/// How long a worker has to exit after `shutdown` is sent before it is killed.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);
/// How often an idle resident checks that its worker is still running.
const IDLE_CHECK: Duration = Duration::from_millis(500);
/// Manifests are `manifest-<pid>-<n>.json`, one per worker start, so a quick
/// restart never races the previous worker's file.
pub(super) const MANIFEST_PREFIX: &str = "manifest";
/// No console window flashes up for the worker or the GPU probe.
#[cfg(windows)]
pub(super) const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// The new process waits until it is in its Job Object.
#[cfg(windows)]
const CREATE_SUSPENDED: u32 = 0x0000_0004;

/// Variables that make a worker load a module from outside its own
/// directory and the system's: ggml's `GGML_BACKEND_PATH` (the worker
/// refuses to start on Windows while it is set), and the Vulkan loader's
/// driver and layer overrides, which the GPU probe would act on even for a
/// Cloud user. The loader then uses only the drivers and layers the system
/// registers.
const LOAD_OVERRIDES: &[&str] = &[
    "GGML_BACKEND_PATH",
    "VK_ICD_FILENAMES",
    "VK_DRIVER_FILES",
    "VK_ADD_DRIVER_FILES",
    "VK_LAYER_PATH",
    "VK_ADD_LAYER_PATH",
    "VK_IMPLICIT_LAYER_PATH",
    "VK_ADD_IMPLICIT_LAYER_PATH",
    "VK_INSTANCE_LAYERS",
    "VK_LOADER_LAYERS_ENABLE",
    "VK_LOADER_LAYERS_DISABLE",
    "VK_LOADER_DRIVERS_SELECT",
    "VK_LOADER_DRIVERS_DISABLE",
];

/// The environment every worker process starts with, the GPU probe's too:
/// none of [`LOAD_OVERRIDES`] reaches it.
pub(super) fn scrub_environment(command: &mut Command) {
    for name in LOAD_OVERRIDES {
        command.env_remove(name);
    }
}

/// What a resident starts its workers with.
pub struct Launch {
    pub worker: PathBuf,
    pub models: PathBuf,
    /// The worker's stderr, appended: whisper.cpp's own log lines.
    pub log: PathBuf,
    pub tier: LocalTier,
    /// Ask for the GPU the probe found. A GPU start that fails falls back to
    /// the CPU.
    pub gpu: bool,
}

/// A Local failure with the diagnostic that goes to the log only.
#[derive(Debug)]
pub struct Failure {
    pub error: LocalError,
    pub diagnostic: String,
    /// The worker can no longer be talked to: it died, or its replies no
    /// longer follow the protocol. A worker error reply leaves it usable.
    broken: bool,
}

impl Failure {
    fn new(error: LocalError, diagnostic: impl Into<String>) -> Self {
        Self {
            error,
            diagnostic: diagnostic.into(),
            broken: false,
        }
    }

    fn broken(diagnostic: impl Into<String>) -> Self {
        Self {
            broken: true,
            ..Self::new(LocalError::WorkerFailed, diagnostic)
        }
    }
}

/// `Some(text)`, or `None` when the worker heard no words.
pub type Heard = Result<Option<String>, Failure>;

/// Where a resident is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum State {
    /// Starting a worker: loading the model and checking its health.
    Starting,
    /// Ready to transcribe on `device`, as the worker reports it (`cpu`, or
    /// a GPU such as `vulkan:0`).
    Ready {
        device: String,
    },
    /// Refusing every Recording until the user acts.
    Failed(LocalError),
    Stopped,
}

/// Told after every [`State`] change.
pub type Changed = Arc<dyn Fn() + Send + Sync>;

struct Job {
    pcm: Vec<u8>,
    prompt: Option<String>,
    reply: oneshot::Sender<Heard>,
}

/// A running worker, and on Windows the Job Object that kills it when this
/// is dropped.
struct Worker {
    child: Child,
    #[cfg(windows)]
    _job: job::Job,
}

struct Shared {
    worker: Mutex<Option<Worker>>,
    state: Mutex<State>,
    stopping: AtomicBool,
    changed: Changed,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Shared {
    fn set(&self, state: State) {
        *lock(&self.state) = state;
        (self.changed)();
    }

    fn stopping(&self) -> bool {
        self.stopping.load(Ordering::SeqCst)
    }

    /// Ends the current worker process, if any. Never waits.
    fn kill(&self) {
        if let Some(worker) = lock(&self.worker).as_mut() {
            let _ = worker.child.kill();
        }
    }

    /// The current worker has exited.
    fn exited(&self) -> bool {
        lock(&self.worker)
            .as_mut()
            .is_none_or(|worker| !matches!(worker.child.try_wait(), Ok(None)))
    }

    /// Kills the current worker and waits for it.
    fn reap(&self) {
        let worker = lock(&self.worker).take();
        if let Some(mut worker) = worker {
            let _ = worker.child.kill();
            let _ = worker.child.wait();
        }
    }
}

/// The resident worker for one tier.
pub struct Resident {
    tier: LocalTier,
    jobs: Mutex<Option<mpsc::Sender<Job>>>,
    shared: Arc<Shared>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl Resident {
    /// Starts the worker and its `prepare` on a new thread; never blocks.
    pub fn start(launch: Launch, changed: Changed) -> Self {
        let (jobs, requests) = mpsc::channel();
        let shared = Arc::new(Shared {
            worker: Mutex::new(None),
            state: Mutex::new(State::Starting),
            stopping: AtomicBool::new(false),
            changed,
        });
        let tier = launch.tier;
        let thread_shared = Arc::clone(&shared);
        let thread = std::thread::Builder::new()
            .name("voisu-local-asr".into())
            .spawn(move || run(&launch, &thread_shared, &requests))
            .inspect_err(|error| {
                tracing::warn!(%error, "Local worker thread failed to start");
                shared.set(State::Failed(LocalError::WorkerFailed));
            })
            .ok();
        Self {
            tier,
            jobs: Mutex::new(Some(jobs)),
            shared,
            thread: Mutex::new(thread),
        }
    }

    pub fn tier(&self) -> LocalTier {
        self.tier
    }

    pub fn state(&self) -> State {
        lock(&self.shared.state).clone()
    }

    /// Transcribes one stopped Recording; a Recording sent while the worker
    /// starts waits for it. Bound it with a timeout and [`Self::kill`] the
    /// worker when it runs out.
    pub async fn transcribe(&self, pcm: Vec<u8>, prompt: Option<String>) -> Heard {
        let (reply, heard) = oneshot::channel();
        let sent = lock(&self.jobs)
            .as_ref()
            .is_some_and(|jobs| jobs.send(Job { pcm, prompt, reply }).is_ok());
        if !sent {
            return Err(Failure::new(
                LocalError::WorkerFailed,
                "Local worker stopped",
            ));
        }
        heard.await.unwrap_or_else(|_| {
            Err(Failure::new(
                LocalError::WorkerFailed,
                "Local worker thread ended without a result",
            ))
        })
    }

    /// Ends the worker process now, mid-request; the resident restarts it.
    pub fn kill(&self) {
        self.shared.kill();
    }

    /// Stops the resident. A worker still starting is killed; one serving a
    /// Recording finishes it first, then shuts down. Returns the thread to
    /// join, which can block for [`SHUTDOWN_GRACE`] and more: never join it
    /// on the main thread or the async executor.
    pub fn stop(&self) -> Option<JoinHandle<()>> {
        self.shared.stopping.store(true, Ordering::SeqCst);
        drop(lock(&self.jobs).take());
        if !matches!(self.state(), State::Ready { .. }) {
            self.shared.kill();
        }
        lock(&self.thread).take()
    }
}

/// How many crashes a resident restarts from: more than [`Self::MAX`] within
/// [`Self::WINDOW`] means something is wrong that a restart won't fix.
#[derive(Debug, Default)]
pub struct Restarts(VecDeque<Instant>);

impl Restarts {
    pub const MAX: usize = 3;
    pub const WINDOW: Duration = Duration::from_secs(5 * 60);

    /// Records a crash at `now`; whether to start the worker again.
    pub fn allow(&mut self, now: Instant) -> bool {
        while self
            .0
            .front()
            .is_some_and(|&crash| now.duration_since(crash) >= Self::WINDOW)
        {
            self.0.pop_front();
        }
        self.0.push_back(now);
        self.0.len() <= Self::MAX
    }
}

/// How serving a worker ended.
enum Served {
    /// The resident is stopping; the worker is still running.
    Stopped,
    Crashed(String),
}

/// The resident thread: workers started, served and restarted until stopped.
fn run(launch: &Launch, shared: &Shared, requests: &mpsc::Receiver<Job>) {
    let mut restarts = Restarts::default();
    let mut gpu = launch.gpu;
    let fatal = loop {
        if shared.stopping() {
            return shared.set(State::Stopped);
        }
        let started = Instant::now();
        match Conn::start(launch, gpu, shared) {
            Ok(mut conn) => {
                tracing::info!(
                    tier = launch.tier.name(),
                    device = %conn.device,
                    model_id = launch.tier.model_id(),
                    ms = started.elapsed().as_millis() as u64,
                    "Local worker ready"
                );
                shared.set(State::Ready {
                    device: conn.device.clone(),
                });
                match conn.serve(requests, shared) {
                    Served::Stopped => {
                        conn.shutdown(shared);
                        tracing::info!("Local worker stopped");
                        return shared.set(State::Stopped);
                    }
                    Served::Crashed(reason) => {
                        tracing::warn!(%reason, "Local worker stopped unexpectedly");
                    }
                }
            }
            Err(_) if shared.stopping() => {
                shared.reap();
                return shared.set(State::Stopped);
            }
            Err(failure) if gpu && failure.error != LocalError::Damaged => {
                shared.reap();
                tracing::warn!(
                    diagnostic = failure.diagnostic,
                    "Local worker couldn't start on the GPU; using the CPU"
                );
                gpu = false;
                continue;
            }
            Err(failure) if failure.error.needs_settings() => {
                shared.reap();
                tracing::warn!(diagnostic = failure.diagnostic, "Local worker can't start");
                break failure.error;
            }
            Err(failure) => {
                shared.reap();
                tracing::warn!(
                    diagnostic = failure.diagnostic,
                    "Local worker failed to start"
                );
            }
        }
        shared.reap();
        if shared.stopping() {
            return shared.set(State::Stopped);
        }
        if !restarts.allow(Instant::now()) {
            tracing::warn!(
                crashes = Restarts::MAX + 1,
                minutes = Restarts::WINDOW.as_secs() / 60,
                "Local worker keeps stopping; not restarting it"
            );
            break LocalError::KeepsStopping;
        }
        shared.set(State::Starting);
    };
    shared.set(State::Failed(fatal));
    // Every Recording sent until the user acts gets the failure.
    while let Ok(job) = requests.recv() {
        let _ = job
            .reply
            .send(Err(Failure::new(fatal, "Local worker failed earlier")));
    }
    shared.set(State::Stopped);
}

/// The pipes to one prepared worker.
struct Conn {
    stdin: ChildStdin,
    stdout: ChildStdout,
    /// The worker's observed device, from its `ready` reply.
    device: String,
    /// Transcribe requests sent, for their ids.
    sent: u64,
    _manifest: ManifestFile,
}

impl Conn {
    /// Writes the manifest, starts the worker and prepares it.
    fn start(launch: &Launch, gpu: bool, shared: &Shared) -> Result<Self, Failure> {
        let manifest = ManifestFile::write(launch, gpu).map_err(|error| {
            Failure::new(
                LocalError::WorkerFailed,
                format!("manifest write failed: {error}"),
            )
        })?;
        let (mut stdin, mut stdout) = spawn(launch, &manifest.0, shared)?;
        match request(&mut stdin, &mut stdout, &control(PREPARE), None)? {
            Reply::Ready { device } => Ok(Self {
                stdin,
                stdout,
                device,
                sent: 0,
                _manifest: manifest,
            }),
            other => Err(unexpected(PREPARE, &other)),
        }
    }

    /// Answers Recordings until the resident stops or the worker dies.
    fn serve(&mut self, requests: &mpsc::Receiver<Job>, shared: &Shared) -> Served {
        loop {
            match requests.recv_timeout(IDLE_CHECK) {
                Ok(job) => {
                    let heard = self.transcribe(job.pcm, job.prompt);
                    let broken = match &heard {
                        Err(failure) => failure.broken.then(|| failure.diagnostic.clone()),
                        Ok(_) => None,
                    };
                    let _ = job.reply.send(heard);
                    if let Some(reason) = broken {
                        return Served::Crashed(reason);
                    }
                }
                Err(RecvTimeoutError::Timeout) if shared.exited() => {
                    return Served::Crashed("exited while idle".into());
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return Served::Stopped,
            }
        }
    }

    fn transcribe(&mut self, pcm: Vec<u8>, prompt: Option<String>) -> Heard {
        self.sent += 1;
        let frame = transcribe_frame(self.sent, pcm.len(), prompt);
        match request(&mut self.stdin, &mut self.stdout, &frame, Some(&pcm))? {
            Reply::Transcript(text) => Ok(Some(text)),
            Reply::NoText => Ok(None),
            other => Err(unexpected(TRANSCRIBE, &other)),
        }
    }

    /// `shutdown`, its reply, then the worker's exit. The grace starts before
    /// the first write: a worker that stops reading is killed on time, which
    /// also ends a write stuck on its full pipe.
    fn shutdown(mut self, shared: &Shared) {
        let deadline = Instant::now() + SHUTDOWN_GRACE;
        std::thread::scope(|scope| {
            let watchdog = scope.spawn(|| {
                while !shared.exited() {
                    if Instant::now() >= deadline {
                        tracing::warn!("Local worker ignored shutdown; killed");
                        return shared.kill();
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
            });
            let replied = request(&mut self.stdin, &mut self.stdout, &control(SHUTDOWN), None);
            if !matches!(replied, Ok(Reply::Shutdown)) {
                tracing::debug!("Local worker shutdown not acknowledged");
            }
            drop(self.stdin);
            drop(self.stdout);
            let _ = watchdog.join();
        });
        shared.reap();
    }
}

/// `transcribe-<n>`: unique for the worker, whose ids must never repeat.
fn transcribe_frame(n: u64, pcm_bytes: usize, prompt: Option<String>) -> Value {
    let mut frame = json!({
        "v": PROTOCOL_VERSION,
        "id": format!("{TRANSCRIBE}-{n}"),
        "op": TRANSCRIBE,
        "pcm_bytes": pcm_bytes,
    });
    if let Some(prompt) = prompt {
        frame["prompt"] = Value::String(prompt);
    }
    frame
}

/// Starts the worker contained, and puts it where [`Shared::kill`] reaches
/// it. A resident already stopping gets its worker killed on arrival.
fn spawn(
    launch: &Launch,
    manifest: &Path,
    shared: &Shared,
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
    scrub_environment(&mut command);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(CREATE_NO_WINDOW | CREATE_SUSPENDED);
    }
    let mut child = command
        .spawn()
        .map_err(|error| Failure::broken(format!("worker spawn failed: {error}")))?;
    let pipes = child.stdin.take().zip(child.stdout.take());
    // Fail closed: a worker that cannot be contained never runs.
    #[cfg(windows)]
    let contained = job::Job::assign(&child).and_then(|job| job::resume(&child).map(|()| job));
    #[cfg(windows)]
    let worker = match contained {
        Ok(job) => Worker { child, _job: job },
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(Failure::broken(format!(
                "worker not started in a Job Object: {error}"
            )));
        }
    };
    #[cfg(not(windows))]
    let worker = Worker { child };
    {
        let mut slot = lock(&shared.worker);
        *slot = Some(worker);
        if shared.stopping() {
            drop(slot);
            shared.reap();
            return Err(Failure::broken("Local mode stopped"));
        }
    }
    pipes.ok_or_else(|| Failure::broken("worker pipes missing"))
}

/// Kill-on-close Job Objects: a worker in one ends when the last handle to
/// the job closes, which Windows does for Voisu however it exits.
#[cfg(windows)]
mod job {
    use std::os::windows::io::AsRawHandle;
    use std::process::Child;

    use windows::Win32::Foundation::{CloseHandle, E_FAIL, HANDLE};
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
    };
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_BASIC_LIMIT_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JobObjectExtendedLimitInformation, SetInformationJobObject,
    };
    use windows::Win32::System::Threading::{OpenThread, ResumeThread, THREAD_SUSPEND_RESUME};
    use windows::core::PCWSTR;

    /// A handle closed on drop.
    struct Owned(HANDLE);

    impl Drop for Owned {
        fn drop(&mut self) {
            // SAFETY: the handle was opened by this module and closes once.
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }

    pub struct Job(Owned);

    // SAFETY: a job handle is a kernel handle, valid from any thread.
    unsafe impl Send for Job {}

    impl Job {
        /// A new job holding `child`, which was started suspended, so it runs
        /// nothing before it is inside.
        pub fn assign(child: &Child) -> windows::core::Result<Self> {
            // SAFETY: the job handle is owned here (closed on drop), the
            // limits struct outlives the call, and the child's process
            // handle is open for as long as `child` lives.
            unsafe {
                let job = Self(Owned(CreateJobObjectW(None, PCWSTR::null())?));
                let limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION {
                    BasicLimitInformation: JOBOBJECT_BASIC_LIMIT_INFORMATION {
                        LimitFlags: JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                        ..Default::default()
                    },
                    ..Default::default()
                };
                SetInformationJobObject(
                    job.0.0,
                    JobObjectExtendedLimitInformation,
                    std::ptr::from_ref(&limits).cast(),
                    u32::try_from(size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>())
                        .unwrap_or(u32::MAX),
                )?;
                AssignProcessToJobObject(job.0.0, HANDLE(child.as_raw_handle()))?;
                Ok(job)
            }
        }
    }

    /// Resumes the suspended `child`. std closes the main thread's handle
    /// after `CreateProcessW`, so the thread is found by its process id; a
    /// process started suspended has only that one.
    pub fn resume(child: &Child) -> windows::core::Result<()> {
        // SAFETY: every handle opened here is owned and closed on drop, and
        // `entry` outlives the calls that fill it.
        unsafe {
            let snapshot = Owned(CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0)?);
            let mut entry = THREADENTRY32 {
                dwSize: u32::try_from(size_of::<THREADENTRY32>()).unwrap_or(u32::MAX),
                ..Default::default()
            };
            Thread32First(snapshot.0, &mut entry)?;
            let mut resumed = false;
            loop {
                if entry.th32OwnerProcessID == child.id() {
                    let thread = Owned(OpenThread(
                        THREAD_SUSPEND_RESUME,
                        false,
                        entry.th32ThreadID,
                    )?);
                    if ResumeThread(thread.0) == u32::MAX {
                        return Err(windows::core::Error::from_thread());
                    }
                    resumed = true;
                }
                if Thread32Next(snapshot.0, &mut entry).is_err() {
                    break;
                }
            }
            if resumed {
                Ok(())
            } else {
                Err(windows::core::Error::new(E_FAIL, "worker thread not found"))
            }
        }
    }
}

/// Request ids. `prepare` and `shutdown` go once per worker; each
/// `transcribe` is numbered.
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
            Reply::Invalid(reason) => {
                Err(Failure::broken(format!("invalid reply to {id}: {reason}")))
            }
            reply => Ok(reply),
        },
        Err(error) => Err(Failure::broken(format!(
            "worker reply unreadable: {error}; send: {sent:?}"
        ))),
    }
}

/// Names the reply without its text: diagnostics never carry a Transcript.
fn unexpected(op: &str, reply: &Reply) -> Failure {
    let status = match reply {
        Reply::Ready { .. } => "ready",
        Reply::Transcript(_) => "transcript",
        Reply::NoText => "no_text",
        Reply::Shutdown => "shutdown",
        Reply::Error { .. } => "error",
        Reply::Invalid(_) => "invalid",
    };
    Failure::broken(format!("unexpected reply to {op}: {status}"))
}

/// The reply statuses the app acts on.
#[derive(Debug, PartialEq, Eq)]
pub enum Reply {
    /// `device` is the worker's `observed_device` when it is a safe token
    /// (`cpu`, `vulkan:0`), else `unknown`.
    Ready {
        device: String,
    },
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
        Some("ready") => Reply::Ready {
            device: device_token(field("observed_device")),
        },
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

fn device_token(device: Option<&str>) -> String {
    match device {
        Some(device)
            if (1..=32).contains(&device.len())
                && device.bytes().all(|byte| {
                    byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b':'
                }) =>
        {
            device.to_owned()
        }
        _ => "unknown".to_owned(),
    }
}

/// The error codes `voisu-local-asr` sends, which are safe to log. Anything
/// else is logged as `unrecognized`, so a worker never puts text of its own
/// into Voisu's log.
const KNOWN_CODES: &[&str] = &[
    "manifest_error",
    "cpu_unsupported",
    "model_load_failed",
    "health_empty",
    "health_mismatch",
    "inference_failed",
    "too_many_windows",
    "transcript_too_large",
    "gpu_unavailable",
    "gpu_memory",
    "not_ready",
    "cancelled",
    "duplicate_id",
    "invalid_id",
    "invalid_request",
    "invalid_frame",
    "invalid_pcm",
    "unsupported_version",
    "unsupported_op",
];

fn error_code(code: Option<&str>) -> String {
    code.and_then(|code| KNOWN_CODES.iter().find(|known| **known == code))
        .map_or("unrecognized", |known| known)
        .to_owned()
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

/// One worker start's manifest in `models\`, pinning the worker, the model
/// and the health clip. The worker refuses to start (`manifest_error`) if
/// any pinned file has changed since. Deleted when the worker is done.
struct ManifestFile(PathBuf);

impl ManifestFile {
    fn write(launch: &Launch, gpu: bool) -> io::Result<Self> {
        static STARTS: AtomicU64 = AtomicU64::new(0);
        let worker_bytes = fs::metadata(&launch.worker)?.len();
        let cores = std::thread::available_parallelism()
            .map_or(4, usize::from)
            .min(MAX_THREADS);
        let document = manifest(
            launch,
            &sha256_file(&launch.worker)?,
            worker_bytes,
            gpu,
            cores,
        );
        let start = STARTS.fetch_add(1, Ordering::Relaxed);
        let path = launch.models.join(format!(
            "{MANIFEST_PREFIX}-{}-{start}.json",
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

/// The worker loads its GPU only with `"device": "gpu"` and the memory it
/// needs free there; a CPU manifest asks for none.
fn manifest(
    launch: &Launch,
    worker_sha256: &str,
    worker_bytes: u64,
    gpu: bool,
    cores: usize,
) -> Value {
    let model = launch.tier.model();
    let model_path = launch.models.join(model.name);
    let clip = launch.models.join(HEALTH_CLIP.name);
    let (device, vram_bytes) = if gpu {
        ("gpu", launch.tier.vram_bytes())
    } else {
        ("cpu", 0)
    };
    json!({
        "version": 1,
        "model_id": launch.tier.model_id(),
        "device": device,
        "language": "en",
        "limits": { "cpu_cores": cores, "vram_bytes": vram_bytes },
        "runtime": { "worker": launch.worker },
        "model_path": model_path,
        "files": [
            { "path": launch.worker, "sha256": worker_sha256, "bytes": worker_bytes },
            { "path": model_path, "sha256": model.sha256, "bytes": model.bytes },
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

    fn launch(dir: &Path, tier: LocalTier) -> Launch {
        let worker = dir.join("voisu-local-asr.exe");
        fs::write(&worker, b"worker").unwrap();
        Launch {
            worker,
            models: dir.to_owned(),
            log: dir.join("worker.log"),
            tier,
            gpu: false,
        }
    }

    #[test]
    fn replies_parse_into_what_the_resident_acts_on() {
        let cases = [
            (
                reply(
                    "prepare",
                    json!({"status": "ready", "model_id": "m", "observed_device": "vulkan:0"}),
                ),
                Reply::Ready {
                    device: "vulkan:0".into(),
                },
            ),
            (
                reply(
                    "prepare",
                    json!({"status": "ready", "observed_device": "Bad Device\n"}),
                ),
                Reply::Ready {
                    device: "unknown".into(),
                },
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
                reply("prepare", json!({"status": "error", "code": "gpu_memory"})),
                Reply::Error {
                    code: "gpu_memory".into(),
                },
            ),
            // A well-formed code the worker doesn't send is still not logged.
            (
                reply(
                    "prepare",
                    json!({"status": "error", "code": "my_secret_word"}),
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
        let invalid =
            |value: Value| matches!(parse_reply(&value, "transcribe-1"), Reply::Invalid(_));
        // Another request's id, or the empty id on anything but an error.
        assert!(invalid(reply("transcribe-2", json!({"status": "no_text"}))));
        assert!(invalid(reply("", json!({"status": "no_text"}))));
        assert!(invalid(json!({"v": PROTOCOL_VERSION, "status": "no_text"})));
        // Another protocol version, or none.
        assert!(invalid(
            json!({"v": 2, "id": "transcribe-1", "status": "no_text"})
        ));
        assert!(invalid(json!({"id": "transcribe-1", "status": "no_text"})));
        // A Transcript that is empty, missing or past the protocol's bound.
        assert!(invalid(reply(
            "transcribe-1",
            json!({"status": "transcript", "text": ""})
        )));
        assert!(invalid(reply(
            "transcribe-1",
            json!({"status": "transcript"})
        )));
        let long = "a".repeat(MAX_TRANSCRIPT_BYTES + 1);
        assert!(invalid(reply(
            "transcribe-1",
            json!({"status": "transcript", "text": long})
        )));
        let longest = "a".repeat(MAX_TRANSCRIPT_BYTES);
        assert_eq!(
            parse_reply(
                &reply(
                    "transcribe-1",
                    json!({"status": "transcript", "text": longest.clone()})
                ),
                "transcribe-1"
            ),
            Reply::Transcript(longest)
        );
        assert!(invalid(reply("transcribe-1", json!({"status": "warming"}))));
    }

    #[test]
    fn each_recording_gets_its_own_request_id() {
        let first = transcribe_frame(1, 4, None);
        let second = transcribe_frame(2, 6, Some("Voisu".into()));
        assert_eq!(first["id"], "transcribe-1");
        assert_eq!(second["id"], "transcribe-2");
        assert_eq!(first["op"], "transcribe");
        assert_eq!(second["pcm_bytes"], 6);
        assert_eq!(second["prompt"], "Voisu");
        assert!(first.get("prompt").is_none());
    }

    #[test]
    fn a_request_round_trips_and_only_a_dead_worker_is_broken() {
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
        // The worker's message stays out of the log, and the worker that
        // sent an error can still be asked again.
        assert_eq!(failure.diagnostic, "worker error manifest_error");
        assert!(!failure.broken);
        // A worker that died without a reply.
        let mut nothing: &[u8] = &[];
        let failure = request(&mut Vec::new(), &mut nothing, &control(PREPARE), None).unwrap_err();
        assert_eq!(failure.error, LocalError::WorkerFailed);
        assert!(failure.broken);
    }

    #[test]
    fn crashes_restart_the_worker_until_they_come_too_often() {
        let mut restarts = Restarts::default();
        let start = Instant::now();
        for crash in 0..Restarts::MAX {
            assert!(restarts.allow(start + Duration::from_secs(crash as u64)));
        }
        assert!(!restarts.allow(start + Duration::from_secs(10)));
        // Crashes older than the window are forgotten.
        let mut restarts = Restarts::default();
        for crash in 0..10u64 {
            assert!(restarts.allow(start + Restarts::WINDOW * u32::try_from(crash).unwrap()));
        }
    }

    #[test]
    fn each_worker_start_writes_its_own_manifest_and_deletes_it() {
        let dir = tempfile::tempdir().unwrap();
        let launch = launch(dir.path(), LocalTier::Fast);
        let first = ManifestFile::write(&launch, false).unwrap();
        let second = ManifestFile::write(&launch, true).unwrap();
        assert_ne!(first.0, second.0);
        assert!(first.0.exists() && second.0.exists());
        let kept = second.0.clone();
        drop(second);
        assert!(!kept.exists());
        assert!(first.0.exists());
    }

    #[test]
    fn the_manifest_pins_the_tier_and_asks_for_the_gpu_with_its_memory() {
        let dir = tempfile::tempdir().unwrap();
        for tier in LocalTier::ALL {
            let launch = launch(dir.path(), tier);
            let sha = sha256_file(&launch.worker).unwrap();
            let cpu = manifest(&launch, &sha, 6, false, 4);
            assert_eq!(cpu["model_id"], tier.model_id());
            assert_eq!(cpu["runtime"]["worker"], cpu["files"][0]["path"]);
            assert_eq!(cpu["model_path"], cpu["files"][1]["path"]);
            assert_eq!(cpu["options"]["health_audio"], cpu["files"][2]["path"]);
            assert_eq!(
                cpu["files"][0]["sha256"],
                // sha256("worker")
                "87eba76e7f3164534045ba922e7770fb58bbd14ad732bbf5ba6f11cc56989e6e"
            );
            assert_eq!(cpu["files"][1]["sha256"], tier.model().sha256);
            assert_eq!(cpu["files"][1]["bytes"], tier.model().bytes);
            assert_eq!(cpu["device"], "cpu");
            assert_eq!(cpu["limits"]["vram_bytes"], 0);
            let gpu = manifest(&launch, &sha, 6, true, 4);
            assert_eq!(gpu["device"], "gpu");
            assert_eq!(gpu["limits"]["vram_bytes"], tier.vram_bytes());
        }
    }

    #[test]
    fn the_worker_never_inherits_a_backend_path_or_vulkan_overrides() {
        let mut command = Command::new("voisu-local-asr");
        command.env("GGML_BACKEND_PATH", "C:\\evil.dll");
        command.env("VK_ICD_FILENAMES", "C:\\evil.json");
        command.env("VK_ADD_LAYER_PATH", "C:\\layers");
        scrub_environment(&mut command);
        let removed: Vec<_> = command
            .get_envs()
            .filter(|(_, value)| value.is_none())
            .map(|(key, _)| key.to_string_lossy().into_owned())
            .collect();
        for name in [
            "GGML_BACKEND_PATH",
            "VK_ICD_FILENAMES",
            "VK_DRIVER_FILES",
            "VK_ADD_DRIVER_FILES",
            "VK_LAYER_PATH",
            "VK_ADD_LAYER_PATH",
            "VK_IMPLICIT_LAYER_PATH",
            "VK_INSTANCE_LAYERS",
            "VK_LOADER_DRIVERS_SELECT",
        ] {
            assert!(removed.iter().any(|key| key == name), "{name} kept");
        }
    }
}
