use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
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
        let mut child = Command::new(env!("CARGO_BIN_EXE_voisu-daemon"))
            .env("XDG_RUNTIME_DIR", runtime_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("daemon should start");

        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if socket_path(runtime_dir).exists() {
                return Self { child };
            }
            if let Some(status) = child.try_wait().expect("daemon status should be readable") {
                panic!("daemon exited before binding its socket: {status}");
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("daemon did not bind its socket");
    }
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
    Command::new(env!("CARGO_BIN_EXE_voisu"))
        .arg(command)
        .env("XDG_RUNTIME_DIR", runtime_dir)
        .output()
        .expect("CLI should run")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
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
}

#[test]
fn stop_completes_recording_and_delivery_then_returns_to_idle() {
    let runtime = TempDir::new().unwrap();
    let _daemon = Daemon::start(runtime.path());

    let start = voisu(runtime.path(), "start");
    assert!(start.status.success(), "{}", stderr(&start));
    assert_eq!(stdout(&start), "Recording started\n");

    let stop = voisu(runtime.path(), "stop");
    assert!(stop.status.success(), "{}", stderr(&stop));
    assert_eq!(
        stdout(&stop),
        "Recording completed; Transcript delivered\n"
    );

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
