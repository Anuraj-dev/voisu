//! L5 status/doctor/Setup presentation: pending vs active, skipped Cloud probes.
#![allow(clippy::zombie_processes)]

use std::fs;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use tempfile::TempDir;
use voisu_core::PROTOCOL_VERSION;

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

#[test]
fn status_exposes_pending_active_revision_capability_and_unavailable() {
    let harness = Harness::new();
    let _daemon = harness.start_daemon();
    assert!(harness.voisu(&["mode", "local"]).status.success());
    let printed = stdout(&harness.voisu(&["status"]));
    assert!(printed.contains("asr mode pending: local"), "{printed}");
    assert!(printed.contains("config revision:"), "{printed}");
    assert!(printed.contains("asr capability: asr-mode-v1"), "{printed}");
    assert!(
        printed.contains("local readiness: unavailable (Local selected; model unavailable)"),
        "{printed}"
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
fn pending_local_does_not_make_an_active_cloud_recording_offline() {
    let harness = Harness::new();
    let _daemon = harness.start_daemon();
    assert!(harness.voisu(&["start"]).status.success());
    assert!(harness.voisu(&["mode", "local"]).status.success());
    let printed = stdout(&harness.voisu(&["status"]));
    assert!(printed.starts_with("Recording\n"), "{printed}");
    assert!(printed.contains("asr mode pending: local"), "{printed}");
    assert!(printed.contains("asr mode active: cloud"), "{printed}");
    assert!(
        printed.contains("recording path: cloud (this Recording is not offline; Local is pending)"),
        "{printed}"
    );
    let _ = harness.voisu(&["stop"]);
}

#[test]
fn local_doctor_skips_cloud_probes_with_explicit_skip_rows() {
    let harness = Harness::new();
    assert!(harness.voisu(&["mode", "local"]).status.success());
    let doctor = harness
        .command(env!("CARGO_BIN_EXE_voisu"))
        .args(["doctor"])
        .env("VOISU_TEST_READINESS", "pass")
        .env("VOISU_TEST_FOCUS_BACKEND", "hyprland")
        .output()
        .expect("doctor");
    let printed = stdout(&doctor);
    assert!(printed.contains("Deepgram key"), "{printed}");
    assert!(printed.contains("not probed"), "{printed}");
    assert!(printed.contains("SKIP"), "{printed}");
    assert!(printed.contains("Groq key"), "{printed}");
    assert!(
        !printed.contains("valid"),
        "Cloud key probe results must not be hidden behind a skip: {printed}"
    );
}

#[test]
fn local_setup_does_not_prompt_for_cloud_keys() {
    let harness = Harness::new();
    assert!(harness.voisu(&["mode", "local"]).status.success());
    let setup = harness
        .command(env!("CARGO_BIN_EXE_voisu"))
        .args(["setup"])
        .env("VOISU_TEST_SETUP_WIZARD_ONLY", "1")
        .stdin(Stdio::null())
        .output()
        .expect("setup");
    let combined = format!("{}{}", stdout(&setup), stderr(&setup));
    assert!(
        combined.contains("Local Setup") || combined.contains("never run from Local setup"),
        "{combined}"
    );
    assert!(
        !combined.contains("Enter your Deepgram API key"),
        "{combined}"
    );
    assert!(!combined.contains("Enter your Groq API key"), "{combined}");
    assert!(
        !combined.contains("Download and install the catalog fixture"),
        "{combined}"
    );
    assert!(
        combined.contains("no bakeoff winner; production weights are not downloaded"),
        "{combined}"
    );
}
