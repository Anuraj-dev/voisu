use std::collections::VecDeque;

use std::ffi::CString;

use std::fs::{self, File, OpenOptions};

use std::io::{Read, Write};

use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd};

use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

use std::os::unix::net::UnixStream;

use std::os::unix::process::ExitStatusExt;

use std::path::{Path, PathBuf};

use std::process::{Child, Command, Stdio};

use std::sync::{Arc, Mutex, OnceLock};

use std::thread;

use std::time::{Duration, Instant};

use voisu_core::{
    ActiveCapture, AudioCapture, AudioChunk, BoundaryError, BoundaryFuture, BoundaryKind,
    CancelRegistry, CaptureLimit, CapturedAudio, ClipboardTool, Command as DaemonCommand,
    Credential, DeadlineClock, DeliveryAdapter, DeliveryOutcome, IntentReconstructionAttempt,
    IntentReconstructionRequest, KeyDiagnosis, KeyLocation, MergeResult, PACKAGE_MANAGERS,
    PROTOCOL_VERSION, PackageManager, PreparedTranscriptDecision, Provider, ProviderAuthenticator,
    ProviderKeyStatus, ProviderStream, ProviderWordConfidences, ReadinessCapability,
    ReadinessFinding, ReadinessInspector, ReadinessStatus, ReconciliationKind, ReconciliationModel,
    Request, Response, SecretStore, SessionKind, SessionResolution, ShortcutPortal,
    ShortcutSession, SourceTranscript, Transcript, TranscriptDecision, TranscriptDecisionPipeline,
    TranscriptProvider, TranscriptValidator, TriggerKeyBinding, VersionEnvelope, WavScan,
    clipboard_candidates, install_instruction, resolve_session, scan_wav_pcm, socket_path,
};

use crate::audio_level::{
    BandState, LevelRegistry, PCM_CHUNK_BYTES, PcmChunkAssembler, SampleDecoder, bands,
};

use crate::focus::SharedFocusProbe;

use crate::hyprland_bindings::VerifiedPasteAction;

use crate::process::guard_external_child;

use crate::secret_file::{FileSecretStore, RemoveError};

const PROCESS_DEADLINE: Duration = Duration::from_secs(2);

pub const CAPTURE_FINALIZE_DEADLINE: Duration = PROCESS_DEADLINE;

pub const PROVIDER_COMPLETION_DEADLINE: Duration = Duration::from_secs(15);

pub const CLIPBOARD_DELIVERY_DEADLINE: Duration = PROCESS_DEADLINE;

pub const LIBEI_DELIVERY_DEADLINE: Duration = Duration::from_secs(5);

/// Grace granted to the bounded capture/provider aborts that run when a
/// Recording fails or a partial start is rolled back.
pub const RECOVERY_ABORT_DEADLINE: Duration = PROCESS_DEADLINE;

pub const RECONCILIATION_DEADLINE: Duration = Duration::from_secs(3);

pub const INTENT_RECONSTRUCTION_DEADLINE: Duration = Duration::from_secs(5);

pub const PROCESSING_RESPONSE_DEADLINE: Duration = Duration::from_secs(
    CAPTURE_FINALIZE_DEADLINE.as_secs()
        + PROVIDER_COMPLETION_DEADLINE.as_secs()
        + CLIPBOARD_DELIVERY_DEADLINE.as_secs()
        + LIBEI_DELIVERY_DEADLINE.as_secs()
        + RECOVERY_ABORT_DEADLINE.as_secs()
        + RECONCILIATION_DEADLINE.as_secs() * 2
        + 1
        + crate::smart_writing::FINAL_TRANSFORM_GATE_DEADLINE.as_secs(),
);

/// Pre-validation credential preparation work budget (#103 / SW7). At expiry the
/// owner stops retries, kills any credential process group, and begins terminal
/// reap. Concurrent with provider completion; does not consume the Final
/// Transform Gate second.
pub const CREDENTIAL_PREP_WORK_DEADLINE: Duration = Duration::from_secs(13);

/// Diagnostic watchdog for credential kill/reap after cancel or work-deadline
/// expiry. Crossing it is not permission to detach: remain Processing, log once,
/// and keep awaiting terminal child wait + pipe EOF.
pub const CREDENTIAL_REAP_WATCHDOG: Duration = Duration::from_secs(2);

/// The transcription language every provider is asked for when
/// `VOISU_TRANSCRIPTION_LANGUAGE` is unset (or invalid — see
/// `voisu_app::config::resolve_transcription_language`). Exactly the historic
/// default, so an unset environment behaves byte-identically to before B6.
pub const DEFAULT_TRANSCRIPTION_LANGUAGE: &str = "en";

/// End-to-end budget shared by both sides of a paged history/export transfer.
pub const DIAGNOSTIC_RESPONSE_DEADLINE: Duration = Duration::from_secs(15);

const PROCESS_POLL: Duration = Duration::from_millis(10);

const MAX_DAEMON_RESPONSE_BYTES: usize = 16 * 1024;

const MAX_RETAINED_STDERR_BYTES: usize = 4 * 1024;

const MAX_RETAINED_STDOUT_BYTES: usize = 64 * 1024;

/// `hyprctl binds -j` on Omarchy-sized bind tables is ~100 KiB. The shared
/// helper stdout cap stays 64 KiB; this inspection-only ceiling is 1 MiB and
/// fail-closes if the dump is truncated.
const HYPRCTL_BINDS_JSON_CAP: usize = 1024 * 1024;

const PROVIDER_PROCESS_DEADLINE: Duration = Duration::from_secs(14);

const RECONCILIATION_PROCESS_DEADLINE: Duration = Duration::from_secs(2);

/// Default Groq chat-completions model for Transcript reconciliation.
/// Exact id only — no family-wide Qwen rule. Co-lands with the #98
/// `reasoning_effort: "none"` body field for this same exact id.
pub const DEFAULT_GROQ_RECONCILIATION_MODEL: &str = "qwen/qwen3.6-27b";

const MIN_RECORDING_BYTES: usize = PCM_CHUNK_BYTES;

/// The one configured maximum for a Recording. Both capture's retained PCM
/// buffer and its default Deadline derive from this value — and so do the
/// Overlay's approaching-limit warnings, which is why this is visible to the
/// rest of the crate rather than copied into the presentation layer.
pub(crate) const MAX_RECORDING_DURATION: Duration = Duration::from_secs(600);

/// Recordings at or below this length (120 s of 16 kHz s16le mono) take a
/// single full-audio Groq request at finalize: no pre-streamed chunks, no
/// seams, full context for Whisper. Only Recordings that grow past this switch
/// to pre-streamed chunking.
const GROQ_FULL_AUDIO_MAX_BYTES: usize = 16_000 * 2 * 120;

/// Pre-streamed chunk length for Recordings longer than the full-audio limit:
/// 60 s windows with a 4 s overlap so the word-overlap dedup can stitch seams.
const GROQ_CHUNK_BYTES: usize = 16_000 * 2 * 60;

const GROQ_CHUNK_OVERLAP_BYTES: usize = 16_000 * 2 * 4;

/// Word-overlap window for `merge_chunk_transcripts`, widened from the old 24
/// to cover the 4 s chunk overlap comfortably.
const GROQ_MERGE_OVERLAP_WORDS: usize = 48;

/// Bounded app-level redials for the Deepgram streaming websocket, covering
/// ONLY failed dials and connections that dropped before any audio was
/// delivered on them. Once audio has been accepted by a socket a drop is
/// unrecoverable (Deepgram has no server-side resume, and unfinalized audio
/// cannot be replayed), so it fails the provider visibly and the parallel
/// Groq stream carries the Recording.
const DEEPGRAM_RECONNECT_ATTEMPTS: usize = 2;

const DEEPGRAM_RECONNECT_BACKOFF: Duration = Duration::from_millis(250);

/// Whole-handshake bound (DNS + TCP + TLS + websocket upgrade) for one dial
/// of the streaming endpoint, so a black-holing network cannot pin the I/O
/// task past the Provider Deadline.
const DEEPGRAM_CONNECT_DEADLINE: Duration = Duration::from_secs(5);

/// Poll cadence at which the streaming I/O task observes `CancelRegistry`
/// (a poll-style flag, matching the subprocess poll-bound discipline).
const DEEPGRAM_CANCEL_POLL: Duration = Duration::from_millis(100);

/// Deepgram closes idle streaming sockets after ~10-12s without data; a JSON
/// `KeepAlive` text frame is sent whenever nothing else has gone out for this
/// long, well under that window.
const DEEPGRAM_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(5);

/// After `CloseStream` is sent, bounded wait for Deepgram to flush the final
/// `Results`, send the terminal summary `Metadata`, and close. A server that
/// never confirms within this grace fails the provider visibly — returning
/// the accumulated prefix would deliver a plausible but truncated Transcript.
const DEEPGRAM_CLOSE_GRACE: Duration = Duration::from_secs(10);

// Subsystem modules. `pub use <module>::*;` keeps every existing
// `voisu_app::system::Item` path working after the split (pure move).
mod capture;
mod credential;
mod deepgram;
mod delivery;
mod grammar;
mod groq;
mod libei_delivery;
mod portal_shortcuts;
mod provider_http;
mod readiness;
mod reconcile;
mod secret_store;

pub use capture::*;
pub use credential::*;
pub use deepgram::*;
pub use delivery::*;
pub use grammar::*;
pub use groq::*;
pub use libei_delivery::*;
pub use portal_shortcuts::*;
pub use provider_http::*;
pub use readiness::*;
pub use reconcile::*;
pub use secret_store::*;

fn curl_config_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Raw-endpoint pre-parse gate shared by every endpoint transport policy.
/// Rejects, before any parsing:
/// - control characters the URL parser silently strips before validating
///   (`\n`, `\r`, `\t` — so `localho\tst` cannot gate as plain `localhost`),
///   plus `\0`, which has no place in an endpoint;
/// - any `\`: the url crate ends its userinfo/host scan at `\` for special
///   schemes while curl splits userinfo at the LAST `@` and accepts `\`
///   inside it, so `http://localhost:8080\@attacker.example/` gates as
///   loopback-without-userinfo yet curl connects to attacker.example;
/// - any `@` in the raw authority: userinfo of ANY shape is invalid on a
///   provider endpoint, and the EMPTY form (`http://@localhost/`) is
///   invisible to the parsed accessors — `username()` is empty, `password()`
///   is None, and the url crate drops the `@` from its serialization
///   entirely — so only the raw authority can see it.
pub(crate) fn endpoint_raw_string_is_allowed(endpoint: &str) -> bool {
    if endpoint.contains(['\n', '\r', '\t', '\0', '\\']) {
        return false;
    }
    let Some((_scheme, authority)) = endpoint.split_once("://") else {
        // No scheme-shaped authority; the parse and scheme gates decide.
        return true;
    };
    !authority
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default()
        .contains('@')
}

/// Shared endpoint-transport policy on the PARSED URL: the host must be
/// present, and any surviving userinfo (non-empty username or any password)
/// rejects — no legitimate provider endpoint authenticates through the URL
/// itself. The raw empty-`@` form is caught earlier by
/// `endpoint_raw_string_is_allowed`, which sees the string the way curl does.
pub(crate) fn endpoint_authority_is_allowed(url: &url::Url) -> bool {
    url.host_str().is_some_and(|host| !host.is_empty())
        && url.username().is_empty()
        && url.password().is_none()
}

/// Whether the URL's PARSED host is loopback: exactly `localhost`, an IPv4 in
/// 127.0.0.0/8, or ::1. The comparison is on the parsed host only — never a
/// prefix or suffix of the raw authority string — so `localhost.evil.test` is a
/// different host and `localhost:8080@evil.test` is evil.test with userinfo.
pub(crate) fn parsed_host_is_loopback(url: &url::Url) -> bool {
    match url.host() {
        Some(url::Host::Domain(domain)) => domain == "localhost",
        Some(url::Host::Ipv4(address)) => address.is_loopback(),
        Some(url::Host::Ipv6(address)) => address.is_loopback(),
        None => false,
    }
}

fn restricted_command(program: &str) -> Command {
    let mut command = Command::new(program);
    guard_external_child(&mut command);
    command.env_clear();
    if let Some(path) = std::env::var_os("PATH") {
        command.env("PATH", path);
    }
    for name in [
        "XDG_RUNTIME_DIR",
        "DBUS_SESSION_BUS_ADDRESS",
        "WAYLAND_DISPLAY",
        "XDG_SESSION_TYPE",
        "HYPRLAND_INSTANCE_SIGNATURE",
        // X11 helpers (xclip, and any tool that talks to the X server) need
        // DISPLAY to find the server and XAUTHORITY to authenticate to it.
        // Without these, a spawned X11 helper can never reach the display —
        // which is why the field clipboard wrappers had to restore them by
        // hand. Forwarding XAUTHORITY widens what a helper can read (it names a
        // file holding X credentials), so this line is reviewed as
        // security-relevant and gated on a real host.
        "DISPLAY",
        "XAUTHORITY",
    ] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    command
}

fn run_restricted(
    program: &str,
    arguments: &[&str],
    input: Option<&[u8]>,
    capture_stdout: bool,
) -> Result<ProcessOutcome, ProcessError> {
    run_restricted_with_deadline(
        program,
        arguments,
        input,
        capture_stdout,
        PROCESS_DEADLINE,
        None,
    )
}

pub(crate) fn run_restricted_stdout(program: &str, arguments: &[&str]) -> Option<Vec<u8>> {
    run_restricted(program, arguments, None, true)
        .ok()
        .filter(|outcome| outcome.success)
        .map(|outcome| outcome.stdout)
}

