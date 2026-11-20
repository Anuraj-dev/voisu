//! `voisu overlay-position` persists the Overlay's spot through the real CLI
//! and reads it back.

use std::process::{Command, Output};

#[test]
fn overlay_position_round_trips_through_the_cli_and_defaults_to_top() {
    let temp = tempfile::tempdir().expect("config tempdir");
    let run = |arguments: &[&str]| -> Output {
        Command::new(env!("CARGO_BIN_EXE_voisu"))
            .args(arguments)
            .env("XDG_CONFIG_HOME", temp.path())
            .output()
            .expect("run voisu CLI")
    };
    let shown = |output: &Output| String::from_utf8_lossy(&output.stdout).into_owned();

    let fresh = run(&["overlay-position"]);
    assert!(fresh.status.success(), "{fresh:?}");
    assert_eq!(shown(&fresh), "Overlay position: top\n");

    for position in [
        "bottom",
        "top-left",
        "top-right",
        "bottom-left",
        "bottom-right",
        "top",
    ] {
        let set = run(&["overlay-position", position]);
        assert!(set.status.success(), "set {position}: {set:?}");
        assert_eq!(
            shown(&set),
            format!("Overlay position set to {position}. It applies to your next Recording.\n")
        );
        let query = run(&["overlay-position"]);
        assert_eq!(
            shown(&query),
            format!("Overlay position: {position}\n"),
            "{query:?}"
        );
    }

    let set = run(&["overlay-position", "bottom-left"]);
    assert!(set.status.success(), "{set:?}");
    let config = std::fs::read_to_string(temp.path().join("voisu").join("config.toml"))
        .expect("config written");
    assert!(
        config.contains("overlay_position = \"bottom-left\""),
        "{config}"
    );

    let invalid = run(&["overlay-position", "middle"]);
    assert_eq!(invalid.status.code(), Some(2), "{invalid:?}");
    assert!(
        String::from_utf8_lossy(&invalid.stderr).contains("overlay-position must be top"),
        "{invalid:?}"
    );
    let unchanged = run(&["overlay-position"]);
    assert_eq!(shown(&unchanged), "Overlay position: bottom-left\n");
}

#[test]
fn a_hand_edited_bad_overlay_position_reads_as_the_default() {
    let temp = tempfile::tempdir().expect("config tempdir");
    let dir = temp.path().join("voisu");
    std::fs::create_dir_all(&dir).expect("config dir");
    std::fs::write(dir.join("config.toml"), "overlay_position = \"centre\"\n")
        .expect("write config");
    let output = Command::new(env!("CARGO_BIN_EXE_voisu"))
        .arg("overlay-position")
        .env("XDG_CONFIG_HOME", temp.path())
        .output()
        .expect("run voisu CLI");
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "Overlay position: top\n"
    );
}
