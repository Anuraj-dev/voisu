// Credential preparation/cleanup: cache, lanes, process ownership and the provider reaper.
//
// Split out of system.rs as a pure move; module-global items come from `super`.

use super::*;

/// How long a successfully-loaded credential is reused from the session cache
/// before the keyring is consulted again. Chosen to comfortably outlast any
/// transient Secret-Service hiccup (seconds) while bounding how long a
/// mid-session key rotation can be served stale to a few minutes — the daemon
/// re-reads the keyring once the entry expires. See docs/adr/
/// (2026-07-20) for why staleness is bounded by a TTL rather than by a
/// per-Recording 401/403 signal.
pub(super) const CREDENTIAL_CACHE_TTL: Duration = Duration::from_secs(300);

/// The credential-cache TTL, overridable via `VOISU_TEST_CREDENTIAL_CACHE_TTL_MS`
/// (0 = never cache, so every load re-reads) so tests can exercise the cache and
/// its expiry without real waits.
pub(super) fn credential_cache_ttl() -> Duration {
    if let Some(ms) = std::env::var("VOISU_TEST_CREDENTIAL_CACHE_TTL_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
    {
        return Duration::from_millis(ms);
    }
    CREDENTIAL_CACHE_TTL
}

/// One cached credential and the instant it was stored, for TTL expiry.
struct CachedCredential {
    credential: Credential,
    stored: Instant,
}

/// A session-scoped, in-process credential cache: at most one entry per provider,
/// each stamped with its load time so it expires after a bounded TTL. It lets a
/// credential that was successfully loaded earlier in the daemon's life survive a
/// later transient Secret-Service denial without re-shelling to `secret-tool` —
/// the reported failure mode (a warm daemon hitting one mid-session lookup
/// hiccup). The cache lives only in process memory: it is never written to disk
/// or logged, and `Credential` has no `Debug`, so a value cannot leak through it.
pub(super) struct CredentialCache {
    /// One slot per provider, indexed by [`CredentialCache::slot`].
    slots: Mutex<[Option<CachedCredential>; 2]>,
}

impl CredentialCache {
    pub(super) const fn new() -> Self {
        Self {
            slots: Mutex::new([None, None]),
        }
    }

    fn slot(provider: Provider) -> usize {
        match provider {
            Provider::Deepgram => 0,
            Provider::Groq => 1,
        }
    }

    /// Returns a fresh cached credential, or `None` when absent or expired. An
    /// expired entry is dropped in passing so a stale credential is never served.
    fn get(&self, provider: Provider, ttl: Duration) -> Option<Credential> {
        let mut slots = self.slots.lock().ok()?;
        let slot = &mut slots[Self::slot(provider)];
        match slot {
            Some(entry) if entry.stored.elapsed() < ttl => Some(entry.credential.clone()),
            Some(_) => {
                *slot = None;
                None
            }
            None => None,
        }
    }

    pub(super) fn put(&self, provider: Provider, credential: Credential) {
        if let Ok(mut slots) = self.slots.lock() {
            slots[Self::slot(provider)] = Some(CachedCredential {
                credential,
                stored: Instant::now(),
            });
        }
    }

    /// Drops a provider's entry so the next load re-reads the keyring. Kept for
    /// on-demand eviction (e.g. a future provider auth-rejection hook); today the
    /// TTL is the sole staleness bound.
    #[allow(dead_code)]
    pub(super) fn invalidate(&self, provider: Provider) {
        if let Ok(mut slots) = self.slots.lock() {
            slots[Self::slot(provider)] = None;
        }
    }
}

/// The daemon-process-wide credential cache. `SecretToolStore` is a unit struct
/// re-created at each call site, so the cache must be process-global to persist
/// across a session's Recordings.
static CREDENTIAL_CACHE: CredentialCache = CredentialCache::new();

pub(super) fn credential_cache() -> &'static CredentialCache {
    &CREDENTIAL_CACHE
}

/// Serves a provider credential from the session cache when a fresh entry exists,
/// otherwise runs `load`, caches a successful result, and returns it. Only a
/// successful load is cached — a failed load never poisons the cache — so a
/// transient denial neither serves nor stores a bad value.
pub(super) fn resolve_with_cache(
    provider: Provider,
    cache: &CredentialCache,
    ttl: Duration,
    load: impl FnOnce() -> Result<Credential, BoundaryError>,
) -> Result<Credential, BoundaryError> {
    if let Some(credential) = cache.get(provider, ttl) {
        return Ok(credential);
    }
    let credential = load()?;
    cache.put(provider, credential.clone());
    Ok(credential)
}

/// Actor-owned supervisor that keeps capture and provider-stream cleanup alive
/// and awaitable. When an adapter is dropped mid-abort — for example the abort
/// deadline elapsed and Tokio dropped the future that owned it — the adapter
/// hands its still-live cleanup task here. Adoption is SYNCHRONOUS: it retains
/// the raw handles inside a
/// future without spawning and without touching `Handle::try_current()`, so a
/// stream dropped from any thread — including during runtime teardown — always
/// lands its cleanup in this supervisor. The retained cleanup AWAITS each task
/// (never `abort()`, which would drop a nested `spawn_blocking` handle and
/// detach the still-running process cleanup before the child is reaped).
/// Drains are serialized: a concurrent drain waits for the in-flight one and
/// then re-checks, so it can never observe an empty supervisor while another
/// drain still holds unfinished cleanup. Each workflow task drains this
/// supervisor under an explicit bound after its streams have dropped and before
/// it acknowledges completion to the actor — the acknowledgement that alone
/// permits Idle — and the daemon drains it again after the actor has joined,
/// before the runtime is torn down.
///
/// The dedicated **credential lane** retains Architecture A pre-validation
/// credential process state (`CredentialCleanupEntry`) separately from provider
/// curl/capture cleanup futures. See `CredentialPreparationOwner`.
#[derive(Clone, Default)]
pub struct ProviderReaper {
    tasks: Arc<std::sync::Mutex<Vec<ReapTask>>>,
    /// Serializes `drain` calls. While one drain temporarily holds cleanup
    /// futures out of `tasks`, a concurrent drain must wait here instead of
    /// reading `tasks` as empty and reporting a completed drain over live work.
    serial: Arc<tokio::sync::Mutex<()>>,
    /// Dedicated lane for Smart Writing credential preparation cleanup (SW7).
    credential_lane: CredentialLane,
}

// ---------------------------------------------------------------------------
// SW7 — CredentialPreparationOwner + dedicated ProviderReaper credential lane
// ---------------------------------------------------------------------------

