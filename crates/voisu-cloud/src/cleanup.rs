// Cleanup supervision and the host seam the cloud providers run against.
//
// The retained-cleanup machinery moved here from voisu-app's `ProviderReaper`
// unchanged; the app's reaper now wraps a `CleanupSupervisor` and adds its
// Linux-only capture and credential-lane concerns on top.

use std::io::Write;

use super::*;

/// A retained provider-stream cleanup: a future that awaits an adopted chunk
/// task deque until every chunk task — and therefore every nested
/// `spawn_blocking` curl reap those tasks own — has completed.
pub type ReapTask = std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>;

/// Retains cleanup futures from dropped provider streams so their curl reaps
/// and websocket teardowns are awaited by an owner-driven `drain`, never
/// detached.
#[derive(Clone, Default)]
pub struct CleanupSupervisor {
    tasks: Arc<std::sync::Mutex<Vec<ReapTask>>>,
    /// Serializes `drain` calls. While one drain temporarily holds cleanup
    /// futures out of `tasks`, a concurrent drain must wait here instead of
    /// reading `tasks` as empty and reporting a completed drain over live work.
    serial: Arc<tokio::sync::Mutex<()>>,
}

impl CleanupSupervisor {
    pub fn new() -> Self {
        Self::default()
    }

    /// Retains one cleanup future. Called from `Drop` (capture and
    /// provider-stream adoption), so it must never unwind: a poisoned lock is
    /// recovered rather than `expect`ed. The lock is only ever held for a
    /// push/take/len, so its guarded state stays consistent under recovery.
    pub fn retain(&self, task: ReapTask) {
        let mut guard = match self.tasks.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.push(task);
    }

    /// Adopts the still-live chunk tasks of a dropped stream. Cancellation MUST
    /// already be signalled so each task's owning bounded wait observes the
    /// flag, kills and reaps its curl child, and returns. Synchronous and
    /// runtime-free: the handles are wrapped in a future and retained; only a
    /// later `drain` polls them, so adopting from a non-runtime thread or a
    /// shutting-down runtime can never detach or abort the cleanup.
    pub fn adopt<T: Send + 'static>(&self, mut chunks: VecDeque<tokio::task::JoinHandle<T>>) {
        if chunks.is_empty() {
            return;
        }
        self.retain(Box::pin(async move {
            while let Some(chunk) = chunks.pop_front() {
                let _ = chunk.await;
            }
        }));
    }

    /// Number of cleanup futures currently retained and not being drained.
    /// Test-observability only — production callers gate on `drain` /
    /// `drain_to_completion`, never on this count, because a cleanup being
    /// awaited by an in-flight `drain` is not counted.
    #[doc(hidden)]
    pub fn pending(&self) -> usize {
        self.tasks
            .lock()
            .expect("provider reaper mutex poisoned")
            .len()
    }

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

    /// Awaits every retained cleanup future, bounded by `within`. Returns
    /// `true` when the supervisor fully drained, re-checking for cleanup adopted
    /// while draining. On timeout it puts every unfinished future back — so
    /// cleanup is retained, never detached — and returns `false`. Serialized
    /// with every other drain.
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

/// What the cloud pipeline needs from the platform it runs on: a cleanup
/// supervisor the owner drains before Idle, a credential store, and the
/// owner-supervised credential preparation Intent Reconstruction awaits.
/// voisu-app implements it over Secret Service and its credential lane; a
/// Windows host supplies its own.
pub trait CloudHost: Send + Sync + 'static {
    /// The supervisor that adopts dropped streams' cleanup.
    fn cleanup(&self) -> &CleanupSupervisor;

    /// Loads `provider`'s credential. Synchronous and possibly slow. Callers
    /// invoke it from a blocking task or directly on a runtime worker at stream
    /// start, so it must never block on a tokio runtime (`block_on`).
    fn load_credential(&self, provider: Provider) -> Result<Credential, BoundaryError>;

    /// Prepares the Groq credential for one Intent Reconstruction attempt. It
    /// must observe `cancel` and complete only after any preparation work it
    /// started has been cleaned up, returning a `Validation` error on
    /// cancellation or when no credential is available.
    fn prepare_intent_credential(
        &self,
        cancel: Arc<CancelRegistry>,
    ) -> BoundaryFuture<'static, Credential>;
}
