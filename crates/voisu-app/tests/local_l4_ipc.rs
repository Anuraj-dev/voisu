//! L4 Local routing IPC: refuse without Ready, ignore test env seam, Cloud regressions.
#![allow(clippy::zombie_processes)]

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use tempfile::TempDir;
use voisu_core::PROTOCOL_VERSION;

struct Harness {
    runtime: TempDir,
    config: TempDir,
    state: TempDir,
    data: TempDir,
}

impl Harness {
    fn new() -> Self {
        let runtime = TempDir::new().unwrap();
        fs::set_permissions(runtime.path(), fs::Permissions::from_mode(0o700)).unwrap();
        Self {
            runtime,
            config: TempDir::new().unwrap(),
            state: TempDir::new().unwrap(),
            data: TempDir::new().unwrap(),
        }
    }

    fn socket(&self) -> PathBuf {
        self.runtime
            .path()
            .join("voisu")
            .join(format!("v{PROTOCOL_VERSION}"))
            .join("daemon.sock")
    }

    fn command(&self, bin: &str) -> Command {
        let mut command = Command::new(bin);
        command
            .env("XDG_RUNTIME_DIR", self.runtime.path())
            .env("XDG_CONFIG_HOME", self.config.path())
            .env("XDG_STATE_HOME", self.state.path())
            .env("XDG_DATA_HOME", self.data.path())
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

    fn start_daemon(&self, local_ready: bool) -> Daemon {
        let mut command = self.command(env!("CARGO_BIN_EXE_voisu-daemon"));
        command
            .env("VOISU_TEST_MODE", "controlled")
            .env("VOISU_TEST_PW_RECORD_RAW", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        if local_ready {
            command.env("VOISU_TEST_LOCAL_READY", "1");
            command.env("VOISU_TEST_LOCAL_TEXT", "hello from local");
        }
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

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
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
fn local_without_ready_worker_still_refuses_before_capture() {
    let harness = Harness::new();
    let _daemon = harness.start_daemon(false);
    let set = harness.voisu(&["mode", "local"]);
    assert!(set.status.success(), "{}", stderr(&set));
    let started = harness.voisu(&["start"]);
    assert_eq!(started.status.code(), Some(4), "{started:?}");
    assert!(
        stderr(&started).contains("refused before capture")
            || stderr(&started).contains("unavailable"),
        "{}",
        stderr(&started)
    );
}

#[test]
fn local_test_env_seam_is_ignored_in_production_binary() {
    // Behavioral proof for T1: the non-test daemon binary must ignore
    // VOISU_TEST_LOCAL_READY/VOISU_TEST_LOCAL_TEXT. Even with the seam set
    // via per-command env, Local stays unavailable and Start is refused
    // before capture; no scripted "hello" is admitted or delivered.
    let harness = Harness::new();
    let _daemon = harness.start_daemon(true);
    let set = harness.voisu(&["mode", "local"]);
    assert!(set.status.success(), "{}", stderr(&set));
    let status = ipc(&harness.socket(), r#"{"version":1,"command":"status"}"#);
    assert_eq!(status["asr_mode"]["pending"], "local");
    let state = status["asr_mode"]["local_readiness"]["state"]
        .as_str()
        .unwrap_or_default();
    assert_ne!(state, "ready", "{status}");
    assert_eq!(state, "unavailable", "{status}");
    let started = harness.voisu(&["start"]);
    assert_eq!(started.status.code(), Some(4), "{started:?}");
    let diagnostics = stderr(&started);
    assert!(
        diagnostics.contains("refused before capture") || diagnostics.contains("unavailable"),
        "{diagnostics}"
    );
    assert!(
        !diagnostics.contains("hello from local"),
        "scripted text must never reach Delivery: {diagnostics}"
    );
    assert!(
        !stdout(&started).contains("hello from local"),
        "scripted text must never reach Delivery"
    );
    let during = ipc(&harness.socket(), r#"{"version":1,"command":"status"}"#);
    assert_eq!(during["asr_mode"]["local_path_clean"], true, "{during}");
    let after = ipc(&harness.socket(), r#"{"version":1,"command":"status"}"#);
    assert_eq!(after["asr_mode"]["local_path_clean"], true, "{after}");
}

#[test]
fn cloud_start_still_works_after_local_mode_round_trip() {
    let harness = Harness::new();
    let _daemon = harness.start_daemon(false);
    assert!(harness.voisu(&["mode", "local"]).status.success());
    assert!(harness.voisu(&["mode", "cloud"]).status.success());
    let started = harness.voisu(&["start"]);
    assert!(started.status.success(), "{}", stderr(&started));
    let printed = stdout(&harness.voisu(&["status"]));
    assert!(printed.contains("asr mode active: cloud"), "{printed}");
    let _ = harness.voisu(&["stop"]);
}
