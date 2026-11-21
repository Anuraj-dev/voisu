//! One resident `voisu-local-asr.exe` while Local mode is on, as on Linux.
//!
//! A [`Resident`] owns one thread that starts the worker, sends `prepare`
//! once (the model load and the health check), then serves each Recording
//! with a single `transcribe`. Worker protocol v1 (`voisu-local-asr`): a
//! 4-byte little-endian length, then a JSON object; PCM follows a
//! `transcribe` frame as raw s16le bytes. Requests go one at a time, each id
//! is used once per worker, and each reply must answer the request sent.
//!
//! A worker asked for the GPU that fails to start there, or crashes there
//! [`GPU_CRASHES`] times, is started again on the CPU with the CPU's tier. A
//! worker that crashes is restarted, up to [`Restarts`]; past that the
//! resident fails with [`LocalError::KeepsStopping`] until the user acts.
//! `prepare` gets [`PREPARE_DEADLINE`], and each Recording its own deadline:
//! one that runs out kills only the worker decoding it, and a Recording
//! still queued when its deadline passes is never sent.
//!
//! A resident replaced by another (a new tier, Cloud, Remove) is retired: it
//! serves the Recordings that hold a [`Lease`] on it, then stops. Its
//! replacement starts its first worker only once the old one has exited, so
//! two models never compete for memory. Stopping sends `shutdown` and reads
//! its reply before closing the pipes, so the worker never writes into a
//! closed pipe; it is killed if it has not exited within [`SHUTDOWN_GRACE`].
//!
//! On Windows each worker starts suspended, goes into a kill-on-close Job
//! Object, and only then runs, so it outlives Voisu only if Voisu dies in
//! the moment between the two, while the worker is still suspended.

use std::collections::VecDeque;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
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
/// How long `prepare` (the model load and the health check) may take before
/// the worker is killed. Accurate on the CPU takes longest; only a stuck
/// worker reaches it.
pub const PREPARE_DEADLINE: Duration = Duration::from_secs(90);
/// Crashes after a good `prepare` on the GPU before the resident moves to
/// the CPU: a GPU that loads the model but fails on longer Recordings.
const GPU_CRASHES: u32 = 2;
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
pub(super) const CREATE_SUSPENDED: u32 = 0x0000_0004;

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
    /// The tier to run if the GPU fails: `auto` resolved for the CPU, or the
    /// tier the user picked, which never changes.
    pub cpu_tier: LocalTier,
    /// Ask for the GPU the probe found. A GPU start that fails falls back to
    /// the CPU.
    pub gpu: bool,
}

/// A Local failure with the diagnostic that goes to the log only.
#[derive(Debug)]
pub struct Failure {
    pub error: LocalError,
    pub diagnostic: String,
    /// The worker can no longer be talked to: it died, its replies no
    /// longer follow the protocol, the request did not reach it whole, or
    /// it replied with an error it exits after. Only a
    /// [`RECOVERABLE_CODES`] reply leaves it usable.
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
    /// Ready to transcribe with `tier` on `device`, as the worker reports
    /// it (`cpu`, or a GPU such as `vulkan:0`). After a GPU fallback `tier`
    /// is the CPU's, which can differ from [`Resident::tier`].
    Ready {
        tier: LocalTier,
        device: String,
    },
    /// Refusing every Recording until the user acts.
    Failed(LocalError),
    Stopped,
}

/// Told after every [`State`] change.
pub type Changed = Arc<dyn Fn() + Send + Sync>;

struct Job {
    id: u64,
    pcm: Vec<u8>,
    prompt: Option<String>,
    /// When its Recording gives up: a job still queued then is never sent.
    expires: Instant,
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
    /// The job the worker is decoding, which its Recording's timeout kills.
    serving: Mutex<Option<u64>>,
    /// The resident this one replaced, until its last worker has exited.
    after: Mutex<Option<Arc<Resident>>>,
    /// The resident thread has ended and reaped its last worker.
    finished: Mutex<bool>,
    done: Condvar,
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

    /// A Recording gave up on job `id`: the worker dies only if it is
    /// decoding that job. Checked under the lock [`Conn::serve`] takes to
    /// start a job, so an expired job is either killed here or skipped there.
    fn cancel(&self, id: u64) {
        let serving = lock(&self.serving);
        if *serving == Some(id) {
            self.kill();
        }
    }

    fn finish(&self) {
        *lock(&self.finished) = true;
        self.done.notify_all();
    }

    /// Waits until the resident thread has ended, at most until `deadline`.
    fn wait_finished(&self, deadline: Option<Instant>) -> bool {
        let mut finished = lock(&self.finished);
        while !*finished {
            match deadline {
                None => {
                    finished = self
                        .done
                        .wait(finished)
                        .unwrap_or_else(PoisonError::into_inner)
                }
                Some(deadline) => {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return false;
                    }
                    finished = self
                        .done
                        .wait_timeout(finished, left)
                        .unwrap_or_else(PoisonError::into_inner)
                        .0;
                }
            }
        }
        true
    }
}

