//! The interface changes settings through the same calls the CLI uses, so the
//! result on disk must be what the CLI would read back. This is one test on
//! purpose: it points the whole process at temporary directories.

use std::os::unix::fs::PermissionsExt;
use std::process::Command;
use std::sync::mpsc::channel;

use tempfile::TempDir;
use voisu_app::config::{DeliveryMode, RenderingPolicy, WritingMode};
use voisu_app::tui::model::{Job, Reply, Tone};
use voisu_app::tui::worker::execute;
use voisu_core::AsrMode;

fn run(job: Job) -> Vec<Reply> {
    let (replies, received) = channel();
    execute(job, &replies);
    drop(replies);
    received.iter().collect()
}

fn toast(job: Job) -> (Tone, String) {
    let mut replies = run(job);
    match replies.pop() {
        Some(Reply::Applied { kind, text, .. }) => (kind, text),
        other => panic!("expected a result in words, got {other:?}"),
    }
}

fn cli(dirs: &TempDir, args: &[&str]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_voisu"))
        .args(args)
        .env("HOME", dirs.path())
        .env("XDG_CONFIG_HOME", dirs.path().join("config"))
        .env("XDG_STATE_HOME", dirs.path().join("state"))
        .env("XDG_RUNTIME_DIR", dirs.path().join("runtime"))
        .output()
        .expect("voisu should run");
    assert!(output.status.success(), "{args:?}: {output:?}");
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn the_interface_and_the_cli_read_and_write_the_same_settings() {
    let dirs = TempDir::new().expect("temporary home");
    for dir in ["config", "state", "runtime"] {
        std::fs::create_dir_all(dirs.path().join(dir)).expect("temporary dir");
    }
    // The daemon socket's directory is refused unless it is private.
    std::fs::set_permissions(
        dirs.path().join("runtime"),
        std::fs::Permissions::from_mode(0o700),
    )
    .expect("private runtime dir");
    // SAFETY: this test binary has this one test, and it runs before any other
    // thread reads the environment.
    unsafe {
        std::env::set_var("HOME", dirs.path());
        std::env::set_var("XDG_CONFIG_HOME", dirs.path().join("config"));
        std::env::set_var("XDG_STATE_HOME", dirs.path().join("state"));
        std::env::set_var("XDG_RUNTIME_DIR", dirs.path().join("runtime"));
        std::env::set_var("VOISU_TEST_SECRET_STORE", "unavailable");
    }

    // A setting lands where the CLI reads it, and the toast says so in words.
    let (kind, text) = toast(Job::SetDelivery(DeliveryMode::Copy));
    assert_eq!(kind, Tone::Success, "{text}");
    assert!(text.starts_with("Delivery set to Copy."), "{text}");
    assert!(text.ends_with("Restart Voisu to apply."), "{text}");
    assert!(cli(&dirs, &["delivery"]).contains("copy"));

    // The daemon picks its Intent Reconstruction validator at start.
    let (kind, text) = toast(Job::SetWriting(WritingMode::Literal));
    assert_eq!(kind, Tone::Success, "{text}");
    assert!(text.ends_with("Restart Voisu to apply it fully."), "{text}");
    assert!(cli(&dirs, &["writing"]).contains("exact"));

    let (kind, text) = toast(Job::SetRendering(RenderingPolicy::Structured));
    assert_eq!(kind, Tone::Success, "{text}");
    assert!(
        text.ends_with("It applies to your next Recording."),
        "{text}"
    );
    assert!(cli(&dirs, &["layout"]).contains("sections"));

    // The dictionary is shared the same way, in both directions.
    let (kind, text) = toast(Job::AddTerm("Anuraj".to_owned()));
    assert_eq!(kind, Tone::Success, "{text}");
    assert_eq!(text, "Added \"Anuraj\".");
    assert!(cli(&dirs, &["dictionary", "list"]).contains("Anuraj"));
    let (kind, _) = toast(Job::AddTerm("Anuraj".to_owned()));
    assert_eq!(kind, Tone::Caution, "adding twice changes nothing");
    let (kind, text) = toast(Job::RemoveTerm("Anuraj".to_owned()));
    assert_eq!(
        (kind, text.as_str()),
        (Tone::Success, "Removed \"Anuraj\".")
    );
    assert!(!cli(&dirs, &["dictionary", "list"]).contains("Anuraj"));

    // Consent is never bypassed: no install and no daemon means no switch.
    let (kind, text) = toast(Job::SetEngine {
        mode: AsrMode::Local,
        local_ready: false,
    });
    assert_eq!(kind, Tone::Caution);
    assert!(text.contains("Run setup"), "{text}");
    assert!(
        !voisu_app::local_setup::is_local_mode(),
        "Engine must stay on Cloud"
    );

    // With no daemon, History says why in words and the poll reports the absence.
    let replies = run(Job::LoadHistory);
    assert!(
        matches!(&replies[..], [Reply::History(Err(message))] if message.contains("isn't running")),
        "{replies:?}"
    );
    let replies = run(Job::LoadShortcut);
    assert!(
        matches!(&replies[..], [Reply::Shortcut(None)]),
        "{replies:?}"
    );
}
