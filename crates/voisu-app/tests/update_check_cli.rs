//! `voisu update-check` turns the daily update check on and off through the
//! real CLI and reads it back.

use std::process::{Command, Output};

#[test]
fn update_check_round_trips_through_the_cli_and_defaults_to_on() {
    let temp = tempfile::tempdir().expect("config tempdir");
    let run = |arguments: &[&str]| -> Output {
        Command::new(env!("CARGO_BIN_EXE_voisu"))
            .args(arguments)
            .env("XDG_CONFIG_HOME", temp.path())
            .output()
            .expect("run voisu CLI")
    };
    let shown = |output: &Output| String::from_utf8_lossy(&output.stdout).into_owned();

    let fresh = run(&["update-check"]);
    assert!(fresh.status.success(), "{fresh:?}");
    assert!(shown(&fresh).starts_with("Update check: on"), "{fresh:?}");

    for state in ["off", "on", "off"] {
        let set = run(&["update-check", state]);
        assert!(set.status.success(), "set {state}: {set:?}");
        assert!(
            shown(&set).starts_with(&format!("Update check turned {state}")),
            "{set:?}"
        );
        let query = run(&["update-check"]);
        assert!(
            shown(&query).starts_with(&format!("Update check: {state}")),
            "{query:?}"
        );
    }

    let config = std::fs::read_to_string(temp.path().join("voisu").join("config.toml"))
        .expect("config written");
    assert!(config.contains("update_check = \"off\""), "{config}");

    let invalid = run(&["update-check", "maybe"]);
    assert_eq!(invalid.status.code(), Some(2), "{invalid:?}");
    assert!(
        String::from_utf8_lossy(&invalid.stderr).contains("update-check must be on or off"),
        "{invalid:?}"
    );
}
