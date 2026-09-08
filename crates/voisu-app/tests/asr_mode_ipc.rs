//! ASR mode IPC: old/new client matrix, daemon races, and Local admission.
#![allow(clippy::zombie_processes)]

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use tempfile::TempDir;
use voisu_core::{ASR_MODE_V1, PROTOCOL_VERSION};

struct Harness {
    runtime: TempDir,
    config: TempDir,
    state: TempDir,
}

impl Harness {
    fn new() -> Self {
        let runtime = TempDir::new().unwrap();
        fs::set_permissions(runtime.path(), fs::Permissions::from_mode(0o700)).unwrap();
        Self {
            runtime,
            config: TempDir::new().unwrap(),
            state: TempDir::new().unwrap(),
        }
    }

    fn socket(&self) -> PathBuf {
        self.runtime
            .path()
            .join("voisu")
            .join(format!("v{PROTOCOL_VERSION}"))
            .join("daemon.sock")
    }

    fn config_file(&self) -> PathBuf {
        self.config.path().join("voisu").join("config.toml")
    }

    fn command(&self, bin: &str) -> Command {
        let mut command = Command::new(bin);
        command
            .env("XDG_RUNTIME_DIR", self.runtime.path())
            .env("XDG_CONFIG_HOME", self.config.path())
            .env("XDG_STATE_HOME", self.state.path())
            .env("HOME", self.config.path())
            .env("VOISU_DISABLE_SHORTCUTS", "1")
            .env("VOISU_DISABLE_DIRECT_DELIVERY", "1")
            .env("VOISU_ENABLE_INTENT_RECONSTRUCTION", "0")
            .env_remove("VOISU_ENABLE_DPR")
            .env_remove("VOISU_ENABLE_QWEN_FORMAT");
        command
    }

    fn voisu(&self, args: &[&str]) -> Output {
        self.command(env!("CARGO_BIN_EXE_voisu"))
            .args(args)
            .output()
            .expect("voisu should run")
    }

