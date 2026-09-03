// Credential secret store: secret-tool / keyring with plaintext fallback and warnings.
//
// Split out of system.rs as a pure move; module-global items come from `super`.

use super::*;

pub struct SecretToolStore;

/// Why the desktop Secret Service could not serve a request. It selects the
/// fallback warning wording — ticket 06 established that an unowned/activatable
/// name is a distinct failure from an owned-but-locked collection, and a missing
/// helper binary is distinct from both.
#[derive(Clone, Copy)]
pub(super) enum FallbackReason {
    /// No Secret Service owns `org.freedesktop.secrets`, or it could not start.
    Unavailable,
    /// The service answered but is locked or refused access.
    Locked,
    /// The `secret-tool` helper binary is not installed.
    ToolMissing,
}

impl FallbackReason {
    fn detail(self) -> &'static str {
        match self {
            Self::Unavailable => {
                "no desktop Secret Service is available on this session (no keyring is running or activatable)"
            }
            Self::Locked => "the desktop keyring is locked or refused access",
            Self::ToolMissing => "the secret-tool helper is not installed",
        }
    }

    fn remedy(self) -> &'static str {
        match self {
            Self::Unavailable => {
                "start a Secret Service (KWallet or GNOME Keyring) then re-run `voisu setup` to migrate"
            }
            Self::Locked => {
                "unlock your keyring (e.g. in KWallet) then re-run `voisu setup` to migrate"
            }
            Self::ToolMissing => {
                "install secret-tool (libsecret-tools) then re-run `voisu setup` to migrate"
            }
        }
    }

    /// The reason-specific error surfaced when no credential can be produced —
    /// its public message steers the user at the real fix, not a generic hint.
    fn load_error(self) -> BoundaryError {
        BoundaryError::new(BoundaryKind::SecretStorage, "keyring load failed").with_public_message(
            match self {
                Self::Unavailable => {
                    "no desktop Secret Service is available; run `voisu setup` to store a key"
                }
                Self::Locked => "the desktop keyring is locked; unlock it, or run `voisu setup`",
                Self::ToolMissing => "the secret-tool helper is not installed",
            },
        )
    }
}

/// The situation that triggered a plaintext-fallback warning. Storing to the
/// file and reading from it are different acts with different wording, and
/// reading plaintext while the keyring is actually available is a migration
/// nudge, not a keyring failure.
pub(super) enum FallbackNotice {
    Store(FallbackReason),
    Read(FallbackReason),
    ReadWhileKeyringAvailable,
}

/// The default desktop keyring retry budget: one immediate attempt then short
/// backoffs, ≈4.25s total, absorbing an edge-case slow activation without ever
/// blocking daemon startup (the load is lazy, at first use — ticket 06). Only an
/// `Unavailable` (activating) result is retried; a `Locked` collection is not,
/// since retries cannot unlock it.
const KEYRING_RETRY_BACKOFF: [Duration; 3] = [
    Duration::from_millis(250),
    Duration::from_secs(1),
    Duration::from_secs(3),
];

