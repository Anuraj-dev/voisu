//! `voisu local-model` through the real CLI: it shows what the setting
//! resolves to, and a script can never start a download.

use std::process::{Command, Output};

#[test]
fn local_model_shows_the_resolved_tier_and_refuses_a_download_without_a_terminal() {
    let temp = tempfile::tempdir().expect("tempdir");
    let config = temp.path().join("config");
    let state = temp.path().join("state");
    let run = |arguments: &[&str]| -> Output {
        Command::new(env!("CARGO_BIN_EXE_voisu"))
            .args(arguments)
            .env("XDG_CONFIG_HOME", &config)
            .env("XDG_STATE_HOME", &state)
            .env("HOME", temp.path())
            .output()
            .expect("run voisu CLI")
    };
    let shown = |output: &Output| String::from_utf8_lossy(&output.stdout).into_owned();

    let fresh = run(&["local-model"]);
    assert!(fresh.status.success(), "{fresh:?}");
    assert_eq!(
        shown(&fresh),
        "Local model: auto (Accurate with a GPU, Fast without; the GPU is checked when it installs)\nThe Local model it picks is not installed. Run `voisu local-model auto` to install it.\n",
        "with local_device auto and nothing installed, no GPU is claimed unprobed"
    );

    // `local-device cpu` makes auto resolve to Fast.
    assert!(run(&["local-device", "cpu"]).status.success());
    let cpu = run(&["local-model"]);
    assert!(
        shown(&cpu).starts_with("Local model: auto (Fast (small.en, CPU))\n"),
        "{cpu:?}"
    );

    // Standard input is not a terminal here: nothing is fetched or saved.
    let refused = run(&["local-model", "fast"]);
    assert_eq!(refused.status.code(), Some(4), "{refused:?}");
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains(
            "Fast (small.en) is not installed and needs a 252.6 MiB download; run `voisu local-model fast` in a terminal to agree to it. Nothing changed."
        ),
        "{refused:?}"
    );
    let saved = std::fs::read_to_string(config.join("voisu").join("config.toml"))
        .expect("local-device wrote the config");
    assert!(!saved.contains("local_model"), "{saved}");
    assert!(!state.join("voisu").join("models").exists());

    let invalid = run(&["local-model", "medium"]);
    assert_eq!(invalid.status.code(), Some(2), "{invalid:?}");
    assert!(
        String::from_utf8_lossy(&invalid.stderr).contains("expected auto, accurate or fast"),
        "{invalid:?}"
    );

    // A hand-edited unknown value runs as auto and warns.
    std::fs::write(
        config.join("voisu").join("config.toml"),
        "local_device = \"cpu\"\nlocal_model = \"medium\"\n",
    )
    .expect("hand-edit config");
    let unknown = run(&["local-model"]);
    assert!(
        shown(&unknown).starts_with("Local model: auto (Fast (small.en, CPU))"),
        "{unknown:?}"
    );
    assert!(
        String::from_utf8_lossy(&unknown.stderr).contains("local_model = \"medium\""),
        "{unknown:?}"
    );
}
