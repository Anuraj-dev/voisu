//! L6 packaging/rollback contracts. Host gates stay unclaimed.
#![allow(clippy::zombie_processes)]

use std::fs;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use tempfile::TempDir;
use voisu_app::local_model::{
    InstallConsent, ci_fixture_entry, encode_receipt, from_entry, retained_receipt_path,
    store_receipt_atomic,
};
use voisu_app::local_packaging::{
    BinaryIdentity, BinaryRollbackError, USER_OWNED_HINT, binary_rollback, package_may_remove,
    user_owned_trees,
};
use voisu_app::local_setup::{LocalSetupActions, LocalSetupOutcome, run_with};
use voisu_app::local_worker::{FakeWorker, SupervisorError, WorkerSupervisor};
use voisu_app::setup::WizardIo;
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
                    let _ = stderr.read_to_string(&mut diagnostics);
                }
                panic!("daemon exited early: {status}; {diagnostics}");
            }
            thread::sleep(Duration::from_millis(20));
        }
        let _ = child.kill();
        panic!("daemon did not become ready");
    }

    fn config_file(&self) -> PathBuf {
        self.config.path().join("voisu").join("config.toml")
    }

    fn models_root(&self) -> PathBuf {
        self.state.path().join("voisu").join("models")
    }
}

struct Daemon {
    child: Child,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|path| path.parent())
        .expect("workspace")
        .to_path_buf()
}

#[test]
fn clean_install_ships_no_production_weights_and_defaults_cloud() {
    let harness = Harness::new();
    let _daemon = harness.start_daemon();
    let status = harness.voisu(&["status"]);
    assert!(status.status.success(), "{}", stdout(&status));
    let printed = stdout(&status);
    assert!(
        printed.contains("asr mode pending:") || printed.contains("idle"),
        "{printed}"
    );
    assert!(
        printed.contains("local readiness:") || printed.contains("idle"),
        "{printed}"
    );
    assert!(
        !harness
            .models_root()
            .join("private/active.receipt")
            .exists()
    );
    let catalog = voisu_app::local_model::shipped_catalog();
    assert!(voisu_app::local_model::bakeoff_winner(&catalog).is_none());
    assert!(
        catalog
            .entries
            .iter()
            .all(|entry| !entry.id.to_ascii_lowercase().contains("ollama"))
    );
}

#[test]
fn offline_local_selection_survives_daemon_restart() {
    let harness = Harness::new();
    let set = harness.voisu(&["mode", "local"]);
    assert!(set.status.success(), "{}", stdout(&set));
    let first = harness.start_daemon();
    drop(first);
    let _second = harness.start_daemon();
    let status = harness.voisu(&["status"]);
    assert!(status.status.success());
    let printed = stdout(&status);
    assert!(printed.contains("local"), "{printed}");
    let config = fs::read_to_string(harness.config_file()).unwrap();
    assert!(config.contains("asr_mode = \"local\""), "{config}");
}

#[test]
fn upgrade_preserves_private_settings_and_receipts() {
    let harness = Harness::new();
    assert!(harness.voisu(&["mode", "local"]).status.success());
    assert!(harness.voisu(&["deepgram", "off"]).status.success());
    assert!(harness.voisu(&["writing", "literal"]).status.success());
    assert!(
        harness
            .voisu(&["dictionary", "add", "raja"])
            .status
            .success()
    );
    let store = voisu_app::local_model::ModelStore::open(harness.models_root()).unwrap();
    let receipt = from_entry(ci_fixture_entry(), "kept");
    store_receipt_atomic(store.root(), &receipt).unwrap();
    let recovery = harness
        .state
        .path()
        .join("voisu")
        .join("local-recovery")
        .join("keep.wav");
    fs::create_dir_all(recovery.parent().unwrap()).unwrap();
    fs::write(&recovery, b"pcm").unwrap();

    let _daemon = harness.start_daemon();
    let config = fs::read_to_string(harness.config_file()).unwrap();
    assert!(config.contains("asr_mode = \"local\""), "{config}");
    assert!(config.contains("deepgram_enabled = false"), "{config}");
    assert!(config.contains("writing_mode = \"literal\""), "{config}");
    let dict = harness.config.path().join("voisu").join("dictionary.txt");
    assert!(fs::read_to_string(&dict).unwrap().contains("raja"));
    assert_eq!(store.load_active().unwrap().unwrap().artifact_id, "kept");
    assert_eq!(fs::read(&recovery).unwrap(), b"pcm");
}

#[test]
fn cloud_switch_is_opt_out_not_a_model_repair() {
    let harness = Harness::new();
    let store = voisu_app::local_model::ModelStore::open(harness.models_root()).unwrap();
    let broken = from_entry(ci_fixture_entry(), "broken");
    let retained = from_entry(ci_fixture_entry(), "kept");
    store_receipt_atomic(store.root(), &broken).unwrap();
    fs::write(
        retained_receipt_path(store.root()),
        encode_receipt(&retained),
    )
    .unwrap();
    let cloud = harness.voisu(&["mode", "cloud"]);
    assert!(cloud.status.success(), "{}", stdout(&cloud));
    let printed = stdout(&cloud);
    assert!(printed.contains("opt-out"), "{printed}");
    assert!(printed.contains("not a Local repair"), "{printed}");
    assert_eq!(store.load_active().unwrap().unwrap().artifact_id, "broken");
}