    fn start_daemon(&self) -> Daemon {
        let mut command = self.command(env!("CARGO_BIN_EXE_voisu-daemon"));
        command
            .env("VOISU_TEST_MODE", "controlled")
            .env("VOISU_TEST_PW_RECORD_RAW", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let mut child = command.spawn().expect("daemon should start");
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            if self.socket().exists() {
                let status = self.voisu(&["status"]);
                if status.status.success() {
                    return Daemon { child };
                }
            }
            if let Some(status) = child.try_wait().expect("daemon status") {
                let mut diagnostics = String::new();
                if let Some(mut stderr) = child.stderr.take() {
                    stderr.read_to_string(&mut diagnostics).ok();
                }
                panic!("daemon exited before bind: {status}: {diagnostics}");
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("daemon did not become reachable");
    }
}

struct Daemon {
    child: Child,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = Command::new("kill")
            .args(["-TERM", &self.child.id().to_string()])
            .status();
        let _ = self.child.wait();
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn ipc(path: &Path, frame: &str) -> serde_json::Value {
    let mut stream = UnixStream::connect(path).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    stream.write_all(frame.as_bytes()).unwrap();
    stream.write_all(b"\n").unwrap();
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap()
}

#[test]
fn invalid_mode_argument_exits_two() {
    let harness = Harness::new();
    let output = harness.voisu(&["mode", "hybrid"]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(stderr(&output).contains("local or cloud"), "{output:?}");
    assert!(!harness.config_file().exists());
}

#[test]
fn matching_cli_and_daemon_set_cloud_and_keep_local_unavailable() {
    let harness = Harness::new();
    let _daemon = harness.start_daemon();
    let set = harness.voisu(&["mode", "cloud"]);
    assert!(set.status.success(), "{}", stderr(&set));
    assert!(stdout(&set).contains("cloud"), "{}", stdout(&set));

    let status = ipc(&harness.socket(), r#"{"version":1,"command":"status"}"#);
    assert!(
        status["capabilities"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value.as_str() == Some(ASR_MODE_V1)),
        "{status}"
    );
    assert_eq!(status["asr_mode"]["pending"], "cloud");
    assert_eq!(status["asr_mode"]["local_readiness"]["state"], "absent");

    let started = harness.voisu(&["start"]);
    assert!(started.status.success(), "{}", stderr(&started));
    let _ = harness.voisu(&["stop"]);
}

#[test]
fn local_mode_refuses_start_before_capture() {
    let harness = Harness::new();
    let _daemon = harness.start_daemon();
    let set = harness.voisu(&["mode", "local"]);
    assert!(set.status.success(), "{}", stderr(&set));

    let status = ipc(&harness.socket(), r#"{"version":1,"command":"status"}"#);
    assert_eq!(status["asr_mode"]["pending"], "local");
    assert_eq!(
        status["asr_mode"]["local_readiness"]["state"],
        "unavailable"
    );
    assert!(
        status["asr_mode"]["local_readiness"]["error"]
            .as_str()
            .unwrap()
            .contains("unavailable"),
        "{status}"
    );

    let started = harness.voisu(&["start"]);
    assert_eq!(started.status.code(), Some(4), "{started:?}");
    assert!(
        stderr(&started).contains("refused before capture"),
        "{}",
        stderr(&started)
    );
}

#[test]
fn new_cli_old_daemon_is_unsupported_and_leaves_config_untouched() {
    let harness = Harness::new();
    let parent = harness.socket().parent().unwrap().to_path_buf();
    fs::create_dir_all(&parent).unwrap();
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).unwrap();
    let listener = UnixListener::bind(harness.socket()).unwrap();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut line = String::new();
        BufReader::new(&stream).read_line(&mut line).unwrap();
        assert!(
            line.contains("\"status\""),
            "handshake must use status: {line}"
        );
        assert!(
            !line.contains("set_asr_mode"),
            "old daemon must not receive SetAsrMode: {line}"
        );
        let reply = format!(
            r#"{{"version":{PROTOCOL_VERSION},"ok":true,"state":"idle","message":"idle"}}"#
        );
        stream.write_all(reply.as_bytes()).unwrap();
        stream.write_all(b"\n").unwrap();
    });

    let output = harness.voisu(&["mode", "local"]);
    server.join().unwrap();
    assert_eq!(output.status.code(), Some(3), "{output:?}");
    assert!(
        stderr(&output).contains("does not support ASR mode"),
        "{}",
        stderr(&output)
    );
    assert!(!harness.config_file().exists());
}

#[test]
fn old_client_status_on_new_daemon_stays_readable() {
    let harness = Harness::new();
    let _daemon = harness.start_daemon();
    let status = ipc(&harness.socket(), r#"{"version":1,"command":"status"}"#);
    assert_eq!(status["ok"], true);
    assert_eq!(status["message"], "idle");
    assert!(status.get("asr_mode").is_some());
}

#[test]
fn offline_mode_persists_when_the_lifetime_lock_is_free() {
    let harness = Harness::new();
    let output = harness.voisu(&["mode", "local"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("next supported daemon start"),
        "{}",
        stdout(&output)
    );
    let contents = fs::read_to_string(harness.config_file()).unwrap();
    assert!(contents.contains("asr_mode = \"local\""), "{contents}");
    let marker = harness.state.path().join("voisu").join("mode-initialized");
    let marker = fs::read_to_string(marker).unwrap();
    assert!(!marker.contains("local"));
    assert!(!marker.contains("cloud"));
}

#[test]
fn held_lock_without_socket_does_not_persist_mode() {
    let harness = Harness::new();
    let parent = harness.socket().parent().unwrap().to_path_buf();
    fs::create_dir_all(&parent).unwrap();
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).unwrap();
    let lock_path = parent.join("daemon.lock");
    let lock = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .unwrap();
    unsafe {
        assert_eq!(
            libc::flock(
                std::os::fd::AsRawFd::as_raw_fd(&lock),
                libc::LOCK_EX | libc::LOCK_NB
            ),
            0
        );
    }
    let output = harness.voisu(&["mode", "local"]);
    assert_eq!(output.status.code(), Some(3), "{output:?}");
    assert!(!harness.config_file().exists());
    drop(lock);
}

#[test]
fn deleting_mode_after_explicit_local_never_admits_cloud() {
    let harness = Harness::new();
    assert!(harness.voisu(&["mode", "local"]).status.success());
    fs::remove_file(harness.config_file()).unwrap();
    let _daemon = harness.start_daemon();
    let started = harness.voisu(&["start"]);
    assert_eq!(started.status.code(), Some(4), "{started:?}");
    assert!(
        !stderr(&started).to_ascii_lowercase().contains("started"),
        "{}",
        stderr(&started)
    );
}