/// How many Recordings hold a resident, and whether it was replaced.
#[derive(Default)]
struct Leases {
    held: usize,
    retired: bool,
}

/// The resident worker for one tier.
pub struct Resident {
    tier: LocalTier,
    jobs: Mutex<Option<mpsc::Sender<Job>>>,
    shared: Arc<Shared>,
    leases: Mutex<Leases>,
    next_job: AtomicU64,
}

impl Resident {
    /// Starts the worker and its `prepare` on a new thread; never blocks.
    /// With `after`, the resident it replaces, the first worker starts once
    /// that one's last worker has exited.
    pub fn start(launch: Launch, after: Option<Arc<Resident>>, changed: Changed) -> Self {
        let (jobs, requests) = mpsc::channel();
        let shared = Arc::new(Shared {
            worker: Mutex::new(None),
            state: Mutex::new(State::Starting),
            stopping: AtomicBool::new(false),
            serving: Mutex::new(None),
            after: Mutex::new(after),
            finished: Mutex::new(false),
            done: Condvar::new(),
            changed,
        });
        let tier = launch.tier;
        let thread_shared = Arc::clone(&shared);
        let spawned = std::thread::Builder::new()
            .name("voisu-local-asr".into())
            .spawn(move || {
                let shared = thread_shared;
                let after = lock(&shared.after).clone();
                if let Some(after) = after {
                    after.wait_finished(None);
                    lock(&shared.after).take();
                }
                run(&launch, &shared, &requests);
                shared.finish();
            });
        if let Err(error) = spawned {
            tracing::warn!(%error, "Local worker thread failed to start");
            shared.set(State::Failed(LocalError::WorkerFailed));
            shared.finish();
        }
        Self {
            tier,
            jobs: Mutex::new(Some(jobs)),
            shared,
            leases: Mutex::new(Leases::default()),
            next_job: AtomicU64::new(0),
        }
    }

    /// The tier it was started for. [`State::Ready`] says which one runs.
    pub fn tier(&self) -> LocalTier {
        self.tier
    }

    pub fn state(&self) -> State {
        lock(&self.shared.state).clone()
    }

    /// Binds a Recording to this resident: retired meanwhile, it still
    /// serves that Recording, and stops only once every lease is dropped.
    pub fn lease(self: &Arc<Self>) -> Lease {
        lock(&self.leases).held += 1;
        Lease(Arc::clone(self))
    }

    /// Replaced: stops as soon as no Recording holds it. Never blocks.
    pub fn retire(&self) {
        let mut leases = lock(&self.leases);
        leases.retired = true;
        if leases.held == 0 {
            drop(leases);
            self.stop();
        }
    }

    /// Stops it now, held or not, and every resident it was waiting for.
    /// For Quit and Remove. Never blocks.
    pub fn halt(&self) {
        self.stop();
        let after = lock(&self.shared.after).clone();
        if let Some(after) = after {
            after.halt();
        }
    }

    /// Kills its worker and those of the residents it was waiting for, and
    /// waits for each to exit: what Quit does once its grace runs out.
    pub fn reap(&self) {
        self.shared.reap();
        let after = lock(&self.shared.after).clone();
        if let Some(after) = after {
            after.reap();
        }
    }

    /// Waits until its last worker, and those of the residents it waits
    /// for, have exited; at most until `deadline`. Whether they have.
    pub fn wait_finished(&self, deadline: Option<Instant>) -> bool {
        let after = lock(&self.shared.after).clone();
        after.is_none_or(|after| after.wait_finished(deadline))
            && self.shared.wait_finished(deadline)
    }

    /// Transcribes one stopped Recording within `limit`; a Recording sent
    /// while the worker starts waits for it. When `limit` runs out the
    /// worker is killed if it is decoding this Recording (the resident
    /// starts a fresh one), and the Recording is never sent if it is still
    /// queued.
    pub async fn transcribe(&self, pcm: Vec<u8>, prompt: Option<String>, limit: Duration) -> Heard {
        let id = self.next_job.fetch_add(1, Ordering::Relaxed);
        let expires = Instant::now() + limit;
        let (reply, heard) = oneshot::channel();
        let job = Job {
            id,
            pcm,
            prompt,
            expires,
            reply,
        };
        let sent = lock(&self.jobs)
            .as_ref()
            .is_some_and(|jobs| jobs.send(job).is_ok());
        if !sent {
            return Err(Failure::new(
                LocalError::WorkerFailed,
                "Local worker stopped",
            ));
        }
        match tokio::time::timeout(limit, heard).await {
            Ok(heard) => heard.unwrap_or_else(|_| {
                Err(Failure::new(
                    LocalError::WorkerFailed,
                    "Local worker thread ended without a result",
                ))
            }),
            Err(_) => {
                self.shared.cancel(id);
                Err(Failure::new(
                    LocalError::TimedOut,
                    format!("Local worker exceeded {} s", limit.as_secs()),
                ))
            }
        }
    }