/// Work-deadline duration, overridable via `VOISU_TEST_CREDENTIAL_PREP_DEADLINE_MS`.
fn credential_prep_work_deadline() -> Duration {
    if let Some(ms) = std::env::var("VOISU_TEST_CREDENTIAL_PREP_DEADLINE_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
    {
        return Duration::from_millis(ms);
    }
    CREDENTIAL_PREP_WORK_DEADLINE
}

/// Reap-watchdog duration, overridable via `VOISU_TEST_CREDENTIAL_REAP_WATCHDOG_MS`.
fn credential_reap_watchdog() -> Duration {
    if let Some(ms) = std::env::var("VOISU_TEST_CREDENTIAL_REAP_WATCHDOG_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
    {
        return Duration::from_millis(ms);
    }
    CREDENTIAL_REAP_WATCHDOG
}

/// Artificial post-kill stall for hermetic watchdog-overrun tests.
fn credential_reap_stall() -> Option<Duration> {
    std::env::var("VOISU_TEST_CREDENTIAL_REAP_STALL_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_millis)
}

/// Closed reason why Minimal Grammar capability is unavailable for this Recording.
/// Secret-free; safe for diagnostics.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GrammarUnavailableReason {
    /// Prep was cancelled (provider failure, caught panic, or explicit cancel).
    Cancelled,
    /// The 13 s work deadline expired before a credential was resolved.
    WorkDeadlineExceeded,
    /// No env/cache/keyring/file credential was found.
    NoCredential,
    /// Desktop keyring is locked or refused access (and no file fallback).
    KeyringLocked,
    /// No Secret Service is available (and no file fallback).
    KeyringUnavailable,
    /// `secret-tool` is not installed (and no file fallback).
    ToolMissing,
    /// Credential bytes were present but rejected by `Credential::new`.
    InvalidCredential,
}

/// Who currently drives the retained entry toward Terminal.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DriveClaim {
    Free,
    Normal,
    Supervisor,
}

/// Live Tokio child + capped async pipes retained on the entry (not a task local).
struct CredentialRunningChild {
    child: tokio::process::Child,
    pgid: libc::pid_t,
    stdout: Option<tokio::process::ChildStdout>,
    stderr: Option<tokio::process::ChildStderr>,
    stdout_buf: Vec<u8>,
    stderr_buf: Vec<u8>,
    /// True after `Child::wait`/`try_wait` observed exit. A second wait must not
    /// run; remaining work is stdout/stderr drain to EOF only.
    wait_done: bool,
    /// Exit success bit when `wait_done` (ignored otherwise).
    exit_success: bool,
    /// True after a read observed EOF (or hard error) on stdout, or never had a pipe.
    stdout_eof: bool,
    /// True after a read observed EOF (or hard error) on stderr, or never had a pipe.
    stderr_eof: bool,
}

/// Terminal outcome after child wait + both pipe EOFs (or no-child fast path).
#[derive(Clone)]
enum CredentialTerminalOutcome {
    Ready(Credential),
    Unavailable(GrammarUnavailableReason),
}

struct CredentialEntryInner {
    phase: CredentialEntryPhase,
    drive: DriveClaim,
    running: Option<CredentialRunningChild>,
    /// Durable process-group id retained after `running` is taken for wait.
    /// Drop / supervisor kill use this when the Child handle is mid-reap so a
    /// cancelled drive future cannot leave a live OS process without a signal.
    last_pgid: Option<libc::pid_t>,
    terminal: Option<CredentialTerminalOutcome>,
    /// True once a child was spawned for this entry.
    launched_child: bool,
    watchdog_overrun_logged: bool,
    /// Hermetic test stall applied at most once after kill.
    reap_stall_applied: bool,
    /// Set when a reap path observed stdout EOF (or no-pipe / no-child terminal).
    stdout_eof_observed: bool,
    /// Set when a reap path observed stderr EOF (or no-pipe / no-child terminal).
    stderr_eof_observed: bool,
}

/// Retained credential process state registered on the reaper credential lane
/// **before** the first owner poll may launch work.
pub struct CredentialCleanupEntry {
    id: u64,
    kill_requested: std::sync::atomic::AtomicBool,
    cancel_requested: std::sync::atomic::AtomicBool,
    inner: std::sync::Mutex<CredentialEntryInner>,
}

impl CredentialCleanupEntry {
    fn new(id: u64) -> Self {
        Self {
            id,
            kill_requested: std::sync::atomic::AtomicBool::new(false),
            cancel_requested: std::sync::atomic::AtomicBool::new(false),
            inner: std::sync::Mutex::new(CredentialEntryInner {
                phase: CredentialEntryPhase::Registered,
                drive: DriveClaim::Free,
                running: None,
                last_pgid: None,
                terminal: None,
                launched_child: false,
                watchdog_overrun_logged: false,
                reap_stall_applied: false,
                stdout_eof_observed: false,
                stderr_eof_observed: false,
            }),
        }
    }

    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn phase(&self) -> CredentialEntryPhase {
        self.inner.lock().unwrap_or_else(|p| p.into_inner()).phase
    }

    pub fn has_launched_child(&self) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .launched_child
    }

    /// Process-group id of the last launched credential child, if any still
    /// needs kill/wait ownership. Survives take-for-reap so Drop can signal.
    pub fn retained_pgid(&self) -> Option<libc::pid_t> {
        let guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        guard.running.as_ref().map(|r| r.pgid).or(guard.last_pgid)
    }

    pub fn watchdog_overrun_logged(&self) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .watchdog_overrun_logged
    }

    /// Whether stdout and stderr EOFs were observed (or no-child/no-pipe terminal).
    /// Used by tests to prove cancel/Drop paths never abandon pipes without EOF.
    pub fn both_pipe_eofs_observed(&self) -> bool {
        let guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        guard.stdout_eof_observed && guard.stderr_eof_observed
    }

    /// True when no live child/pipe buffers remain on the entry (post-terminal).
    pub fn has_retained_running_child(&self) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .running
            .is_some()
    }

    pub fn is_cancel_requested(&self) -> bool {
        self.cancel_requested
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Synchronously request cancel and process-group SIGKILL. Does **not**
    /// wait for terminal, deregister, or release a live drive claim — the
    /// driving owner finishes or `Drop` releases the claim so the supervisor
    /// can resume.
    pub fn request_cancel_and_kill(&self) {
        self.cancel_requested
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.kill_process_group_once();
        let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        if matches!(
            guard.phase,
            CredentialEntryPhase::Registered | CredentialEntryPhase::Running
        ) {
            guard.phase = CredentialEntryPhase::CancelRequested;
        }
    }

    /// Signal SIGKILL to the retained process group when a durable pgid exists.
    ///
    /// Pre-spawn cancel must **not** sticky-consume this path with no signal:
    /// if `running` / `last_pgid` is still empty, leave `kill_requested` false so
    /// a later post-spawn kill can fire. Once a pgid is known, SIGKILL is always
    /// sent (idempotent) and `kill_requested` is set for classification.
    fn kill_process_group_once(&self) {
        let pgid = {
            let guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
            guard.running.as_ref().map(|r| r.pgid).or(guard.last_pgid)
        };
        let Some(pgid) = pgid else {
            // No child yet (or fully reaped). Do not stick kill_requested.
            return;
        };
        // SAFETY: pgid is the child's process-group id established at spawn
        // via setpgid(0,0). Negative pid targets the whole group.
        let _ = unsafe { libc::kill(-pgid, libc::SIGKILL) };
        self.kill_requested
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    fn try_claim(&self, who: DriveClaim) -> bool {
        let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        if guard.drive != DriveClaim::Free {
            return false;
        }
        if matches!(
            guard.phase,
            CredentialEntryPhase::Terminal | CredentialEntryPhase::Deregistered
        ) {
            return false;
        }
        guard.drive = who;
        true
    }

    fn release_claim(&self) {
        let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        guard.drive = DriveClaim::Free;
    }

    fn mark_watchdog_overrun(&self) -> bool {
        let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        if guard.watchdog_overrun_logged {
            return false;
        }
        guard.watchdog_overrun_logged = true;
        true
    }

    fn set_terminal(&self, outcome: CredentialTerminalOutcome) {
        let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        if matches!(
            guard.phase,
            CredentialEntryPhase::Terminal | CredentialEntryPhase::Deregistered
        ) {
            return;
        }
        // Drop any retained child handles; wait+EOF already completed.
        // No-child (or never-launched) terminal is vacuously both-EOF.
        if !guard.launched_child {
            guard.stdout_eof_observed = true;
            guard.stderr_eof_observed = true;
        }
        guard.running = None;
        guard.last_pgid = None;
        guard.terminal = Some(outcome);
        guard.phase = CredentialEntryPhase::Terminal;
        guard.drive = DriveClaim::Free;
    }

    fn terminal_capability(&self) -> Option<GrammarCapability> {
        let guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        match &guard.terminal {
            Some(CredentialTerminalOutcome::Ready(credential)) => Some(GrammarCapability::Ready(
                ReadyGrammarCapability::new(credential.clone()),
            )),
            Some(CredentialTerminalOutcome::Unavailable(reason)) => {
                Some(GrammarCapability::Unavailable(reason.clone()))
            }
            None => None,
        }
    }
}

/// Dedicated ProviderReaper lane for credential cleanup entries.
#[derive(Clone, Default)]
pub struct CredentialLane {
    inner: Arc<CredentialLaneInner>,
}

struct CredentialLaneInner {
    entries: std::sync::Mutex<std::collections::HashMap<u64, Arc<CredentialCleanupEntry>>>,
    next_id: std::sync::atomic::AtomicU64,
}

impl Default for CredentialLaneInner {
    fn default() -> Self {
        Self {
            entries: std::sync::Mutex::new(std::collections::HashMap::new()),
            next_id: std::sync::atomic::AtomicU64::new(1),
        }
    }
}

impl CredentialLane {
    pub fn new() -> Self {
        Self::default()
    }

    /// Inserts a retained cleanup entry **before** any child may launch.
    /// First owner poll is the only path that may start work against this entry.
    pub fn register(&self) -> Arc<CredentialCleanupEntry> {
        let id = self
            .inner
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let entry = Arc::new(CredentialCleanupEntry::new(id));
        let mut guard = self.inner.entries.lock().unwrap_or_else(|p| p.into_inner());
        guard.insert(id, Arc::clone(&entry));
        entry
    }

    /// Idempotent removal after Terminal. Double deregistration is a no-op.
    pub fn deregister(&self, entry: &CredentialCleanupEntry) {
        let mut guard = entry.inner.lock().unwrap_or_else(|p| p.into_inner());
        if guard.phase == CredentialEntryPhase::Deregistered {
            return;
        }
        // Only Terminal → Deregistered (or force from Terminal).
        if guard.phase != CredentialEntryPhase::Terminal {
            return;
        }
        guard.phase = CredentialEntryPhase::Deregistered;
        drop(guard);
        let mut map = self.inner.entries.lock().unwrap_or_else(|p| p.into_inner());
        map.remove(&entry.id);
    }

    /// Force-remove after supervisor has driven Terminal (or entry is stuck
    /// CancelRequested with no child). Idempotent.
    pub fn force_deregister(&self, entry: &CredentialCleanupEntry) {
        {
            let mut guard = entry.inner.lock().unwrap_or_else(|p| p.into_inner());
            if guard.phase == CredentialEntryPhase::Deregistered {
                return;
            }
            if guard.phase != CredentialEntryPhase::Terminal {
                // Supervisor may only force-remove once terminal outcome is set.
                if guard.terminal.is_none() {
                    return;
                }
                guard.phase = CredentialEntryPhase::Terminal;
            }
            guard.phase = CredentialEntryPhase::Deregistered;
        }
        let mut map = self.inner.entries.lock().unwrap_or_else(|p| p.into_inner());
        map.remove(&entry.id);
    }

    pub fn contains(&self, id: u64) -> bool {
        self.inner
            .entries
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .contains_key(&id)
    }

    pub fn pending_count(&self) -> usize {
        self.inner
            .entries
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .len()
    }

    fn snapshot_entries(&self) -> Vec<Arc<CredentialCleanupEntry>> {
        self.inner
            .entries
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .values()
            .cloned()
            .collect()
    }

    /// Supervisor / shutdown drain: claim each retained entry, drive to Terminal,
    /// deregister. Blocks `Completed`/Idle semantics when called from
    /// `supervise_recording` (SW10 wiring). Idempotent when the lane is empty.
    pub async fn drain_all(&self) {
        loop {
            let entries = self.snapshot_entries();
            if entries.is_empty() {
                return;
            }
            for entry in entries {
                drive_entry_to_terminal_as(entry.as_ref(), DriveClaim::Supervisor).await;
                // Ensure a terminal outcome even if already terminal with none
                // (should not happen).
                if entry.terminal_capability().is_none() {
                    entry.set_terminal(CredentialTerminalOutcome::Unavailable(
                        GrammarUnavailableReason::Cancelled,
                    ));
                }
                self.force_deregister(entry.as_ref());
            }
        }
    }
}

/// Owns one pre-validation credential preparation for Groq Minimal Grammar.
///
/// Lifecycle (Architecture A):
/// 1. `CredentialLane::register` retains the entry.
/// 2. `CredentialPreparationOwner::new` binds the entry (no launch yet).
/// 3. First `poll_outcome` / drive may launch restricted Tokio `secret-tool`.
/// 4. Normal path: Terminal then Deregistered **before** Validation.
/// 5. `Drop`: sync cancel + process-group kill; cannot claim Terminal.
///
/// Does **not** reuse blocking `SecretToolStore::load` / `run_restricted`.
pub struct CredentialPreparationOwner {
    entry: Arc<CredentialCleanupEntry>,
    lane: CredentialLane,
    provider: Provider,
    /// Whether this owner currently holds (or held) the Normal drive claim.
    claimed_normal: bool,
    finished: bool,
}

impl CredentialPreparationOwner {
    /// Bind a previously registered entry. First poll may launch work.
    pub fn new(
        entry: Arc<CredentialCleanupEntry>,
        lane: CredentialLane,
        provider: Provider,
    ) -> Self {
        Self {
            entry,
            lane,
            provider,
            claimed_normal: false,
            finished: false,
        }
    }

    pub fn entry(&self) -> &Arc<CredentialCleanupEntry> {
        &self.entry
    }

    pub fn lane(&self) -> &CredentialLane {
        &self.lane
    }

    /// Poll the preparation to a terminal `GrammarCapability`, then deregister.
    /// Fast paths: env override, session cache, test seam. Cache miss uses a
    /// Tokio process for restricted `secret-tool lookup` retained on the entry.
    pub async fn poll_outcome(&mut self) -> GrammarCapability {
        if self.finished {
            return self
                .entry
                .terminal_capability()
                .unwrap_or(GrammarCapability::Unavailable(
                    GrammarUnavailableReason::Cancelled,
                ));
        }
        if !self.entry.try_claim(DriveClaim::Normal) {
            // Another driver owns the entry; wait until terminal appears.
            let capability = wait_for_terminal_capability(&self.entry).await;
            self.finished = true;
            return capability;
        }
        self.claimed_normal = true;
        let capability = drive_credential_work(&self.entry, self.provider).await;
        self.entry.release_claim();
        self.claimed_normal = false;
        self.lane.deregister(&self.entry);
        // If phase was Terminal, deregister advanced to Deregistered.
        // If set_terminal was skipped (race), force.
        if self.lane.contains(self.entry.id()) && self.entry.terminal_capability().is_some() {
            self.lane.force_deregister(&self.entry);
        }
        self.finished = true;
        capability
    }

    /// Cancel, kill, reap to Terminal (with 2 s watchdog diagnostic), deregister.
    /// Used on provider failure, caught concurrent panic, or explicit abort.
    pub async fn cancel_and_drive_terminal(&mut self) -> GrammarCapability {
        if self.finished
            && matches!(
                self.entry.phase(),
                CredentialEntryPhase::Deregistered | CredentialEntryPhase::Terminal
            )
        {
            let cap = self
                .entry
                .terminal_capability()
                .unwrap_or(GrammarCapability::Unavailable(
                    GrammarUnavailableReason::Cancelled,
                ));
            if self.entry.phase() == CredentialEntryPhase::Terminal {
                self.lane.deregister(&self.entry);
                if self.lane.contains(self.entry.id()) {
                    self.lane.force_deregister(&self.entry);
                }
            }
            self.finished = true;
            return cap;
        }

        self.entry
            .cancel_requested
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.entry.kill_process_group_once();
        {
            let mut guard = self.entry.inner.lock().unwrap_or_else(|p| p.into_inner());
            if matches!(
                guard.phase,
                CredentialEntryPhase::Registered | CredentialEntryPhase::Running
            ) {
                guard.phase = CredentialEntryPhase::CancelRequested;
            }
        }

        // Resume a Normal claim interrupted by dropping an in-flight poll, or
        // take a free claim. Never spin forever on a stale Normal we own.
        let claimed = if self.claimed_normal {
            true
        } else if self.entry.try_claim(DriveClaim::Normal) {
            self.claimed_normal = true;
            true
        } else {
            false
        };

        let entry = Arc::clone(&self.entry);
        // reap_running_child_to_terminal already applies the 2 s watchdog; the
        // wait_for_terminal_capability branch may claim and reap under the same
        // helper. Wrap the whole drive so a stuck peer-wait also diagnoses.
        with_credential_reap_watchdog(&entry, async {
            if claimed {
                reap_running_child_to_terminal(
                    &entry,
                    CredentialTerminalOutcome::Unavailable(GrammarUnavailableReason::Cancelled),
                )
                .await;
            } else {
                // Wait for the other driver or become supervisor-like.
                wait_for_terminal_capability(&entry).await;
            }
        })
        .await;

        if claimed {
            self.entry.release_claim();
            self.claimed_normal = false;
        }

        if self.entry.terminal_capability().is_none() {
            self.entry
                .set_terminal(CredentialTerminalOutcome::Unavailable(
                    GrammarUnavailableReason::Cancelled,
                ));
        }
        self.lane.deregister(&self.entry);
        if self.lane.contains(self.entry.id()) {
            self.lane.force_deregister(&self.entry);
        }
        self.finished = true;
        self.entry
            .terminal_capability()
            .unwrap_or(GrammarCapability::Unavailable(
                GrammarUnavailableReason::Cancelled,
            ))
    }

    /// After concurrent join: ensure Terminal + Deregistered, return capability.
    pub async fn finish_terminal(&mut self, outcome: GrammarCapability) -> GrammarCapability {
        if self.finished && self.entry.phase() == CredentialEntryPhase::Deregistered {
            return outcome;
        }
        if !matches!(
            self.entry.phase(),
            CredentialEntryPhase::Terminal | CredentialEntryPhase::Deregistered
        ) {
            return self.cancel_and_drive_terminal().await;
        }
        if self.entry.phase() == CredentialEntryPhase::Terminal {
            self.lane.deregister(&self.entry);
            if self.lane.contains(self.entry.id()) {
                self.lane.force_deregister(&self.entry);
            }
        }
        self.finished = true;
        self.entry.terminal_capability().unwrap_or(outcome)
    }
}

impl Drop for CredentialPreparationOwner {
    fn drop(&mut self) {
        if self.finished && self.entry.phase() == CredentialEntryPhase::Deregistered {
            return;
        }
        // Sync cancel + process-group kill backstop. Cannot declare Terminal or
        // remove the entry — supervisor / shutdown drain owns that. Releasing
        // the drive claim lets supervise_recording resume the retained entry.
        self.entry.request_cancel_and_kill();
        self.entry.release_claim();
        self.claimed_normal = false;
    }
}

async fn wait_for_terminal_capability(entry: &CredentialCleanupEntry) -> GrammarCapability {
    loop {
        if let Some(cap) = entry.terminal_capability() {
            return cap;
        }
        if matches!(entry.phase(), CredentialEntryPhase::Deregistered) {
            return entry
                .terminal_capability()
                .unwrap_or(GrammarCapability::Unavailable(
                    GrammarUnavailableReason::Cancelled,
                ));
        }
        // Try to claim as supervisor-style driver if free.
        if entry.try_claim(DriveClaim::Supervisor) {
            reap_running_child_to_terminal(
                entry,
                CredentialTerminalOutcome::Unavailable(GrammarUnavailableReason::Cancelled),
            )
            .await;
            entry.release_claim();
            return entry
                .terminal_capability()
                .unwrap_or(GrammarCapability::Unavailable(
                    GrammarUnavailableReason::Cancelled,
                ));
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

async fn drive_entry_to_terminal_as(entry: &CredentialCleanupEntry, who: DriveClaim) {
    if matches!(
        entry.phase(),
        CredentialEntryPhase::Terminal | CredentialEntryPhase::Deregistered
    ) {
        return;
    }
    entry
        .cancel_requested
        .store(true, std::sync::atomic::Ordering::SeqCst);
    entry.kill_process_group_once();
    if !entry.try_claim(who) {
        // Spin until the other driver finishes or frees the claim.
        let _ = wait_for_terminal_capability(entry).await;
        return;
    }
    reap_running_child_to_terminal(
        entry,
        CredentialTerminalOutcome::Unavailable(GrammarUnavailableReason::Cancelled),
    )
    .await;
    entry.release_claim();
}

/// Drive credential resolution on a claimed entry through Terminal.
async fn drive_credential_work(
    entry: &Arc<CredentialCleanupEntry>,
    provider: Provider,
) -> GrammarCapability {
    let work_deadline = Instant::now() + credential_prep_work_deadline();

    // --- Fast path: environment override (never cached) ---
    if let Some(value) = std::env::var_os(provider.environment_variable()) {
        return finish_no_child(
            entry,
            match Credential::new(value.to_string_lossy().into_owned()) {
                Ok(credential) => CredentialTerminalOutcome::Ready(credential),
                Err(_) => CredentialTerminalOutcome::Unavailable(
                    GrammarUnavailableReason::InvalidCredential,
                ),
            },
        );
    }

    // --- Fast path: session cache ---
    if let Some(credential) = credential_cache().get(provider, credential_cache_ttl()) {
        return finish_no_child(entry, CredentialTerminalOutcome::Ready(credential));
    }

    // --- Test seam: VOISU_TEST_SECRET_STORE (no real secret-tool) ---
    if let Some(mode) = secret_seam_mode() {
        let outcome = match mode.as_str() {
            "available" => {
                let name = match provider {
                    Provider::Groq => "VOISU_TEST_STORED_GROQ_CREDENTIAL",
                    Provider::Deepgram => "VOISU_TEST_STORED_DEEPGRAM_CREDENTIAL",
                };
                match std::env::var(name)
                    .ok()
                    .and_then(|value| Credential::new(value).ok())
                {
                    Some(credential) => {
                        credential_cache().put(provider, credential.clone());
                        CredentialTerminalOutcome::Ready(credential)
                    }
                    None => match file_fallback_credential(provider) {
                        Some(credential) => CredentialTerminalOutcome::Ready(credential),
                        None => CredentialTerminalOutcome::Unavailable(
                            GrammarUnavailableReason::NoCredential,
                        ),
                    },
                }
            }
            "denied" | "locked" => match file_fallback_credential(provider) {
                Some(credential) => CredentialTerminalOutcome::Ready(credential),
                None => {
                    CredentialTerminalOutcome::Unavailable(GrammarUnavailableReason::KeyringLocked)
                }
            },
            _ => match file_fallback_credential(provider) {
                Some(credential) => CredentialTerminalOutcome::Ready(credential),
                None => CredentialTerminalOutcome::Unavailable(
                    GrammarUnavailableReason::KeyringUnavailable,
                ),
            },
        };
        return finish_no_child(entry, outcome);
    }

    // --- Cache miss: Tokio secret-tool with bounded retries under work deadline ---
    let mut backoff = lookup_retry_backoff().into_iter();
    loop {
        if entry.is_cancel_requested() {
            return cancel_reap(entry).await;
        }
        if Instant::now() >= work_deadline {
            return deadline_reap(entry).await;
        }

        match run_secret_tool_lookup_attempt(entry, provider, work_deadline).await {
            AsyncLoadStep::Found(credential) => {
                credential_cache().put(provider, credential.clone());
                return finish_no_child(entry, CredentialTerminalOutcome::Ready(credential));
            }
            AsyncLoadStep::Missing => {
                return finish_no_child(
                    entry,
                    match file_fallback_credential(provider) {
                        Some(credential) => CredentialTerminalOutcome::Ready(credential),
                        None => CredentialTerminalOutcome::Unavailable(
                            GrammarUnavailableReason::NoCredential,
                        ),
                    },
                );
            }
            AsyncLoadStep::Stop(reason) => {
                return finish_no_child(
                    entry,
                    match file_fallback_credential(provider) {
                        Some(credential) => CredentialTerminalOutcome::Ready(credential),
                        None => CredentialTerminalOutcome::Unavailable(reason),
                    },
                );
            }
            AsyncLoadStep::Retry(reason) => match backoff.next() {
                Some(delay) => {
                    let remaining = work_deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return deadline_reap(entry).await;
                    }
                    let sleep_for = delay.min(remaining);
                    tokio::select! {
                        _ = tokio::time::sleep(sleep_for) => {}
                        _ = wait_cancel_or_deadline(entry, work_deadline) => {
                            if entry.is_cancel_requested() {
                                return cancel_reap(entry).await;
                            }
                            return deadline_reap(entry).await;
                        }
                    }
                    let _ = reason;
                }
                None => {
                    return finish_no_child(
                        entry,
                        match file_fallback_credential(provider) {
                            Some(credential) => CredentialTerminalOutcome::Ready(credential),
                            None => CredentialTerminalOutcome::Unavailable(reason),
                        },
                    );
                }
            },
            AsyncLoadStep::Cancelled => return cancel_reap(entry).await,
            AsyncLoadStep::Deadline => return deadline_reap(entry).await,
        }
    }
}

fn finish_no_child(
    entry: &CredentialCleanupEntry,
    outcome: CredentialTerminalOutcome,
) -> GrammarCapability {
    let capability = match &outcome {
        CredentialTerminalOutcome::Ready(credential) => {
            GrammarCapability::Ready(ReadyGrammarCapability::new(credential.clone()))
        }
        CredentialTerminalOutcome::Unavailable(reason) => {
            GrammarCapability::Unavailable(reason.clone())
        }
    };
    entry.set_terminal(outcome);
    capability
}

async fn cancel_reap(entry: &CredentialCleanupEntry) -> GrammarCapability {
    entry
        .cancel_requested
        .store(true, std::sync::atomic::Ordering::SeqCst);
    entry.kill_process_group_once();
    // Watchdog is inside reap_running_child_to_terminal — every cleanup path.
    reap_running_child_to_terminal(
        entry,
        CredentialTerminalOutcome::Unavailable(GrammarUnavailableReason::Cancelled),
    )
    .await;
    entry
        .terminal_capability()
        .unwrap_or(GrammarCapability::Unavailable(
            GrammarUnavailableReason::Cancelled,
        ))
}

async fn deadline_reap(entry: &CredentialCleanupEntry) -> GrammarCapability {
    entry
        .cancel_requested
        .store(true, std::sync::atomic::Ordering::SeqCst);
    entry.kill_process_group_once();
    // Watchdog is inside reap_running_child_to_terminal — every cleanup path.
    // Kill is signalled here first so the watchdog covers wait+EOF (not after).
    reap_running_child_to_terminal(
        entry,
        CredentialTerminalOutcome::Unavailable(GrammarUnavailableReason::WorkDeadlineExceeded),
    )
    .await;
    entry
        .terminal_capability()
        .unwrap_or(GrammarCapability::Unavailable(
            GrammarUnavailableReason::WorkDeadlineExceeded,
        ))
}

/// 2 s diagnostic threshold around a terminal reap future. Crossing it logs once
/// and keeps awaiting — never detach before child wait + both pipe EOFs.
async fn with_credential_reap_watchdog<F>(entry: &CredentialCleanupEntry, fut: F)
where
    F: std::future::Future<Output = ()>,
{
    tokio::pin!(fut);
    if tokio::time::timeout(credential_reap_watchdog(), &mut fut)
        .await
        .is_err()
    {
        if entry.mark_watchdog_overrun() {
            let _ = writeln!(
                std::io::stderr(),
                "voisu: credential reap crossed {} ms watchdog; remaining Processing until terminal",
                credential_reap_watchdog().as_millis()
            );
        }
        fut.await;
    }
}

async fn wait_cancel_or_deadline(entry: &CredentialCleanupEntry, work_deadline: Instant) {
    loop {
        if entry.is_cancel_requested() || Instant::now() >= work_deadline {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn file_fallback_credential(provider: Provider) -> Option<Credential> {
    match FileSecretStore::at_default().read(provider) {
        Ok(Some(credential)) => {
            // Best-effort migration/read warnings match the blocking load path.
            warn_fallback(FallbackNotice::ReadWhileKeyringAvailable);
            Some(credential)
        }
        _ => None,
    }
}

enum AsyncLoadStep {
    Found(Credential),
    Missing,
    Retry(GrammarUnavailableReason),
    Stop(GrammarUnavailableReason),
    Cancelled,
    Deadline,
}

/// One restricted Tokio `secret-tool lookup`. Child/pipes live on the entry.
async fn run_secret_tool_lookup_attempt(
    entry: &Arc<CredentialCleanupEntry>,
    provider: Provider,
    work_deadline: Instant,
) -> AsyncLoadStep {
    // Per-attempt process bound (mirrors PROCESS_DEADLINE), clamped by work deadline.
    let attempt_deadline = Instant::now() + PROCESS_DEADLINE;
    let effective_deadline = if attempt_deadline < work_deadline {
        attempt_deadline
    } else {
        work_deadline
    };

    if entry.is_cancel_requested() {
        return AsyncLoadStep::Cancelled;
    }
    if Instant::now() >= work_deadline {
        return AsyncLoadStep::Deadline;
    }

    let mut command = tokio::process::Command::new("secret-tool");
    command
        .args(["lookup", "voisu-provider", provider.secret_service_value()])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(false)
        .env_clear();
    if let Some(path) = std::env::var_os("PATH") {
        command.env("PATH", path);
    }
    for name in [
        "XDG_RUNTIME_DIR",
        "DBUS_SESSION_BUS_ADDRESS",
        "WAYLAND_DISPLAY",
        "XDG_SESSION_TYPE",
        "HYPRLAND_INSTANCE_SIGNATURE",
        "DISPLAY",
        "XAUTHORITY",
    ] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }

    // Own process group + parent-death SIGKILL, matching restricted_command
    // contracts while remaining Tokio-process based for drop-safe reaping.
    // tokio::process::Command exposes inherent Unix pre_exec.
    #[cfg(target_os = "linux")]
    {
        let expected_parent = unsafe { libc::getpid() };
        unsafe {
            command.pre_exec(move || {
                if libc::setpgid(0, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::getppid() != expected_parent {
                    return Err(std::io::Error::from_raw_os_error(libc::ECHILD));
                }
                Ok(())
            });
        }
    }

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(_) => return AsyncLoadStep::Stop(GrammarUnavailableReason::ToolMissing),
    };

    let pid = match child.id() {
        Some(pid) => pid as libc::pid_t,
        None => return AsyncLoadStep::Stop(GrammarUnavailableReason::KeyringUnavailable),
    };
    // Child is process-group leader after setpgid(0,0).
    let pgid = pid;

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    {
        let mut guard = entry.inner.lock().unwrap_or_else(|p| p.into_inner());
        guard.launched_child = true;
        guard.phase = CredentialEntryPhase::Running;
        // Durable pgid survives take-for-reap so Drop/cancel can always SIGKILL.
        guard.last_pgid = Some(pgid);
        // No captured pipe means EOF is already satisfied for that stream.
        let stdout_eof = stdout.is_none();
        let stderr_eof = stderr.is_none();
        guard.running = Some(CredentialRunningChild {
            child,
            pgid,
            stdout,
            stderr,
            stdout_buf: Vec::new(),
            stderr_buf: Vec::new(),
            wait_done: false,
            exit_success: false,
            stdout_eof,
            stderr_eof,
        });
    }

    // Pre-spawn cancel must still kill once the durable pgid is parked.
    if entry.is_cancel_requested() {
        entry.kill_process_group_once();
    }

    // Drive this attempt until the child is fully reaped (wait + EOFs).
    drive_attempt_until_reaped(entry, effective_deadline, work_deadline).await
}

/// Drive one retained attempt: wait + both pipe EOFs on the cooperative path,
/// then classify. Cancel/deadline **kills and returns** without claiming a
/// terminal reap here so the outer `cancel_reap` / `deadline_reap` (watchdog-
/// wrapped) owns wait + EOFs.
async fn drive_attempt_until_reaped(
    entry: &CredentialCleanupEntry,
    attempt_deadline: Instant,
    work_deadline: Instant,
) -> AsyncLoadStep {
    // Cancel/deadline before wait: kill and hand off to outer terminal reap.
    // Do not wait here — that would run kill/wait before the 2 s watchdog starts.
    if entry.is_cancel_requested()
        || Instant::now() >= work_deadline
        || Instant::now() >= attempt_deadline
    {
        entry.kill_process_group_once();
        return if entry.is_cancel_requested() && Instant::now() < work_deadline {
            AsyncLoadStep::Cancelled
        } else {
            AsyncLoadStep::Deadline
        };
    }

    match take_and_reap_running_child(entry, attempt_deadline, work_deadline).await {
        TakeReapResult::NoChild => {
            if entry.is_cancel_requested() && Instant::now() < work_deadline {
                AsyncLoadStep::Cancelled
            } else if Instant::now() >= work_deadline || Instant::now() >= attempt_deadline {
                AsyncLoadStep::Deadline
            } else {
                AsyncLoadStep::Stop(GrammarUnavailableReason::KeyringUnavailable)
            }
        }
        TakeReapResult::NeedsTerminalReap => {
            // Child re-parked; kill already signalled. Outer cancel/deadline reap
            // applies the 2 s watchdog around wait + both EOFs.
            if entry.is_cancel_requested() && Instant::now() < work_deadline {
                AsyncLoadStep::Cancelled
            } else {
                AsyncLoadStep::Deadline
            }
        }
        TakeReapResult::Reaped {
            success,
            stdout,
            stderr,
            was_killed,
        } => {
            // Between attempts the entry is idle-Registered (or CancelRequested).
            {
                let mut guard = entry.inner.lock().unwrap_or_else(|p| p.into_inner());
                if guard.phase == CredentialEntryPhase::Running {
                    guard.phase = CredentialEntryPhase::Registered;
                }
            }

            if was_killed {
                if Instant::now() >= work_deadline {
                    return AsyncLoadStep::Deadline;
                }
                if entry.is_cancel_requested() {
                    return AsyncLoadStep::Cancelled;
                }
                return AsyncLoadStep::Deadline;
            }

            if success {
                match String::from_utf8(stdout) {
                    Ok(value) => match Credential::new(value.trim_end().to_owned()) {
                        Ok(credential) => AsyncLoadStep::Found(credential),
                        Err(_) => AsyncLoadStep::Missing,
                    },
                    Err(_) => AsyncLoadStep::Stop(GrammarUnavailableReason::KeyringLocked),
                }
            } else if stderr.is_empty() {
                AsyncLoadStep::Missing
            } else {
                AsyncLoadStep::Retry(GrammarUnavailableReason::KeyringLocked)
            }
        }
    }
}

/// Outcome of taking and (attempting to) reap a retained credential child.
enum TakeReapResult {
    /// No running child was parked on the entry.
    NoChild,
    /// Cancel/deadline killed; Child re-parked for outer watchdog-wrapped reap.
    /// Must not claim Terminal here.
    NeedsTerminalReap,
    /// Child wait completed and **both** stdout/stderr reached EOF.
    Reaped {
        success: bool,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
        was_killed: bool,
    },
}

/// RAII take of the running child. Pipe handles live as fields of this type
/// until each observes EOF (or hard read error). They are **never** moved into
/// short-lived locals across `.await` points: Drop of a cancelled drive future
/// therefore re-parks Child + remaining live pipes, never silent `None` pipes
/// when EOF was not observed. SIGKILL uses the durable pgid so owner Drop /
/// supervisor drain can finish wait + EOFs.
struct TakenRunningChild<'a> {
    entry: &'a CredentialCleanupEntry,
    child: Option<tokio::process::Child>,
    pgid: libc::pid_t,
    stdout: Option<tokio::process::ChildStdout>,
    stderr: Option<tokio::process::ChildStderr>,
    stdout_buf: Vec<u8>,
    stderr_buf: Vec<u8>,
    wait_done: bool,
    exit_success: bool,
    stdout_eof: bool,
    stderr_eof: bool,
    /// Set after full wait + both pipe EOFs so Drop is a pure no-op.
    fully_reaped: bool,
}

impl<'a> TakenRunningChild<'a> {
    fn take(entry: &'a CredentialCleanupEntry) -> Option<Self> {
        let mut guard = entry.inner.lock().unwrap_or_else(|p| p.into_inner());
        let running = guard.running.take()?;
        // last_pgid stays set until a successful reap clears it.
        Some(Self {
            entry,
            child: Some(running.child),
            pgid: running.pgid,
            stdout: running.stdout,
            stderr: running.stderr,
            stdout_buf: running.stdout_buf,
            stderr_buf: running.stderr_buf,
            wait_done: running.wait_done,
            exit_success: running.exit_success,
            stdout_eof: running.stdout_eof,
            stderr_eof: running.stderr_eof,
            fully_reaped: false,
        })
    }

    fn child_mut(&mut self) -> &mut tokio::process::Child {
        self.child
            .as_mut()
            .expect("TakenRunningChild without child")
    }

    /// Non-blocking opportunistic drain so a full pipe buffer cannot block exit.
    /// Never moves handles out of `self` (safe if the outer future is dropped).
    fn try_drain_pipes_nonblocking(&mut self) {
        use std::task::{Context, Poll};
        use tokio::io::AsyncRead;
        let waker = futures_util::task::noop_waker();
        let mut cx = Context::from_waker(&waker);
        let mut tmp = [0u8; 1024];

        if !self.stdout_eof {
            if let Some(pipe) = self.stdout.as_mut() {
                let mut read_buf = tokio::io::ReadBuf::new(&mut tmp);
                match std::pin::Pin::new(pipe).poll_read(&mut cx, &mut read_buf) {
                    Poll::Ready(Ok(())) => {
                        let n = read_buf.filled().len();
                        if n == 0 {
                            self.stdout = None;
                            self.stdout_eof = true;
                        } else {
                            let room =
                                MAX_RETAINED_STDOUT_BYTES.saturating_sub(self.stdout_buf.len());
                            let filled = read_buf.filled();
                            self.stdout_buf.extend_from_slice(&filled[..n.min(room)]);
                        }
                    }
                    Poll::Ready(Err(_)) => {
                        self.stdout = None;
                        self.stdout_eof = true;
                    }
                    Poll::Pending => {}
                }
            } else {
                self.stdout_eof = true;
            }
        }

        if !self.stderr_eof {
            if let Some(pipe) = self.stderr.as_mut() {
                let mut read_buf = tokio::io::ReadBuf::new(&mut tmp);
                match std::pin::Pin::new(pipe).poll_read(&mut cx, &mut read_buf) {
                    Poll::Ready(Ok(())) => {
                        let n = read_buf.filled().len();
                        if n == 0 {
                            self.stderr = None;
                            self.stderr_eof = true;
                        } else {
                            let room =
                                MAX_RETAINED_STDERR_BYTES.saturating_sub(self.stderr_buf.len());
                            let filled = read_buf.filled();
                            self.stderr_buf.extend_from_slice(&filled[..n.min(room)]);
                        }
                    }
                    Poll::Ready(Err(_)) => {
                        self.stderr = None;
                        self.stderr_eof = true;
                    }
                    Poll::Pending => {}
                }
            } else {
                self.stderr_eof = true;
            }
        }
    }

    /// Record both-EOF observation on the entry and mark fully reaped.
    /// Caller must only invoke after wait + both EOFs.
    fn finish(mut self) -> (bool, Vec<u8>, Vec<u8>) {
        debug_assert!(self.stdout_eof && self.stderr_eof);
        {
            let mut guard = self.entry.inner.lock().unwrap_or_else(|p| p.into_inner());
            guard.stdout_eof_observed = self.stdout_eof;
            guard.stderr_eof_observed = self.stderr_eof;
            // Wait + both EOFs: the OS process is fully owned/reaped. Clear the
            // durable kill target immediately so retry-backoff Drop/cancel cannot
            // SIGKILL a PGID that the kernel may reuse for an unrelated process.
            guard.last_pgid = None;
        }
        self.fully_reaped = true;
        let _ = self.child.take();
        let _ = self.stdout.take();
        let _ = self.stderr.take();
        (
            self.exit_success,
            std::mem::take(&mut self.stdout_buf),
            std::mem::take(&mut self.stderr_buf),
        )
    }

    /// Consume and re-park via Drop: cancel/deadline hands off to the outer
    /// terminal reap (watchdog covers wait + EOFs). Live pipes stay on the
    /// re-parked child until EOF is observed there.
    fn repark_for_terminal_reap(self) {
        // Drop impl re-parks + SIGKILLs.
        drop(self);
    }
}

impl Drop for TakenRunningChild<'_> {
    fn drop(&mut self) {
        if self.fully_reaped {
            return;
        }
        let Some(child) = self.child.take() else {
            return;
        };
        let pgid = self.pgid;

        // Always re-park remaining state (even after wait_done) so pipe EOFs are
        // never abandoned by dropping handles without EOF. Terminal = wait + both
        // EOFs; a waited Child is re-parked with wait_done and remaining pipes.
        // SAFETY: pgid is the retained process-group leader from setpgid(0,0).
        let _ = unsafe { libc::kill(-pgid, libc::SIGKILL) };
        self.entry
            .kill_requested
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.entry
            .cancel_requested
            .store(true, std::sync::atomic::Ordering::SeqCst);

        let running = CredentialRunningChild {
            child,
            pgid,
            stdout: self.stdout.take(),
            stderr: self.stderr.take(),
            stdout_buf: std::mem::take(&mut self.stdout_buf),
            stderr_buf: std::mem::take(&mut self.stderr_buf),
            wait_done: self.wait_done,
            exit_success: self.exit_success,
            stdout_eof: self.stdout_eof,
            stderr_eof: self.stderr_eof,
        };

        let mut guard = self.entry.inner.lock().unwrap_or_else(|p| p.into_inner());
        if matches!(
            guard.phase,
            CredentialEntryPhase::Terminal | CredentialEntryPhase::Deregistered
        ) {
            // Already terminal — discard handle; process was SIGKILLed above.
            return;
        }
        if guard.running.is_none() {
            guard.last_pgid = Some(pgid);
            if matches!(
                guard.phase,
                CredentialEntryPhase::Registered | CredentialEntryPhase::Running
            ) {
                guard.phase = CredentialEntryPhase::CancelRequested;
            }
            // Preserve wait_done / exit_success / partial pipe buffers + live pipes.
            guard.running = Some(running);
        } else {
            // Another path re-parked; keep durable pgid, drop our duplicate handle.
            guard.last_pgid = Some(pgid);
        }
    }
}

/// Future that owns [`TakenRunningChild`] and drains both pipes to EOF without
/// ever moving pipe handles into short-lived locals. Drop of this future drops
/// the RAII type, which re-parks any pipes that have not yet observed EOF.
///
/// A 50 ms stall timer is polled alongside the pipes so non-cooperative
/// write-end holders (grandchildren that inherited stdout/stderr) are
/// process-group SIGKILLed even when the reactor would otherwise never re-wake
/// a Pending `poll_read`.
struct DrainBothPipes<'a> {
    taken: Option<TakenRunningChild<'a>>,
    tmp: [u8; 4096],
    stall: std::pin::Pin<Box<tokio::time::Sleep>>,
}

impl<'a> DrainBothPipes<'a> {
    fn new(taken: TakenRunningChild<'a>) -> Self {
        Self {
            taken: Some(taken),
            tmp: [0u8; 4096],
            stall: Box::pin(tokio::time::sleep(Duration::from_millis(50))),
        }
    }
}

impl<'a> std::future::Future for DrainBothPipes<'a> {
    type Output = TakenRunningChild<'a>;

    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        use std::task::Poll;
        use tokio::io::AsyncRead;
        let this = self.get_mut();
        let taken = this
            .taken
            .as_mut()
            .expect("DrainBothPipes polled after completion");
        let pgid = taken.pgid;

        loop {
            let mut pending = false;

            if !taken.stdout_eof {
                match taken.stdout.as_mut() {
                    None => taken.stdout_eof = true,
                    Some(pipe) => {
                        let mut read_buf = tokio::io::ReadBuf::new(&mut this.tmp);
                        match std::pin::Pin::new(pipe).poll_read(cx, &mut read_buf) {
                            Poll::Pending => pending = true,
                            Poll::Ready(Ok(())) => {
                                let n = read_buf.filled().len();
                                if n == 0 {
                                    taken.stdout = None;
                                    taken.stdout_eof = true;
                                } else {
                                    let room = MAX_RETAINED_STDOUT_BYTES
                                        .saturating_sub(taken.stdout_buf.len());
                                    let filled = read_buf.filled();
                                    taken.stdout_buf.extend_from_slice(&filled[..n.min(room)]);
                                }
                            }
                            Poll::Ready(Err(_)) => {
                                taken.stdout = None;
                                taken.stdout_eof = true;
                            }
                        }
                    }
                }
            }

            if !taken.stderr_eof {
                match taken.stderr.as_mut() {
                    None => taken.stderr_eof = true,
                    Some(pipe) => {
                        let mut read_buf = tokio::io::ReadBuf::new(&mut this.tmp);
                        match std::pin::Pin::new(pipe).poll_read(cx, &mut read_buf) {
                            Poll::Pending => pending = true,
                            Poll::Ready(Ok(())) => {
                                let n = read_buf.filled().len();
                                if n == 0 {
                                    taken.stderr = None;
                                    taken.stderr_eof = true;
                                } else {
                                    let room = MAX_RETAINED_STDERR_BYTES
                                        .saturating_sub(taken.stderr_buf.len());
                                    let filled = read_buf.filled();
                                    taken.stderr_buf.extend_from_slice(&filled[..n.min(room)]);
                                }
                            }
                            Poll::Ready(Err(_)) => {
                                taken.stderr = None;
                                taken.stderr_eof = true;
                            }
                        }
                    }
                }
            }

            if taken.stdout_eof && taken.stderr_eof {
                return Poll::Ready(
                    this.taken
                        .take()
                        .expect("DrainBothPipes taken missing at ready"),
                );
            }

            if pending {
                // Arm / poll stall timer so we re-wake even if the pipe fd
                // stays non-readable while a grandchild holds the write end.
                if this.stall.as_mut().poll(cx).is_ready() {
                    // SAFETY: process-group SIGKILL for the retained pgid.
                    let _ = unsafe { libc::kill(-pgid, libc::SIGKILL) };
                    this.stall
                        .as_mut()
                        .reset(tokio::time::Instant::now() + Duration::from_millis(50));
                    // Continue loop: pipes may now be readable after kill.
                    continue;
                }
                return Poll::Pending;
            }
            // Progress without Pending — keep reading until EOF or block.
        }
    }
}

/// Take the running child off the entry, wait + drain both pipes to EOF, or
/// hand off to outer terminal reap on cancel/deadline.
///
/// Cancel of this future re-parks the Child + remaining pipes (see
/// [`TakenRunningChild`]) so wait/EOF ownership is never lost.
async fn take_and_reap_running_child(
    entry: &CredentialCleanupEntry,
    attempt_deadline: Instant,
    work_deadline: Instant,
) -> TakeReapResult {
    // Cleanup already required: do not wait here without the outer watchdog.
    if entry.is_cancel_requested()
        || Instant::now() >= work_deadline
        || Instant::now() >= attempt_deadline
    {
        entry.kill_process_group_once();
        let has_child = {
            let guard = entry.inner.lock().unwrap_or_else(|p| p.into_inner());
            guard.running.is_some() || guard.last_pgid.is_some()
        };
        return if has_child {
            TakeReapResult::NeedsTerminalReap
        } else {
            TakeReapResult::NoChild
        };
    }

    let mut taken = match TakenRunningChild::take(entry) {
        Some(t) => t,
        None => return TakeReapResult::NoChild,
    };
    let pgid = taken.pgid;

    // Resume post-wait pipe drain if a prior driver observed exit then dropped.
    let already_waited = taken.wait_done;
    // True if kill was already sticky when we took the child (or becomes true
    // from cancel/deadline before we observe exit). Post-wait process-group
    // re-signals for non-cooperative pipe holders alone must not reclassify a
    // natural cooperative exit as Cancelled/Deadline.
    let mut killed_for_cleanup = entry
        .kill_requested
        .load(std::sync::atomic::Ordering::SeqCst);
    if killed_for_cleanup {
        // SAFETY: process-group SIGKILL for the retained pgid.
        let _ = unsafe { libc::kill(-pgid, libc::SIGKILL) };
    }

    if !already_waited {
        // Wait for exit via try_wait + sleep so Child is never borrowed across
        // an .await (pipes stay on `taken` for Drop re-park). On cancel/deadline:
        // kill, re-park, hand off to outer watchdog-wrapped terminal reap.
        let status = loop {
            if entry.is_cancel_requested()
                || Instant::now() >= work_deadline
                || Instant::now() >= attempt_deadline
            {
                if !killed_for_cleanup {
                    let _ = unsafe { libc::kill(-pgid, libc::SIGKILL) };
                    entry
                        .kill_requested
                        .store(true, std::sync::atomic::Ordering::SeqCst);
                }
                // Pipes/buffers remain on `taken`; Drop re-parks them intact.
                taken.repark_for_terminal_reap();
                return TakeReapResult::NeedsTerminalReap;
            }

            match taken.child_mut().try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {
                    // Still live — opportunistic non-blocking drain so a full
                    // buffer cannot block exit. Strict post-wait drain follows.
                    taken.try_drain_pipes_nonblocking();
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                Err(_) => break std::process::ExitStatus::from_raw(256),
            }
        };
        taken.exit_success = status.success();
        taken.wait_done = true;

        // External cancel/kill can race exit: SIGKILL makes wait return before
        // this loop's cancel check. Re-sample flags so a killed exit is never
        // classified as Missing/NoCredential.
        if entry.is_cancel_requested()
            || entry
                .kill_requested
                .load(std::sync::atomic::Ordering::SeqCst)
            || Instant::now() >= work_deadline
            || Instant::now() >= attempt_deadline
        {
            killed_for_cleanup = true;
        }
    }

    // Spec Terminal = child wait + **both** pipe EOFs. Drain owns `taken` so a
    // cancelled future re-parks live pipes rather than dropping silent Nones.
    let taken = DrainBothPipes::new(taken).await;

    let (success, stdout, stderr) = taken.finish();

    TakeReapResult::Reaped {
        success,
        stdout,
        stderr,
        was_killed: killed_for_cleanup,
    }
}

/// After kill (or no child): await wait + both EOFs under the 2 s reap
/// watchdog diagnostic, then set terminal outcome. Watchdog is not permission
/// to detach: on overrun log once and keep awaiting terminal cleanup.
async fn reap_running_child_to_terminal(
    entry: &CredentialCleanupEntry,
    outcome_if_empty: CredentialTerminalOutcome,
) {
    // Kill first so the watchdog covers the real wait+EOF work, not only a
    // no-child set_terminal after an earlier unreaped path.
    entry.kill_process_group_once();

    with_credential_reap_watchdog(entry, async {
        {
            let stall = {
                let mut guard = entry.inner.lock().unwrap_or_else(|p| p.into_inner());
                if !guard.reap_stall_applied {
                    if let Some(stall) = credential_reap_stall() {
                        guard.reap_stall_applied = true;
                        Some(stall)
                    } else {
                        None
                    }
                } else {
                    None
                }
            };
            if let Some(stall) = stall {
                tokio::time::sleep(stall).await;
            }
        }

        // Dedicated cleanup reap always completes wait + both EOFs (never
        // NeedsTerminalReap hand-off — this *is* the terminal path).
        let _ = take_and_reap_for_cleanup(entry).await;

        if entry.terminal_capability().is_none() {
            entry.set_terminal(outcome_if_empty);
        }
    })
    .await;
}

/// Terminal cleanup reap: always completes wait + both EOFs (or NoChild).
/// Unlike attempt reap, cancel/deadline never returns `NeedsTerminalReap` —
/// this **is** the terminal path. Kill stays sticky from the entry.
async fn take_and_reap_for_cleanup(entry: &CredentialCleanupEntry) -> TakeReapResult {
    let mut taken = match TakenRunningChild::take(entry) {
        Some(t) => t,
        None => return TakeReapResult::NoChild,
    };
    let pgid = taken.pgid;
    // SAFETY: always re-signal; cleanup is kill+wait+EOF.
    let _ = unsafe { libc::kill(-pgid, libc::SIGKILL) };
    entry
        .kill_requested
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let was_killed = true;

    if !taken.wait_done {
        let status = loop {
            match taken.child_mut().try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {
                    // Re-signal while waiting; non-cooperative children die.
                    let _ = unsafe { libc::kill(-pgid, libc::SIGKILL) };
                    taken.try_drain_pipes_nonblocking();
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                Err(_) => break std::process::ExitStatus::from_raw(256),
            }
        };
        taken.exit_success = status.success();
        taken.wait_done = true;
    }

    let taken = DrainBothPipes::new(taken).await;

    let (success, stdout, stderr) = taken.finish();
    TakeReapResult::Reaped {
        success,
        stdout,
        stderr,
        was_killed,
    }
}

impl ProviderReaper {
    pub fn new() -> Self {
        Self::default()
    }

    /// Dedicated Architecture A credential-preparation cleanup lane (SW7).
    pub fn credential_lane(&self) -> &CredentialLane {
        &self.credential_lane
    }

    /// Supervisor/shutdown drain of any retained credential entries, then the
    /// existing provider/capture cleanup futures. SW10 wires this into
    /// `supervise_recording` and daemon teardown; abnormal owner Drop leaves
    /// entries here until this path claims them.
    pub async fn drain_credential_lane(&self) {
        self.credential_lane.drain_all().await;
    }

    /// Adopts the still-live chunk tasks of a dropped stream. Cancellation MUST
    /// already be signalled so each task's owning bounded wait observes the
    /// flag, kills and reaps its curl child, and returns. Synchronous and
    /// runtime-free: the handles are wrapped in a future and retained; only a
    /// later `drain` polls them, so adopting from a non-runtime thread or a
    /// shutting-down runtime can never detach or abort the cleanup.
    /// Retains one cleanup future. Called from `Drop` (capture and
    /// provider-stream adoption), so it must never unwind: a poisoned lock is
    /// recovered rather than `expect`ed. The lock is only ever held for a
    /// push/take/len, so its guarded state stays consistent under recovery.
    fn retain(&self, task: ReapTask) {
        let mut guard = match self.tasks.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.push(task);
    }

    pub(super) fn adopt<T: Send + 'static>(
        &self,
        mut chunks: VecDeque<tokio::task::JoinHandle<T>>,
    ) {
        if chunks.is_empty() {
            return;
        }
        self.retain(Box::pin(async move {
            while let Some(chunk) = chunks.pop_front() {
                let _ = chunk.await;
            }
        }));
    }

    pub(super) fn adopt_capture(
        &self,
        cleanup: tokio::task::JoinHandle<Result<Vec<u8>, BoundaryError>>,
    ) {
        self.adopt(VecDeque::from([cleanup]));
    }

    /// Adopts a pre-stop capture whose `stop_child` never ran: the raw pw-record
    /// child and reader threads are still live. A dedicated OS thread performs
    /// `stop_child_blocking`'s bounded kill/reap/join off any async worker, and
    /// the retained future awaits its completion signal, so a drain blocks the
    /// Idle transition until the child and both reader threads are actually gone
    /// — not merely `reap_briefly`'s 250 ms. Runtime-free and non-panicking: no
    /// `spawn_blocking` and no `Handle::try_current`, so this still lands its
    /// cleanup when `Drop` runs on a non-runtime teardown thread. The thread's
    /// own bounds (`wait_for_child` and the two `bounded_join`s under
    /// `PROCESS_DEADLINE`) guarantee it signals, so the drain terminates.
    pub(super) fn adopt_capture_blocking(
        &self,
        child: Child,
        reader: Option<thread::JoinHandle<()>>,
        stderr_reader: Option<thread::JoinHandle<Vec<u8>>>,
    ) {
        let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();
        thread::spawn(move || {
            // Abandoned Recording: SIGKILL (graceful = false), mirroring the
            // prior `child.kill()`. The classification result is irrelevant to a
            // dropped capture and is discarded; only the reap matters.
            let _ = stop_child_blocking(child, reader, stderr_reader, false);
            let _ = done_tx.send(());
        });
        self.retain(Box::pin(async move {
            let _ = done_rx.await;
        }));
    }

    /// Number of cleanup futures currently retained and not being drained.
    /// Test-observability only — production callers gate on `drain` /
    /// `drain_to_completion`, never on this count, because a cleanup being
    /// awaited by an in-flight `drain` is not counted.
    #[cfg(test)]
    pub(super) fn pending(&self) -> usize {
        self.tasks
            .lock()
            .expect("provider reaper mutex poisoned")
            .len()
    }

    /// Awaits every retained cleanup future, bounded by `within`. Returns
    /// `true` when the supervisor fully drained, re-checking for cleanup adopted
    /// while draining. On timeout it puts every unfinished future back — so
    /// cleanup is retained, never detached — and returns `false`. Serialized
    /// with every other drain.
    /// Drains to completion in bounded passes, returning only once the
    /// supervisor is empty. A single bounded `drain` that times out RETAINS the
    /// unfinished cleanup — but a caller about to tear down the runtime would
    /// then drop the supervisor and detach that cleanup after all, so teardown
    /// paths must use this instead and keep draining. Each retained cleanup is
    /// internally bounded (capture and provider waits kill and reap their child
    /// within their own poll bounds), so this terminates; the service unit's
    /// explicit TimeoutStopSec is the external last-resort backstop.
    pub async fn drain_to_completion(&self, pass: Duration) {
        while !self.drain(pass).await {
            // Guaranteed-completion callers gate the Idle transition on this
            // drain; a failed stderr write must not panic it (`eprintln!` does).
            let _ = writeln!(std::io::stderr(), "provider cleanup still draining");
        }
    }

    pub async fn drain(&self, within: Duration) -> bool {
        let _serial = self.serial.lock().await;
        let deadline = tokio::time::Instant::now() + within;
        loop {
            let mut batch: Vec<ReapTask> = {
                let mut guard = self.tasks.lock().expect("provider reaper mutex poisoned");
                std::mem::take(&mut *guard)
            };
            if batch.is_empty() {
                return true;
            }
            while let Some(mut task) = batch.pop() {
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                if tokio::time::timeout(remaining, &mut task).await.is_err() {
                    let mut guard = self.tasks.lock().expect("provider reaper mutex poisoned");
                    guard.push(task);
                    guard.append(&mut batch);
                    return false;
                }
            }
        }
    }
}