struct RestoreIo {
    lines: Vec<String>,
    out: Vec<String>,
}

impl WizardIo for RestoreIo {
    fn writeln(&mut self, line: &str) {
        self.out.push(line.to_owned());
    }
    fn prompt_line(&mut self, prompt: &str) -> Option<String> {
        self.out.push(prompt.to_owned());
        if self.lines.is_empty() {
            None
        } else {
            Some(self.lines.remove(0))
        }
    }
    fn prompt_secret(&mut self, _prompt: &str) -> Option<String> {
        panic!("Local Setup must not prompt for Cloud keys");
    }
}

struct RestoreActions {
    restored: bool,
}

impl LocalSetupActions for RestoreActions {
    fn catalog_lines(&self) -> Vec<String> {
        vec!["fixture".into()]
    }
    fn active_identity(&self) -> Option<String> {
        Some("broken".into())
    }
    fn retained_identity(&self) -> Option<String> {
        Some("l3-health-fixture".into())
    }
    fn restore_retained(&mut self) -> Result<String, String> {
        self.restored = true;
        Ok("l3-health-fixture".into())
    }
    fn install_fixture(
        &mut self,
        _consent: voisu_app::local_model::InstallConsent,
    ) -> Result<String, String> {
        Err("repair must not download".into())
    }
}

#[test]
fn retained_model_restore_is_explicit_setup() {
    let mut io = RestoreIo {
        lines: vec!["y".into()],
        out: Vec::new(),
    };
    let mut actions = RestoreActions { restored: false };
    let outcome = run_with(&mut io, &mut actions).unwrap();
    assert_eq!(outcome, LocalSetupOutcome::Restored);
    assert!(actions.restored);
    // Repair restores the retained receipt; it never takes the download path.
    assert!(
        actions
            .install_fixture(InstallConsent {
                bytes: 0,
                license_spdx: "MIT".into(),
            })
            .is_err()
    );
    let transcript = io.out.join("\n");
    assert!(transcript.contains("without network"));
    assert!(!transcript.to_ascii_lowercase().contains("ollama"));
}

#[test]
fn worker_death_does_not_select_cloud() {
    let mut supervisor = WorkerSupervisor::<FakeWorker>::absent();
    let worker = FakeWorker {
        crash: true,
        ..FakeWorker::default()
    };
    supervisor.attach_ready(worker, Instant::now()).unwrap();
    let error = supervisor
        .transcribe(voisu_app::local_worker::TranscribeRequest {
            correlation: voisu_app::local_worker::Correlation {
                daemon_nonce: "l6".into(),
                generation: 1,
                request_id: "1".into(),
                recording_id: "1".into(),
                model_receipt_hash: "hash".into(),
            },
            pcm: vec![1, 0],
        })
        .unwrap_err();
    assert!(matches!(error, SupervisorError::Crashed));
    assert_eq!(
        voisu_app::local_packaging::mode_after_worker_death(voisu_core::AsrMode::Local),
        voisu_core::AsrMode::Local
    );
}

#[test]
fn supported_binary_rollback_rejects_pre_mode_releases() {
    let current = BinaryIdentity {
        version: env!("CARGO_PKG_VERSION"),
        protocol: PROTOCOL_VERSION,
        capabilities: &[ASR_MODE_V1],
    };
    let compatible = BinaryIdentity {
        version: "0.56.0",
        protocol: PROTOCOL_VERSION,
        capabilities: &[ASR_MODE_V1],
    };
    let pre_mode = BinaryIdentity {
        version: "0.43.2",
        protocol: PROTOCOL_VERSION,
        capabilities: &[],
    };
    assert!(binary_rollback(&current, &compatible).is_ok());
    assert_eq!(
        binary_rollback(&current, &pre_mode),
        Err(BinaryRollbackError::PreModeRequiresMigration)
    );
}

#[test]
fn uninstall_file_lists_omit_user_owned_trees() {
    let root = workspace_root();
    let spec = fs::read_to_string(root.join("packaging/voisu.spec")).unwrap();
    let files = spec.split("%files").nth(1).unwrap_or(&spec);
    for tree in user_owned_trees() {
        assert!(
            !files.contains(tree.relative),
            "RPM must not package {}",
            tree.relative
        );
    }
    assert!(!package_may_remove(std::path::Path::new(
        "/home/raja/.local/state/voisu/models"
    )));
    let postrm = fs::read_to_string(root.join("packaging/deb/postrm")).unwrap();
    let aur = fs::read_to_string(root.join("packaging/aur/voisu/voisu.install")).unwrap();
    for body in [&postrm, &aur] {
        assert!(body.contains("untouched"), "{body}");
        assert!(body.contains(".config/voisu"), "{body}");
        assert!(body.contains(".local/state/voisu"), "{body}");
        assert!(body.contains(".local/share/voisu"), "{body}");
        assert!(body.contains("leased"), "{body}");
    }
    assert!(USER_OWNED_HINT.contains("leased artifacts"));
}

#[test]
fn daemon_and_cli_stay_call_sites() {
    let daemon = include_str!("../src/bin/voisu-daemon.rs");
    let cli = include_str!("../src/bin/voisu.rs");
    assert!(
        !daemon.contains("local_packaging"),
        "packaging contracts stay out of voisu-daemon.rs"
    );
    assert!(
        !cli.contains("local_packaging"),
        "packaging contracts stay out of voisu.rs"
    );
    assert!(cli.contains("mode <local|cloud>"));
}