    /// A worker still starting is killed; one serving a Recording finishes
    /// it first, then shuts down on the resident thread.
    fn stop(&self) {
        self.shared.stopping.store(true, Ordering::SeqCst);
        drop(lock(&self.jobs).take());
        if !matches!(self.state(), State::Ready { .. }) {
            self.shared.kill();
        }
    }
}

/// A Recording's hold on the resident that transcribes it.
pub struct Lease(Arc<Resident>);

impl std::ops::Deref for Lease {
    type Target = Resident;

    fn deref(&self) -> &Resident {
        &self.0
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        let mut leases = lock(&self.0.leases);
        leases.held -= 1;
        if leases.held == 0 && leases.retired {
            drop(leases);
            self.0.stop();
        }
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
    let mut tier = launch.tier;
    let mut gpu_crashes = 0;
    let fatal = loop {
        if shared.stopping() {
            return shared.set(State::Stopped);
        }
        let started = Instant::now();
        match Conn::start(launch, tier, gpu, shared) {
            Ok(mut conn) => {
                tracing::info!(
                    tier = tier.name(),
                    device = %conn.device,
                    model_id = tier.model_id(),
                    ms = started.elapsed().as_millis() as u64,
                    "Local worker ready"
                );
                shared.set(State::Ready {
                    tier,
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
                        if gpu {
                            gpu_crashes += 1;
                        }
                        if gpu && gpu_crashes >= GPU_CRASHES {
                            tracing::warn!(
                                crashes = gpu_crashes,
                                "Local worker keeps stopping on the GPU; using the CPU"
                            );
                            gpu = false;
                            tier = launch.cpu_tier;
                        }
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
                tier = launch.cpu_tier;
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
    /// A `prepare` past [`PREPARE_DEADLINE`] kills the worker.
    fn start(
        launch: &Launch,
        tier: LocalTier,
        gpu: bool,
        shared: &Shared,
    ) -> Result<Self, Failure> {
        let manifest = ManifestFile::write(launch, tier, gpu).map_err(|error| {
            Failure::new(
                LocalError::WorkerFailed,
                format!("manifest write failed: {error}"),
            )
        })?;
        let (mut stdin, mut stdout) = spawn(launch, &manifest.0, shared)?;
        let (replied, expired) = std::thread::scope(|scope| {
            let (prepared, wait) = mpsc::channel::<()>();
            let watchdog = scope.spawn(move || {
                let expired = matches!(
                    wait.recv_timeout(PREPARE_DEADLINE),
                    Err(RecvTimeoutError::Timeout)
                );
                if expired {
                    shared.kill();
                }
                expired
            });
            let replied = request(&mut stdin, &mut stdout, &control(PREPARE), None);
            drop(prepared);
            (replied, watchdog.join().unwrap_or(false))
        });
        if expired {
            return Err(Failure::broken(format!(
                "prepare took over {} s",
                PREPARE_DEADLINE.as_secs()
            )));
        }
        match replied? {
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
                    {
                        let mut serving = lock(&shared.serving);
                        if Instant::now() >= job.expires {
                            // Its Recording gave up while it waited.
                            continue;
                        }
                        *serving = Some(job.id);
                    }
                    let heard = self.transcribe(job.pcm, job.prompt);
                    *lock(&shared.serving) = None;
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
    let worker = match job::contain(&child) {
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
pub(super) mod job {
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
        fn assign(child: &Child) -> windows::core::Result<Self> {
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

    /// Puts `child`, started with [`super::CREATE_SUSPENDED`], into a new
    /// kill-on-close job, then lets it run. On an error it is still
    /// suspended: the caller kills it.
    pub fn contain(child: &Child) -> windows::core::Result<Job> {
        let job = Job::assign(child)?;
        resume(child)?;
        Ok(job)
    }

    /// Resumes the suspended `child`. std closes the main thread's handle
    /// after `CreateProcessW`, so the thread is found by its process id; a
    /// process started suspended has only that one.
    fn resume(child: &Child) -> windows::core::Result<()> {
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

/// Error replies after which the worker still serves: that one request
/// failed and the stream is intact. After any other error the worker exits
/// (a broken frame or PCM, a refused manifest) or is not worth another
/// request, so the connection is retired before the next one can write into
/// a closing pipe.
const RECOVERABLE_CODES: &[&str] = &[
    "inference_failed",
    "too_many_windows",
    "transcript_too_large",
    "cancelled",
];

/// Sends one request and reads its reply. A worker that refused its
/// manifest writes its error and exits before reading anything, so a failed
/// write still reads the reply waiting on stdout; the connection is retired
/// either way.
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
    let reply = match read_frame(stdout) {
        Ok(reply) => parse_reply(&reply, id),
        Err(error) => {
            return Err(Failure::broken(format!(
                "worker reply unreadable: {error}; send: {sent:?}"
            )));
        }
    };
    let failure = match reply {
        // The worker's message is not logged: it can quote input.
        Reply::Error { code } => Failure {
            broken: sent.is_err() || !RECOVERABLE_CODES.contains(&code.as_str()),
            ..Failure::new(LocalError::from_code(&code), format!("worker error {code}"))
        },
        Reply::Invalid(reason) => Failure::broken(format!("invalid reply to {id}: {reason}")),
        reply => match sent {
            Ok(()) => return Ok(reply),
            Err(error) => Failure::broken(format!("request {id} not sent whole: {error}")),
        },
    };
    Err(failure)
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
    "unknown_operation",
    // Sent just before the worker exits on a broken stream.
    "eof_mid_frame",
    "eof_mid_pcm",
    "read_failed",
    "frame_too_large",
    "invalid_json",
    "invalid_pcm_bytes",
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
    fn write(launch: &Launch, tier: LocalTier, gpu: bool) -> io::Result<Self> {
        static STARTS: AtomicU64 = AtomicU64::new(0);
        let worker_bytes = fs::metadata(&launch.worker)?.len();
        let cores = std::thread::available_parallelism()
            .map_or(4, usize::from)
            .min(MAX_THREADS);
        let document = manifest(
            launch,
            tier,
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
    tier: LocalTier,
    worker_sha256: &str,
    worker_bytes: u64,
    gpu: bool,
    cores: usize,
) -> Value {
    let model = tier.model();
    let model_path = launch.models.join(model.name);
    let clip = launch.models.join(HEALTH_CLIP.name);
    let (device, vram_bytes) = if gpu {
        ("gpu", tier.vram_bytes())
    } else {
        ("cpu", 0)
    };
    json!({
        "version": 1,
        "model_id": tier.model_id(),
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
            cpu_tier: tier,
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
    fn a_request_round_trips_and_a_dead_or_refusing_worker_is_broken() {
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
        // The worker's message stays out of the log, and a worker that
        // refused its manifest has exited.
        assert_eq!(failure.diagnostic, "worker error manifest_error");
        assert!(failure.broken);
        // A worker that died without a reply.
        let mut nothing: &[u8] = &[];
        let failure = request(&mut Vec::new(), &mut nothing, &control(PREPARE), None).unwrap_err();
        assert_eq!(failure.error, LocalError::WorkerFailed);
        assert!(failure.broken);
    }

    fn error_reply(code: &str) -> Vec<u8> {
        let mut bytes = Vec::new();
        write_frame(
            &mut bytes,
            &reply(
                "x",
                json!({"status": "error", "code": code, "message": "m"}),
            ),
        )
        .unwrap();
        bytes
    }

    /// A pipe the worker has already closed.
    struct Closed;

    impl Write for Closed {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::ErrorKind::BrokenPipe.into())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn only_a_recoverable_error_keeps_the_connection() {
        let ask = |answer: Vec<u8>| {
            request(&mut Vec::new(), &mut answer.as_slice(), &control("x"), None).unwrap_err()
        };
        let failed = ask(error_reply("inference_failed"));
        assert_eq!(failed.error, LocalError::TranscribeFailed);
        assert!(!failed.broken);
        assert!(!ask(error_reply("cancelled")).broken);
        // The worker exits after these: the next request would hit its
        // closing pipe (OS error 232).
        for code in [
            "invalid_pcm_bytes",
            "eof_mid_pcm",
            "frame_too_large",
            "not_ready",
        ] {
            let failure = ask(error_reply(code));
            assert!(failure.broken, "{code}");
            assert_eq!(failure.diagnostic, format!("worker error {code}"));
        }
        // A request that did not reach the worker whole retires it, even
        // when a reply can still be read.
        let failure = request(
            &mut Closed,
            &mut error_reply("inference_failed").as_slice(),
            &control("x"),
            None,
        )
        .unwrap_err();
        assert!(failure.broken);
        let mut answer = Vec::new();
        write_frame(&mut answer, &reply("x", json!({"status": "no_text"}))).unwrap();
        let failure =
            request(&mut Closed, &mut answer.as_slice(), &control("x"), None).unwrap_err();
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
        let first = ManifestFile::write(&launch, LocalTier::Fast, false).unwrap();
        let second = ManifestFile::write(&launch, LocalTier::Fast, true).unwrap();
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
            let cpu = manifest(&launch, tier, &sha, 6, false, 4);
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
            let gpu = manifest(&launch, tier, &sha, 6, true, 4);
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