/// Reads the compositor bind table without the 64 KiB helper stdout cap.
/// Truncation is a hard failure so setup never parses a chopped JSON prefix.
pub(crate) fn run_hyprctl_binds_json() -> Result<Vec<u8>, String> {
    let started = Instant::now();
    let mut command = restricted_command("hyprctl");
    command
        .args(["binds", "-j"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = command
        .spawn()
        .map_err(|_| "`hyprctl binds -j` returned a failure".to_owned())?;
    let stdout_reader = child.stdout.take().map(|mut stdout| {
        thread::spawn(move || read_capped_checked(&mut stdout, HYPRCTL_BINDS_JSON_CAP))
    });
    let status = wait_for_child(&mut child, started, PROCESS_DEADLINE, None)
        .map_err(|_| "`hyprctl binds -j` returned a failure".to_owned())?;
    let payload = match stdout_reader {
        Some(handle) => bounded_join(handle, started, &mut child, PROCESS_DEADLINE)
            .map_err(|_| "`hyprctl binds -j` returned a failure".to_owned())?
            .map_err(|_| "`hyprctl binds -j` returned a failure".to_owned())?,
        None => CappedRead::Complete(Vec::new()),
    };
    match payload {
        CappedRead::Truncated { .. } => {
            Err("`hyprctl binds -j` response exceeded the 1 MiB inspection budget".to_owned())
        }
        CappedRead::Complete(bytes) if status.success() => Ok(bytes),
        CappedRead::Complete(_) => Err("`hyprctl binds -j` returned a failure".to_owned()),
    }
}

/// Runs a helper whose SUCCESS mode is to fork a descendant that keeps
/// serving after the parent exits — real `wl-copy` serves the clipboard this
/// way. The descendant inherits the parent's pipes, so capturing output would
/// read the healthy case as a pipe held past the deadline; both streams are
/// discarded and only the parent's own exit status is observed.
fn run_restricted_serving(
    program: &str,
    arguments: &[&str],
    input: Option<&[u8]>,
) -> Result<ProcessOutcome, ProcessError> {
    run_restricted_serving_within(program, arguments, input, PROCESS_DEADLINE)
}

/// As [`run_restricted_serving`], but bounded by an explicit deadline so a
/// shared budget can span several candidate backends (see `clipboard_write`).
fn run_restricted_serving_within(
    program: &str,
    arguments: &[&str],
    input: Option<&[u8]>,
    deadline: Duration,
) -> Result<ProcessOutcome, ProcessError> {
    let started = Instant::now();
    let mut command = restricted_command(program);
    command
        .args(arguments)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = command.spawn().map_err(|_| ProcessError::Unavailable)?;
    let writer = match input {
        Some(input) => {
            let input = input.to_vec();
            let mut stdin = child.stdin.take().ok_or(ProcessError::Input)?;
            Some(thread::spawn(move || {
                let result = stdin.write_all(&input);
                drop(stdin);
                result
            }))
        }
        None => None,
    };
    let status = wait_for_child(&mut child, started, deadline, None);
    let writer = writer.map(|handle| bounded_join(handle, started, &mut child, deadline));
    let status = status?;
    if let Some(writer) = writer {
        match writer {
            Ok(Ok(())) => {}
            Err(ProcessError::TimedOut) => return Err(ProcessError::TimedOut),
            _ => return Err(ProcessError::Input),
        }
    }
    Ok(ProcessOutcome {
        success: status.success(),
        stdout: Vec::new(),
        stderr: Vec::new(),
    })
}

fn run_restricted_with_deadline(
    program: &str,
    arguments: &[&str],
    input: Option<&[u8]>,
    capture_stdout: bool,
    deadline: Duration,
    cancel: Option<&CancelRegistry>,
) -> Result<ProcessOutcome, ProcessError> {
    // Fail fast without spawning when the operation is already cancelled.
    if cancel.is_some_and(CancelRegistry::is_cancelled) {
        return Err(ProcessError::TimedOut);
    }
    let started = Instant::now();
    let mut command = restricted_command(program);
    command
        .args(arguments)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(if capture_stdout {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|_| ProcessError::Unavailable)?;
    // The whole-operation deadline starts before spawn and covers startup, the
    // stdin write, pipe drains, and wait. The write runs on its own thread so
    // the polling loop can kill an overdue child, which breaks the pipe and
    // unblocks the writer.
    let writer = match input {
        Some(input) => {
            let input = input.to_vec();
            let mut stdin = child.stdin.take().ok_or(ProcessError::Input)?;
            Some(thread::spawn(move || {
                let result = stdin.write_all(&input);
                drop(stdin);
                result
            }))
        }
        None => None,
    };
    let stdout_reader = child.stdout.take().map(|mut stdout| {
        thread::spawn(move || read_capped(&mut stdout, MAX_RETAINED_STDOUT_BYTES))
    });
    let stderr_reader = child.stderr.take().map(|mut stderr| {
        thread::spawn(move || read_capped(&mut stderr, MAX_RETAINED_STDERR_BYTES))
    });
    // Every helper thread join is bounded by the same Instant budget on every
    // path: a descendant of the child can inherit and hold the pipes open past
    // the child's own exit, which would otherwise block a bare join() forever
    // (or, on the error path, silently leave detached threads blocked).
    // Collect every helper-thread result FIRST, then decide the outcome: an
    // early return between joins would silently detach a later thread while it
    // may still be blocked on a descendant-held pipe.
    let status = wait_for_child(&mut child, started, deadline, cancel);
    let writer = writer.map(|handle| bounded_join(handle, started, &mut child, deadline));
    let stdout_joined =
        stdout_reader.map(|handle| bounded_join(handle, started, &mut child, deadline));
    let stderr_joined =
        stderr_reader.map(|handle| bounded_join(handle, started, &mut child, deadline));
    let stdout = pipe_bytes(stdout_joined)?;
    let stderr = pipe_bytes(stderr_joined)?;
    let status = status?;
    if let Some(writer) = writer {
        match writer {
            Ok(Ok(())) => {}
            Err(ProcessError::TimedOut) => return Err(ProcessError::TimedOut),
            _ => return Err(ProcessError::Input),
        }
    }
    Ok(ProcessOutcome {
        success: status.success(),
        stdout,
        stderr,
    })
}

/// Joins a helper thread under the remaining process budget. On budget
/// exhaustion the overdue child is killed and the thread is deliberately
/// detached — it can never be forced to finish while a descendant holds the
/// pipe — and the caller receives the timeout error.
fn bounded_join<T: Send + 'static>(
    handle: thread::JoinHandle<T>,
    started: Instant,
    child: &mut Child,
    deadline: Duration,
) -> Result<T, ProcessError> {
    while !handle.is_finished() {
        if started.elapsed() >= deadline {
            let _ = child.kill();
            reap_briefly(child);
            drop(handle);
            return Err(ProcessError::TimedOut);
        }
        thread::sleep(PROCESS_POLL);
    }
    handle.join().map_err(|_| ProcessError::Output)
}

/// Best-effort reap of a killed child under a small extra budget so no zombie
/// is left behind; if it still has not been collected, give up rather than
/// block the caller further.
fn reap_briefly(child: &mut Child) {
    let reap_started = Instant::now();
    while reap_started.elapsed() < Duration::from_millis(250) {
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => return,
            Ok(None) => thread::sleep(PROCESS_POLL),
        }
    }
}

fn pipe_bytes(
    joined: Option<Result<std::io::Result<Vec<u8>>, ProcessError>>,
) -> Result<Vec<u8>, ProcessError> {
    match joined {
        Some(result) => result?.map_err(|_| ProcessError::Output),
        None => Ok(Vec::new()),
    }
}

/// Drains a pipe to EOF so the child never blocks on a full buffer, but
/// retains only the first `cap` bytes: a noisy child cannot force unbounded
/// memory growth inside the deadline window.
fn read_capped(source: &mut impl Read, cap: usize) -> std::io::Result<Vec<u8>> {
    match read_capped_checked(source, cap)? {
        CappedRead::Complete(bytes) | CappedRead::Truncated { retained: bytes } => Ok(bytes),
    }
}

enum CappedRead {
    Complete(Vec<u8>),
    Truncated { retained: Vec<u8> },
}

/// Like [`read_capped`], but reports whether any bytes past `cap` were drained.
fn read_capped_checked(source: &mut impl Read, cap: usize) -> std::io::Result<CappedRead> {
    let mut retained = Vec::new();
    let mut truncated = false;
    let mut buffer = [0_u8; 1024];
    loop {
        match source.read(&mut buffer) {
            Ok(0) => {
                return Ok(if truncated {
                    CappedRead::Truncated { retained }
                } else {
                    CappedRead::Complete(retained)
                });
            }
            Ok(read) => {
                if truncated {
                    continue;
                }
                let room = cap.saturating_sub(retained.len());
                let keep = read.min(room);
                retained.extend_from_slice(&buffer[..keep]);
                if keep < read {
                    truncated = true;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
}

fn wait_for_child(
    child: &mut Child,
    started: Instant,
    deadline: Duration,
    cancel: Option<&CancelRegistry>,
) -> Result<std::process::ExitStatus, ProcessError> {
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) => {}
            Err(_) => {
                // The child may still be live even though its status cannot be
                // read; kill and best-effort reap before surfacing the error.
                let _ = child.kill();
                reap_briefly(child);
                return Err(ProcessError::Wait);
            }
        }
        // Cancellation is observed by the loop that owns the Child handle:
        // killing through the handle is pid-reuse-safe because this loop is
        // also the only reaper. Latency is at most one poll tick.
        if cancel.is_some_and(CancelRegistry::is_cancelled) || started.elapsed() >= deadline {
            let _ = child.kill();
            reap_briefly(child);
            return Err(ProcessError::TimedOut);
        }
        thread::sleep(PROCESS_POLL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    use voisu_core::{ProviderCoordinator, ProviderStreams};

    #[test]
    fn read_capped_checked_completes_when_the_dump_fits() {
        let payload = vec![b'a'; 64];
        match read_capped_checked(&mut payload.as_slice(), 64).unwrap() {
            CappedRead::Complete(bytes) => assert_eq!(bytes, payload),
            CappedRead::Truncated { .. } => panic!("exact-cap dump must not look truncated"),
        }
    }

    #[test]
    fn read_capped_checked_fail_closes_when_the_dump_exceeds_the_budget() {
        let payload = vec![b'b'; 65];
        match read_capped_checked(&mut payload.as_slice(), 64).unwrap() {
            CappedRead::Truncated { retained } => assert_eq!(retained, payload[..64]),
            CappedRead::Complete(_) => panic!("over-budget dump must not parse as complete"),
        }
    }

    #[test]
    fn manager_env_missing_display_is_detected_from_show_environment() {
        let present = "LANG=en_US.UTF-8\nWAYLAND_DISPLAY=wayland-0\nDISPLAY=:0\n";
        assert!(manager_env_has(present, "WAYLAND_DISPLAY"));
        assert!(manager_env_has(present, "DISPLAY"));
        // Absent, and set-but-empty, both read as missing.
        let missing = "LANG=en_US.UTF-8\nDISPLAY=\n";
        assert!(!manager_env_has(missing, "WAYLAND_DISPLAY"));
        assert!(!manager_env_has(missing, "DISPLAY"));
    }

    #[test]
    fn pw_help_token_detects_raw_support() {
        assert!(help_advertises_raw(
            b"  -a, --raw     RAW mode (no header)\n  -h, --help\n"
        ));
        // An `=`-attached value form still exposes the exact option token.
        assert!(help_advertises_raw(b"      --raw=MODE   raw capture\n"));
        // PipeWire 1.0.5-era help never mentions --raw.
        assert!(!help_advertises_raw(
            b"  -R, --remote  Remote daemon\n  -h, --help\n"
        ));
    }

    #[test]
    fn pw_help_rejects_raw_near_matches() {
        // A different option that merely starts with the same letters must not
        // be mistaken for --raw support.
        assert!(!help_advertises_raw(
            b"      --raw-file FILE   write raw to FILE\n"
        ));
        assert!(!help_advertises_raw(b"      --rawmode        legacy\n"));
        // Substring inside another word must not match either.
        assert!(!help_advertises_raw(b"  see the xyz--rawabc note\n"));
    }

    fn canonical_wav_with_payload(payload: &[u8]) -> Vec<u8> {
        let mut stream = Vec::new();
        stream.extend_from_slice(b"RIFF");
        stream.extend_from_slice(&u32::MAX.to_le_bytes());
        stream.extend_from_slice(b"WAVE");
        stream.extend_from_slice(b"fmt ");
        stream.extend_from_slice(&16u32.to_le_bytes());
        stream.extend_from_slice(&1u16.to_le_bytes());
        stream.extend_from_slice(&1u16.to_le_bytes());
        stream.extend_from_slice(&16_000u32.to_le_bytes());
        stream.extend_from_slice(&32_000u32.to_le_bytes());
        stream.extend_from_slice(&2u16.to_le_bytes());
        stream.extend_from_slice(&16u16.to_le_bytes());
        stream.extend_from_slice(b"data");
        stream.extend_from_slice(&u32::MAX.to_le_bytes());
        stream.extend_from_slice(payload);
        stream
    }

    #[test]
    fn wav_stripper_yields_only_the_pcm_payload() {
        let payload: Vec<u8> = (0..500u16).flat_map(|n| n.to_le_bytes()).collect();
        let stream = canonical_wav_with_payload(&payload);
        let mut stripper = WavHeaderStripper::new(std::io::Cursor::new(stream));
        let mut recovered = Vec::new();
        std::io::Read::read_to_end(&mut stripper, &mut recovered).unwrap();
        assert_eq!(recovered, payload);
    }

    #[test]
    fn wav_stripper_handles_a_header_split_across_reads() {
        // A reader that hands back one byte at a time must still resolve the
        // header and recover the exact payload.
        struct OneByteAtATime(std::io::Cursor<Vec<u8>>);
        impl Read for OneByteAtATime {
            fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
                if out.is_empty() {
                    return Ok(0);
                }
                self.0.read(&mut out[..1])
            }
        }
        let payload = b"the-pcm-body".to_vec();
        let stream = canonical_wav_with_payload(&payload);
        let mut stripper = WavHeaderStripper::new(OneByteAtATime(std::io::Cursor::new(stream)));
        let mut recovered = Vec::new();
        std::io::Read::read_to_end(&mut stripper, &mut recovered).unwrap();
        assert_eq!(recovered, payload);
    }

    #[test]
    fn wav_stripper_reports_a_wrong_format_as_a_read_error() {
        let mut stream = Vec::new();
        stream.extend_from_slice(b"RIFF");
        stream.extend_from_slice(&u32::MAX.to_le_bytes());
        stream.extend_from_slice(b"WAVE");
        stream.extend_from_slice(b"fmt ");
        stream.extend_from_slice(&16u32.to_le_bytes());
        stream.extend_from_slice(&1u16.to_le_bytes());
        stream.extend_from_slice(&2u16.to_le_bytes()); // stereo — wrong
        stream.extend_from_slice(&48_000u32.to_le_bytes()); // 48 kHz — wrong
        stream.extend_from_slice(&192_000u32.to_le_bytes());
        stream.extend_from_slice(&4u16.to_le_bytes());
        stream.extend_from_slice(&16u16.to_le_bytes());
        stream.extend_from_slice(b"data");
        stream.extend_from_slice(&u32::MAX.to_le_bytes());
        stream.extend_from_slice(&[0_u8; 64]);
        let mut stripper = WavHeaderStripper::new(std::io::Cursor::new(stream));
        let mut sink = Vec::new();
        let error = std::io::Read::read_to_end(&mut stripper, &mut sink).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn path_resolution_honors_path_order_for_shadowing_wrappers() {
        let home = tempfile::tempdir().unwrap();
        let system = tempfile::tempdir().unwrap();
        // A hand-written wrapper in a home dir that precedes the system dir.
        let wrapper = home.path().join("wl-copy");
        fs::write(&wrapper, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).unwrap();
        let packaged = system.path().join("wl-copy");
        fs::write(&packaged, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&packaged, fs::Permissions::from_mode(0o755)).unwrap();

        let path = std::env::join_paths([home.path(), system.path()]).unwrap();
        let winner = resolve_on_path(&path, "wl-copy").unwrap();
        assert_eq!(winner, wrapper);
        assert!(winner.starts_with(home.path()));
    }

    #[test]
    fn setup_clipboard_gate_rejects_shadowed_or_failed_backends() {
        assert!(!clipboard_finding_is_usable(
            &ReadinessFinding::new(
                ReadinessCapability::Clipboard,
                ReadinessStatus::Warn,
                "clipboard wrapper shadows wl-clipboard",
            )
            .with_value("shadowed wrapper")
        ));
        assert!(!clipboard_finding_is_usable(&ReadinessFinding::new(
            ReadinessCapability::Clipboard,
            ReadinessStatus::Fail,
            "wl-copy is missing",
        )));
        assert!(clipboard_finding_is_usable(&ReadinessFinding::new(
            ReadinessCapability::Clipboard,
            ReadinessStatus::Pass,
            "clipboard roundtrip succeeds",
        )));
    }

    #[test]
    fn setup_clipboard_gate_rejects_a_probe_that_could_not_restore() {
        assert!(!clipboard_finding_is_usable(&ReadinessFinding::new(
            ReadinessCapability::Clipboard,
            ReadinessStatus::Warn,
            "clipboard roundtrip succeeds but the prior clipboard could not be restored",
        )));
    }

    #[test]
    fn clipboard_probe_restores_after_readback_failure() {
        let writes = std::cell::RefCell::new(Vec::new());
        let result = probe_clipboard_roundtrip_with(
            Some(b"prior clipboard".to_vec()),
            b"probe",
            |value| {
                writes.borrow_mut().push(value.to_vec());
                Ok(true)
            },
            || false,
        );

        assert!(matches!(result, ClipboardProbe::Failed));
        assert_eq!(
            writes.into_inner(),
            vec![b"probe".to_vec(), b"prior clipboard".to_vec()]
        );
    }

    #[test]
    fn clipboard_probe_restores_after_a_spawned_write_failure() {
        let writes = std::cell::RefCell::new(Vec::new());
        let first_write = std::cell::Cell::new(true);
        let result = probe_clipboard_roundtrip_with(
            Some(b"prior clipboard".to_vec()),
            b"probe",
            |value| {
                writes.borrow_mut().push(value.to_vec());
                if first_write.replace(false) {
                    Err(ProcessError::Wait)
                } else {
                    Ok(true)
                }
            },
            || panic!("readback must not run after a failed write"),
        );

        assert!(matches!(result, ClipboardProbe::Failed));
        assert_eq!(
            writes.into_inner(),
            vec![b"probe".to_vec(), b"prior clipboard".to_vec()]
        );
    }

    #[test]
    fn clipboard_read_probe_treats_empty_selection_as_usable() {
        assert!(clipboard_read_proves_display(true, b""));
        assert!(clipboard_read_proves_display(false, b"Nothing is copied\n"));
        assert!(clipboard_read_proves_display(
            false,
            b"Error: target STRING not available\n"
        ));
    }

    #[test]
    fn clipboard_read_probe_treats_connect_failure_as_unusable() {
        assert!(!clipboard_read_proves_display(
            false,
            b"failed to connect to a Wayland server: No such file or directory\n"
        ));
        assert!(!clipboard_read_proves_display(
            true,
            b"failed to connect to a Wayland server: No such file or directory\n"
        ));
        assert!(!clipboard_read_proves_display(
            false,
            b"Error: Can't open display: :0\n"
        ));
    }

    #[test]
    fn shortcut_session_token_is_stable_across_bind_cycles() {
        // Regression: the session_handle_token names a persistent kglobalaccel
        // component in xdg-desktop-portal-kde, so it MUST be identical across
        // separate bind cycles and independent of process identity — otherwise
        // KWin has no stored binding and re-prompts on every daemon start,
        // leaking an orphaned [token_voisu_session_<pid>] config section. The
        // broken code embedded the PID (`voisu_session_{pid}`).
        let first = shortcut_bind_tokens();
        let second = shortcut_bind_tokens();
        assert_eq!(first.session, second.session);
        assert_eq!(first.session, "voisu_session");
        assert!(
            !first.session.contains(&std::process::id().to_string()),
            "session_handle_token must not embed the PID"
        );

        // The request handle_tokens are a distinct mechanism identifying
        // in-flight Request objects and must stay unique per daemon process.
        let pid = std::process::id().to_string();
        assert!(first.create.contains(&pid));
        assert!(first.bind.contains(&pid));
    }

    #[test]
    fn crypto_provider_installs_and_is_idempotent() {
        install_crypto_provider();
        assert!(rustls::crypto::CryptoProvider::get_default().is_some());
        install_crypto_provider();
        assert!(rustls::crypto::CryptoProvider::get_default().is_some());
    }

    fn credential(value: &str) -> Credential {
        Credential::new(value.to_owned()).expect("test credential is valid")
    }

    #[test]
    fn session_cache_serves_a_second_load_without_re_invoking_the_store() {
        // The observed failure mode: a warm daemon whose credential was loaded on
        // an earlier Recording hits a transient denial on a later one. A cache hit
        // must serve that later load without re-reaching secret-tool at all.
        let cache = CredentialCache::new();
        let ttl = Duration::from_secs(300);
        let calls = std::cell::Cell::new(0_usize);
        let first = resolve_with_cache(Provider::Groq, &cache, ttl, || {
            calls.set(calls.get() + 1);
            Ok(credential("cached-secret"))
        })
        .unwrap();
        let second = resolve_with_cache(Provider::Groq, &cache, ttl, || {
            calls.set(calls.get() + 1);
            Ok(credential("cached-secret"))
        })
        .unwrap();
        assert_eq!(first.expose_to_boundary(), "cached-secret");
        assert_eq!(second.expose_to_boundary(), "cached-secret");
        assert_eq!(
            calls.get(),
            1,
            "the second load must be served from the cache"
        );
    }

    #[test]
    fn session_cache_re_reads_after_the_ttl_expires() {
        // A zero TTL means every entry is already stale, so a rotated key is never
        // served past its bound — each load re-reads.
        let cache = CredentialCache::new();
        let ttl = Duration::from_millis(0);
        let calls = std::cell::Cell::new(0_usize);
        for _ in 0..2 {
            resolve_with_cache(Provider::Groq, &cache, ttl, || {
                calls.set(calls.get() + 1);
                Ok(credential("fresh-secret"))
            })
            .unwrap();
        }
        assert_eq!(
            calls.get(),
            2,
            "an expired entry must be re-read, never served stale"
        );
    }

    #[test]
    fn session_cache_invalidation_forces_a_re_read() {
        let cache = CredentialCache::new();
        let ttl = Duration::from_secs(300);
        let calls = std::cell::Cell::new(0_usize);
        resolve_with_cache(Provider::Groq, &cache, ttl, || {
            calls.set(calls.get() + 1);
            Ok(credential("secret"))
        })
        .unwrap();
        cache.invalidate(Provider::Groq);
        resolve_with_cache(Provider::Groq, &cache, ttl, || {
            calls.set(calls.get() + 1);
            Ok(credential("secret"))
        })
        .unwrap();
        assert_eq!(calls.get(), 2, "invalidation must drop the cached entry");
    }

    #[test]
    fn session_cache_keys_each_provider_independently() {
        let cache = CredentialCache::new();
        let ttl = Duration::from_secs(300);
        let groq =
            resolve_with_cache(Provider::Groq, &cache, ttl, || Ok(credential("groq-key"))).unwrap();
        let deepgram = resolve_with_cache(Provider::Deepgram, &cache, ttl, || {
            Ok(credential("deepgram-key"))
        })
        .unwrap();
        assert_eq!(groq.expose_to_boundary(), "groq-key");
        assert_eq!(
            deepgram.expose_to_boundary(),
            "deepgram-key",
            "one provider must not read another's slot"
        );
    }

    #[test]
    fn session_cache_does_not_store_a_failed_load() {
        // A failed load must not poison the cache: the next attempt must retry the
        // store, not serve a cached error.
        let cache = CredentialCache::new();
        let ttl = Duration::from_secs(300);
        let first = resolve_with_cache(Provider::Groq, &cache, ttl, || {
            Err(BoundaryError::new(BoundaryKind::SecretStorage, "transient"))
        });
        assert!(first.is_err());
        let second =
            resolve_with_cache(Provider::Groq, &cache, ttl, || Ok(credential("recovered")))
                .unwrap();
        assert_eq!(
            second.expose_to_boundary(),
            "recovered",
            "a failure must not be cached"
        );
    }

    /// Fixed Groq request tuning for stream constructor tests: deterministic and
    /// independent of the host's environment and user dictionary.
    fn test_groq_params() -> GroqRequestParams {
        GroqRequestParams {
            model: "whisper-large-v3".to_owned(),
            language: "en".to_owned(),
            prompt: "Groq, Tokio".to_owned(),
        }
    }

    #[test]
    fn groq_stays_full_audio_at_or_below_the_limit_and_chunks_above() {
        // A Recording at exactly the full-audio limit still takes one full-audio
        // request; only once it grows past the limit does pre-streaming begin.
        assert!(!groq_prestream_active(GROQ_FULL_AUDIO_MAX_BYTES));
        assert!(!groq_prestream_active(GROQ_FULL_AUDIO_MAX_BYTES - 1));
        assert!(groq_prestream_active(GROQ_FULL_AUDIO_MAX_BYTES + 1));
    }

    #[test]
    fn finalize_is_one_full_audio_request_at_or_below_the_limit() {
        assert_eq!(plan_finalize_chunks(1_000), vec![0..1_000]);
        assert_eq!(
            plan_finalize_chunks(GROQ_FULL_AUDIO_MAX_BYTES),
            vec![0..GROQ_FULL_AUDIO_MAX_BYTES]
        );
    }

    #[test]
    fn finalize_chunks_a_backlog_inflated_recording_past_the_limit() {
        // 130 s finalized (e.g. a large capture backlog appended at Stop pushed a
        // Recording that streamed under 120 s past the limit): it must be chunked
        // into 60 s windows with a 4 s overlap, not one oversized request.
        let len = GROQ_FULL_AUDIO_MAX_BYTES + 16_000 * 2 * 10;
        let ranges = plan_finalize_chunks(len);
        assert!(
            ranges.len() >= 2,
            "past the limit finalize is chunked, not one request"
        );
        assert_eq!(ranges[0], 0..GROQ_CHUNK_BYTES);
        assert_eq!(ranges[1].start, GROQ_CHUNK_BYTES - GROQ_CHUNK_OVERLAP_BYTES);
        assert_eq!(
            ranges.last().unwrap().end,
            len,
            "the last window ends at the recording end"
        );
        for range in &ranges[..ranges.len() - 1] {
            assert_eq!(
                range.end - range.start,
                GROQ_CHUNK_BYTES,
                "non-final windows are full chunks"
            );
        }
    }

    #[test]
    fn groq_chunk_geometry_is_sixty_second_windows_with_a_four_second_overlap() {
        assert_eq!(GROQ_CHUNK_BYTES, 16_000 * 2 * 60);
        assert_eq!(GROQ_CHUNK_OVERLAP_BYTES, 16_000 * 2 * 4);
        assert_eq!(GROQ_FULL_AUDIO_MAX_BYTES, 16_000 * 2 * 120);
    }

    /// send_audio advances its pre-stream buffer with `Vec::drain(..step)`.
    /// This pins the invariant that change relies on: draining the front
    /// `step` bytes leaves exactly the tail that the previous
    /// `buffer[step..].to_vec()` re-allocation produced, at production
    /// geometry and at arbitrary lengths.
    #[test]
    fn groq_overlap_rotation_drain_matches_slice_to_vec() {
        let step = GROQ_CHUNK_BYTES - GROQ_CHUNK_OVERLAP_BYTES;
        for &(len, cut) in &[
            (GROQ_CHUNK_BYTES, step),
            (4 * GROQ_CHUNK_BYTES, step),
            (5_000_000, step),
            (4096, 1000),
            (1000, 1000),
        ] {
            let original: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
            let mut via_drain = original.clone();
            via_drain.drain(..cut);
            assert_eq!(
                via_drain,
                original[cut..].to_vec(),
                "drain(..{cut}) must equal slice+to_vec for len={len}"
            );
        }
    }

    #[test]
    fn groq_curl_config_carries_the_accuracy_gains() {
        let credential = Credential::new("secret-token".to_owned()).unwrap();
        let params = GroqRequestParams {
            model: "whisper-large-v3".to_owned(),
            language: "en".to_owned(),
            prompt: "Tokio, serde, SELinux".to_owned(),
        };
        let config = build_groq_curl_config(
            "https://api.groq.com/v1",
            &credential,
            "/tmp/rec.wav",
            &params,
        )
        .expect("valid config");
        assert!(config.contains("form = \"model=whisper-large-v3\""));
        assert!(config.contains("form = \"language=en\""));
        assert!(config.contains("form = \"temperature=0\""));
        assert!(config.contains("form = \"prompt=Tokio, serde, SELinux\""));
        // Slice B4: word-timestamp granularities ride on verbose_json, and
        // BOTH granularities are requested — `word` for the word list and
        // `segment` for the per-segment avg_logprob the word confidences are
        // derived from.
        assert!(config.contains("form = \"response_format=verbose_json\""));
        assert!(config.contains("form = \"timestamp_granularities[]=word\""));
        assert!(config.contains("form = \"timestamp_granularities[]=segment\""));
        assert!(config.contains("Authorization: Bearer secret-token"));
    }

    // Slice B6: the Groq language request field is wired to the same resolved
    // value as Deepgram's `language` query param — any validated code is
    // forwarded verbatim, not just the English default.
    #[test]
    fn groq_curl_config_carries_a_resolved_non_english_language() {
        let credential = Credential::new("secret-token".to_owned()).unwrap();
        let params = GroqRequestParams {
            model: "whisper-large-v3".to_owned(),
            language: "de-de".to_owned(),
            prompt: String::new(),
        };
        let config = build_groq_curl_config(
            "https://api.groq.com/v1",
            &credential,
            "/tmp/rec.wav",
            &params,
        )
        .expect("valid config");
        assert!(config.contains("form = \"language=de-de\""), "{config}");
    }

    #[test]
    fn groq_recording_payload_is_valid_flac() {
        let pcm = vec![0_u8; 16_000 * 2];
        let flac = flac_from_pcm(&pcm).expect("one second Recording encodes");

        assert_eq!(&flac[..4], b"fLaC");
        assert!(flac.len() < pcm.len(), "silence compresses below raw PCM");
    }

    #[test]
    fn groq_curl_config_uploads_a_flac_recording() {
        let credential = Credential::new("secret-token".to_owned()).unwrap();
        let config = build_groq_curl_config(
            "https://api.groq.com/v1",
            &credential,
            "/tmp/rec.flac",
            &test_groq_params(),
        )
        .expect("valid config");

        assert!(
            config
                .contains("form = \"file=@/tmp/rec.flac;filename=recording.flac;type=audio/flac\"")
        );
        assert!(!config.contains("recording.wav"));
        assert!(!config.contains("audio/wav"));
    }

    #[test]
    fn groq_curl_config_omits_an_empty_prompt_and_language() {
        let credential = Credential::new("secret-token".to_owned()).unwrap();
        let params = GroqRequestParams {
            model: "whisper-large-v3".to_owned(),
            language: String::new(),
            prompt: String::new(),
        };
        let config = build_groq_curl_config(
            "https://api.groq.com/v1",
            &credential,
            "/tmp/rec.wav",
            &params,
        )
        .expect("valid config");
        assert!(!config.contains("prompt="));
        assert!(!config.contains("language="));
        // temperature is unconditional.
        assert!(config.contains("form = \"temperature=0\""));
    }

    #[test]
    fn groq_curl_config_rejects_a_control_character_model() {
        let credential = Credential::new("secret-token".to_owned()).unwrap();
        let params = GroqRequestParams {
            model: "bad\nmodel".to_owned(),
            language: "en".to_owned(),
            prompt: String::new(),
        };
        let error = build_groq_curl_config(
            "https://api.groq.com/v1",
            &credential,
            "/tmp/rec.wav",
            &params,
        )
        .unwrap_err();
        assert_eq!(error.diagnostic(), "invalid Groq model");
    }

    #[test]
    fn merge_dedupes_an_overlap_wider_than_the_old_window() {
        // A ~30-word seam overlap — wider than the previous 24-word window —
        // must be collapsed, not duplicated, at the 4 s chunk boundary.
        let first: Vec<String> = (0..40).map(|i| format!("w{i}")).collect();
        let second: Vec<String> = (10..60).map(|i| format!("w{i}")).collect();
        let merged = merge_chunk_transcripts(vec![
            GroqChunkTranscript {
                text: first.join(" "),
                words: Vec::new(),
            },
            GroqChunkTranscript {
                text: second.join(" "),
                words: Vec::new(),
            },
        ]);
        let expected: Vec<String> = (0..60).map(|i| format!("w{i}")).collect();
        assert_eq!(
            merged.text,
            expected.join(" "),
            "the 30-word overlap is deduped"
        );
    }

    // ─── Slice B4: Groq verbose_json fixtures (documented API shape) ────────

    /// A realistic single-segment verbose_json response, built from the
    /// documented Groq/OpenAI-compatible Whisper shape: top-level `text`, a
    /// `words[]` array of `{word, start, end}` entries, and a `segments[]`
    /// array whose entries carry `avg_logprob` (nats/token, closer to zero is
    /// more confident).
    const VERBOSE_JSON_ONE_SEGMENT: &str = r#"{
        "task": "transcribe",
        "language": "en",
        "duration": 2.1,
        "text": "Deploy the cache migration today.",
        "words": [
            {"word": "Deploy", "start": 0.08, "end": 0.52},
            {"word": "the", "start": 0.56, "end": 0.71},
            {"word": "cache", "start": 0.75, "end": 1.09},
            {"word": "migration", "start": 1.14, "end": 1.72},
            {"word": "today.", "start": 1.78, "end": 2.05}
        ],
        "x_groq": {"id": "req_01j", "runtime": {"model": "whisper-large-v3", "processed_by": "whisper-asr"}},
        "segments": [
            {
                "id": 0,
                "seek": 0,
                "start": 0.0,
                "end": 2.1,
                "text": " Deploy the cache migration today.",
                "tokens": [1, 2],
                "temperature": 0.0,
                "avg_logprob": -0.15,
                "compression_ratio": 1.4,
                "no_speech_prob": 0.013,
                "transient": false
            }
        ]
    }"#;

    #[test]
    fn a_verbose_json_fixture_parses_text_and_word_confidences() {
        let response: serde_json::Value = serde_json::from_str(VERBOSE_JSON_ONE_SEGMENT).unwrap();
        let parsed = parse_groq_transcription_response(&response).expect("parses");
        assert_eq!(parsed.text, "Deploy the cache migration today.");
        assert_eq!(parsed.words.len(), 5);
        // exp(-0.15) ≈ 0.861: a confident segment yields confident words.
        let expected = (-0.15_f64).exp().clamp(0.0, 1.0);
        assert!(
            parsed
                .words
                .iter()
                .all(|(_, confidence)| (confidence - expected).abs() < 1e-9),
            "every word inherits its covering segment's confidence: {parsed:?}"
        );
        assert_eq!(parsed.words[0].0, "Deploy");
        assert_eq!(parsed.words[4].0, "today.");
    }

    #[test]
    fn a_verbose_json_word_inherits_its_own_covering_segment_logprob() {
        // Two segments: the first confident (-0.10), the second shaky (-1.20).
        // Each word must read the segment its start timestamp falls in — the
        // occurrence/position rule, never a single global confidence.
        let response: serde_json::Value = serde_json::from_str(
            r#"{
            "text": "Deploy the service now",
            "words": [
                {"word": "Deploy", "start": 0.10, "end": 0.50},
                {"word": "the", "start": 0.55, "end": 0.70},
                {"word": "service", "start": 1.20, "end": 1.80},
                {"word": "now", "start": 1.85, "end": 2.10}
            ],
            "segments": [
                {"id": 0, "start": 0.0, "end": 1.0, "text": "Deploy the", "avg_logprob": -0.10},
                {"id": 1, "start": 1.0, "end": 2.2, "text": "service now", "avg_logprob": -1.20}
            ]
        }"#,
        )
        .unwrap();
        let parsed = parse_groq_transcription_response(&response).expect("parses");
        let confident = (-0.10_f64).exp();
        let shaky = (-1.20_f64).exp();
        assert_eq!(parsed.words[0].1, confident);
        assert_eq!(parsed.words[1].1, confident);
        assert_eq!(parsed.words[2].1, shaky);
        assert_eq!(parsed.words[3].1, shaky);
    }

    #[test]
    fn a_word_before_every_segment_falls_back_to_the_earliest_segment() {
        let response: serde_json::Value = serde_json::from_str(
            r#"{
            "text": "hello",
            "words": [{"word": "hello", "start": 0.0, "end": 0.4}],
            "segments": [
                {"id": 0, "start": 0.5, "end": 1.0, "text": "hello", "avg_logprob": -0.30}
            ]
        }"#,
        )
        .unwrap();
        let parsed = parse_groq_transcription_response(&response).expect("parses");
        assert_eq!(parsed.words[0].1, (-0.30_f64).exp().clamp(0.0, 1.0));
    }

    #[test]
    fn non_finite_and_missing_logprob_evidence_is_unproven_not_confident() {
        // A word whose covering segment carries a NaN avg_logprob — and a
        // response whose segments carry none at all — degrades to 0.0,
        // mirroring the Deepgram ingest clamp.
        let response: serde_json::Value = serde_json::from_str(
            r#"{
            "text": "hello there",
            "words": [
                {"word": "hello", "start": 0.1, "end": 0.4},
                {"word": "there", "start": 0.5, "end": 0.9}
            ],
            "segments": [
                {"id": 0, "start": 0.0, "end": 1.0, "text": "hello there", "avg_logprob": "not-a-number"},
                {"id": 1, "start": 1.0, "end": 2.0, "text": "more"}
            ]
        }"#,
        )
        .unwrap();
        let parsed = parse_groq_transcription_response(&response).expect("parses");
        assert!(
            parsed
                .words
                .iter()
                .all(|(_, confidence)| *confidence == 0.0)
        );
    }

    #[test]
    fn a_plain_json_response_degrades_to_text_without_evidence() {
        // A server that ignored response_format and returned the historic
        // plain shape must still deliver its text — the Groq flow degrades to
        // the pre-B4 no-evidence behavior instead of failing the Recording.
        let response: serde_json::Value =
            serde_json::from_str(r#"{"text": "deploy the cache migration today"}"#).unwrap();
        let parsed = parse_groq_transcription_response(&response).expect("parses");
        assert_eq!(parsed.text, "deploy the cache migration today");
        assert!(parsed.words.is_empty());
    }

    #[test]
    fn a_verbose_json_response_without_text_or_segments_is_rejected() {
        let response: serde_json::Value =
            serde_json::from_str(r#"{"task": "transcribe"}"#).unwrap();
        let error = parse_groq_transcription_response(&response).unwrap_err();
        assert_eq!(error.diagnostic(), "Groq response omitted text");
    }

    #[test]
    fn chunk_confidence_evidence_merges_with_the_same_overlap_skip() {
        // Two chunks whose texts share a 3-word seam: the merged confidence
        // list must skip the second chunk's first three entries exactly as
        // the text skip drops its first three words, so no seam word carries
        // doubled evidence.
        let first = GroqChunkTranscript {
            text: "deploy the cache migration".to_owned(),
            words: vec![
                ("deploy".to_owned(), 0.9),
                ("the".to_owned(), 0.9),
                ("cache".to_owned(), 0.2),
                ("migration".to_owned(), 0.9),
            ],
        };
        let second = GroqChunkTranscript {
            text: "cache migration finished today".to_owned(),
            words: vec![
                ("cache".to_owned(), 0.8),
                ("migration".to_owned(), 0.8),
                ("finished".to_owned(), 0.8),
                ("today".to_owned(), 0.8),
            ],
        };
        let merged = merge_chunk_transcripts(vec![first, second]);
        assert_eq!(merged.text, "deploy the cache migration finished today");
        assert_eq!(
            merged
                .words
                .iter()
                .map(|(word, _)| word.as_str())
                .collect::<Vec<_>>(),
            vec!["deploy", "the", "cache", "migration", "finished", "today"],
            "the seam's duplicated words do not double their evidence"
        );
        assert_eq!(
            merged.words[2].1, 0.2,
            "the FIRST chunk's seam confidence wins"
        );
    }

    #[tokio::test]
    async fn dropped_capture_retains_blocking_cleanup_until_reaper_drain() {
        let reaper = ProviderReaper::new();
        let entered = Arc::new(AtomicBool::new(false));
        let cleanup_done = Arc::new(AtomicBool::new(false));
        let (release, release_rx) = std::sync::mpsc::channel::<()>();
        let entered_task = Arc::clone(&entered);
        let cleanup_done_task = Arc::clone(&cleanup_done);
        let cleanup = tokio::task::spawn_blocking(move || {
            entered_task.store(true, Ordering::SeqCst);
            let _ = release_rx.recv();
            cleanup_done_task.store(true, Ordering::SeqCst);
            Ok(Vec::new())
        });
        wait_for(&entered).await;

        let capture = PipeWireActiveCapture {
            child: None,
            state: Arc::new(Mutex::new(CaptureReaderState {
                chunks: VecDeque::new(),
                received_bytes: 0,
                eof: true,
                error: None,
                buffer_cap_reached: false,
            })),
            reader: None,
            stderr_reader: None,
            cleanup: Some(cleanup),
            reaper: reaper.clone(),
            pcm: Vec::new(),
            started: Instant::now(),
            deadline: DEFAULT_RECORDING_DEADLINE,
        };

        // This is the state produced when an outer abort deadline drops
        // stop_child while its spawn_blocking cleanup still owns pw-record.
        drop(capture);
        assert_eq!(
            reaper.pending(),
            1,
            "capture cleanup must be retained instead of detached"
        );
        assert!(
            !cleanup_done.load(Ordering::SeqCst),
            "cleanup must still be live before the actor drains the reaper"
        );

        let _ = release.send(());
        assert!(
            reaper.drain(Duration::from_secs(2)).await,
            "the retained capture cleanup must drain before Idle"
        );
        assert!(
            cleanup_done.load(Ordering::SeqCst),
            "draining must await the blocking capture cleanup"
        );
    }

    #[tokio::test]
    async fn pipewire_buffer_cap_retains_the_exact_sample_aligned_prefix() {
        let mut emitted_pcm = Vec::new();
        for sample in 0_i16..3_520 {
            emitted_pcm.extend_from_slice(&(sample.saturating_add(64)).to_le_bytes());
        }
        let pcm_byte_cap = resolve_recording_maximum(Some("199".to_owned())).pcm_byte_cap;
        assert_eq!(pcm_byte_cap, 6_368);
        let retained_pcm = emitted_pcm[..pcm_byte_cap].to_vec();
        let state = Arc::new(Mutex::new(CaptureReaderState {
            chunks: VecDeque::new(),
            received_bytes: 0,
            eof: false,
            error: None,
            buffer_cap_reached: false,
        }));

        read_capture_stream(
            std::io::Cursor::new(emitted_pcm),
            Arc::clone(&state),
            None,
            pcm_byte_cap,
        );
        assert_eq!(
            state.lock().unwrap().chunks.len(),
            2,
            "the production reader must emit both complete chunks before reporting the cap"
        );

        let mut capture = PipeWireActiveCapture {
            child: None,
            state,
            reader: None,
            stderr_reader: None,
            cleanup: Some(tokio::spawn(async { Ok(Vec::new()) })),
            reaper: ProviderReaper::new(),
            pcm: Vec::new(),
            started: Instant::now(),
            deadline: DEFAULT_RECORDING_DEADLINE,
        };
        let first = capture
            .next_chunk()
            .await
            .expect("the first queued chunk is readable")
            .expect("the first queued chunk exists");
        assert_eq!(first.0, retained_pcm[..PCM_CHUNK_BYTES]);

        let audio = capture.finish().await.expect("cap-hit audio finalizes");
        assert_eq!(audio.pcm_s16le_mono_16khz(), retained_pcm);
        assert_eq!(audio.truncated_by(), Some(CaptureLimit::Buffer));
    }

    #[tokio::test]
    async fn pipewire_recording_deadline_retains_every_queued_byte() {
        // The Recording Deadline — not the byte cap — is the enforcer that fires
        // in production: wall clock from begin() always beats a byte counter fed
        // by a real-time microphone. next_chunk reports it WITHOUT popping the
        // queue, so the audio the user already spoke survives only because
        // finish() drains the queue AFTER stop_child joins the reader. That
        // coupling is the whole delivery guarantee on this path; this test pins
        // both halves of it.
        let mut spoken_pcm = Vec::new();
        let state = Arc::new(Mutex::new(CaptureReaderState {
            chunks: VecDeque::new(),
            received_bytes: 0,
            eof: false,
            error: None,
            buffer_cap_reached: false,
        }));
        for chunk in 0..2_usize {
            let pcm: Vec<u8> = (0..PCM_CHUNK_BYTES)
                .map(|byte| ((byte + chunk) % 251) as u8)
                .collect();
            spoken_pcm.extend_from_slice(&pcm);
            state.lock().unwrap().chunks.push_back(AudioChunk(pcm));
        }
        let mut capture = PipeWireActiveCapture {
            child: None,
            state,
            reader: None,
            stderr_reader: None,
            // The tail-pushing cleanup is installed further down, after the
            // Deadline has been reported: nothing may run concurrently with the
            // queue-length assertion below, so it cannot observe a late tail.
            cleanup: None,
            reaper: ProviderReaper::new(),
            pcm: Vec::new(),
            // A start backdated past the Deadline is the shape of a user who
            // dictated past the maximum and never released the Trigger Key. It
            // elapses structurally rather than by sleeping, so the report below
            // rests on no timing margin at all.
            started: Instant::now()
                .checked_sub(Duration::from_secs(1))
                .expect("the monotonic clock is at least a second past boot"),
            deadline: Duration::from_millis(1),
        };

        let error = capture
            .next_chunk()
            .await
            .expect_err("the elapsed Recording Deadline stops the pump");
        assert_eq!(error.kind(), BoundaryKind::RecordingDeadline);
        assert_eq!(
            capture.state.lock().unwrap().chunks.len(),
            2,
            "the Deadline must report without consuming the audio it hands to finish()"
        );

        // pw-record's reader thread flushes its final assembled chunk as it
        // reaches EOF, which stop_child's bounded join awaits: the tail reaches
        // the queue during finish(), not before it. Draining before the join
        // would leave this chunk behind. The 25 ms sleep is load-bearing — it
        // holds the tail out of the queue for the whole window a wrongly-early
        // drain_chunks would run in, so a swapped ordering fails instead of
        // passing on a coincidence.
        let tail: Vec<u8> = (0..PCM_CHUNK_BYTES).map(|byte| (byte % 97) as u8).collect();
        spoken_pcm.extend_from_slice(&tail);
        let tail_state = Arc::clone(&capture.state);
        capture.cleanup = Some(tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(25)).await;
            tail_state
                .lock()
                .unwrap()
                .chunks
                .push_back(AudioChunk(tail));
            Ok(Vec::new())
        }));

        let audio = capture
            .finish()
            .await
            .expect("deadline-hit audio finalizes instead of failing");
        assert_eq!(
            audio.pcm_s16le_mono_16khz(),
            spoken_pcm,
            "every queued byte the user spoke must survive the Recording Deadline"
        );
    }

    #[tokio::test]
    async fn dropped_capture_before_stop_retains_child_cleanup_until_reaper_drain() {
        // capture_pump can panic or be cancelled while still owning a live
        // pw-record before stop_child ever runs: child is Some, cleanup is None.
        // Drop must not merely kill-and-forget under reap_briefly's 250 ms — a
        // slow-exiting child would then outlive Drop while the reaper looks empty
        // and Idle is permitted mid-cleanup. It must hand a bounded kill/reap to
        // the reaper so the workflow drains it before acknowledging completion.
        let reaper = ProviderReaper::new();
        let child = Command::new("sleep")
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn a killable stand-in child");
        let pid: i32 = child.id().try_into().expect("child pid fits in pid_t");

        let capture = PipeWireActiveCapture {
            child: Some(child),
            state: Arc::new(Mutex::new(CaptureReaderState {
                chunks: VecDeque::new(),
                received_bytes: 0,
                eof: true,
                error: None,
                buffer_cap_reached: false,
            })),
            reader: None,
            stderr_reader: None,
            cleanup: None,
            reaper: reaper.clone(),
            pcm: Vec::new(),
            started: Instant::now(),
            deadline: DEFAULT_RECORDING_DEADLINE,
        };

        drop(capture);
        assert_eq!(
            reaper.pending(),
            1,
            "a pre-stop capture drop must retain its child cleanup in the reaper"
        );

        assert!(
            reaper.drain(Duration::from_secs(4)).await,
            "the retained pre-stop capture cleanup must drain before Idle"
        );
        // Draining awaited the bounded kill/reap, so the child is gone (reaped,
        // not a lingering zombie) — kill(pid, 0) can no longer find it.
        let alive = unsafe { libc::kill(pid, 0) } == 0;
        assert!(
            !alive,
            "draining must have killed and reaped the abandoned pw-record child"
        );
    }

    #[tokio::test]
    async fn a_dropped_finalize_request_is_owned_by_the_reaper_not_detached() {
        // The single full-audio request of a short Recording is issued at
        // finalize. If the Provider Deadline drops complete() while that request
        // is in flight, its curl child must be OWNED — handed to the
        // ProviderReaper by Drop — never detached. A local server that accepts
        // and never answers keeps the finalize request in flight.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let mut held = Vec::new();
            for stream in listener.incoming() {
                match stream {
                    Ok(connection) => held.push(connection),
                    Err(_) => break,
                }
            }
        });

        let reaper = ProviderReaper::new();
        let mut stream = GroqStream {
            credential: Credential::new("controlled-credential".to_owned()).unwrap(),
            endpoint: format!("http://{address}/audio/transcriptions"),
            params: test_groq_params(),
            buffer: Vec::new(),
            streamed_bytes: 0,
            chunks: VecDeque::new(),
            cancel: CancelRegistry::new(),
            reaper: reaper.clone(),
            word_confidence_evidence: Vec::new(),
        };

        // Drive complete() far enough to issue the finalize request, then drop it
        // exactly as the Provider Deadline would.
        {
            let completion = stream.complete(CapturedAudio::empty());
            assert!(
                tokio::time::timeout(Duration::from_millis(750), completion)
                    .await
                    .is_err(),
                "the hanging finalize request must not complete on its own"
            );
        }
        // Dropping the stream must adopt the still-live finalize task into the
        // reaper (cancel-first, then adopt), not detach its curl child. With the
        // finalize request awaited inline this count is zero.
        drop(stream);
        assert_eq!(
            reaper.pending(),
            1,
            "the finalize request handle must be owned by the reaper, not detached"
        );
        // Draining cancels the request and reaps its curl child.
        reaper.drain_to_completion(Duration::from_secs(5)).await;
    }

    /// A probe chunk task shaped like a real provider chunk: the outer async
    /// task awaits an inner `spawn_blocking` request. The blocking closure — the
    /// one holding a live curl child in production — waits for cancellation, then
    /// performs a kill-and-reap that the test releases explicitly, so the test
    /// can observe cleanup ownership at the exact instant the coordinator's error
    /// surfaces. `entered` proves the blocking task actually started; `reap_done`
    /// is the reap-completion latch that is set only after the child is reaped.
    struct BlockingChunkProbe {
        entered: Arc<AtomicBool>,
        reap_done: Arc<AtomicBool>,
        release: std::sync::mpsc::Sender<()>,
    }

    fn spawn_blocking_backed_chunk(
        cancel: Arc<CancelRegistry>,
    ) -> (
        tokio::task::JoinHandle<Result<GroqChunkTranscript, BoundaryError>>,
        BlockingChunkProbe,
    ) {
        let entered = Arc::new(AtomicBool::new(false));
        let reap_done = Arc::new(AtomicBool::new(false));
        let (release, release_rx) = std::sync::mpsc::channel::<()>();
        let entered_task = Arc::clone(&entered);
        let reap_done_task = Arc::clone(&reap_done);
        let handle = tokio::spawn(async move {
            tokio::task::spawn_blocking(move || {
                entered_task.store(true, Ordering::SeqCst);
                // Mirror an in-flight curl request owned by this blocking task:
                // run until the owning bounded wait observes cancellation.
                while !cancel.is_cancelled() {
                    std::thread::sleep(Duration::from_millis(1));
                }
                // Cancellation observed. The kill-and-reap of the child is gated
                // by the test so the reap deliberately outlasts the abort
                // deadline, forcing the coordinator down its timeout path while
                // the blocking work is still live.
                let _ = release_rx.recv();
                reap_done_task.store(true, Ordering::SeqCst);
                Err(BoundaryError::new(
                    BoundaryKind::Provider,
                    "request cancelled",
                ))
            })
            .await
            .map_err(|_| BoundaryError::new(BoundaryKind::Provider, "request task failed"))?
        });
        (
            handle,
            BlockingChunkProbe {
                entered,
                reap_done,
                release,
            },
        )
    }

    #[tokio::test]
    async fn provider_abort_deadline_retains_and_reaps_blocking_work_before_idle() {
        let reaper = ProviderReaper::new();
        let credential = Credential::new("controlled-credential".to_owned()).unwrap();
        let deepgram_cancel = CancelRegistry::new();
        let groq_cancel = CancelRegistry::new();
        let (deepgram_chunk, deepgram_probe) =
            spawn_blocking_backed_chunk(Arc::clone(&deepgram_cancel));
        let (groq_chunk, groq_probe) = spawn_blocking_backed_chunk(Arc::clone(&groq_cancel));
        // Shape the Deepgram websocket I/O task like a real one whose teardown
        // still owns nested blocking work when cancellation fires.
        let deepgram_io_task = tokio::spawn(async move {
            deepgram_chunk
                .await
                .map_err(|_| {
                    BoundaryError::new(BoundaryKind::Provider, "Deepgram streaming task failed")
                })?
                .map(|_| ())
        });
        let (deepgram_outbound, _deepgram_outbound_rx) = tokio::sync::mpsc::unbounded_channel();
        let streams = ProviderStreams {
            deepgram: Box::new(DeepgramStream {
                outbound: Some(deepgram_outbound),
                streamed_bytes: 0,
                io_tasks: VecDeque::from([deepgram_io_task]),
                transcript: Arc::new(Mutex::new(TranscriptAccumulator::default())),
                cancel: deepgram_cancel,
                shutdown: Arc::new(tokio::sync::Notify::new()),
                reaper: reaper.clone(),
            }),
            groq: Box::new(GroqStream {
                credential,
                endpoint: "http://localhost/groq".to_owned(),
                params: test_groq_params(),
                buffer: Vec::new(),
                streamed_bytes: 0,
                chunks: VecDeque::from([groq_chunk]),
                cancel: groq_cancel,
                reaper: reaper.clone(),
                word_confidence_evidence: Vec::new(),
            }),
        };

        // Both blocking requests must actually be executing inside spawn_blocking
        // before the deadline fires, so cleanup has real nested ownership to lose.
        wait_for(&deepgram_probe.entered).await;
        wait_for(&groq_probe.entered).await;

        let error = ProviderCoordinator::start(
            Duration::from_millis(10),
            Duration::from_millis(10),
            streams,
        )
        .complete(CapturedAudio::empty())
        .await
        .unwrap_err();
        assert_eq!(error.diagnostic(), "provider deadline cleanup timed out");

        // The moment the coordinator's cleanup-timeout error surfaces, the
        // blocking curl work is still live (its reap has not been released).
        // Publishing Idle here without draining would strand that live work.
        assert!(
            !deepgram_probe.reap_done.load(Ordering::SeqCst),
            "Deepgram curl reap must still be in flight when cleanup times out"
        );
        assert!(
            !groq_probe.reap_done.load(Ordering::SeqCst),
            "Groq curl reap must still be in flight when cleanup times out"
        );
        // Cleanup ownership was RETAINED by the actor-owned supervisor rather
        // than aborted and detached: with the detach defect this count is zero.
        assert_eq!(
            reaper.pending(),
            2,
            "both dropped streams must hand their curl reap to the supervisor"
        );

        // Release the reaps and drain the supervisor, exactly as the actor does
        // before it publishes Idle. Draining must await the retained reaper tasks
        // until the nested blocking work has actually completed its reap.
        let _ = deepgram_probe.release.send(());
        let _ = groq_probe.release.send(());
        assert!(
            reaper.drain(Duration::from_secs(2)).await,
            "the supervisor must fully drain within the bound"
        );
        assert!(
            deepgram_probe.reap_done.load(Ordering::SeqCst),
            "draining must not return until the Deepgram blocking reap completed"
        );
        assert!(
            groq_probe.reap_done.load(Ordering::SeqCst),
            "draining must not return until the Groq blocking reap completed"
        );
        assert_eq!(
            reaper.pending(),
            0,
            "a full drain must leave nothing retained"
        );
    }

    #[tokio::test]
    async fn stream_dropped_without_a_runtime_still_retains_its_blocking_cleanup() {
        // Runtime teardown (and any non-runtime thread) can drop a provider
        // stream where Handle::try_current() fails. Adoption must be synchronous
        // and runtime-free: the cleanup is retained for a later drain, never
        // aborted — aborting would detach the nested spawn_blocking curl reap.
        let reaper = ProviderReaper::new();
        let cancel = CancelRegistry::new();
        let (chunk, probe) = spawn_blocking_backed_chunk(Arc::clone(&cancel));
        wait_for(&probe.entered).await;
        let stream = GroqStream {
            credential: Credential::new("controlled-credential".to_owned()).unwrap(),
            endpoint: "http://localhost/groq".to_owned(),
            params: test_groq_params(),
            buffer: Vec::new(),
            streamed_bytes: 0,
            chunks: VecDeque::from([chunk]),
            cancel,
            reaper: reaper.clone(),
            word_confidence_evidence: Vec::new(),
        };
        std::thread::spawn(move || drop(stream))
            .join()
            .expect("dropping a stream off the runtime must not panic");
        assert_eq!(
            reaper.pending(),
            1,
            "a stream dropped without a runtime must still retain its cleanup"
        );
        let _ = probe.release.send(());
        assert!(
            reaper.drain(Duration::from_secs(2)).await,
            "the retained cleanup must drain once released"
        );
        assert!(
            probe.reap_done.load(Ordering::SeqCst),
            "draining must await the blocking reap to completion"
        );
    }

    #[tokio::test]
    async fn concurrent_drains_never_report_completion_over_live_cleanup() {
        // While one drain temporarily holds cleanup futures out of the
        // supervisor, a concurrent drain must serialize behind it — never
        // observe an empty supervisor and report a completed drain while the
        // blocking reap is still running.
        let reaper = ProviderReaper::new();
        let cancel = CancelRegistry::new();
        cancel.cancel();
        let (chunk, probe) = spawn_blocking_backed_chunk(Arc::clone(&cancel));
        wait_for(&probe.entered).await;
        let mut chunks = VecDeque::new();
        chunks.push_back(chunk);
        reaper.adopt(chunks);

        let first = tokio::spawn({
            let reaper = reaper.clone();
            async move { reaper.drain(Duration::from_secs(2)).await }
        });
        // Give the first drain time to take the cleanup batch out of the
        // supervisor before the concurrent drain starts.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let second = tokio::spawn({
            let reaper = reaper.clone();
            let reap_done = Arc::clone(&probe.reap_done);
            async move {
                let drained = reaper.drain(Duration::from_secs(2)).await;
                (drained, reap_done.load(Ordering::SeqCst))
            }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        let _ = probe.release.send(());

        assert!(
            first.await.expect("first drain must not panic"),
            "the first drain must complete once the reap is released"
        );
        let (second_drained, reap_done_when_second_returned) =
            second.await.expect("second drain must not panic");
        assert!(second_drained, "the concurrent drain must also complete");
        assert!(
            reap_done_when_second_returned,
            "a concurrent drain must not report completion while the blocking reap runs"
        );
    }

    #[tokio::test]
    async fn drain_to_completion_survives_pass_timeouts_without_detaching_cleanup() {
        // A teardown path whose single bounded drain times out would retain the
        // unfinished cleanup only to drop it with the runtime immediately
        // after. drain_to_completion must keep draining across pass timeouts
        // and return only once the blocking reap has actually completed.
        let reaper = ProviderReaper::new();
        let cancel = CancelRegistry::new();
        cancel.cancel();
        let (chunk, probe) = spawn_blocking_backed_chunk(Arc::clone(&cancel));
        wait_for(&probe.entered).await;
        reaper.adopt(VecDeque::from([chunk]));

        // Release the reap well after several 50ms passes have timed out.
        let release = probe.release.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            let _ = release.send(());
        });
        reaper.drain_to_completion(Duration::from_millis(50)).await;
        assert!(
            probe.reap_done.load(Ordering::SeqCst),
            "drain_to_completion must not return before the blocking reap completed"
        );
        assert_eq!(
            reaper.pending(),
            0,
            "a completed teardown drain must leave nothing retained"
        );
    }

    /// Spins until an `entered` latch is set, bounded so a genuine failure to
    /// enter spawn_blocking surfaces as a timeout rather than a hang.
    async fn wait_for(flag: &Arc<AtomicBool>) {
        tokio::time::timeout(Duration::from_secs(2), async {
            while !flag.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("probe blocking task must enter spawn_blocking");
    }

    #[test]
    fn transcript_accumulator_assembles_only_finalized_results_in_order() {
        let mut accumulator = TranscriptAccumulator::default();
        // Interim revision of a window that a later final supersedes.
        accumulator.ingest(&serde_json::json!({
            "type": "Results", "is_final": false,
            "channel": {"alternatives": [{"transcript": "the quick brown"}]}
        }));
        accumulator.ingest(&serde_json::json!({
            "type": "Results", "is_final": true,
            "channel": {"alternatives": [{"transcript": "the quick brown fox"}]}
        }));
        // Non-Results, whitespace-only finals, and shape drift are ignored.
        accumulator.ingest(&serde_json::json!({"type": "Metadata"}));
        accumulator.ingest(&serde_json::json!({
            "type": "Results", "is_final": true,
            "channel": {"alternatives": [{"transcript": "   "}]}
        }));
        accumulator.ingest(&serde_json::json!({"type": "Results", "is_final": true}));
        accumulator.ingest(&serde_json::json!({
            "type": "Results", "is_final": true, "speech_final": true,
            "channel": {"alternatives": [{"transcript": "jumps over."}]}
        }));
        assert_eq!(accumulator.text(), "the quick brown fox jumps over.");
    }

    #[test]
    fn transcript_accumulator_keeps_word_confidences_from_finals_only() {
        let mut accumulator = TranscriptAccumulator::default();
        // An interim revision's words are superseded and must never be kept.
        accumulator.ingest(&serde_json::json!({
            "type": "Results", "is_final": false,
            "channel": {"alternatives": [{"transcript": "the quick brown",
                "words": [{"word": "the", "confidence": 0.1}]}]}
        }));
        accumulator.ingest(&serde_json::json!({
            "type": "Results", "is_final": true,
            "channel": {"alternatives": [{"transcript": "the quick brown fox",
                "words": [
                    {"word": "the", "confidence": 0.98},
                    {"word": "quick", "confidence": 0.31},
                    {"word": "brown", "confidence": 0.87},
                    {"word": "fox", "confidence": 0.99}
                ]}]}
        }));
        // A final segment without a words array contributes no evidence.
        accumulator.ingest(&serde_json::json!({
            "type": "Results", "is_final": true,
            "channel": {"alternatives": [{"transcript": "jumps over."}]}
        }));
        // A word without a numeric confidence is carried as 0.0 (unproven,
        // never confidently transcribed), and a wordless entry is skipped.
        // Confidences are clamped to the [0, 1] domain at ingest.
        accumulator.ingest(&serde_json::json!({
            "type": "Results", "is_final": true,
            "channel": {"alternatives": [{"transcript": "now",
                "words": [{"word": "now"}, {"confidence": 0.9}, {"word": "", "confidence": 1.0},
                          {"word": "big", "confidence": 1.7}, {"word": "tiny", "confidence": -0.5}]}]}
        }));

        assert_eq!(
            accumulator.words(),
            vec![
                ("the".to_owned(), 0.98),
                ("quick".to_owned(), 0.31),
                ("brown".to_owned(), 0.87),
                ("fox".to_owned(), 0.99),
                ("now".to_owned(), 0.0),
                ("big".to_owned(), 1.0),
                ("tiny".to_owned(), 0.0),
            ]
        );
    }

    #[test]
    fn deepgram_streaming_url_carries_nova3_params_and_repeated_encoded_keyterms() {
        let url = deepgram_streaming_url(
            "wss://api.deepgram.com/v1/listen",
            &[
                "Voisu".to_owned(),
                "smart format".to_owned(),
                "  ".to_owned(),
            ],
            "en",
        )
        .unwrap();
        assert!(
            url.starts_with("wss://api.deepgram.com/v1/listen?"),
            "{url}"
        );
        for expected in [
            "model=nova-3",
            "language=en",
            "encoding=linear16",
            "sample_rate=16000",
            "channels=1",
            "interim_results=true",
            "smart_format=true",
            "punctuate=true",
            "endpointing=300",
            "utterance_end_ms=1000",
        ] {
            assert!(url.contains(expected), "{url} is missing {expected}");
        }
        assert!(url.contains("keyterm=Voisu"), "{url}");
        assert!(url.contains("keyterm=smart%20format"), "{url}");
        assert_eq!(
            url.matches("keyterm=").count(),
            2,
            "blank keyterms must be dropped: {url}"
        );
    }

    // Slice B6: the unset-environment default ("en") must produce a
    // BYTE-IDENTICAL streaming URL to the pre-B6 fixed param list — same
    // values, same order.
    #[test]
    fn deepgram_streaming_url_default_language_is_byte_identical_to_the_pre_b6_request() {
        let url = deepgram_streaming_url("wss://api.deepgram.com/v1/listen", &[], "en").unwrap();
        assert_eq!(
            url,
            "wss://api.deepgram.com/v1/listen?model=nova-3&language=en&encoding=linear16\
             &sample_rate=16000&channels=1&interim_results=true&smart_format=true\
             &punctuate=true&endpointing=300&utterance_end_ms=1000"
        );
    }

    // A resolved non-English language rides the same query slot, and provider
    // codes pass through verbatim — BCP-47 region tags and Deepgram's
    // multi-language code alike.
    #[test]
    fn deepgram_streaming_url_carries_the_resolved_transcription_language() {
        for language in ["de-de", "es-419", "multi"] {
            let url =
                deepgram_streaming_url("wss://api.deepgram.com/v1/listen", &[], language).unwrap();
            assert!(
                url.contains(&format!("&language={language}&")),
                "{url} is missing language={language}"
            );
        }
    }

    // An EMPTY dictionary must produce a byte-identical request: no keyterm
    // parameter at all, so the provider sees exactly the pre-B2 request.
    #[test]
    fn deepgram_streaming_url_with_no_keyterms_omits_the_parameter_entirely() {
        let plain = deepgram_streaming_url("wss://api.deepgram.com/v1/listen", &[], "en").unwrap();
        assert!(!plain.contains("keyterm="), "{plain}");
        // Blank terms are dropped by the URL builder itself (mirroring the
        // persisted-dictionary parser, which can never yield an empty term):
        // even a caller handing it blanks keeps the request byte-identical.
        let blanks = deepgram_streaming_url(
            "wss://api.deepgram.com/v1/listen",
            &vec![String::new(); 3],
            "en",
        )
        .unwrap();
        assert_eq!(plain, blanks, "blank keyterms must change nothing");
    }

    // The DAEMON wiring caps the keyterms through `deepgram_keyterms` before
    // the URL builder sees them; through that seam the streaming request never
    // exceeds Deepgram's count and token budgets.
    #[test]
    fn deepgram_streaming_url_through_the_keyterm_cap_never_exceeds_the_budgets() {
        let terms: Vec<String> = (0..600).map(|index| format!("term{index:03}")).collect();
        let capped = crate::dictionary::deepgram_keyterms(&terms);
        let url =
            deepgram_streaming_url("wss://api.deepgram.com/v1/listen", &capped, "en").unwrap();
        assert_eq!(
            url.matches("keyterm=").count(),
            capped.len(),
            "every capped term is sent exactly once: {url}"
        );
        assert!(capped.len() <= crate::dictionary::DEEPGRAM_KEYTERM_COUNT_LIMIT);
    }

    #[test]
    fn deepgram_streaming_url_rewrites_http_schemes_and_requires_wss_off_loopback() {
        assert!(
            deepgram_streaming_url("https://api.deepgram.com/v1/listen", &[], "en")
                .unwrap()
                .starts_with("wss://api.deepgram.com/v1/listen?")
        );
        assert!(
            deepgram_streaming_url("http://127.0.0.1:9999/v1/listen", &[], "en")
                .unwrap()
                .starts_with("ws://127.0.0.1:9999/v1/listen?")
        );
        assert!(deepgram_streaming_url("ws://deepgram.test/v1/listen", &[], "en").is_err());
        assert!(deepgram_streaming_url("http://deepgram.test/v1/listen", &[], "en").is_err());
        // The loopback decision is the PARSED host: the whole 127.0.0.0/8 range
        // is loopback, and a lookalike suffix is a different host entirely.
        assert!(deepgram_streaming_url("ws://127.7.7.7:9/v1/listen", &[], "en").is_ok());
        assert!(
            deepgram_streaming_url("ws://localhost.attacker.example/v1/listen", &[], "en").is_err()
        );
        // A base that already carries a query keeps it and appends with '&'.
        let url = deepgram_streaming_url("wss://host/listen?tier=custom", &[], "en").unwrap();
        assert!(url.contains("?tier=custom&model=nova-3"), "{url}");
    }

    async fn mock_deepgram_listener() -> (tokio::net::TcpListener, String) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("ws://{}/v1/listen", listener.local_addr().unwrap());
        (listener, base)
    }

    fn test_deepgram_stream(
        base: &str,
        keepalive: Duration,
        reaper: &ProviderReaper,
    ) -> DeepgramStream {
        test_deepgram_stream_with_grace(base, keepalive, DEEPGRAM_CLOSE_GRACE, reaper)
    }

    fn test_deepgram_stream_with_grace(
        base: &str,
        keepalive: Duration,
        close_grace: Duration,
        reaper: &ProviderReaper,
    ) -> DeepgramStream {
        DeepgramStream::connect(
            deepgram_streaming_url(base, &[], "en").unwrap(),
            Credential::new("controlled-credential".to_owned()).unwrap(),
            keepalive,
            close_grace,
            reaper.clone(),
        )
    }

    /// The terminal summary message Deepgram sends after `CloseStream`, before
    /// closing the connection.
    fn deepgram_metadata_frame() -> String {
        serde_json::json!({"type": "Metadata", "request_id": "mock-request"}).to_string()
    }

    fn deepgram_results_frame(transcript: &str, is_final: bool) -> String {
        serde_json::json!({
            "type": "Results",
            "is_final": is_final,
            "speech_final": is_final,
            "channel": {"alternatives": [{"transcript": transcript}]}
        })
        .to_string()
    }

    #[tokio::test]
    // The tungstenite accept_hdr callback's Err type is the crate's ~136-byte
    // http::Response — fixed by the third-party signature, not shrinkable here.
    #[allow(clippy::result_large_err)]
    async fn deepgram_streams_binary_audio_and_returns_only_finalized_segments() {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;
        use tokio_tungstenite::tungstenite::handshake::server::{
            Request as WsRequest, Response as WsResponse,
        };

        let (listener, base) = mock_deepgram_listener().await;
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let handshake: Arc<Mutex<Option<(String, String)>>> = Arc::default();
            let capture = Arc::clone(&handshake);
            let mut socket = tokio_tungstenite::accept_hdr_async(
                tcp,
                move |request: &WsRequest, response: WsResponse| {
                    let authorization = request
                        .headers()
                        .get("authorization")
                        .and_then(|value| value.to_str().ok())
                        .unwrap_or_default()
                        .to_owned();
                    *capture.lock().unwrap() = Some((request.uri().to_string(), authorization));
                    Ok(response)
                },
            )
            .await
            .unwrap();
            let mut audio: Vec<u8> = Vec::new();
            let mut control: Vec<String> = Vec::new();
            let mut interim_sent = false;
            while let Some(message) = socket.next().await {
                match message.unwrap() {
                    Message::Binary(bytes) => {
                        audio.extend_from_slice(&bytes);
                        if !interim_sent {
                            interim_sent = true;
                            socket
                                .send(Message::Text(deepgram_results_frame(
                                    "this interim revision must never reach the Transcript",
                                    false,
                                )))
                                .await
                                .unwrap();
                        }
                    }
                    Message::Text(text) => {
                        let closing = text.contains("CloseStream");
                        control.push(text);
                        if closing {
                            socket
                                .send(Message::Text(deepgram_results_frame("Hello world.", true)))
                                .await
                                .unwrap();
                            socket
                                .send(Message::Text(deepgram_results_frame(
                                    "Second segment.",
                                    true,
                                )))
                                .await
                                .unwrap();
                            socket
                                .send(Message::Text(deepgram_metadata_frame()))
                                .await
                                .unwrap();
                            let _ = socket.send(Message::Close(None)).await;
                            break;
                        }
                    }
                    Message::Close(_) => break,
                    _ => {}
                }
            }
            let captured = handshake.lock().unwrap().clone();
            (captured, audio, control)
        });

        let reaper = ProviderReaper::new();
        let mut stream = test_deepgram_stream(&base, Duration::from_secs(5), &reaper);
        let mut pcm = Vec::new();
        for chunk in [vec![1u8; 64], vec![2u8; 64]] {
            pcm.extend_from_slice(&chunk);
            stream.send_audio(AudioChunk(chunk)).await.unwrap();
        }
        // An un-streamed tail that complete() must top up before Finalize.
        pcm.extend_from_slice(&[3u8; 32]);
        let transcript = stream
            .complete(CapturedAudio::new(pcm.clone()))
            .await
            .unwrap();
        assert_eq!(transcript.provider, Provider::Deepgram);
        assert_eq!(transcript.text, "Hello world. Second segment.");

        let (captured, audio, control) = server.await.unwrap();
        let (uri, authorization) = captured.expect("handshake must be captured");
        assert_eq!(authorization, "Token controlled-credential");
        assert!(uri.contains("model=nova-3"), "{uri}");
        assert!(uri.contains("interim_results=true"), "{uri}");
        assert_eq!(audio, pcm, "every PCM byte must arrive as binary frames");
        assert!(
            control.iter().any(|text| text.contains("\"Finalize\"")),
            "{control:?}"
        );
        assert!(
            control.iter().any(|text| text.contains("\"CloseStream\"")),
            "{control:?}"
        );
        assert_eq!(reaper.pending(), 0);
    }

    #[tokio::test]
    async fn deepgram_connection_lost_mid_recording_fails_the_provider_visibly() {
        use futures_util::StreamExt;

        let (listener, base) = mock_deepgram_listener().await;
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(tcp).await.unwrap();
            // Take the first frame, then drop both the connection and the
            // listener so every redial within the bounded budget is refused.
            let _ = socket.next().await;
            drop(socket);
            drop(listener);
        });

        let reaper = ProviderReaper::new();
        let mut stream = test_deepgram_stream(&base, Duration::from_secs(5), &reaper);
        stream.send_audio(AudioChunk(vec![0u8; 32])).await.unwrap();
        server.await.unwrap();
        let error = stream
            .complete(CapturedAudio::new(vec![0u8; 32]))
            .await
            .unwrap_err();
        assert!(
            error.diagnostic().contains("Deepgram"),
            "a mid-Recording drop must surface a visible provider error, got {:?}",
            error.diagnostic()
        );
    }

    #[tokio::test]
    async fn deepgram_server_error_message_fails_the_provider_visibly() {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;

        let (listener, base) = mock_deepgram_listener().await;
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(tcp).await.unwrap();
            let _ = socket.next().await;
            socket
                .send(Message::Text(
                    r#"{"type":"Error","description":"rejected"}"#.to_owned(),
                ))
                .await
                .unwrap();
            // Hold the connection open; the client must fail on the Error
            // message itself, not on a transport drop.
            while let Some(Ok(message)) = socket.next().await {
                if matches!(message, Message::Close(_)) {
                    break;
                }
            }
        });

        let reaper = ProviderReaper::new();
        let mut stream = test_deepgram_stream(&base, Duration::from_secs(5), &reaper);
        stream.send_audio(AudioChunk(vec![0u8; 32])).await.unwrap();
        let error = stream
            .complete(CapturedAudio::new(vec![0u8; 32]))
            .await
            .unwrap_err();
        assert_eq!(error.diagnostic(), "Deepgram reported a streaming error");
        server.abort();
    }

    #[tokio::test]
    async fn deepgram_drop_after_delivered_audio_fails_visibly_without_redialing() {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;

        let (listener, base) = mock_deepgram_listener().await;
        let redialed = Arc::new(AtomicBool::new(false));
        let redialed_flag = Arc::clone(&redialed);
        let server = tokio::spawn(async move {
            // Finalize one segment for delivered audio, then drop abruptly.
            let (tcp, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(tcp).await.unwrap();
            let _ = socket.next().await;
            socket
                .send(Message::Text(deepgram_results_frame("schedule the", true)))
                .await
                .unwrap();
            drop(socket);
            // Keep listening: a redial WOULD succeed here, so a pass proves the
            // client refused to redial rather than that it couldn't.
            if tokio::time::timeout(Duration::from_secs(1), listener.accept())
                .await
                .is_ok()
            {
                redialed_flag.store(true, Ordering::SeqCst);
            }
        });

        let reaper = ProviderReaper::new();
        let mut stream = test_deepgram_stream(&base, Duration::from_secs(5), &reaper);
        stream.send_audio(AudioChunk(vec![7u8; 32])).await.unwrap();
        // Wait until the finalized segment was ingested, then give a
        // redial-and-continue implementation time to observe the drop and
        // redial BEFORE the Recording completes — the exact window where a
        // silent audio gap would hide.
        tokio::time::timeout(Duration::from_secs(2), async {
            while stream.transcript.lock().unwrap().text() != "schedule the" {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the finalized segment must be ingested");
        tokio::time::sleep(Duration::from_millis(600)).await;
        // Audio already accepted by the dropped socket cannot be replayed, so a
        // redial-and-continue would return a plausible Transcript with a silent
        // gap. The provider must fail visibly instead; Groq carries.
        let error = stream
            .complete(CapturedAudio::new(vec![7u8; 32]))
            .await
            .unwrap_err();
        assert!(
            error.diagnostic().contains("Deepgram"),
            "got {:?}",
            error.diagnostic()
        );
        let _ = server.await;
        assert!(
            !redialed.load(Ordering::SeqCst),
            "a drop after delivered audio must not be redialed"
        );
    }

    #[tokio::test]
    async fn deepgram_redials_a_failed_dial_before_any_audio_within_the_budget() {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;

        let (listener, base) = mock_deepgram_listener().await;
        let (second_up_tx, second_up_rx) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(async move {
            // First connection dies before any audio was delivered on it.
            let (tcp, _) = listener.accept().await.unwrap();
            let socket = tokio_tungstenite::accept_async(tcp).await.unwrap();
            drop(socket);
            // Second connection: the bounded redial carries the Recording.
            let (tcp, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(tcp).await.unwrap();
            let _ = second_up_tx.send(());
            while let Some(Ok(message)) = socket.next().await {
                if let Message::Text(text) = message
                    && text.contains("CloseStream")
                {
                    socket
                        .send(Message::Text(deepgram_results_frame("after redial", true)))
                        .await
                        .unwrap();
                    socket
                        .send(Message::Text(deepgram_metadata_frame()))
                        .await
                        .unwrap();
                    let _ = socket.send(Message::Close(None)).await;
                    break;
                }
            }
        });

        let reaper = ProviderReaper::new();
        let mut stream = test_deepgram_stream(&base, Duration::from_secs(5), &reaper);
        // No audio has been handed to the first connection, so nothing can be
        // lost: the dial phase stays covered by the bounded reconnect budget.
        second_up_rx.await.unwrap();
        stream.send_audio(AudioChunk(vec![7u8; 32])).await.unwrap();
        let transcript = stream
            .complete(CapturedAudio::new(vec![7u8; 32]))
            .await
            .unwrap();
        assert_eq!(transcript.text, "after redial");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn deepgram_close_without_terminal_metadata_fails_visibly() {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;

        let (listener, base) = mock_deepgram_listener().await;
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(tcp).await.unwrap();
            while let Some(Ok(message)) = socket.next().await {
                if let Message::Text(text) = message
                    && text.contains("CloseStream")
                {
                    // A final Results but NO terminal Metadata before the
                    // close: the server-side flush is unconfirmed, so the
                    // Transcript may be truncated.
                    socket
                        .send(Message::Text(deepgram_results_frame("truncated", true)))
                        .await
                        .unwrap();
                    let _ = socket.send(Message::Close(None)).await;
                    break;
                }
            }
        });

        let reaper = ProviderReaper::new();
        let mut stream = test_deepgram_stream(&base, Duration::from_secs(5), &reaper);
        stream.send_audio(AudioChunk(vec![5u8; 32])).await.unwrap();
        let error = stream
            .complete(CapturedAudio::new(vec![5u8; 32]))
            .await
            .unwrap_err();
        assert!(
            error.diagnostic().contains("Deepgram"),
            "a close without the terminal Metadata must fail visibly, got {:?}",
            error.diagnostic()
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn deepgram_unanswered_closestream_fails_visibly_at_the_close_grace() {
        use futures_util::StreamExt;
        use tokio_tungstenite::tungstenite::Message;

        let (listener, base) = mock_deepgram_listener().await;
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(tcp).await.unwrap();
            // Read everything, never answer CloseStream.
            while let Some(Ok(message)) = socket.next().await {
                if matches!(message, Message::Close(_)) {
                    break;
                }
            }
        });

        let reaper = ProviderReaper::new();
        let mut stream = test_deepgram_stream_with_grace(
            &base,
            Duration::from_secs(5),
            Duration::from_millis(200),
            &reaper,
        );
        stream.send_audio(AudioChunk(vec![4u8; 32])).await.unwrap();
        // Returning the partial accumulator here would deliver a plausible but
        // truncated Source Transcript inside the 14s Provider Deadline.
        let error = stream
            .complete(CapturedAudio::new(vec![4u8; 32]))
            .await
            .unwrap_err();
        assert!(
            error.diagnostic().contains("Deepgram"),
            "an unanswered CloseStream must fail visibly at the close grace, got {:?}",
            error.diagnostic()
        );
        drop(stream);
        let _ = reaper.drain(Duration::from_secs(2)).await;
        let _ = server.await;
    }

    #[test]
    fn deepgram_streaming_url_rejects_userinfo_in_the_authority() {
        // ws://127.0.0.1:80@attacker.example/… — the raw authority starts with
        // a loopback-looking userinfo but the HOST is attacker.example; the
        // Token header must never travel over that connection (let alone in
        // plaintext).
        assert!(
            deepgram_streaming_url("ws://127.0.0.1:80@attacker.example/listen", &[], "en").is_err()
        );
        assert!(
            deepgram_streaming_url("http://localhost@attacker.example/listen", &[], "en").is_err()
        );
        assert!(
            deepgram_streaming_url("wss://user@api.deepgram.com/v1/listen", &[], "en").is_err()
        );
        assert!(deepgram_streaming_url("ws:///listen", &[], "en").is_err());
        // The EMPTY userinfo form parses with no username and no password (the
        // url crate drops the `@` entirely), so the raw authority is what sees it.
        assert!(deepgram_streaming_url("ws://@localhost/listen", &[], "en").is_err());
        // A `\` ends the url crate's userinfo scan but is accepted inside
        // curl's last-`@` split: fail closed on the raw string.
        assert!(
            deepgram_streaming_url("ws://localhost:8080\\@attacker.example/listen", &[], "en")
                .is_err()
        );
    }

    #[test]
    fn provider_endpoint_gate_parses_the_url_instead_of_prefix_matching() {
        let secure = |endpoint: &str| provider_endpoint_url(endpoint).is_some();
        assert!(secure(
            "https://api.groq.com/openai/v1/audio/transcriptions"
        ));
        assert!(secure("http://localhost:8080/transcribe"));
        assert!(secure("http://127.0.0.1:9999/transcribe"));
        assert!(secure("http://127.7.7.7/transcribe"));
        assert!(!secure("http://attacker.example/transcribe"));
        // Userinfo smuggling: both of these passed the old prefix checks — any
        // `https://…` passed, and `localhost:8080@…` starts with `localhost:` —
        // while the real parsed host is attacker.example.
        assert!(!secure(
            "https://user:pass@api.groq.com@attacker.example/transcribe"
        ));
        assert!(!secure("http://localhost:8080@attacker.example/transcribe"));
        // Exact host comparison: a lookalike suffix is a different host.
        assert!(!secure("http://localhost.attacker.example/transcribe"));
        assert!(!secure("not a url"));
    }

    #[test]
    fn provider_endpoint_gate_rejects_backslash_and_stripped_control_characters() {
        let secure = |endpoint: &str| provider_endpoint_url(endpoint).is_some();
        // The url crate stops its userinfo/host scan at `\` for special schemes
        // while curl splits userinfo at the LAST `@` and accepts `\` inside it:
        // this shape gates as loopback-without-userinfo yet connects to
        // attacker.example, so the raw string fails closed before parsing.
        assert!(!secure(
            "http://localhost:8080\\@attacker.example/transcribe"
        ));
        assert!(!secure(
            "https://api.groq.com\\@attacker.example/transcribe"
        ));
        // The URL parser silently strips tab (and newline) before validating:
        // `localho\tst` must not gate as `localhost`, and `\0` has no place in
        // an endpoint.
        assert!(!secure("http://localho\tst/transcribe"));
        assert!(!secure("http://localhost/transcribe\r"));
        assert!(!secure("http://localhost\0/transcribe"));
    }

    #[test]
    fn provider_endpoint_gate_rejects_empty_userinfo() {
        let secure = |endpoint: &str| provider_endpoint_url(endpoint).is_some();
        // `http://@localhost/` parses with username()=="" and no password —
        // indistinguishable from no userinfo at the parsed level, and the url
        // crate drops the `@` from its serialization. The raw-authority gate
        // sees it: any `@` in the authority is userinfo.
        assert!(!secure("http://@localhost/transcribe"));
        assert!(!secure("http://:pass@localhost/transcribe"));
        assert!(!secure("http://user@localhost/transcribe"));
        // An `@` in the path or query is not userinfo.
        assert!(secure(
            "https://api.groq.com/openai/v1/audio/transcriptions?at=@here"
        ));
    }

    #[test]
    fn groq_curl_paths_hand_over_the_gated_serialization() {
        // A parseable endpoint is handed over exactly as the url crate
        // serialized it — the string the policy verified is the string curl
        // receives, so curl's looser last-`@`/`\` authority reading can never
        // reinterpret it.
        let handed = provider_endpoint_url("https://api.groq.com/openai/v1/audio/transcriptions")
            .unwrap()
            .as_str()
            .to_owned();
        assert_eq!(
            handed,
            "https://api.groq.com/openai/v1/audio/transcriptions"
        );
        let loopback = provider_endpoint_url("http://localhost:8080/transcribe")
            .unwrap()
            .as_str()
            .to_owned();
        assert_eq!(loopback, "http://localhost:8080/transcribe");
        assert!(!loopback.contains('\\'));
        // Even a backslash that survived into a parse (had the raw gate ever
        // been bypassed) is serialized out of the authority: a `\`-terminated
        // authority becomes a `/@…` PATH, which curl treats as a path.
        let path_borne = url::Url::parse("https://api.groq.com/openai\\@attacker.example/x")
            .unwrap()
            .as_str()
            .to_owned();
        assert!(!path_borne.contains('\\'), "{path_borne}");
        assert!(
            path_borne.starts_with("https://api.groq.com/")
                && path_borne.contains("/@attacker.example/x"),
            "{path_borne}"
        );
    }

    #[tokio::test]
    async fn deepgram_abort_surfaces_a_stored_streaming_failure() {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;

        let (listener, base) = mock_deepgram_listener().await;
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(tcp).await.unwrap();
            let _ = socket.next().await;
            socket
                .send(Message::Text(
                    r#"{"type":"Error","description":"rejected"}"#.to_owned(),
                ))
                .await
                .unwrap();
            while let Some(Ok(message)) = socket.next().await {
                if matches!(message, Message::Close(_)) {
                    break;
                }
            }
        });

        let reaper = ProviderReaper::new();
        let mut stream = test_deepgram_stream(&base, Duration::from_secs(5), &reaper);
        stream.send_audio(AudioChunk(vec![3u8; 32])).await.unwrap();
        // Wait until the I/O task stored the failure, mirroring a Recording
        // whose capture fails (or whose Recording Deadline fires) after
        // Deepgram already failed: abort(), not complete(), is what runs.
        tokio::time::timeout(Duration::from_secs(2), async {
            while !stream.io_tasks.front().unwrap().is_finished() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the streaming task must settle its failure");
        let error = Box::new(stream).abort().await.unwrap_err();
        assert_eq!(
            error.diagnostic(),
            "Deepgram reported a streaming error",
            "abort must surface the stored provider failure, not discard it"
        );
        server.abort();
    }

    #[test]
    fn malformed_deepgram_text_frames_fail_visibly() {
        let transcript = Arc::new(Mutex::new(TranscriptAccumulator::default()));
        // Not JSON at all.
        assert!(ingest_deepgram_message(&transcript, "not-json").is_err());
        // A Results frame with no is_final marker.
        assert!(ingest_deepgram_message(&transcript, r#"{"type":"Results"}"#).is_err());
        // A finalized Results frame whose transcript text is missing: skipping
        // it would silently truncate the Transcript.
        assert!(
            ingest_deepgram_message(
                &transcript,
                r#"{"type":"Results","is_final":true,"speech_final":true}"#
            )
            .is_err()
        );
        // Unknown message types stay tolerated (server-side schema additions),
        // and interim shape drift is UI-only.
        assert!(ingest_deepgram_message(&transcript, r#"{"type":"SpeechStarted"}"#).is_ok());
        assert!(
            ingest_deepgram_message(&transcript, r#"{"type":"Results","is_final":false}"#).is_ok()
        );
        assert_eq!(
            transcript.lock().unwrap().text(),
            "",
            "no rejected frame may leak text into the Transcript"
        );
    }

    #[tokio::test]
    async fn deepgram_malformed_final_results_fail_the_provider_visibly() {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;

        let (listener, base) = mock_deepgram_listener().await;
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(tcp).await.unwrap();
            let _ = socket.next().await;
            // A malformed finalized Results frame, followed by a perfectly
            // clean drain: only frame-level strictness can catch this.
            socket
                .send(Message::Text(
                    r#"{"type":"Results","is_final":true}"#.to_owned(),
                ))
                .await
                .unwrap();
            while let Some(Ok(message)) = socket.next().await {
                match message {
                    Message::Text(text) if text.contains("CloseStream") => {
                        let _ = socket.send(Message::Text(deepgram_metadata_frame())).await;
                        let _ = socket.send(Message::Close(None)).await;
                        break;
                    }
                    Message::Close(_) => break,
                    _ => {}
                }
            }
        });

        let reaper = ProviderReaper::new();
        let mut stream = test_deepgram_stream(&base, Duration::from_secs(5), &reaper);
        stream.send_audio(AudioChunk(vec![2u8; 32])).await.unwrap();
        let error = stream
            .complete(CapturedAudio::new(vec![2u8; 32]))
            .await
            .unwrap_err();
        assert!(
            error.diagnostic().contains("Deepgram"),
            "a malformed finalized Results frame must fail visibly, got {:?}",
            error.diagnostic()
        );
        let _ = server.await;
    }

    #[tokio::test]
    async fn deepgram_sends_keepalive_text_frames_during_outbound_gaps() {
        use futures_util::StreamExt;
        use tokio_tungstenite::tungstenite::Message;

        let (listener, base) = mock_deepgram_listener().await;
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(tcp).await.unwrap();
            loop {
                match socket.next().await {
                    Some(Ok(Message::Text(text))) if text.contains("KeepAlive") => return true,
                    Some(Ok(Message::Close(_))) | Some(Err(_)) | None => return false,
                    _ => {}
                }
            }
        });

        let reaper = ProviderReaper::new();
        let stream = test_deepgram_stream(&base, Duration::from_millis(50), &reaper);
        let keepalive_seen = tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .expect("the mock server must observe a frame within the bound")
            .unwrap();
        assert!(keepalive_seen, "an idle outbound side must emit KeepAlive");
        Box::new(stream).abort().await.unwrap();
    }

    #[tokio::test]
    async fn deepgram_stream_dropped_mid_abort_hands_the_streaming_task_to_the_reaper() {
        use futures_util::StreamExt;
        use tokio_tungstenite::tungstenite::Message;

        let (listener, base) = mock_deepgram_listener().await;
        let (connected_tx, connected_rx) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(tcp).await.unwrap();
            let _ = connected_tx.send(());
            while let Some(Ok(message)) = socket.next().await {
                if matches!(message, Message::Close(_)) {
                    break;
                }
            }
        });

        let reaper = ProviderReaper::new();
        let stream = test_deepgram_stream(&base, Duration::from_secs(5), &reaper);
        // Cancellation before the connection exists would end the I/O task
        // pre-connect; the drop under test must land on a live websocket.
        connected_rx.await.unwrap();
        drop(stream);
        assert_eq!(
            reaper.pending(),
            1,
            "a dropped stream must hand its websocket I/O task to the supervisor"
        );
        assert!(
            reaper.drain(Duration::from_secs(2)).await,
            "the retained I/O task must observe cancellation and drain"
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn deepgram_abort_awaits_the_streaming_task_and_leaves_nothing_retained() {
        use futures_util::StreamExt;
        use tokio_tungstenite::tungstenite::Message;

        let (listener, base) = mock_deepgram_listener().await;
        let (connected_tx, connected_rx) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(tcp).await.unwrap();
            let _ = connected_tx.send(());
            while let Some(Ok(message)) = socket.next().await {
                if matches!(message, Message::Close(_)) {
                    break;
                }
            }
        });

        let reaper = ProviderReaper::new();
        let mut stream = test_deepgram_stream(&base, Duration::from_secs(5), &reaper);
        stream.send_audio(AudioChunk(vec![9u8; 32])).await.unwrap();
        // The abort under test must land on a live websocket, not on an I/O
        // task that observed cancellation before it ever connected.
        connected_rx.await.unwrap();
        Box::new(stream).abort().await.unwrap();
        assert_eq!(
            reaper.pending(),
            0,
            "abort must await the I/O task itself, leaving nothing for the supervisor"
        );
        server.await.unwrap();
    }

    #[test]
    fn every_restricted_external_child_receives_the_parent_death_contract() {
        let mut child = restricted_command("python3");
        child.args([
            "-c",
            "import ctypes, signal, sys; value = ctypes.c_int(); result = ctypes.CDLL(None).prctl(2, ctypes.byref(value)); sys.exit(result != 0 or value.value != signal.SIGKILL)",
        ]);

        assert!(child.status().unwrap().success());
    }

    #[test]
    fn cancel_set_mid_wait_kills_the_owned_child_within_the_poll_bound() {
        let cancel = CancelRegistry::new();
        let registry = Arc::clone(&cancel);
        let canceller = thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            registry.cancel();
        });
        let started = Instant::now();
        let result = run_restricted_with_deadline(
            "sleep",
            &["5"],
            None,
            false,
            Duration::from_secs(4),
            Some(&cancel),
        );
        canceller.join().unwrap();
        assert!(matches!(result, Err(ProcessError::TimedOut)));
        assert!(
            started.elapsed() < Duration::from_millis(600),
            "a mid-wait cancel must kill within the poll bound, elapsed {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn already_cancelled_operations_fail_fast_without_spawning() {
        let cancel = CancelRegistry::new();
        cancel.cancel();
        let started = Instant::now();
        let result = run_restricted_with_deadline(
            "sleep",
            &["5"],
            None,
            false,
            Duration::from_secs(4),
            Some(&cancel),
        );
        assert!(matches!(result, Err(ProcessError::TimedOut)));
        assert!(
            started.elapsed() < Duration::from_millis(100),
            "an already-cancelled operation must not spawn, elapsed {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn libei_device_must_resume_and_stays_failed_after_removal() {
        let mut link = EiDeviceLink::default();
        assert_eq!(
            link.observe(EiLinkEvent::DeviceAddedWithKeyboard),
            EiLinkDirective::AdoptDevice
        );
        assert!(!link.ready(), "DEVICE_ADDED alone cannot accept events");
        link.observe(EiLinkEvent::DeviceResumed { ours: true });
        assert!(link.ready());
        link.observe(EiLinkEvent::DevicePaused { ours: true });
        assert!(!link.ready());
        link.observe(EiLinkEvent::DeviceResumed { ours: true });
        assert!(link.ready());
        assert_eq!(
            link.observe(EiLinkEvent::DeviceRemoved { ours: true }),
            EiLinkDirective::Fail("libei disconnected")
        );
        assert!(!link.ready());
    }

    #[test]
    fn libei_confirmation_drains_a_synthetic_pong_before_disconnect() {
        let mut confirmation = EiDeliveryConfirmation::default();
        confirmation.observe(EiLinkEvent::Pong { ours: false });
        assert_eq!(confirmation.verdict(), None);
        confirmation.observe(EiLinkEvent::Pong { ours: true });
        confirmation.observe(EiLinkEvent::Disconnect);
        assert_eq!(
            confirmation.verdict(),
            Some(Err("libei disconnected during compositor submission"))
        );
    }

    #[test]
    fn keyboard_paste_resolves_the_v_key_from_the_active_layout_group() {
        use xkbcommon::xkb;

        let context = xkb::Context::new(xkb::CONTEXT_NO_ENVIRONMENT_NAMES);
        let keymap = xkb::Keymap::new_from_names(
            &context,
            "",
            "",
            "us,us",
            ",dvorak",
            Some(String::new()),
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .unwrap();
        let text = keymap.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1);
        let us = resolve_keyboard_paste_keys(text.clone(), 0).unwrap();
        let dvorak = resolve_keyboard_paste_keys(text, 1).unwrap();

        assert_eq!(us.control, dvorak.control);
        assert_ne!(us.paste, dvorak.paste);
    }

    #[test]
    fn verified_shortcuts_use_unshifted_letters_and_ei_codes() {
        use xkbcommon::xkb;

        let context = xkb::Context::new(xkb::CONTEXT_NO_ENVIRONMENT_NAMES);
        let keymap = xkb::Keymap::new_from_names(
            &context,
            "",
            "",
            "us",
            "",
            Some(String::new()),
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .unwrap();
        let text = keymap.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1);
        let uppercase = resolve_shortcut_keycodes(text.clone(), 0, "SUPER + V").unwrap();
        let lowercase = resolve_shortcut_keycodes(text.clone(), 0, "SUPER + v").unwrap();
        assert_eq!(uppercase, lowercase);
        assert_eq!(
            resolve_shortcut_keycodes(text.clone(), 0, "SUPER + INSERT").unwrap(),
            resolve_shortcut_keycodes(text.clone(), 0, "SUPER + insert").unwrap()
        );
        assert_eq!(
            resolve_shortcut_keycodes(text.clone(), 0, "code:64").unwrap(),
            vec![56]
        );
        assert_eq!(
            resolve_shortcut_keycodes(text, 0, "code:108").unwrap(),
            vec![100]
        );
    }

    struct RecordingShortcutSession(Arc<Mutex<Vec<String>>>);

    impl DirectDeliverySession for RecordingShortcutSession {
        fn deliver_text(&mut self, _text: &str) -> BoundaryFuture<'_, ()> {
            Box::pin(async { Ok(()) })
        }

        fn deliver_shortcut(&mut self, shortcut: &str) -> BoundaryFuture<'_, ()> {
            let events = Arc::clone(&self.0);
            let shortcut = shortcut.to_owned();
            Box::pin(async move {
                events.lock().unwrap().push(format!("shortcut:{shortcut}"));
                Ok(())
            })
        }
    }

    fn test_paste_action() -> VerifiedPasteAction {
        use crate::hyprland_bindings::{PasteBehavior, PasteShortcut};

        VerifiedPasteAction {
            shortcut: PasteShortcut {
                binding: "SUPER + V".to_owned(),
            },
            description: "Universal paste".to_owned(),
            live_binding_identity: "91".to_owned(),
            behavior: PasteBehavior::Simple,
        }
    }

    #[tokio::test]
    async fn production_paste_starts_portal_setup_before_the_first_invoke() {
        let paste = PortalPasteAction::with_live_revalidation(
            test_paste_action(),
            Box::new(DisabledRemoteDesktopPortal),
        );
        assert!(paste.setup.is_some());
        assert!(paste.session.is_none());
    }

    #[tokio::test]
    async fn paste_action_rejects_a_cached_binding_after_live_revalidation_changes() {
        let cached = test_paste_action();
        let live = VerifiedPasteAction {
            live_binding_identity: "92".to_owned(),
            ..cached.clone()
        };
        let mut paste = PortalPasteAction::with_test_revalidation(
            cached.clone(),
            Box::new(DisabledRemoteDesktopPortal),
            live,
        );

        let error = tokio::time::timeout(Duration::from_secs(1), paste.invoke(&cached))
            .await
            .expect("live verification should complete")
            .expect_err("a stale cached action must fail closed");
        assert_eq!(
            error.diagnostic(),
            "verified Paste Action is no longer active"
        );
    }

    #[tokio::test]
    async fn live_paste_revalidation_awaits_and_pastes_on_the_first_invoke() {
        use std::sync::mpsc;

        let action = test_paste_action();
        let events = Arc::new(Mutex::new(Vec::new()));
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let release_rx = Arc::new(Mutex::new(release_rx));
        let verifier_action = action.clone();
        let verifier_release = Arc::clone(&release_rx);
        let verifier = Arc::new(move || {
            started_tx
                .send(())
                .expect("live verifier start should be observed");
            let _ = verifier_release.lock().unwrap().recv();
            Some(verifier_action.clone())
        });
        let mut paste = PortalPasteAction::with_options(
            action.clone(),
            Box::new(DisabledRemoteDesktopPortal),
            Some(verifier),
        );
        // Portal permission stays non-blocking and is not under test here.
        paste.session = Some(Box::new(RecordingShortcutSession(Arc::clone(&events))));

        let requested = action.clone();
        let invoke = tokio::spawn(async move { paste.invoke(&requested).await });
        let releaser = std::thread::spawn(move || {
            started_rx.recv().expect("live verifier should start");
            release_tx
                .send(())
                .expect("live verifier should be released");
        });
        tokio::time::timeout(Duration::from_secs(1), invoke)
            .await
            .expect("live verification must complete on the first invoke")
            .expect("invoke task should finish")
            .expect("the first invocation must paste once live verification matches");
        releaser
            .join()
            .expect("verifier release thread should finish");
        assert_eq!(events.lock().unwrap().as_slice(), ["shortcut:SUPER + V"]);
    }

    #[test]
    fn remote_desktop_classification_preserves_permanent_denials() {
        let error = classify_remote_desktop_failure(
            BoundaryError::new(
                BoundaryKind::Delivery,
                "the desktop did not approve the SelectDevices request (response 1)",
            )
            .permanent(),
        );

        assert_eq!(error.diagnostic(), "permission denied");
        assert!(error.is_permanent());
    }

    /// A compositor that populates the keymap memfd with `write()` leaves the
    /// shared offset at the end; reading through the file cursor then returned
    /// an empty keymap that libxkbcommon rejected, forcing the clipboard
    /// fallback. The read must not depend on the shared offset.
    #[test]
    fn keymap_fd_reads_the_whole_keymap_regardless_of_the_shared_file_offset() {
        use std::io::{Seek, SeekFrom, Write};
        use xkbcommon::xkb;

        let context = xkb::Context::new(xkb::CONTEXT_NO_ENVIRONMENT_NAMES);
        let source = xkb::Keymap::new_from_names(
            &context,
            "",
            "pc105",
            "us",
            "",
            Some(String::new()),
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .unwrap();
        let expected = source.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1);

        // Mirror the EIS handoff: keymap plus terminating NUL, size counting it.
        let mut payload = expected.clone().into_bytes();
        payload.push(0);
        let size = payload.len();

        // SAFETY: a fresh anonymous descriptor owned by this test.
        let raw = unsafe { libc::memfd_create(c"voisu-keymap-test".as_ptr(), libc::MFD_CLOEXEC) };
        assert!(raw >= 0, "memfd_create failed");
        let mut backing = unsafe { File::from_raw_fd(raw) };
        backing.write_all(&payload).unwrap();

        // The write left the offset at the end — the failing production case.
        assert_eq!(backing.stream_position().unwrap(), size as u64);
        let text = read_keymap_fd(backing.as_raw_fd(), size).unwrap();
        assert_eq!(text, expected);
        assert!(resolve_keyboard_paste_keys(text, 0).is_ok());

        // An offset already at the start stays correct, and the read leaves the
        // shared offset untouched for any later reader.
        backing.seek(SeekFrom::Start(0)).unwrap();
        assert_eq!(read_keymap_fd(backing.as_raw_fd(), size).unwrap(), expected);
        assert_eq!(backing.stream_position().unwrap(), 0);
    }

    #[test]
    fn recording_deadline_defaults_to_ten_minutes_and_survives_past_sixty_seconds() {
        // With no override the Recording Deadline must be generous enough that a
        // routine multi-minute Recording is never killed. Sixty seconds was the
        // old, wrong default that discarded audio before providers ever saw it.
        let default = resolve_recording_maximum(None).deadline;
        assert_eq!(default, Duration::from_secs(600));
        assert!(
            default > Duration::from_secs(60),
            "default Recording Deadline must not kill a >60s Recording"
        );

        // A parseable, non-zero override still wins; junk and zero fall back.
        assert_eq!(
            resolve_recording_maximum(Some("5000".to_owned())).deadline,
            Duration::from_millis(5000)
        );
        assert_eq!(
            resolve_recording_maximum(Some("0".to_owned())).deadline,
            default
        );
        assert_eq!(
            resolve_recording_maximum(Some("nonsense".to_owned())).deadline,
            default
        );
    }

    #[test]
    fn configured_recording_maximum_drives_deadline_and_pcm_byte_cap() {
        let configured = resolve_recording_maximum(Some("5000".to_owned()));
        assert_eq!(
            configured.deadline,
            Duration::from_millis(5000),
            "the configured maximum must set the per-Recording Deadline"
        );
        assert_eq!(
            configured.pcm_byte_cap,
            16_000 * 2 * 5,
            "the same configured maximum must set the per-Recording PCM byte cap"
        );
    }

    #[test]
    fn an_override_cannot_raise_the_recording_maximum_past_the_absolute_ceiling() {
        // Retained PCM lives in memory, so MAX_RECORDING_DURATION has to be a
        // real ceiling and not merely the default: an operator asking for an
        // hour must not be handed an hour's worth of buffer.
        let overreaching = resolve_recording_maximum(Some("3600000".to_owned()));
        assert_eq!(
            overreaching.deadline, MAX_RECORDING_DURATION,
            "an over-long override must clamp to the one bounded maximum"
        );
        assert_eq!(
            overreaching.pcm_byte_cap,
            16_000 * 2 * MAX_RECORDING_DURATION.as_secs() as usize,
            "the clamped deadline must bound the retained PCM cap too"
        );
        // Shortening still works — the clamp is a ceiling, not a pin.
        assert_eq!(
            resolve_recording_maximum(Some("5000".to_owned())).deadline,
            Duration::from_millis(5000)
        );
    }

    #[test]
    fn a_clamped_override_is_reported_to_the_operator_and_a_respected_one_is_not() {
        // A configured maximum that is quietly ignored is the same complaint as
        // being cut off at an unannounced limit, just moved to the config
        // surface: the operator must be told the number they will actually get.
        let notice = recording_deadline_override_notice(Some("1200000".to_owned()))
            .expect("an over-long override must be reported");
        assert!(
            notice.contains("1200000 ms") && notice.contains("600 s"),
            "the notice must name both what was asked for and what is enforced: {notice}"
        );
        // Nothing is said when nothing was ignored — including at exactly the
        // ceiling, which is honoured in full.
        assert_eq!(
            recording_deadline_override_notice(Some("600000".to_owned())),
            None
        );
        assert_eq!(
            recording_deadline_override_notice(Some("5000".to_owned())),
            None
        );
        assert_eq!(recording_deadline_override_notice(None), None);
        assert_eq!(
            recording_deadline_override_notice(Some("not-a-number".to_owned())),
            None
        );
        assert_eq!(
            recording_deadline_override_notice(Some("0".to_owned())),
            None
        );
    }

    #[test]
    fn a_sub_hundred_millisecond_override_floors_the_retained_pcm_cap() {
        // 50 ms derives a 1_600-byte cap, below the minimum a Recording needs to
        // pass validate_audio, which would make the byte cap — not the Deadline
        // — the enforcer that ends the Recording, at a length that can only fail.
        // The floor bounds the cap from below so the cap never describes less
        // than a deliverable Recording. It does NOT make a 50 ms Recording
        // deliverable: with a real microphone the Deadline still stops the pump
        // after ~1_600 bytes and validate_audio still rejects it as
        // TooShortRecording. Only the cap's floor is pinned here.
        let tiny = resolve_recording_maximum(Some("50".to_owned()));
        assert_eq!(tiny.deadline, Duration::from_millis(50));
        assert_eq!(
            tiny.pcm_byte_cap, MIN_RECORDING_BYTES,
            "the retained-PCM cap must never fall below a deliverable Recording"
        );
    }

    #[test]
    fn libei_text_buffer_is_nul_terminated_and_rejects_interior_nul() {
        let text = libei_text_buffer("Hello, दुनिया!").unwrap();
        assert_eq!(text.as_bytes_with_nul().last(), Some(&0));
        assert!(libei_text_buffer("unsafe\0tail").is_err());
    }

    /// #98: the default reconciliation model is the exact selected Qwen id.
    #[test]
    fn default_groq_reconciliation_model_is_exact_qwen_id() {
        assert_eq!(DEFAULT_GROQ_RECONCILIATION_MODEL, "qwen/qwen3.6-27b");
    }

    /// #98: only the exact selected model gets `reasoning_effort: "none"`.
    #[test]
    fn selected_qwen_reconciliation_body_sets_reasoning_effort_none() {
        let body = groq_reconciliation_request_body(
            DEFAULT_GROQ_RECONCILIATION_MODEL,
            "Reconcile these Source Transcripts.",
        );
        assert_eq!(
            body.get("model").and_then(|value| value.as_str()),
            Some("qwen/qwen3.6-27b")
        );
        assert_eq!(
            body.get("reasoning_effort")
                .and_then(|value| value.as_str()),
            Some("none")
        );
        assert_eq!(
            body.get("temperature").and_then(|value| value.as_f64()),
            Some(0.0)
        );
    }

    #[test]
    fn intent_reconstruction_body_uses_json_object_mode_with_both_sources_and_dictionary() {
        let body = groq_intent_reconstruction_request_body(&IntentReconstructionRequest {
            sources: vec![
                SourceTranscript {
                    provider: Provider::Deepgram,
                    text: "sanitized one".to_owned(),
                },
                SourceTranscript {
                    provider: Provider::Groq,
                    text: "sanitized two".to_owned(),
                },
            ],
            dictionary_terms: vec!["Voisu".to_owned()],
        });
        assert_eq!(body["model"], "qwen/qwen3.6-27b");
        assert_eq!(body["reasoning_effort"], "none");
        assert_eq!(
            body["response_format"],
            serde_json::json!({"type": "json_object"})
        );
        assert!(
            body["messages"][0]["content"]
                .as_str()
                .unwrap()
                .contains("exactly this shape: {\"wording\":\"...\"}")
        );
        let user: serde_json::Value =
            serde_json::from_str(body["messages"][1]["content"].as_str().unwrap()).unwrap();
        assert_eq!(user["sources"].as_array().unwrap().len(), 2);
        assert_eq!(user["dictionary"], serde_json::json!(["Voisu"]));
    }

    /// #98: every other override omits `reasoning_effort` (no family-wide Qwen
    /// rule; GPT-OSS rejects `none` with HTTP 400).
    #[test]
    fn non_selected_reconciliation_models_omit_reasoning_effort() {
        let overrides = [
            "openai/gpt-oss-120b",
            "llama-3.3-70b-versatile",
            "qwen/qwen3-32b",
            "configured-model",
        ];
        for model in overrides {
            let body =
                groq_reconciliation_request_body(model, "Reconcile these Source Transcripts.");
            assert_eq!(
                body.get("model").and_then(|value| value.as_str()),
                Some(model),
                "model id must pass through unchanged: {model}"
            );
            assert!(
                body.get("reasoning_effort").is_none(),
                "reasoning_effort must be omitted for override {model}: {body}"
            );
        }
    }

    // -----------------------------------------------------------------
    // SW7 — CredentialPreparationOwner + reaper credential lane seams
    // -----------------------------------------------------------------

    /// Serializes tests that mutate process-global credential env/cache/PATH.
    /// Tokio mutex is intentional: the guard is held across `.await` points.
    async fn credential_owner_test_lock() -> tokio::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
            .lock()
            .await
    }

    struct CredentialEnvGuard {
        keys: Vec<(&'static str, Option<std::ffi::OsString>)>,
    }

    impl CredentialEnvGuard {
        fn capture(keys: &[&'static str]) -> Self {
            let keys = keys.iter().map(|k| (*k, std::env::var_os(k))).collect();
            Self { keys }
        }

        fn set(key: &str, value: &str) {
            // SAFETY: tests hold credential_owner_test_lock; single-threaded mutation.
            unsafe { std::env::set_var(key, value) };
        }

        fn remove(key: &str) {
            unsafe { std::env::remove_var(key) };
        }
    }

    impl Drop for CredentialEnvGuard {
        fn drop(&mut self) {
            for (key, value) in self.keys.drain(..) {
                match value {
                    Some(v) => unsafe { std::env::set_var(key, v) },
                    None => unsafe { std::env::remove_var(key) },
                }
            }
        }
    }

    fn clear_groq_credential_surface() {
        CredentialEnvGuard::remove("VOISU_GROQ_API_KEY");
        CredentialEnvGuard::remove("VOISU_TEST_SECRET_STORE");
        CredentialEnvGuard::remove("VOISU_TEST_STORED_GROQ_CREDENTIAL");
        CredentialEnvGuard::remove("VOISU_TEST_CREDENTIAL_PREP_DEADLINE_MS");
        CredentialEnvGuard::remove("VOISU_TEST_CREDENTIAL_REAP_WATCHDOG_MS");
        CredentialEnvGuard::remove("VOISU_TEST_CREDENTIAL_REAP_STALL_MS");
        CredentialEnvGuard::remove("VOISU_TEST_KEYRING_RETRY_MS");
        credential_cache().invalidate(Provider::Groq);
    }

    fn install_fake_secret_tool(dir: &std::path::Path, script: &str) {
        let path = dir.join("secret-tool");
        fs::write(&path, script).unwrap();
        let mut perms = fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&path, perms).unwrap();
        let old_path = std::env::var_os("PATH").unwrap_or_default();
        let mut new_path = dir.as_os_str().to_os_string();
        new_path.push(":");
        new_path.push(old_path);
        unsafe { std::env::set_var("PATH", new_path) };
    }

    /// Prove the OS process (and its /proc entry) is fully gone after reap.
    fn assert_os_process_reaped(pgid: libc::pid_t) {
        let proc_path = std::path::PathBuf::from(format!("/proc/{pgid}"));
        assert!(
            !proc_path.exists(),
            "credential child pgid {pgid} still has /proc entry after reap"
        );
        // kill(pid, 0) must fail with ESRCH when the process is fully gone
        // (including no unreaped zombie still owned by us).
        let rc = unsafe { libc::kill(pgid, 0) };
        assert_eq!(
            rc, -1,
            "credential child pgid {pgid} still accepts kill(0) after reap"
        );
        let errno = std::io::Error::last_os_error().raw_os_error();
        assert_eq!(
            errno,
            Some(libc::ESRCH),
            "credential child pgid {pgid} kill(0) errno {errno:?}, expected ESRCH"
        );
    }

    /// Wait until a child has launched (or panic after budget).
    async fn wait_until_credential_child_launched(entry: &CredentialCleanupEntry) -> libc::pid_t {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(pgid) = entry.retained_pgid() {
                return pgid;
            }
            if entry.has_launched_child()
                && let Some(pgid) = entry.retained_pgid()
            {
                return pgid;
            }
            assert!(
                Instant::now() < deadline,
                "credential child never launched / no durable pgid within 5s"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    #[test]
    fn credential_prep_constants_match_spec() {
        assert_eq!(CREDENTIAL_PREP_WORK_DEADLINE, Duration::from_secs(13));
        assert_eq!(CREDENTIAL_REAP_WATCHDOG, Duration::from_secs(2));
    }

    #[tokio::test]
    async fn intent_deadline_cancels_and_reaps_credential_lookup_before_fallback() {
        let _lock = credential_owner_test_lock().await;
        let _env = CredentialEnvGuard::capture(&[
            "VOISU_GROQ_API_KEY",
            "VOISU_TEST_SECRET_STORE",
            "VOISU_TEST_STORED_GROQ_CREDENTIAL",
            "PATH",
        ]);
        clear_groq_credential_surface();
        let tools = tempfile::TempDir::new().unwrap();
        install_fake_secret_tool(tools.path(), "#!/bin/sh\nsleep 30\n");
        let reaper = ProviderReaper::new();
        let mut pipeline = TranscriptDecisionPipeline::with_intent_reconstruction(
            GroqReconciliationModel {
                reaper: Some(reaper.clone()),
            },
            Duration::from_millis(30),
            Vec::new(),
        );
        let prepared = pipeline
            .prepare(vec![
                SourceTranscript {
                    provider: Provider::Deepgram,
                    text: "Book the room Tuesday afternoon.".to_owned(),
                },
                SourceTranscript {
                    provider: Provider::Groq,
                    text: "Schedule the review Wednesday morning.".to_owned(),
                },
            ])
            .await
            .unwrap();
        let PreparedTranscriptDecision::Reconstruct(attempt) = prepared else {
            panic!("material disagreement must reconstruct");
        };
        let decision = pipeline.reconstruct(attempt).await.unwrap();

        assert_eq!(
            decision.intent_reconstruction.unwrap().outcome,
            voisu_core::IntentReconstructionOutcome::Deadline
        );
        assert_eq!(reaper.credential_lane().pending_count(), 0);
    }

    #[tokio::test]
    async fn credential_lane_registers_before_owner_may_launch() {
        let _lock = credential_owner_test_lock().await;
        let _env = CredentialEnvGuard::capture(&[
            "VOISU_GROQ_API_KEY",
            "VOISU_TEST_SECRET_STORE",
            "VOISU_TEST_STORED_GROQ_CREDENTIAL",
            "PATH",
        ]);
        clear_groq_credential_surface();

        let reaper = ProviderReaper::new();
        let lane = reaper.credential_lane().clone();
        let entry = lane.register();

        assert_eq!(entry.phase(), CredentialEntryPhase::Registered);
        assert!(
            !entry.has_launched_child(),
            "registration alone must not launch secret-tool"
        );
        assert!(lane.contains(entry.id()));
        assert_eq!(lane.pending_count(), 1);

        // Owner construction still must not launch.
        let mut owner =
            CredentialPreparationOwner::new(Arc::clone(&entry), lane.clone(), Provider::Groq);
        assert!(!entry.has_launched_child());
        assert_eq!(entry.phase(), CredentialEntryPhase::Registered);

        // Fast path (env) reaches Terminal without a child, then Deregistered.
        CredentialEnvGuard::set("VOISU_GROQ_API_KEY", "sw7-env-key");
        let capability = owner.poll_outcome().await;
        assert!(capability.is_ready(), "{capability:?}");
        assert!(!entry.has_launched_child());
        assert_eq!(entry.phase(), CredentialEntryPhase::Deregistered);
        assert!(!lane.contains(entry.id()));
        assert_eq!(lane.pending_count(), 0);
    }

    #[tokio::test]
    async fn credential_prep_cache_hit_ready_without_child() {
        let _lock = credential_owner_test_lock().await;
        let _env = CredentialEnvGuard::capture(&[
            "VOISU_GROQ_API_KEY",
            "VOISU_TEST_SECRET_STORE",
            "VOISU_TEST_STORED_GROQ_CREDENTIAL",
            "PATH",
        ]);
        clear_groq_credential_surface();

        let credential = Credential::new("sw7-cached-key".to_owned()).unwrap();
        credential_cache().put(Provider::Groq, credential);

        let reaper = ProviderReaper::new();
        let lane = reaper.credential_lane().clone();
        let entry = lane.register();
        let mut owner =
            CredentialPreparationOwner::new(Arc::clone(&entry), lane.clone(), Provider::Groq);

        let capability = owner.poll_outcome().await;
        match capability {
            GrammarCapability::Ready(ready) => {
                assert_eq!(ready.credential().expose_to_boundary(), "sw7-cached-key");
            }
            other => panic!("expected Ready from cache hit, got {other:?}"),
        }
        assert!(!entry.has_launched_child());
        assert_eq!(entry.phase(), CredentialEntryPhase::Deregistered);
        assert_eq!(lane.pending_count(), 0);
    }

    #[tokio::test]
    async fn credential_prep_test_seam_ready_and_single_deregistration() {
        let _lock = credential_owner_test_lock().await;
        let _env = CredentialEnvGuard::capture(&[
            "VOISU_GROQ_API_KEY",
            "VOISU_TEST_SECRET_STORE",
            "VOISU_TEST_STORED_GROQ_CREDENTIAL",
            "PATH",
        ]);
        clear_groq_credential_surface();
        CredentialEnvGuard::set("VOISU_TEST_SECRET_STORE", "available");
        CredentialEnvGuard::set("VOISU_TEST_STORED_GROQ_CREDENTIAL", "sw7-seam-key");

        let reaper = ProviderReaper::new();
        let lane = reaper.credential_lane().clone();
        let entry = lane.register();
        let id = entry.id();
        let mut owner =
            CredentialPreparationOwner::new(Arc::clone(&entry), lane.clone(), Provider::Groq);

        let capability = owner.poll_outcome().await;
        assert!(capability.is_ready(), "{capability:?}");
        assert_eq!(entry.phase(), CredentialEntryPhase::Deregistered);
        assert!(!lane.contains(id));

        // Second deregister is a no-op (idempotent).
        lane.deregister(&entry);
        lane.force_deregister(&entry);
        assert_eq!(entry.phase(), CredentialEntryPhase::Deregistered);
        assert_eq!(lane.pending_count(), 0);

        // finish_terminal is stable after completion.
        let again = owner.finish_terminal(capability).await;
        assert!(again.is_ready());
    }

    #[tokio::test]
    async fn credential_prep_cancel_kills_child_and_deregisters() {
        let _lock = credential_owner_test_lock().await;
        let _env = CredentialEnvGuard::capture(&[
            "VOISU_GROQ_API_KEY",
            "VOISU_TEST_SECRET_STORE",
            "VOISU_TEST_STORED_GROQ_CREDENTIAL",
            "VOISU_TEST_KEYRING_RETRY_MS",
            "PATH",
        ]);
        clear_groq_credential_surface();
        CredentialEnvGuard::set("VOISU_TEST_KEYRING_RETRY_MS", "0");

        let tool_dir = tempfile::tempdir().unwrap();
        install_fake_secret_tool(
            tool_dir.path(),
            "#!/bin/sh\n# SW7 cancel seam: hang until killed\nsleep 30\n",
        );

        let reaper = ProviderReaper::new();
        let lane = reaper.credential_lane().clone();
        let entry = lane.register();
        let entry_watch = Arc::clone(&entry);
        let mut owner =
            CredentialPreparationOwner::new(Arc::clone(&entry), lane.clone(), Provider::Groq);

        let poll = owner.poll_outcome();
        tokio::pin!(poll);

        // Wait until the child has launched, then cancel via entry (poll holds &mut owner).
        let launched = wait_until_credential_child_launched(&entry_watch);
        let pgid = tokio::select! {
            _ = &mut poll => panic!("hanging secret-tool should not finish before cancel"),
            pgid = launched => {
                entry_watch.request_cancel_and_kill();
                pgid
            }
        };
        let capability = poll.await;
        assert!(
            matches!(
                capability,
                GrammarCapability::Unavailable(GrammarUnavailableReason::Cancelled)
            ),
            "cancel must yield Cancelled (not deadline), got {capability:?}"
        );
        assert_eq!(entry.phase(), CredentialEntryPhase::Deregistered);
        assert_eq!(lane.pending_count(), 0);
        assert_os_process_reaped(pgid);
        assert!(
            entry.both_pipe_eofs_observed(),
            "cancel path must observe both stdout/stderr EOFs before terminal"
        );
    }

    #[tokio::test]
    async fn credential_prep_work_deadline_stops_and_reaps() {
        let _lock = credential_owner_test_lock().await;
        let _env = CredentialEnvGuard::capture(&[
            "VOISU_GROQ_API_KEY",
            "VOISU_TEST_SECRET_STORE",
            "VOISU_TEST_STORED_GROQ_CREDENTIAL",
            "VOISU_TEST_CREDENTIAL_PREP_DEADLINE_MS",
            "VOISU_TEST_KEYRING_RETRY_MS",
            "PATH",
        ]);
        clear_groq_credential_surface();
        CredentialEnvGuard::set("VOISU_TEST_CREDENTIAL_PREP_DEADLINE_MS", "80");
        CredentialEnvGuard::set("VOISU_TEST_KEYRING_RETRY_MS", "0");

        let tool_dir = tempfile::tempdir().unwrap();
        install_fake_secret_tool(tool_dir.path(), "#!/bin/sh\nsleep 30\n");

        let reaper = ProviderReaper::new();
        let lane = reaper.credential_lane().clone();
        let entry = lane.register();
        let entry_watch = Arc::clone(&entry);
        let mut owner =
            CredentialPreparationOwner::new(Arc::clone(&entry), lane.clone(), Provider::Groq);

        let started = Instant::now();
        let poll = owner.poll_outcome();
        tokio::pin!(poll);
        let mut seen_pgid = None;
        let capability = loop {
            tokio::select! {
                biased;
                result = &mut poll => break result,
                _ = tokio::time::sleep(Duration::from_millis(5)) => {
                    if seen_pgid.is_none() {
                        seen_pgid = entry_watch.retained_pgid();
                    }
                    assert!(
                        started.elapsed() < Duration::from_secs(3),
                        "deadline path must not wait out the full sleep, elapsed {:?}",
                        started.elapsed()
                    );
                }
            }
        };
        assert!(
            matches!(
                capability,
                GrammarCapability::Unavailable(GrammarUnavailableReason::WorkDeadlineExceeded)
            ),
            "deadline must yield WorkDeadlineExceeded (not Cancelled), got {capability:?}"
        );
        assert!(
            entry.has_launched_child(),
            "deadline path must have launched secret-tool"
        );
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "deadline path must not wait out the full sleep, elapsed {:?}",
            started.elapsed()
        );
        assert_eq!(entry.phase(), CredentialEntryPhase::Deregistered);
        assert_eq!(lane.pending_count(), 0);
        let pgid = seen_pgid.expect("deadline hang child must expose durable pgid before reap");
        assert_os_process_reaped(pgid);
    }

    #[tokio::test]
    async fn credential_reap_watchdog_overrun_logs_and_awaits_terminal() {
        let _lock = credential_owner_test_lock().await;
        let _env = CredentialEnvGuard::capture(&[
            "VOISU_GROQ_API_KEY",
            "VOISU_TEST_SECRET_STORE",
            "VOISU_TEST_STORED_GROQ_CREDENTIAL",
            "VOISU_TEST_CREDENTIAL_PREP_DEADLINE_MS",
            "VOISU_TEST_CREDENTIAL_REAP_WATCHDOG_MS",
            "VOISU_TEST_CREDENTIAL_REAP_STALL_MS",
            "VOISU_TEST_KEYRING_RETRY_MS",
            "PATH",
        ]);
        clear_groq_credential_surface();
        // Short work deadline forces kill; short watchdog + artificial stall
        // crosses the diagnostic threshold once; stall ends and terminal arrives.
        CredentialEnvGuard::set("VOISU_TEST_CREDENTIAL_PREP_DEADLINE_MS", "50");
        CredentialEnvGuard::set("VOISU_TEST_CREDENTIAL_REAP_WATCHDOG_MS", "30");
        CredentialEnvGuard::set("VOISU_TEST_CREDENTIAL_REAP_STALL_MS", "120");
        CredentialEnvGuard::set("VOISU_TEST_KEYRING_RETRY_MS", "0");

        let tool_dir = tempfile::tempdir().unwrap();
        install_fake_secret_tool(tool_dir.path(), "#!/bin/sh\nsleep 30\n");

        let reaper = ProviderReaper::new();
        let lane = reaper.credential_lane().clone();
        let entry = lane.register();
        let mut owner =
            CredentialPreparationOwner::new(Arc::clone(&entry), lane.clone(), Provider::Groq);

        let capability = owner.poll_outcome().await;
        assert!(capability.is_unavailable(), "{capability:?}");
        assert!(
            entry.watchdog_overrun_logged(),
            "crossing the reap watchdog must log once"
        );
        assert_eq!(entry.phase(), CredentialEntryPhase::Deregistered);
        assert_eq!(lane.pending_count(), 0);
    }

    #[tokio::test]
    async fn credential_noncooperative_pipe_awaits_both_eofs_before_terminal() {
        // Parent exits 0 after writing the secret, but a grandchild inherits
        // stdout and holds the write end open. Terminal requires wait + both
        // EOFs: post-wait drain must not abandon on a short read timeout.
        // Stall re-signals the process group so the holder dies and EOF arrives.
        let _lock = credential_owner_test_lock().await;
        let _env = CredentialEnvGuard::capture(&[
            "VOISU_GROQ_API_KEY",
            "VOISU_TEST_SECRET_STORE",
            "VOISU_TEST_STORED_GROQ_CREDENTIAL",
            "VOISU_TEST_KEYRING_RETRY_MS",
            "PATH",
        ]);
        clear_groq_credential_surface();
        CredentialEnvGuard::set("VOISU_TEST_KEYRING_RETRY_MS", "0");

        let tool_dir = tempfile::tempdir().unwrap();
        install_fake_secret_tool(
            tool_dir.path(),
            "#!/bin/sh\nprintf 'noncoop-secret-token\\n'\nsleep 100 &\nexit 0\n",
        );

        let reaper = ProviderReaper::new();
        let lane = reaper.credential_lane().clone();
        let entry = lane.register();
        let mut owner =
            CredentialPreparationOwner::new(Arc::clone(&entry), lane.clone(), Provider::Groq);

        let started = Instant::now();
        let capability = owner.poll_outcome().await;
        assert!(
            matches!(capability, GrammarCapability::Ready(_)),
            "after wait + both EOFs the secret must classify Ready, got {capability:?}"
        );
        assert_eq!(entry.phase(), CredentialEntryPhase::Deregistered);
        assert_eq!(lane.pending_count(), 0);
        // Must not hang on the grandchild's full sleep; process-group kill on
        // pipe stall frees the write end. Bound well under sleep 100.
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "non-cooperative pipe drain must free via process-group kill, elapsed {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn credential_cancel_reap_applies_watchdog_diagnostic() {
        // In-band cancel must use the same 2 s watchdog as deadline cleanup —
        // unbounded cancel_reap is forbidden.
        let _lock = credential_owner_test_lock().await;
        let _env = CredentialEnvGuard::capture(&[
            "VOISU_GROQ_API_KEY",
            "VOISU_TEST_SECRET_STORE",
            "VOISU_TEST_STORED_GROQ_CREDENTIAL",
            "VOISU_TEST_CREDENTIAL_REAP_WATCHDOG_MS",
            "VOISU_TEST_CREDENTIAL_REAP_STALL_MS",
            "VOISU_TEST_KEYRING_RETRY_MS",
            "PATH",
        ]);
        clear_groq_credential_surface();
        CredentialEnvGuard::set("VOISU_TEST_CREDENTIAL_REAP_WATCHDOG_MS", "30");
        CredentialEnvGuard::set("VOISU_TEST_CREDENTIAL_REAP_STALL_MS", "120");
        CredentialEnvGuard::set("VOISU_TEST_KEYRING_RETRY_MS", "0");

        let tool_dir = tempfile::tempdir().unwrap();
        install_fake_secret_tool(tool_dir.path(), "#!/bin/sh\nsleep 30\n");

        let reaper = ProviderReaper::new();
        let lane = reaper.credential_lane().clone();
        let entry = lane.register();
        let entry_watch = Arc::clone(&entry);
        let mut owner =
            CredentialPreparationOwner::new(Arc::clone(&entry), lane.clone(), Provider::Groq);

        let pgid = {
            let poll = owner.poll_outcome();
            tokio::pin!(poll);
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                tokio::select! {
                    biased;
                    _ = &mut poll => panic!("hanging secret-tool finished before cancel"),
                    _ = tokio::time::sleep(Duration::from_millis(5)) => {
                        if let Some(pgid) = entry_watch.retained_pgid() {
                            break pgid;
                        }
                        assert!(Instant::now() < deadline, "child never launched");
                    }
                }
            }
        };

        let capability = owner.cancel_and_drive_terminal().await;
        assert!(
            matches!(
                capability,
                GrammarCapability::Unavailable(GrammarUnavailableReason::Cancelled)
            ),
            "{capability:?}"
        );
        assert!(
            entry.watchdog_overrun_logged(),
            "cancel reap must apply the 2 s watchdog diagnostic"
        );
        assert_eq!(entry.phase(), CredentialEntryPhase::Deregistered);
        assert_eq!(lane.pending_count(), 0);
        assert_os_process_reaped(pgid);
        assert!(
            entry.both_pipe_eofs_observed(),
            "cancel+watchdog path must observe both pipe EOFs"
        );
    }

    #[tokio::test]
    async fn credential_owner_drop_kills_without_claiming_terminal() {
        let _lock = credential_owner_test_lock().await;
        let _env = CredentialEnvGuard::capture(&[
            "VOISU_GROQ_API_KEY",
            "VOISU_TEST_SECRET_STORE",
            "VOISU_TEST_STORED_GROQ_CREDENTIAL",
            "VOISU_TEST_KEYRING_RETRY_MS",
            "PATH",
        ]);
        clear_groq_credential_surface();
        CredentialEnvGuard::set("VOISU_TEST_KEYRING_RETRY_MS", "0");

        let tool_dir = tempfile::tempdir().unwrap();
        let ready = tool_dir.path().join("ready");
        let ready_path = ready.to_string_lossy().into_owned();
        // Write a secret-like payload, barrier-signal, then hang so Drop mid-drive
        // must re-park live pipes (not silent Nones) until supervisor observes
        // both EOFs. Ready-file avoids racing cancel before the secret write.
        install_fake_secret_tool(
            tool_dir.path(),
            &format!(
                "#!/bin/sh\nprintf 'drop-secret-must-drain\\n'\n: > '{ready_path}'\nsleep 30\n"
            ),
        );

        let reaper = ProviderReaper::new();
        let lane = reaper.credential_lane().clone();
        let entry = lane.register();
        let entry_watch = Arc::clone(&entry);

        // Drive first poll past the secret write + launch, then drop the owner
        // without awaiting terminal — Drop kill-signals but must not deregister.
        let pgid = {
            let mut owner =
                CredentialPreparationOwner::new(Arc::clone(&entry), lane.clone(), Provider::Groq);
            let pgid = {
                let poll = owner.poll_outcome();
                tokio::pin!(poll);
                let deadline = Instant::now() + Duration::from_secs(5);
                loop {
                    tokio::select! {
                        biased;
                        _ = &mut poll => {
                            panic!("owner should be dropped before poll completes");
                        }
                        _ = tokio::time::sleep(Duration::from_millis(5)) => {
                            // Barrier: secret payload written before Drop.
                            if ready.exists()
                                && let Some(pgid) = entry_watch.retained_pgid() {
                                    break pgid;
                                }
                            assert!(
                                Instant::now() < deadline,
                                "child never signalled ready before Drop test budget"
                            );
                        }
                    }
                }
                // End the poll borrow: cancels drive future. TakenRunningChild
                // re-parks Child + live pipes so supervisor can wait + both EOFs.
            };
            drop(owner);
            pgid
        };
        assert!(
            entry.is_cancel_requested(),
            "Drop must request cancellation"
        );
        assert!(
            lane.contains(entry.id()),
            "Drop must not deregister; supervisor retains the entry"
        );
        assert_ne!(
            entry.phase(),
            CredentialEntryPhase::Deregistered,
            "Drop cannot claim Deregistered"
        );
        // After Drop: durable kill must have signalled. Process may be zombie
        // until supervisor wait; must not still be a running sleep.
        assert!(
            entry.retained_pgid().is_some()
                || !std::path::Path::new(&format!("/proc/{pgid}")).exists(),
            "after Drop, either wait ownership (pgid) remains or OS process is already gone"
        );
        // Drop alone must not claim both-EOF terminal (supervisor finishes EOFs).
        // Pipes must still be owned (re-parked) or already fully drained by a
        // racing path — never "terminal without EOF proof".
        assert!(
            !entry.both_pipe_eofs_observed()
                || entry.phase() == CredentialEntryPhase::Terminal
                || entry.phase() == CredentialEntryPhase::Deregistered,
            "EOF flags only after a reap path, not silent abandon"
        );

        // Supervisor drain claims, reaps (wait + both EOFs), deregisters.
        reaper.drain_credential_lane().await;
        assert_eq!(entry.phase(), CredentialEntryPhase::Deregistered);
        assert_eq!(lane.pending_count(), 0);
        assert_os_process_reaped(pgid);
        assert!(
            entry.both_pipe_eofs_observed(),
            "Drop→supervisor path must observe both stdout/stderr EOFs, not only process reaped"
        );
        assert!(
            !entry.has_retained_running_child(),
            "terminal must clear running child + pipe buffers (no undrained secrets)"
        );
        // Idempotent second drain.
        reaper.drain_credential_lane().await;
        assert_eq!(lane.pending_count(), 0);
    }

    #[tokio::test]
    async fn credential_cancel_observes_both_pipe_eofs_with_secret_payload() {
        // Cancel after the tool writes a secret-like line must still drain both
        // pipes to EOF (ownership retained until EOF), then terminal Cancelled.
        let _lock = credential_owner_test_lock().await;
        let _env = CredentialEnvGuard::capture(&[
            "VOISU_GROQ_API_KEY",
            "VOISU_TEST_SECRET_STORE",
            "VOISU_TEST_STORED_GROQ_CREDENTIAL",
            "VOISU_TEST_KEYRING_RETRY_MS",
            "PATH",
        ]);
        clear_groq_credential_surface();
        CredentialEnvGuard::set("VOISU_TEST_KEYRING_RETRY_MS", "0");

        let tool_dir = tempfile::tempdir().unwrap();
        let ready = tool_dir.path().join("ready");
        let ready_path = ready.to_string_lossy().into_owned();
        install_fake_secret_tool(
            tool_dir.path(),
            &format!("#!/bin/sh\nprintf 'cancel-secret-token\\n'\n: > '{ready_path}'\nsleep 30\n"),
        );

        let reaper = ProviderReaper::new();
        let lane = reaper.credential_lane().clone();
        let entry = lane.register();
        let entry_watch = Arc::clone(&entry);
        let mut owner =
            CredentialPreparationOwner::new(Arc::clone(&entry), lane.clone(), Provider::Groq);

        let poll = owner.poll_outcome();
        tokio::pin!(poll);

        // Barrier: wait until the tool has written its payload (not a sleep race).
        let pgid = {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                tokio::select! {
                    biased;
                    _ = &mut poll => panic!("hanging secret-tool finished before cancel"),
                    _ = tokio::time::sleep(Duration::from_millis(5)) => {
                        if ready.exists()
                            && let Some(pgid) = entry_watch.retained_pgid() {
                                break pgid;
                            }
                        assert!(Instant::now() < deadline, "tool never signalled ready");
                    }
                }
            }
        };

        entry_watch.request_cancel_and_kill();
        let capability = poll.await;
        assert!(
            matches!(
                capability,
                GrammarCapability::Unavailable(GrammarUnavailableReason::Cancelled)
            ),
            "cancel after secret write must yield Cancelled, got {capability:?}"
        );
        assert_eq!(entry.phase(), CredentialEntryPhase::Deregistered);
        assert_eq!(lane.pending_count(), 0);
        assert_os_process_reaped(pgid);
        assert!(
            entry.both_pipe_eofs_observed(),
            "cancel path must observe both pipe EOFs after secret-bearing write"
        );
        assert!(
            !entry.has_retained_running_child(),
            "no undrained pipe buffers may remain after terminal"
        );
    }

    #[tokio::test]
    async fn credential_cancel_during_retry_backoff_does_not_retain_stale_pgid() {
        // First secret-tool attempt fails with stderr → Retry. Between attempts
        // the child is fully reaped (wait + both EOFs). last_pgid must already
        // be cleared so cancel/Drop during backoff cannot SIGKILL a reused PGID.
        let _lock = credential_owner_test_lock().await;
        let _env = CredentialEnvGuard::capture(&[
            "VOISU_GROQ_API_KEY",
            "VOISU_TEST_SECRET_STORE",
            "VOISU_TEST_STORED_GROQ_CREDENTIAL",
            "VOISU_TEST_KEYRING_RETRY_MS",
            "PATH",
        ]);
        clear_groq_credential_surface();
        // Long flat backoff so cancel lands squarely between attempts.
        CredentialEnvGuard::set("VOISU_TEST_KEYRING_RETRY_MS", "5000");

        let tool_dir = tempfile::tempdir().unwrap();
        let ready = tool_dir.path().join("ready");
        let ready_path = ready.to_string_lossy().into_owned();
        // Nonzero exit + stderr → KeyringLocked → Retry (not Missing).
        install_fake_secret_tool(
            tool_dir.path(),
            &format!("#!/bin/sh\necho 'keyring locked' >&2\n: > '{ready_path}'\nexit 1\n"),
        );

        let reaper = ProviderReaper::new();
        let lane = reaper.credential_lane().clone();
        let entry = lane.register();
        let entry_watch = Arc::clone(&entry);
        let mut owner =
            CredentialPreparationOwner::new(Arc::clone(&entry), lane.clone(), Provider::Groq);

        let poll = owner.poll_outcome();
        tokio::pin!(poll);

        // Wait until first attempt is fully reaped and durable pgid is cleared.
        let first_pgid = {
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut seen_pgid = None;
            loop {
                tokio::select! {
                    biased;
                    _ = &mut poll => {
                        panic!("long retry backoff must not finish before cancel");
                    }
                    _ = tokio::time::sleep(Duration::from_millis(5)) => {
                        if let Some(pgid) = entry_watch.retained_pgid() {
                            seen_pgid = Some(pgid);
                        }
                        // Ready means the failing attempt wrote and exited; full
                        // reap clears last_pgid and leaves no running child.
                        if ready.exists()
                            && entry_watch.has_launched_child()
                            && entry_watch.both_pipe_eofs_observed()
                            && entry_watch.retained_pgid().is_none()
                            && !entry_watch.has_retained_running_child()
                        {
                            break seen_pgid;
                        }
                        assert!(
                            Instant::now() < deadline,
                            "first retry attempt never fully reaped before backoff"
                        );
                    }
                }
            }
        };

        // Core r4 invariant: after wait + both EOFs there is no durable kill target.
        assert!(
            entry_watch.retained_pgid().is_none(),
            "last_pgid must be cleared after full reap so retry-backoff cancel is a no-op kill"
        );
        if let Some(pgid) = first_pgid {
            assert_os_process_reaped(pgid);
        }

        // Cancel during backoff: no live child / no last_pgid → kill is no-op.
        entry_watch.request_cancel_and_kill();
        let capability = poll.await;
        assert!(
            matches!(
                capability,
                GrammarCapability::Unavailable(GrammarUnavailableReason::Cancelled)
            ),
            "cancel during retry backoff must yield Cancelled, got {capability:?}"
        );
        assert_eq!(entry.phase(), CredentialEntryPhase::Deregistered);
        assert_eq!(lane.pending_count(), 0);
        assert!(
            entry.retained_pgid().is_none(),
            "cancel during backoff must not re-arm a stale last_pgid"
        );
        assert!(
            entry.both_pipe_eofs_observed(),
            "first-attempt EOFs must remain observed after backoff cancel"
        );
        assert!(
            !entry.has_retained_running_child(),
            "no undrained child may remain after backoff cancel terminal"
        );
    }

    #[tokio::test]
    async fn credential_cancel_and_drive_terminal_api_reaps() {
        let _lock = credential_owner_test_lock().await;
        let _env = CredentialEnvGuard::capture(&[
            "VOISU_GROQ_API_KEY",
            "VOISU_TEST_SECRET_STORE",
            "VOISU_TEST_STORED_GROQ_CREDENTIAL",
            "PATH",
        ]);
        clear_groq_credential_surface();
        // Registered, no launch yet — cancel still reaches Terminal + Deregistered.
        let reaper = ProviderReaper::new();
        let lane = reaper.credential_lane().clone();
        let entry = lane.register();
        let mut owner =
            CredentialPreparationOwner::new(Arc::clone(&entry), lane.clone(), Provider::Groq);

        let capability = owner.cancel_and_drive_terminal().await;
        assert!(
            matches!(
                capability,
                GrammarCapability::Unavailable(GrammarUnavailableReason::Cancelled)
            ),
            "{capability:?}"
        );
        assert_eq!(entry.phase(), CredentialEntryPhase::Deregistered);
        assert_eq!(lane.pending_count(), 0);
    }

    #[tokio::test]
    async fn credential_caught_panic_seam_cancel_reaps_before_error() {
        // Models the concurrent catch_unwind path: on panic, explicitly
        // cancel_and_drive_terminal before returning a Processing error.
        let _lock = credential_owner_test_lock().await;
        let _env = CredentialEnvGuard::capture(&[
            "VOISU_GROQ_API_KEY",
            "VOISU_TEST_SECRET_STORE",
            "VOISU_TEST_STORED_GROQ_CREDENTIAL",
            "VOISU_TEST_KEYRING_RETRY_MS",
            "PATH",
        ]);
        clear_groq_credential_surface();
        CredentialEnvGuard::set("VOISU_TEST_KEYRING_RETRY_MS", "0");

        let tool_dir = tempfile::tempdir().unwrap();
        install_fake_secret_tool(tool_dir.path(), "#!/bin/sh\nsleep 30\n");

        let reaper = ProviderReaper::new();
        let lane = reaper.credential_lane().clone();
        let entry = lane.register();
        let mut owner =
            CredentialPreparationOwner::new(Arc::clone(&entry), lane.clone(), Provider::Groq);

        // Simulate: prep started, then a caught panic forces explicit cancel-reap.
        let entry_watch = Arc::clone(&entry);
        let pgid = {
            let poll = owner.poll_outcome();
            tokio::pin!(poll);
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                tokio::select! {
                    _ = &mut poll => panic!("hanging secret-tool finished before panic-seam cancel"),
                    _ = tokio::time::sleep(Duration::from_millis(5)) => {
                        if let Some(pgid) = entry_watch.retained_pgid() {
                            break pgid;
                        }
                        assert!(
                            Instant::now() < deadline,
                            "child never launched before panic-seam cancel"
                        );
                    }
                }
            }
            // Drop the in-flight poll so we can mutably use owner again.
            // Re-parks wait ownership for cancel_and_drive_terminal.
        };
        // Explicit cancel-reap (caught panic path) — do not rely on Drop alone.
        let capability = owner.cancel_and_drive_terminal().await;
        assert!(
            matches!(
                capability,
                GrammarCapability::Unavailable(GrammarUnavailableReason::Cancelled)
            ),
            "panic-seam cancel must yield Cancelled, got {capability:?}"
        );
        assert_eq!(entry.phase(), CredentialEntryPhase::Deregistered);
        assert_eq!(lane.pending_count(), 0);
        assert_os_process_reaped(pgid);
    }

    #[tokio::test]
    async fn credential_secret_tool_miss_path_uses_tokio_process() {
        let _lock = credential_owner_test_lock().await;
        let _env = CredentialEnvGuard::capture(&[
            "VOISU_GROQ_API_KEY",
            "VOISU_TEST_SECRET_STORE",
            "VOISU_TEST_STORED_GROQ_CREDENTIAL",
            "VOISU_TEST_KEYRING_RETRY_MS",
            "PATH",
        ]);
        clear_groq_credential_surface();
        CredentialEnvGuard::set("VOISU_TEST_KEYRING_RETRY_MS", "0");

        let tool_dir = tempfile::tempdir().unwrap();
        // Clean no-match: nonzero exit, empty stderr → Missing → NoCredential
        // (no file fallback in this hermetic home).
        install_fake_secret_tool(tool_dir.path(), "#!/bin/sh\nexit 1\n");

        let reaper = ProviderReaper::new();
        let lane = reaper.credential_lane().clone();
        let entry = lane.register();
        let mut owner =
            CredentialPreparationOwner::new(Arc::clone(&entry), lane.clone(), Provider::Groq);

        let capability = owner.poll_outcome().await;
        assert!(
            entry.has_launched_child(),
            "cache miss must launch Tokio secret-tool"
        );
        assert!(
            matches!(
                capability,
                GrammarCapability::Unavailable(GrammarUnavailableReason::NoCredential)
            ),
            "missing keyring+file must be NoCredential, got {capability:?}"
        );
        assert_eq!(entry.phase(), CredentialEntryPhase::Deregistered);
    }

    #[tokio::test]
    async fn credential_secret_tool_found_path_caches_and_ready() {
        let _lock = credential_owner_test_lock().await;
        let _env = CredentialEnvGuard::capture(&[
            "VOISU_GROQ_API_KEY",
            "VOISU_TEST_SECRET_STORE",
            "VOISU_TEST_STORED_GROQ_CREDENTIAL",
            "VOISU_TEST_KEYRING_RETRY_MS",
            "PATH",
        ]);
        clear_groq_credential_surface();
        CredentialEnvGuard::set("VOISU_TEST_KEYRING_RETRY_MS", "0");

        let tool_dir = tempfile::tempdir().unwrap();
        install_fake_secret_tool(
            tool_dir.path(),
            "#!/bin/sh\nprintf '%s' 'sw7-from-secret-tool'\n",
        );

        let reaper = ProviderReaper::new();
        let lane = reaper.credential_lane().clone();
        let entry = lane.register();
        let mut owner =
            CredentialPreparationOwner::new(Arc::clone(&entry), lane.clone(), Provider::Groq);

        let capability = owner.poll_outcome().await;
        match capability {
            GrammarCapability::Ready(ready) => {
                assert_eq!(
                    ready.credential().expose_to_boundary(),
                    "sw7-from-secret-tool"
                );
            }
            other => panic!("expected Ready from secret-tool, got {other:?}"),
        }
        assert!(entry.has_launched_child());
        assert_eq!(entry.phase(), CredentialEntryPhase::Deregistered);

        // Cache filled — second owner must Ready without another child.
        let entry2 = lane.register();
        let mut owner2 =
            CredentialPreparationOwner::new(Arc::clone(&entry2), lane.clone(), Provider::Groq);
        let cap2 = owner2.poll_outcome().await;
        assert!(cap2.is_ready(), "{cap2:?}");
        assert!(
            !entry2.has_launched_child(),
            "cache hit after miss must not re-launch secret-tool"
        );
    }
}
