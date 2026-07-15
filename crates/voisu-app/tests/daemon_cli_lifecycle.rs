use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;
use tempfile::TempDir;

const PROTOCOL_VERSION: u32 = 1;

struct Daemon {
    child: Child,
}

impl Daemon {
    fn start(runtime_dir: &Path) -> Self {
        Self::start_with_env(runtime_dir, &[])
    }

    fn start_with_env(runtime_dir: &Path, environment: &[(&str, &str)]) -> Self {
        fs::set_permissions(runtime_dir, fs::Permissions::from_mode(0o700)).unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_voisu-daemon"));
        command
            .env("XDG_RUNTIME_DIR", runtime_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        for (name, value) in environment {
            command.env(name, value);
        }
        let mut child = command.spawn().expect("daemon should start");

        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if socket_path(runtime_dir).exists()
                && voisu(runtime_dir, "status").status.success()
            {
                return Self { child };
            }
            if let Some(status) = child.try_wait().expect("daemon status should be readable") {
                panic!("daemon exited before binding its socket: {status}");
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("daemon did not bind its socket");
    }

    fn terminate(mut self) {
        let status = Command::new("kill")
            .args(["-TERM", &self.child.id().to_string()])
            .status()
            .expect("SIGTERM should be sent");
        assert!(status.success());
        assert!(self.child.wait().unwrap().success());
    }

    fn crash(mut self) {
        self.child.kill().expect("daemon should be killed");
        let _ = self.child.wait();
    }

    /// Sends SIGTERM, then drains the daemon's stderr to EOF so local
    /// diagnostics emitted during the run can be asserted on.
    fn terminate_and_stderr(mut self) -> String {
        let status = Command::new("kill")
            .args(["-TERM", &self.child.id().to_string()])
            .status()
            .expect("SIGTERM should be sent");
        assert!(status.success());
        let mut diagnostics = String::new();
        if let Some(mut stderr) = self.child.stderr.take() {
            stderr.read_to_string(&mut diagnostics).ok();
        }
        let _ = self.child.wait();
        diagnostics
    }
}

#[test]
fn status_is_responsive_and_processing_is_observable_during_provider_work() {
    let runtime = TempDir::new().unwrap();
    let _daemon = Daemon::start_with_env(
        runtime.path(),
        &[("VOISU_TEST_PROVIDER_DELAY_MS", "400")],
    );
    assert!(voisu(runtime.path(), "start").status.success());

    let runtime_dir = runtime.path().to_owned();
    let stop = thread::spawn(move || voisu(&runtime_dir, "stop"));
    let deadline = Instant::now() + Duration::from_millis(250);
    let mut observed = None;
    while Instant::now() < deadline {
        let started = Instant::now();
        let status = voisu(runtime.path(), "status");
        if stdout(&status) == "processing\n" {
            observed = Some(started.elapsed());
            break;
        }
        thread::sleep(Duration::from_millis(5));
    }
    assert!(
        observed.is_some_and(|elapsed| elapsed < Duration::from_millis(100)),
        "status should promptly expose processing"
    );

    let stop = stop.join().unwrap();
    assert!(stop.status.success(), "{}", stderr(&stop));
    assert_eq!(stdout(&voisu(runtime.path(), "status")), "idle\n");
}

#[test]
fn capture_finalization_failure_is_redacted_and_the_next_recording_succeeds() {
    let runtime = TempDir::new().unwrap();
    let _daemon = Daemon::start_with_env(
        runtime.path(),
        &[("VOISU_TEST_CAPTURE_FINISH_FAILURE", "1")],
    );
    assert!(voisu(runtime.path(), "start").status.success());

    let failed = voisu(runtime.path(), "stop");
    assert_eq!(failed.status.code(), Some(4));
    assert_eq!(stderr(&failed), "Recording capture failed\n");
    assert!(!stderr(&failed).contains("controlled-secret"));
    assert_eq!(stdout(&voisu(runtime.path(), "status")), "idle\n");

    assert!(voisu(runtime.path(), "start").status.success());
    let recovered = voisu(runtime.path(), "stop");
    assert!(recovered.status.success(), "{}", stderr(&recovered));
    assert_eq!(stdout(&voisu(runtime.path(), "status")), "idle\n");
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn socket_path(runtime_dir: &Path) -> PathBuf {
    runtime_dir
        .join("voisu")
        .join(format!("v{PROTOCOL_VERSION}"))
        .join("daemon.sock")
}

fn voisu(runtime_dir: &Path, command: &str) -> Output {
    voisu_with_env(runtime_dir, &[command], &[])
}

fn voisu_with_env(runtime_dir: &Path, arguments: &[&str], environment: &[(&str, &str)]) -> Output {
    fs::set_permissions(runtime_dir, fs::Permissions::from_mode(0o700)).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_voisu"));
    command.args(arguments).env("XDG_RUNTIME_DIR", runtime_dir);
    for (name, value) in environment {
        command.env(name, value);
    }
    command.output().expect("CLI should run")
}

fn voisu_with_secret(
    runtime_dir: &Path,
    arguments: &[&str],
    environment: &[(&str, &str)],
    credential: &str,
) -> Output {
    fs::set_permissions(runtime_dir, fs::Permissions::from_mode(0o700)).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_voisu"));
    command
        .args(arguments)
        .env("XDG_RUNTIME_DIR", runtime_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (name, value) in environment {
        command.env(name, value);
    }
    let mut child = command.spawn().expect("CLI should run");
    child
        .stdin
        .as_mut()
        .expect("CLI stdin should be available")
        .write_all(credential.as_bytes())
        .expect("credential should be written to stdin");
    child.wait_with_output().expect("CLI should complete")
}

fn ipc_request(runtime_dir: &Path, request: &str) -> Value {
    let mut stream = UnixStream::connect(socket_path(runtime_dir)).unwrap();
    stream.write_all(request.as_bytes()).unwrap();
    stream.write_all(b"\n").unwrap();
    let mut response = String::new();
    BufReader::new(stream).read_line(&mut response).unwrap();
    serde_json::from_str(&response).unwrap()
}

fn wait_until_missing(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if !path.exists() {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("{} was not removed", path.display());
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn doctor_reports_each_fedora_capability_through_the_public_cli() {
    let runtime = TempDir::new().unwrap();
    let _daemon = Daemon::start(runtime.path());
    let doctor = Command::new(env!("CARGO_BIN_EXE_voisu"))
        .arg("doctor")
        .env("XDG_RUNTIME_DIR", runtime.path())
        .env("VOISU_TEST_READINESS", "pass")
        .output()
        .expect("doctor should run");

    assert!(doctor.status.success(), "{}", stderr(&doctor));
    assert_eq!(
        stdout(&doctor),
        "PipeWire: PASS (available)\nMicrophone: PASS (present)\nPortals: PASS (available)\nClipboard: PASS (available)\nSecret storage: PASS (available)\nDaemon: PASS (reachable)\n"
    );
}

#[test]
fn doctor_exposes_actionable_warn_and_fail_outcomes() {
    let runtime = TempDir::new().unwrap();
    let doctor = voisu_with_env(
        runtime.path(),
        &["doctor"],
        &[("VOISU_TEST_READINESS", "pipewire=fail,clipboard=warn")],
    );

    assert_eq!(doctor.status.code(), Some(4));
    assert!(stdout(&doctor).contains("PipeWire: FAIL (not available)\n"));
    assert!(stdout(&doctor).contains("Clipboard: WARN (needs attention)\n"));
    assert!(stdout(&doctor).contains("Daemon: FAIL (unavailable)\n"));
}

#[test]
fn auth_set_replaces_a_credential_without_echoing_it() {
    let runtime = TempDir::new().unwrap();
    let first = voisu_with_secret(
        runtime.path(),
        &["auth", "set", "groq"],
        &[("VOISU_TEST_SECRET_STORE", "available")],
        "controlled-secret-one",
    );
    let replacement = voisu_with_secret(
        runtime.path(),
        &["auth", "set", "groq"],
        &[("VOISU_TEST_SECRET_STORE", "available")],
        "controlled-secret-two",
    );

    assert!(first.status.success(), "{}", stderr(&first));
    assert_eq!(stdout(&first), "Groq credential stored\n");
    assert!(replacement.status.success(), "{}", stderr(&replacement));
    assert_eq!(stdout(&replacement), "Groq credential stored\n");
    let combined = format!("{}{}{}{}", stdout(&first), stderr(&first), stdout(&replacement), stderr(&replacement));
    assert!(!combined.contains("controlled-secret-one"));
    assert!(!combined.contains("controlled-secret-two"));
}

#[test]
fn denied_secret_storage_names_the_headless_fallback_without_leaking_credential() {
    let runtime = TempDir::new().unwrap();
    let denied = voisu_with_secret(
        runtime.path(),
        &["auth", "set", "deepgram"],
        &[("VOISU_TEST_SECRET_STORE", "denied")],
        "controlled-secret",
    );

    assert_eq!(denied.status.code(), Some(4));
    assert_eq!(
        stderr(&denied),
        "Secret storage is unavailable; set VOISU_GROQ_API_KEY or VOISU_DEEPGRAM_API_KEY for development or headless use\n"
    );
    assert!(!stderr(&denied).contains("controlled-secret"));
}

#[test]
fn auth_verify_checks_each_provider_without_retaining_or_printing_response_content() {
    let runtime = TempDir::new().unwrap();
    let groq = voisu_with_env(
        runtime.path(),
        &["auth", "verify", "groq"],
        &[
            ("VOISU_TEST_SECRET_STORE", "denied"),
            ("VOISU_GROQ_API_KEY", "controlled-secret"),
            ("VOISU_TEST_AUTH_GROQ", "authorized"),
        ],
    );
    let deepgram = voisu_with_env(
        runtime.path(),
        &["auth", "verify", "deepgram"],
        &[
            ("VOISU_TEST_SECRET_STORE", "available"),
            ("VOISU_TEST_STORED_DEEPGRAM_CREDENTIAL", "controlled-secret"),
            ("VOISU_TEST_AUTH_DEEPGRAM", "denied"),
        ],
    );

    assert!(groq.status.success(), "{}", stderr(&groq));
    assert_eq!(stdout(&groq), "Groq authentication verified\n");
    assert_eq!(deepgram.status.code(), Some(4));
    assert_eq!(stderr(&deepgram), "Provider authentication failed\n");
    let combined = format!("{}{}{}{}", stdout(&groq), stderr(&groq), stdout(&deepgram), stderr(&deepgram));
    assert!(!combined.contains("controlled-secret"));
    assert!(!combined.contains("response content"));
}

#[test]
fn status_distinguishes_daemon_unavailable_from_idle() {
    let runtime = TempDir::new().unwrap();

    let unavailable = voisu(runtime.path(), "status");
    assert_eq!(unavailable.status.code(), Some(3));
    assert_eq!(stdout(&unavailable), "daemon unavailable\n");

    let _daemon = Daemon::start(runtime.path());
    let idle = voisu(runtime.path(), "status");
    assert!(idle.status.success(), "{}", stderr(&idle));
    assert_eq!(stdout(&idle), "idle\n");
}

#[test]
fn concurrent_start_begins_exactly_one_recording() {
    let runtime = TempDir::new().unwrap();
    let _daemon = Daemon::start(runtime.path());
    let barrier = Arc::new(Barrier::new(3));

    let attempts: Vec<_> = (0..2)
        .map(|_| {
            let runtime_dir = runtime.path().to_owned();
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                voisu(&runtime_dir, "start")
            })
        })
        .collect();
    barrier.wait();
    let outputs: Vec<_> = attempts.into_iter().map(|attempt| attempt.join().unwrap()).collect();

    assert_eq!(outputs.iter().filter(|output| output.status.success()).count(), 1);
    assert_eq!(
        outputs
            .iter()
            .filter(|output| stderr(output) == "Recording already active\n")
            .count(),
        1
    );
    let status = voisu(runtime.path(), "status");
    assert!(status.status.success(), "{}", stderr(&status));
    assert_eq!(stdout(&status), "Recording\n");

    for _ in 0..2 {
        let stop = voisu(runtime.path(), "stop");
        assert!(stop.status.success(), "{}", stderr(&stop));
        assert_eq!(stdout(&voisu(runtime.path(), "status")), "idle\n");

        let start = voisu(runtime.path(), "start");
        assert!(start.status.success(), "{}", stderr(&start));
        assert_eq!(stdout(&voisu(runtime.path(), "status")), "Recording\n");
    }
    let stop = voisu(runtime.path(), "stop");
    assert!(stop.status.success(), "{}", stderr(&stop));
    assert_eq!(stdout(&voisu(runtime.path(), "status")), "idle\n");
}

#[test]
fn stop_completes_recording_and_delivery_then_returns_to_idle() {
    let runtime = TempDir::new().unwrap();
    let _daemon = Daemon::start(runtime.path());

    let start = voisu(runtime.path(), "start");
    assert!(start.status.success(), "{}", stderr(&start));
    assert_eq!(stdout(&start), "Recording started\n");

    let stop = ipc_request(
        runtime.path(),
        r#"{"version":1,"command":"stop"}"#,
    );
    assert_eq!(stop["ok"], true);
    assert_eq!(
        stop["message"],
        "Recording completed; Transcript delivered"
    );
    assert_eq!(
        stop["evidence"]["stages"],
        serde_json::json!([
            "capture_started",
            "providers_started",
            "capture_finalized",
            "providers_completed",
            "validation_completed",
            "delivery_completed"
        ])
    );
    assert_eq!(stop["evidence"]["delivery_count"], 1);

    let status = voisu(runtime.path(), "status");
    assert!(status.status.success(), "{}", stderr(&status));
    assert_eq!(stdout(&status), "idle\n");
}

#[test]
fn toggle_has_the_same_observable_transitions_as_start_then_stop() {
    let runtime = TempDir::new().unwrap();
    let _daemon = Daemon::start(runtime.path());

    let start = voisu(runtime.path(), "toggle");
    assert!(start.status.success(), "{}", stderr(&start));
    assert_eq!(stdout(&start), "Recording started\n");
    assert_eq!(stdout(&voisu(runtime.path(), "status")), "Recording\n");

    let stop = voisu(runtime.path(), "toggle");
    assert!(stop.status.success(), "{}", stderr(&stop));
    assert_eq!(
        stdout(&stop),
        "Recording completed; Transcript delivered\n"
    );
    assert_eq!(stdout(&voisu(runtime.path(), "status")), "idle\n");
}

#[test]
fn injected_xdg_runtime_dirs_are_isolated() {
    let active_runtime = TempDir::new().unwrap();
    let other_runtime = TempDir::new().unwrap();
    let _daemon = Daemon::start(active_runtime.path());

    assert!(socket_path(active_runtime.path()).exists());
    assert!(!socket_path(other_runtime.path()).exists());
    assert_eq!(stdout(&voisu(active_runtime.path(), "status")), "idle\n");

    let unavailable = voisu(other_runtime.path(), "status");
    assert_eq!(unavailable.status.code(), Some(3));
    assert_eq!(stdout(&unavailable), "daemon unavailable\n");
}

#[test]
fn protocol_version_mismatches_are_rejected_by_daemon_and_cli() {
    let runtime = TempDir::new().unwrap();
    let daemon = Daemon::start(runtime.path());

    let mut stream = UnixStream::connect(socket_path(runtime.path())).unwrap();
    stream
        .write_all(b"{\"version\":999,\"command\":\"status\"}\n")
        .unwrap();
    let mut response = String::new();
    BufReader::new(stream).read_line(&mut response).unwrap();
    let response: Value = serde_json::from_str(&response).unwrap();
    assert_eq!(response["version"], PROTOCOL_VERSION);
    assert_eq!(response["ok"], false);
    assert_eq!(
        response["message"],
        "unsupported protocol version 999; expected 1"
    );

    drop(daemon);
    let path = socket_path(runtime.path());
    let _ = fs::remove_file(&path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let listener = UnixListener::bind(&path).unwrap();
    let fake_daemon = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = String::new();
        BufReader::new(stream.try_clone().unwrap())
            .read_line(&mut request)
            .unwrap();
        assert!(!request.is_empty());
        stream
            .write_all(
                b"{\"version\":999,\"ok\":true,\"state\":\"idle\",\"message\":\"idle\"}\n",
            )
            .unwrap();
    });

    let status = voisu(runtime.path(), "status");
    fake_daemon.join().unwrap();
    assert!(!status.status.success());
    assert_eq!(
        stderr(&status),
        "IPC protocol mismatch: daemon uses 999, CLI uses 1\n"
    );
}

#[test]
fn incompatible_payload_is_rejected_as_a_protocol_mismatch_from_its_envelope() {
    let runtime = TempDir::new().unwrap();
    let _daemon = Daemon::start(runtime.path());

    let response = ipc_request(
        runtime.path(),
        r#"{"version":999,"command":{"future_schema":"status"}}"#,
    );
    assert_eq!(response["ok"], false);
    assert_eq!(
        response["message"],
        "unsupported protocol version 999; expected 1"
    );
}

#[test]
fn sigterm_cleans_up_and_a_crash_leaves_a_safely_recoverable_socket() {
    let runtime = TempDir::new().unwrap();
    let path = socket_path(runtime.path());

    Daemon::start(runtime.path()).terminate();
    wait_until_missing(&path);

    Daemon::start(runtime.path()).crash();
    assert!(path.exists(), "SIGKILL should leave a stale socket fixture");
    let replacement = Daemon::start(runtime.path());
    assert_eq!(stdout(&voisu(runtime.path(), "status")), "idle\n");
    drop(replacement);
}

#[test]
fn single_instance_rejection_preserves_the_live_daemon_and_cleanup_owns_one_inode() {
    let runtime = TempDir::new().unwrap();
    let daemon = Daemon::start(runtime.path());
    let path = socket_path(runtime.path());

    let second = Command::new(env!("CARGO_BIN_EXE_voisu-daemon"))
        .env("XDG_RUNTIME_DIR", runtime.path())
        .output()
        .unwrap();
    assert!(!second.status.success());
    assert_eq!(stderr(&second), "voisu-daemon is already running\n");
    assert_eq!(stdout(&voisu(runtime.path(), "status")), "idle\n");

    let original_inode = fs::symlink_metadata(&path).unwrap().ino();
    fs::remove_file(&path).unwrap();
    let replacement = UnixListener::bind(&path).unwrap();
    let replacement_inode = fs::symlink_metadata(&path).unwrap().ino();
    assert_ne!(original_inode, replacement_inode);
    daemon.terminate();
    assert_eq!(fs::symlink_metadata(&path).unwrap().ino(), replacement_inode);
    drop(replacement);
}

#[test]
fn runtime_paths_are_private_and_unsafe_runtime_roots_are_rejected() {
    let runtime = TempDir::new().unwrap();
    let daemon = Daemon::start(runtime.path());
    let path = socket_path(runtime.path());
    assert_eq!(fs::metadata(path.parent().unwrap()).unwrap().permissions().mode() & 0o777, 0o700);
    assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    drop(daemon);

    let unsafe_runtime = TempDir::new().unwrap();
    fs::set_permissions(unsafe_runtime.path(), fs::Permissions::from_mode(0o755)).unwrap();
    let rejected = Command::new(env!("CARGO_BIN_EXE_voisu-daemon"))
        .env("XDG_RUNTIME_DIR", unsafe_runtime.path())
        .output()
        .unwrap();
    assert!(!rejected.status.success());
    assert_eq!(
        stderr(&rejected),
        "XDG_RUNTIME_DIR must have mode 0700\n"
    );

    let link_parent = TempDir::new().unwrap();
    let real_runtime = TempDir::new().unwrap();
    let linked_runtime = link_parent.path().join("runtime-link");
    symlink(real_runtime.path(), &linked_runtime).unwrap();
    let rejected = Command::new(env!("CARGO_BIN_EXE_voisu-daemon"))
        .env("XDG_RUNTIME_DIR", &linked_runtime)
        .output()
        .unwrap();
    assert!(!rejected.status.success());
    assert_eq!(
        stderr(&rejected),
        "XDG_RUNTIME_DIR must be a real directory\n"
    );
}

#[test]
fn live_chunks_flow_to_providers_during_the_recording_not_only_after_stop() {
    let runtime = TempDir::new().unwrap();
    let _daemon = Daemon::start_with_env(
        runtime.path(),
        &[
            ("VOISU_TEST_CAPTURE_CHUNKS", "8"),
            ("VOISU_TEST_CHUNK_DELAY_MS", "40"),
        ],
    );

    let start = voisu(runtime.path(), "start");
    assert!(start.status.success(), "{}", stderr(&start));

    // While the Recording is still active, streamed chunks must already be
    // reaching the providers through the streaming seam.
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut streamed_during = 0_u64;
    while Instant::now() < deadline {
        let status = ipc_request(runtime.path(), r#"{"version":1,"command":"status"}"#);
        if status["state"] == "recording" {
            let count = status["evidence"]["streamed_chunk_count"].as_u64().unwrap_or(0);
            if count > 0 {
                streamed_during = count;
                break;
            }
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        streamed_during >= 1,
        "chunks must flow to providers during the Recording, not only after stop"
    );

    let stop = ipc_request(runtime.path(), r#"{"version":1,"command":"stop"}"#);
    assert_eq!(stop["ok"], true, "{stop}");
    assert!(
        stop["evidence"]["streamed_chunk_count"].as_u64().unwrap() >= streamed_during,
        "final evidence must retain the streamed chunk count"
    );
    assert_eq!(stdout(&voisu(runtime.path(), "status")), "idle\n");
}

#[test]
fn partial_provider_start_failure_aborts_the_capture_and_surfaces_abort_errors() {
    let runtime = TempDir::new().unwrap();
    let daemon = Daemon::start_with_env(
        runtime.path(),
        &[
            ("VOISU_TEST_PROVIDER_START_FAILURE", "1"),
            ("VOISU_TEST_CAPTURE_ABORT_FAILURE", "1"),
        ],
    );

    // Groq's start fails after capture and Deepgram already started; the daemon
    // must abort the capture and reject with a redacted public message.
    let failed = voisu(runtime.path(), "start");
    assert_eq!(failed.status.code(), Some(4));
    assert_eq!(stderr(&failed), "Source Transcripts are unavailable\n");
    assert!(!stderr(&failed).contains("controlled"));
    assert_eq!(stdout(&voisu(runtime.path(), "status")), "idle\n");

    // The one-shot provider failure is spent, so the next Recording proves the
    // aborted resources were left in a clean, reusable state.
    assert!(voisu(runtime.path(), "start").status.success());
    let recovered = voisu(runtime.path(), "stop");
    assert!(recovered.status.success(), "{}", stderr(&recovered));

    // The discarded capture-abort error must be surfaced into local diagnostics.
    let diagnostics = daemon.terminate_and_stderr();
    assert!(
        diagnostics.contains("capture abort failed")
            && diagnostics.contains("controlled-abort-detail"),
        "capture-abort failure must be surfaced, got: {diagnostics}"
    );
}

#[test]
fn capture_finalization_abort_failure_is_surfaced_into_diagnostics() {
    let runtime = TempDir::new().unwrap();
    let daemon = Daemon::start_with_env(
        runtime.path(),
        &[
            ("VOISU_TEST_CAPTURE_FINISH_FAILURE", "1"),
            ("VOISU_TEST_CAPTURE_ABORT_FAILURE", "1"),
        ],
    );
    assert!(voisu(runtime.path(), "start").status.success());

    let failed = voisu(runtime.path(), "stop");
    assert_eq!(failed.status.code(), Some(4));
    assert_eq!(stderr(&failed), "Recording capture failed\n");
    assert_eq!(stdout(&voisu(runtime.path(), "status")), "idle\n");

    let diagnostics = daemon.terminate_and_stderr();
    assert!(
        diagnostics.contains("capture abort failed")
            && diagnostics.contains("controlled-abort-detail"),
        "finalization-path abort failure must be surfaced, got: {diagnostics}"
    );
}

#[test]
fn cli_read_has_a_deadline_when_the_daemon_never_responds() {
    let runtime = TempDir::new().unwrap();
    let path = socket_path(runtime.path());
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::set_permissions(runtime.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let listener = UnixListener::bind(&path).unwrap();

    // A silent server that reads the request but never replies.
    let server = thread::spawn(move || {
        if let Ok((stream, _)) = listener.accept() {
            let mut request = String::new();
            let _ = BufReader::new(stream.try_clone().unwrap()).read_line(&mut request);
            thread::sleep(Duration::from_secs(4));
            drop(stream);
        }
    });

    let started = Instant::now();
    let status = voisu(runtime.path(), "status");
    let elapsed = started.elapsed();
    assert!(!status.status.success());
    assert!(
        elapsed >= Duration::from_millis(1800) && elapsed < Duration::from_secs(4),
        "CLI must honor a bounded read deadline, elapsed {elapsed:?}"
    );
    server.join().unwrap();
}

#[test]
fn cli_read_deadline_covers_the_whole_frame_even_under_trickled_traffic() {
    let runtime = TempDir::new().unwrap();
    let path = socket_path(runtime.path());
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::set_permissions(runtime.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let listener = UnixListener::bind(&path).unwrap();

    // A trickle server: one byte every 250ms, never a terminator. A per-read
    // timeout alone would be reset by every byte and wait forever.
    let server = thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let mut request = String::new();
            let _ = BufReader::new(stream.try_clone().unwrap()).read_line(&mut request);
            for _ in 0..24 {
                if stream.write_all(b"x").is_err() {
                    break;
                }
                thread::sleep(Duration::from_millis(250));
            }
        }
    });

    let started = Instant::now();
    let status = voisu(runtime.path(), "status");
    let elapsed = started.elapsed();
    assert!(!status.status.success());
    assert!(
        elapsed >= Duration::from_millis(1800) && elapsed < Duration::from_secs(4),
        "CLI must honor a whole-frame deadline under trickled traffic, elapsed {elapsed:?}"
    );
    server.join().unwrap();
}

#[test]
fn a_stalled_provider_send_does_not_prevent_stop_from_completing() {
    let runtime = TempDir::new().unwrap();
    let _daemon = Daemon::start_with_env(
        runtime.path(),
        &[
            ("VOISU_TEST_CAPTURE_CHUNKS", "1"),
            ("VOISU_TEST_PROVIDER_SEND_STALL_MS", "30000"),
        ],
    );

    assert!(voisu(runtime.path(), "start").status.success());
    // Let the capture pump enter the stalled provider send.
    thread::sleep(Duration::from_millis(100));

    let started = Instant::now();
    let stop = voisu(runtime.path(), "stop");
    let elapsed = started.elapsed();
    assert!(stop.status.success(), "{}", stderr(&stop));
    assert!(
        elapsed < Duration::from_secs(5),
        "stop must not wait on a stalled provider send, elapsed {elapsed:?}"
    );
    assert_eq!(stdout(&voisu(runtime.path(), "status")), "idle\n");
}

#[test]
fn a_stalled_partial_start_abort_keeps_the_daemon_responsive() {
    let runtime = TempDir::new().unwrap();
    let daemon = Daemon::start_with_env(
        runtime.path(),
        &[
            ("VOISU_TEST_PROVIDER_START_FAILURE", "1"),
            ("VOISU_TEST_CAPTURE_ABORT_STALL_MS", "30000"),
        ],
    );

    // The partial-start failure triggers a capture abort that stalls; the start
    // reply and every subsequent command must not wait on it.
    let started = Instant::now();
    let failed = voisu(runtime.path(), "start");
    assert_eq!(failed.status.code(), Some(4));
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "start rejection must not wait on the stalled abort"
    );

    let status_started = Instant::now();
    assert_eq!(stdout(&voisu(runtime.path(), "status")), "idle\n");
    assert!(
        status_started.elapsed() < Duration::from_millis(500),
        "status must stay responsive while the abort is stalled"
    );

    // The bounded abort must surface its timeout into local diagnostics.
    thread::sleep(Duration::from_millis(2300));
    let diagnostics = daemon.terminate_and_stderr();
    assert!(
        diagnostics.contains("capture abort timed out"),
        "abort timeout must be surfaced, got: {diagnostics}"
    );
}

#[test]
fn cli_reports_version_mismatch_from_the_envelope_even_for_incompatible_payloads() {
    let runtime = TempDir::new().unwrap();
    let path = socket_path(runtime.path());
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::set_permissions(runtime.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let listener = UnixListener::bind(&path).unwrap();

    // Version-mismatched AND schema-incompatible for this CLI's Response.
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = String::new();
        BufReader::new(stream.try_clone().unwrap())
            .read_line(&mut request)
            .unwrap();
        stream
            .write_all(b"{\"version\":999,\"ok\":\"not-a-bool\",\"payload\":{\"future\":true}}\n")
            .unwrap();
    });

    let status = voisu(runtime.path(), "status");
    server.join().unwrap();
    assert!(!status.status.success());
    assert_eq!(
        stderr(&status),
        "IPC protocol mismatch: daemon uses 999, CLI uses 1\n"
    );
}

#[test]
fn oversized_and_slow_frames_do_not_block_or_kill_the_daemon() {
    let runtime = TempDir::new().unwrap();
    let _daemon = Daemon::start(runtime.path());
    let path = socket_path(runtime.path());

    let mut slow = UnixStream::connect(&path).unwrap();
    slow.write_all(b"{\"version\":1").unwrap();
    let status_started = Instant::now();
    assert_eq!(stdout(&voisu(runtime.path(), "status")), "idle\n");
    assert!(status_started.elapsed() < Duration::from_millis(250));
    slow.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    let deadline_started = Instant::now();
    let mut closed = String::new();
    BufReader::new(slow).read_line(&mut closed).unwrap();
    assert!(closed.is_empty());
    assert!(deadline_started.elapsed() < Duration::from_millis(2500));

    let mut oversized = UnixStream::connect(&path).unwrap();
    oversized.write_all(&vec![b'x'; 20 * 1024]).unwrap();
    oversized.write_all(b"\n").unwrap();
    oversized
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    let mut response = String::new();
    let _ = BufReader::new(oversized).read_line(&mut response);
    assert!(response.is_empty());
    assert_eq!(stdout(&voisu(runtime.path(), "status")), "idle\n");
}
