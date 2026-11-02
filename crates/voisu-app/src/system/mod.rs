use std::collections::VecDeque;

use std::ffi::CString;

use std::fs::{self, File, OpenOptions};

use std::io::{Read, Write};

use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd};

use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

use std::os::unix::net::UnixStream;

use std::os::unix::process::ExitStatusExt;

use std::path::{Path, PathBuf};

use std::process::{Child, Stdio};

use std::sync::{Arc, Mutex, OnceLock};

use std::thread;

use std::time::{Duration, Instant};

use voisu_core::{
    ActiveCapture, AudioCapture, AudioChunk, BoundaryError, BoundaryFuture, BoundaryKind,
    CancelRegistry, CaptureLimit, CapturedAudio, ClipboardTool, Command as DaemonCommand,
    Credential, DeadlineClock, DeliveryAdapter, DeliveryOutcome, KeyDiagnosis, KeyLocation,
    PACKAGE_MANAGERS, PROTOCOL_VERSION, PackageManager, Provider, ReadinessCapability,
    ReadinessFinding, ReadinessInspector, ReadinessStatus, Request, Response, SecretStore,
    SessionKind, SessionResolution, ShortcutPortal, ShortcutSession, Transcript, TriggerKeyBinding,
    VersionEnvelope, WavScan, clipboard_candidates, install_instruction, resolve_session,
    scan_wav_pcm, socket_path,
};

use crate::audio_level::{
    BandState, LevelRegistry, PCM_CHUNK_BYTES, PcmChunkAssembler, SampleDecoder, bands,
};

use crate::focus::SharedFocusProbe;

use crate::hyprland_bindings::VerifiedPasteAction;

use crate::secret_file::{FileSecretStore, RemoveError};

// The cloud pipeline moved to voisu-cloud. Its bounded subprocess runner is
// shared with the local helpers here, and its public items are re-exported so
// every existing `voisu_app::system::Item` path keeps working.
use voisu_cloud::process::*;
pub use voisu_cloud::{
    AuthProbe, CleanupSupervisor, CloudHost, DEFAULT_GROQ_RECONCILIATION_MODEL, DeepgramProvider,
    GroqProvider, GroqReconciliationModel, INTENT_RECONSTRUCTION_DEADLINE, MergeResultValidator,
    ProviderHttpClient, ProviderHttpRequest, RECONCILIATION_DEADLINE, ReapTask,
    install_crypto_provider,
};

pub const CAPTURE_FINALIZE_DEADLINE: Duration = PROCESS_DEADLINE;

pub const PROVIDER_COMPLETION_DEADLINE: Duration = Duration::from_secs(15);

pub const CLIPBOARD_DELIVERY_DEADLINE: Duration = PROCESS_DEADLINE;

pub const LIBEI_DELIVERY_DEADLINE: Duration = Duration::from_secs(5);

/// Grace granted to the bounded capture/provider aborts that run when a
/// Recording fails or a partial start is rolled back.
pub const RECOVERY_ABORT_DEADLINE: Duration = PROCESS_DEADLINE;

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

const MAX_DAEMON_RESPONSE_BYTES: usize = 16 * 1024;

/// `hyprctl binds -j` on Omarchy-sized bind tables is ~100 KiB. The shared
/// helper stdout cap stays 64 KiB; this inspection-only ceiling is 1 MiB and
/// fail-closes if the dump is truncated.
const HYPRCTL_BINDS_JSON_CAP: usize = 1024 * 1024;

const MIN_RECORDING_BYTES: usize = PCM_CHUNK_BYTES;

/// Resolve the current display session from the live environment. Detection is
/// pure logic in `voisu-core`; this only reads the environment for it.
fn current_session() -> SessionResolution {
    let wayland_display = std::env::var("WAYLAND_DISPLAY").ok();
    let x11_display = std::env::var("DISPLAY").ok();
    let session_type = std::env::var("XDG_SESSION_TYPE").ok();
    resolve_session(
        wayland_display.as_deref(),
        x11_display.as_deref(),
        session_type.as_deref(),
    )
}

/// The one configured maximum for a Recording. Both capture's retained PCM
/// buffer and its default Deadline derive from this value — and so do the
/// Overlay's approaching-limit warnings, which is why this is visible to the
/// rest of the crate rather than copied into the presentation layer.
pub(crate) const MAX_RECORDING_DURATION: Duration = Duration::from_secs(600);

// Subsystem modules. `pub use <module>::*;` keeps every existing
// `voisu_app::system::Item` path working after the split (pure move).
mod capture;
mod credential;
mod delivery;
mod display_wait;
mod grammar;
mod libei_delivery;
mod portal_shortcuts;
mod readiness;
mod secret_store;

pub use capture::*;
pub use credential::*;
pub use delivery::*;
pub use display_wait::*;
pub use grammar::*;
pub use libei_delivery::*;
pub use portal_shortcuts::*;
pub use readiness::*;
pub use secret_store::*;

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    async fn wait_for(flag: &Arc<AtomicBool>) {
        tokio::time::timeout(Duration::from_secs(2), async {
            while !flag.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("probe blocking task must enter spawn_blocking");
    }

    // The DAEMON wiring caps the keyterms through `deepgram_keyterms` before
    // the URL builder sees them; through that seam the streaming request never
    // exceeds Deepgram's count and token budgets.
    #[test]
    fn deepgram_streaming_url_through_the_keyterm_cap_never_exceeds_the_budgets() {
        let terms: Vec<String> = (0..600).map(|index| format!("term{index:03}")).collect();
        let capped = crate::dictionary::deepgram_keyterms(&terms);
        let url =
            voisu_cloud::deepgram_streaming_url("wss://api.deepgram.com/v1/listen", &capped, "en")
                .unwrap();
        assert_eq!(
            url.matches("keyterm=").count(),
            capped.len(),
            "every capped term is sent exactly once: {url}"
        );
        assert!(capped.len() <= crate::dictionary::DEEPGRAM_KEYTERM_COUNT_LIMIT);
    }

    use std::process::Command;
    use voisu_core::{PreparedTranscriptDecision, SourceTranscript, TranscriptDecisionPipeline};

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
    async fn with_hyprland_paste_does_not_start_remote_desktop_setup() {
        let delivery = PortalClipboardDelivery::with_hyprland_paste(test_paste_action());
        assert!(delivery.hyprland_paste_skips_remote_desktop());
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
            GroqReconciliationModel::new(reaper.clone()),
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
