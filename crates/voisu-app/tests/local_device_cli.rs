//! `voisu local-device` sets where the whisper.cpp Local model runs, through
//! the real CLI, and reads it back.

use std::process::{Command, Output};

#[test]
fn local_device_round_trips_through_the_cli_and_defaults_to_auto() {
    let temp = tempfile::tempdir().expect("config tempdir");
    let run = |arguments: &[&str]| -> Output {
        Command::new(env!("CARGO_BIN_EXE_voisu"))
            .args(arguments)
            .env("XDG_CONFIG_HOME", temp.path())
            .output()
            .expect("run voisu CLI")
    };
    let shown = |output: &Output| String::from_utf8_lossy(&output.stdout).into_owned();

    let fresh = run(&["local-device"]);
    assert!(fresh.status.success(), "{fresh:?}");
    assert!(shown(&fresh).starts_with("Local device: auto"), "{fresh:?}");

    for (input, label) in [
        ("cpu", "cpu"),
        ("GPU", "gpu"),
        ("auto", "auto"),
        ("cpu", "cpu"),
    ] {
        let set = run(&["local-device", input]);
        assert!(set.status.success(), "set {input}: {set:?}");
        assert!(
            shown(&set).starts_with(&format!("Local device set to {label}")),
            "{set:?}"
        );
        let query = run(&["local-device"]);
        assert!(
            shown(&query).starts_with(&format!("Local device: {label}")),
            "{query:?}"
        );
    }

    let config = std::fs::read_to_string(temp.path().join("voisu").join("config.toml"))
        .expect("config written");
    assert!(config.contains("local_device = \"cpu\""), "{config}");

    for invalid in ["vulkan", "cuda:0", ""] {
        let refused = run(&["local-device", invalid]);
        assert_eq!(refused.status.code(), Some(2), "{invalid}: {refused:?}");
        assert!(
            String::from_utf8_lossy(&refused.stderr).contains("expected auto, gpu or cpu"),
            "{refused:?}"
        );
    }
    let kept = run(&["local-device"]);
    assert!(shown(&kept).starts_with("Local device: cpu"), "{kept:?}");
    assert!(kept.stderr.is_empty(), "{kept:?}");

    // A hand-edited unknown value runs as auto (a GPU, never the CPU) and warns.
    std::fs::write(
        temp.path().join("voisu").join("config.toml"),
        "local_device = \"vulkan\"\n",
    )
    .expect("hand-edit config");
    let unknown = run(&["local-device"]);
    assert!(unknown.status.success(), "{unknown:?}");
    assert!(
        shown(&unknown).starts_with("Local device: auto"),
        "{unknown:?}"
    );
    let warning = String::from_utf8_lossy(&unknown.stderr);
    assert!(
        warning.contains("local_device = \"vulkan\"") && warning.contains("runs as auto"),
        "{unknown:?}"
    );
}
