//! `voisu local-unload` sets how long an idle Local worker stays loaded,
//! through the real CLI, and reads it back.

use std::process::{Command, Output};

#[test]
fn local_unload_round_trips_through_the_cli_and_defaults_to_five_minutes() {
    let temp = tempfile::tempdir().expect("config tempdir");
    let run = |arguments: &[&str]| -> Output {
        Command::new(env!("CARGO_BIN_EXE_voisu"))
            .args(arguments)
            .env("XDG_CONFIG_HOME", temp.path())
            .output()
            .expect("run voisu CLI")
    };
    let shown = |output: &Output| String::from_utf8_lossy(&output.stdout).into_owned();

    let fresh = run(&["local-unload"]);
    assert!(fresh.status.success(), "{fresh:?}");
    assert!(
        shown(&fresh).starts_with("Local unload: after 5 min"),
        "{fresh:?}"
    );

    for (input, label) in [
        ("90", "after 90 s"),
        ("off", "never"),
        ("never", "never"),
        ("1800", "after 30 min"),
    ] {
        let set = run(&["local-unload", input]);
        assert!(set.status.success(), "set {input}: {set:?}");
        assert!(
            shown(&set).starts_with(&format!("Local unload set to {label}")),
            "{set:?}"
        );
        let query = run(&["local-unload"]);
        assert!(
            shown(&query).starts_with(&format!("Local unload: {label}")),
            "{query:?}"
        );
    }

    let config = std::fs::read_to_string(temp.path().join("voisu").join("config.toml"))
        .expect("config written");
    assert!(config.contains("local_idle_unload = 1800"), "{config}");

    for invalid in ["29", "soon", "86401"] {
        let refused = run(&["local-unload", invalid]);
        assert_eq!(refused.status.code(), Some(2), "{invalid}: {refused:?}");
        assert!(
            String::from_utf8_lossy(&refused.stderr)
                .contains("expected off, never, or whole seconds from 30 to 86400"),
            "{refused:?}"
        );
    }
    let kept = run(&["local-unload"]);
    assert!(
        shown(&kept).starts_with("Local unload: after 30 min"),
        "{kept:?}"
    );
}
