//! `voisu tone` persists the Tone through the real CLI and reads it back.

use std::process::{Command, Output};

#[test]
fn tone_round_trips_through_the_cli_and_defaults_to_formal() {
    let temp = tempfile::tempdir().expect("config tempdir");
    let run = |arguments: &[&str]| -> Output {
        Command::new(env!("CARGO_BIN_EXE_voisu"))
            .args(arguments)
            .env("XDG_CONFIG_HOME", temp.path())
            .output()
            .expect("run voisu CLI")
    };
    let shown = |output: &Output| String::from_utf8_lossy(&output.stdout).into_owned();

    let fresh = run(&["tone"]);
    assert!(fresh.status.success(), "{fresh:?}");
    assert!(shown(&fresh).starts_with("Tone: formal ("), "{fresh:?}");

    for tone in ["informal", "formal", "informal"] {
        let set = run(&["tone", tone]);
        assert!(set.status.success(), "set {tone}: {set:?}");
        assert!(shown(&set).starts_with(&format!("Tone set to {tone} (")));
        let query = run(&["tone"]);
        assert!(
            shown(&query).starts_with(&format!("Tone: {tone} (")),
            "{query:?}"
        );
    }

    let config = std::fs::read_to_string(temp.path().join("voisu").join("config.toml"))
        .expect("config written");
    assert!(config.contains("tone = \"informal\""), "{config}");

    let invalid = run(&["tone", "casual"]);
    assert_eq!(invalid.status.code(), Some(2), "{invalid:?}");
    assert!(
        String::from_utf8_lossy(&invalid.stderr).contains("tone must be formal or informal"),
        "{invalid:?}"
    );
}