/// The retry backoff, overridable to a flat per-step delay (0 = instant) via
/// `VOISU_TEST_KEYRING_RETRY_MS` so a test can exercise the budget without real
/// waits.
fn keyring_retry_backoff() -> Vec<Duration> {
    if let Some(ms) = std::env::var("VOISU_TEST_KEYRING_RETRY_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
    {
        return vec![Duration::from_millis(ms); KEYRING_RETRY_BACKOFF.len()];
    }
    KEYRING_RETRY_BACKOFF.to_vec()
}

/// The per-Recording lookup retry budget: two short backoffs, ≈0.35s total. It is
/// deliberately far smaller than the store budget because the lookup runs on the
/// dictation hot path — a healthy lookup succeeds on the first attempt and pays
/// nothing, a transient Secret-Service denial recovers after the first backoff,
/// and a persistent denial surfaces the loud failure sub-second rather than
/// hanging the activation.
const LOOKUP_RETRY_BACKOFF: [Duration; 2] =
    [Duration::from_millis(100), Duration::from_millis(250)];

/// The lookup retry backoff, overridable to a flat per-step delay (0 = instant)
/// via the same `VOISU_TEST_KEYRING_RETRY_MS` seam the store path uses.
pub(super) fn lookup_retry_backoff() -> Vec<Duration> {
    if let Some(ms) = std::env::var("VOISU_TEST_KEYRING_RETRY_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
    {
        return vec![Duration::from_millis(ms); LOOKUP_RETRY_BACKOFF.len()];
    }
    LOOKUP_RETRY_BACKOFF.to_vec()
}

/// One attempt at the desktop Secret Service store.
enum StoreStep {
    Stored,
    Retry(FallbackReason),
    Stop(FallbackReason),
}

/// One attempt at the desktop Secret Service lookup. A transient D-Bus/ksecretd
/// denial (a nonzero exit WITH a stderr diagnostic) is `Retry`: it is
/// indistinguishable by output from a genuinely locked collection, so a small
/// bounded retry lets a momentary hiccup recover within the single load while a
/// persistent denial still exhausts the budget and surfaces the loud failure.
/// A clean no-match (nonzero exit, EMPTY stderr) is the OPPOSITE of the store
/// path: it is definitive absence, never retried, so the common unconfigured-key
/// and file-fallback reads stay on the fast path.
enum LoadStep {
    Found(Credential),
    /// The service is reachable but holds no such credential — definitive, so
    /// the caller consults the fallback file rather than retrying.
    Missing,
    /// A transient denial that a short bounded retry may clear; the reason is the
    /// terminal classification used once the budget is exhausted.
    Retry(FallbackReason),
    Stop(FallbackReason),
}

impl SecretStore for SecretToolStore {
    fn replace(&mut self, provider: Provider, credential: Credential) -> Result<(), BoundaryError> {
        match store_primary(provider, &credential) {
            Ok(()) => {
                // The keyring now holds the key, so migrate it out of the
                // plaintext fallback: drop that provider's line (deleting the
                // file when empty) so a later locked-at-boot window can never
                // silently serve a stale plaintext key. A failed prune must
                // not report a completed migration — so it is loud and an
                // error — with wording taken straight from `remove`'s own
                // classification (the single place the file is read, so this
                // relay can never disagree with it): "the copy survived" only
                // when the provider's line was verified on disk, "could not
                // verify" when its presence is unknowable.
                match FileSecretStore::at_default().remove(provider) {
                    Ok(_) => Ok(()),
                    Err(RemoveError::TargetPresent(_)) => {
                        warn_plaintext_prune_failed(PlaintextPruneFailure::CopySurvived);
                        Err(BoundaryError::new(
                            BoundaryKind::SecretStorage,
                            "plaintext prune failed after a successful keyring store",
                        )
                        .with_public_message(
                            "the key was stored in your keyring, but the old plaintext \
                             copy could not be removed and would still be used if the \
                             keyring is locked — delete the credentials file next to \
                             config.toml, then re-run `voisu doctor`",
                        ))
                    }
                    Err(RemoveError::Unverifiable(_)) => {
                        warn_plaintext_prune_failed(PlaintextPruneFailure::Unverifiable);
                        Err(BoundaryError::new(
                            BoundaryKind::SecretStorage,
                            "plaintext prune unverifiable after a successful keyring store",
                        )
                        .with_public_message(
                            "the key was stored in your keyring, but Voisu could not \
                             verify whether an old plaintext copy remains — check for a \
                             credentials file next to config.toml, then re-run \
                             `voisu doctor`",
                        ))
                    }
                }
            }
            Err(reason) => {
                warn_fallback(FallbackNotice::Store(reason));
                FileSecretStore::at_default().store(provider, &credential)
            }
        }
    }

    fn load(&mut self, provider: Provider) -> Result<Credential, BoundaryError> {
        // The env override wins over any stored key AND over the cache, preserving
        // the historic development/headless path; it is cheap to read so it is
        // never cached.
        if let Some(credential) = std::env::var_os(provider.environment_variable()) {
            return Credential::new(credential.to_string_lossy().into_owned());
        }
        // Serve a still-fresh credential from the session cache, so a later
        // transient Secret-Service denial never re-reaches secret-tool. Only a
        // successful load is cached; failures fall through and surface loudly.
        resolve_with_cache(provider, credential_cache(), credential_cache_ttl(), || {
            let fallback = FileSecretStore::at_default();
            match load_primary(provider) {
                LoadPrimary::Found(credential) => Ok(credential),
                // Keyring reachable but no key: a prior headless fallback write may
                // still hold it — reading that plaintext while the keyring is
                // available is a migration nudge, not a keyring failure.
                LoadPrimary::Missing => match fallback.read(provider)? {
                    Some(credential) => {
                        warn_fallback(FallbackNotice::ReadWhileKeyringAvailable);
                        Ok(credential)
                    }
                    None => Err(BoundaryError::new(
                        BoundaryKind::SecretStorage,
                        "no stored credential for provider",
                    )),
                },
                // Keyring unreachable: only warn about reading the file when we
                // actually read one; otherwise surface the keyring's real problem.
                LoadPrimary::Failed(reason) => match fallback.read(provider)? {
                    Some(credential) => {
                        warn_fallback(FallbackNotice::Read(reason));
                        Ok(credential)
                    }
                    None => Err(reason.load_error()),
                },
            }
        })
    }

    fn diagnose(&mut self, provider: Provider) -> KeyDiagnosis {
        // Mirror `load`: any PRESENT env variable is authoritative at runtime,
        // so a present-but-malformed value (empty, stray newline) must be
        // diagnosed as the broken override it is — never silently skipped in
        // favour of the keyring/file key it shadows.
        if let Some(value) = std::env::var_os(provider.environment_variable()) {
            return match Credential::new(value.to_string_lossy().into_owned()) {
                Ok(credential) => KeyDiagnosis::Found {
                    location: KeyLocation::EnvOverride,
                    credential,
                },
                Err(_) => KeyDiagnosis::EnvOverrideInvalid,
            };
        }
        let fallback = FileSecretStore::at_default();
        match load_primary(provider) {
            LoadPrimary::Found(credential) => KeyDiagnosis::Found {
                location: KeyLocation::Keyring,
                credential,
            },
            LoadPrimary::Missing => match fallback.read(provider) {
                Ok(Some(credential)) => KeyDiagnosis::Found {
                    location: KeyLocation::PlaintextFile,
                    credential,
                },
                _ => KeyDiagnosis::Absent,
            },
            LoadPrimary::Failed(reason) => match fallback.read(provider) {
                Ok(Some(credential)) => KeyDiagnosis::Found {
                    location: KeyLocation::PlaintextFile,
                    credential,
                },
                _ => match reason {
                    FallbackReason::Locked => KeyDiagnosis::Locked,
                    FallbackReason::ToolMissing => KeyDiagnosis::ToolMissing,
                    FallbackReason::Unavailable => KeyDiagnosis::Unavailable,
                },
            },
        }
    }
}

/// The outcome of the primary (desktop Secret Service) load after its retry
/// budget.
enum LoadPrimary {
    Found(Credential),
    Missing,
    Failed(FallbackReason),
}

/// The controlled seam value, if the test harness set one.
pub(super) fn secret_seam_mode() -> Option<String> {
    std::env::var_os("VOISU_TEST_SECRET_STORE").map(|value| value.to_string_lossy().into_owned())
}

/// Stores to the primary with a bounded retry, honoring the test seam. `Ok`
/// means stored; `Err(reason)` means the caller should fall back to the file.
fn store_primary(provider: Provider, credential: &Credential) -> Result<(), FallbackReason> {
    if let Some(mode) = secret_seam_mode() {
        return match mode.as_str() {
            "available" => Ok(()),
            "denied" | "locked" => Err(FallbackReason::Locked),
            _ => Err(FallbackReason::Unavailable),
        };
    }
    let mut backoff = keyring_retry_backoff().into_iter();
    loop {
        match secret_tool_store(provider, credential) {
            StoreStep::Stored => return Ok(()),
            StoreStep::Stop(reason) => return Err(reason),
            StoreStep::Retry(reason) => match backoff.next() {
                Some(delay) => std::thread::sleep(delay),
                None => return Err(reason),
            },
        }
    }
}

/// Loads from the primary with a bounded retry, honoring the test seam.
fn load_primary(provider: Provider) -> LoadPrimary {
    if let Some(mode) = secret_seam_mode() {
        if mode == "available" {
            let name = match provider {
                Provider::Groq => "VOISU_TEST_STORED_GROQ_CREDENTIAL",
                Provider::Deepgram => "VOISU_TEST_STORED_DEEPGRAM_CREDENTIAL",
            };
            return match std::env::var(name)
                .ok()
                .and_then(|value| Credential::new(value).ok())
            {
                Some(credential) => LoadPrimary::Found(credential),
                None => LoadPrimary::Missing,
            };
        }
        return match mode.as_str() {
            "denied" | "locked" => LoadPrimary::Failed(FallbackReason::Locked),
            _ => LoadPrimary::Failed(FallbackReason::Unavailable),
        };
    }
    let mut backoff = lookup_retry_backoff().into_iter();
    loop {
        match secret_tool_load(provider) {
            LoadStep::Found(credential) => return LoadPrimary::Found(credential),
            LoadStep::Missing => return LoadPrimary::Missing,
            LoadStep::Stop(reason) => return LoadPrimary::Failed(reason),
            // A transient denial: retry within the small budget, then fall back to
            // the terminal classification once it is exhausted.
            LoadStep::Retry(reason) => match backoff.next() {
                Some(delay) => std::thread::sleep(delay),
                None => return LoadPrimary::Failed(reason),
            },
        }
    }
}

/// One real `secret-tool store`. An empty stderr on failure reads as the service
/// still activating (retryable); a diagnostic on stderr, a timed-out prompt, or
/// invalid data read as a locked/denied collection (not retryable).
fn secret_tool_store(provider: Provider, credential: &Credential) -> StoreStep {
    match run_restricted(
        "secret-tool",
        &[
            "store",
            "--label=Voisu cloud credential",
            "voisu-provider",
            provider.secret_service_value(),
        ],
        Some(credential.expose_to_boundary().as_bytes()),
        false,
    ) {
        Ok(outcome) if outcome.success => StoreStep::Stored,
        // A nonzero exit with no diagnostic reads as the service still
        // activating — the one edge worth a bounded retry (ticket 06).
        Ok(outcome) if outcome.stderr.is_empty() => StoreStep::Retry(FallbackReason::Unavailable),
        Ok(_) => StoreStep::Stop(FallbackReason::Locked),
        Err(ProcessError::Unavailable) => StoreStep::Stop(FallbackReason::ToolMissing),
        Err(ProcessError::TimedOut) => StoreStep::Stop(FallbackReason::Locked),
        // A crashed or otherwise anomalous child is not retried — retrying only
        // reproduces the crash and would blow the bounded budget.
        Err(_) => StoreStep::Stop(FallbackReason::Unavailable),
    }
}

/// One real `secret-tool lookup`. A clean no-match (nonzero exit, empty stderr)
/// is `Missing`; a stderr diagnostic or a timed-out prompt is a locked/denied
/// collection; a spawn failure is the service being unavailable.
fn secret_tool_load(provider: Provider) -> LoadStep {
    match run_restricted(
        "secret-tool",
        &["lookup", "voisu-provider", provider.secret_service_value()],
        None,
        true,
    ) {
        Ok(outcome) if outcome.success => match String::from_utf8(outcome.stdout) {
            Ok(value) => match Credential::new(value.trim_end().to_owned()) {
                Ok(credential) => LoadStep::Found(credential),
                Err(_) => LoadStep::Missing,
            },
            Err(_) => LoadStep::Stop(FallbackReason::Locked),
        },
        Ok(outcome) if outcome.stderr.is_empty() => LoadStep::Missing,
        // A nonzero exit WITH a stderr diagnostic is the transient-denial shape:
        // a momentary D-Bus/ksecretd hiccup looks identical to a genuinely locked
        // collection here, so a short bounded retry lets the hiccup recover while
        // a real lock still exhausts the budget and stays loud.
        Ok(_) => LoadStep::Retry(FallbackReason::Locked),
        Err(ProcessError::Unavailable) => LoadStep::Stop(FallbackReason::ToolMissing),
        // A timeout already consumed the full process deadline; retrying would
        // multiply the hot-path latency, so it stays terminal.
        Err(ProcessError::TimedOut) => LoadStep::Stop(FallbackReason::Locked),
        // A crashed/anomalous child is not retried (see `secret_tool_store`).
        Err(_) => LoadStep::Stop(FallbackReason::Unavailable),
    }
}

/// Prints the loud, one-time-per-process fallback warning with wording that
/// matches what actually happened — storing to the file, reading from it because
/// the keyring is down, or reading plaintext while the keyring is up (a migration
/// nudge). Naming the file and the remedy is the whole point: gh's *silent*
/// keyring fallback is the named anti-pattern we refuse to repeat.
pub(super) fn warn_fallback(notice: FallbackNotice) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static WARNED: AtomicBool = AtomicBool::new(false);
    if WARNED.swap(true, Ordering::Relaxed) {
        return;
    }
    let path = FileSecretStore::at_default().path().display().to_string();
    match notice {
        FallbackNotice::Store(reason) => eprintln!(
            "voisu: WARNING — {}. Storing the API key in a 0600 file at {} instead \
             (less secure than the keyring). To fix: {}.",
            reason.detail(),
            path,
            reason.remedy()
        ),
        FallbackNotice::Read(reason) => eprintln!(
            "voisu: WARNING — {}. Reading the API key from the 0600 file at {} \
             (less secure than the keyring). To fix: {}.",
            reason.detail(),
            path,
            reason.remedy()
        ),
        FallbackNotice::ReadWhileKeyringAvailable => eprintln!(
            "voisu: WARNING — reading the API key from the 0600 file at {} even though \
             your keyring is available. Run `voisu setup` to migrate it into the keyring.",
            path
        ),
    }
}

/// What is actually known when the post-store plaintext prune fails: the copy
/// is demonstrably still on disk, or its existence could not be checked at
/// all. The wording must never claim more than what was observed.
enum PlaintextPruneFailure {
    CopySurvived,
    Unverifiable,
}

/// The loud notice for a plaintext prune that failed after a successful
/// keyring store. It shares the fallback warnings' channel (stderr, naming
/// the file and the remedy) but not their once-per-process gate: this is a
/// distinct, rarer situation that must never be swallowed because an ordinary
/// fallback warning fired first.
fn warn_plaintext_prune_failed(failure: PlaintextPruneFailure) {
    let path = FileSecretStore::at_default().path().display().to_string();
    match failure {
        PlaintextPruneFailure::CopySurvived => eprintln!(
            "voisu: WARNING — the key is stored in your keyring, but the old plaintext copy at \
             {path} could not be removed. If the keyring is ever locked at start, that stale key \
             would be used. Delete {path}, then re-run `voisu doctor`."
        ),
        PlaintextPruneFailure::Unverifiable => eprintln!(
            "voisu: WARNING — the key is stored in your keyring, but Voisu could not verify \
             whether an old plaintext copy remains at {path}. If one does and the keyring is \
             ever locked at start, that stale key would be used. Check for and delete {path}, \
             then re-run `voisu doctor`."
        ),
    }
}
