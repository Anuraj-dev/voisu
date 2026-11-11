//! `voisu star` and `voisu star-prompt` through the real CLI, with a fake
//! `xdg-open` on `PATH` so no browser opens.

use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Output};

struct Host {
    root: tempfile::TempDir,
}

impl Host {
    fn new(xdg_open: &str) -> Self {
        let root = tempfile::tempdir().unwrap();
        let bin = root.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let script = bin.join("xdg-open");
        std::fs::write(&script, format!("#!/bin/sh\n{xdg_open}\n")).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        Self { root }
    }

    fn log(&self) -> std::path::PathBuf {
        self.root.path().join("opened.log")
    }

    fn run(&self, arguments: &[&str]) -> Output {
        let path = self.root.path();
        Command::new(env!("CARGO_BIN_EXE_voisu"))
            .args(arguments)
            .env_clear()
            .env("HOME", path)
            .env("XDG_CONFIG_HOME", path.join("config"))
            .env("XDG_STATE_HOME", path.join("state"))
            .env("XDG_RUNTIME_DIR", path)
            .env("PATH", path.join("bin"))
            .output()
            .expect("run voisu CLI")
    }

    fn state(&self) -> String {
        std::fs::read_to_string(self.root.path().join("state/voisu/star-prompt.json"))
            .unwrap_or_default()
    }
}

fn shown(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn star_opens_the_repository_and_records_the_answer() {
    let host = Host::new("");
    let host_log = host.log();
    std::fs::write(
        host.root.path().join("bin/xdg-open"),
        format!("#!/bin/sh\necho \"$*\" >> '{}'\n", host_log.display()),
    )
    .unwrap();

    let output = host.run(&["star"]);

    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        std::fs::read_to_string(&host_log).unwrap(),
        "https://github.com/Anuraj-dev/voisu\n"
    );
    assert!(shown(&output).contains("Thank you"), "{output:?}");
    assert!(
        host.state().contains("\"answered\":true"),
        "{}",
        host.state()
    );
}

#[test]
fn star_without_a_browser_still_records_the_answer_and_names_the_page() {
    let host = Host::new("exit 3");

    let output = host.run(&["star"]);

    assert!(output.status.success(), "{output:?}");
    assert!(
        shown(&output).contains("https://github.com/Anuraj-dev/voisu"),
        "{output:?}"
    );
    assert!(
        host.state().contains("\"answered\":true"),
        "{}",
        host.state()
    );
}

#[test]
fn star_prompt_round_trips_and_status_never_mentions_it() {
    let host = Host::new("exit 99");

    let fresh = host.run(&["star-prompt"]);
    assert!(fresh.status.success(), "{fresh:?}");
    assert!(shown(&fresh).starts_with("Star prompt: on"), "{fresh:?}");

    let off = host.run(&["star-prompt", "off"]);
    assert!(off.status.success(), "{off:?}");
    assert!(shown(&off).starts_with("Star prompt turned off"), "{off:?}");
    assert!(shown(&host.run(&["star-prompt"])).starts_with("Star prompt: off"));

    let invalid = host.run(&["star-prompt", "later"]);
    assert_eq!(invalid.status.code(), Some(2), "{invalid:?}");
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("star-prompt must be on or off"));

    // An eligible prompt is still never mentioned by `voisu status`.
    std::fs::create_dir_all(host.root.path().join("state/voisu")).unwrap();
    std::fs::write(
        host.root.path().join("state/voisu/star-prompt.json"),
        r#"{"count":20,"dates":["a","b","c"],"notified":true,"answered":false}"#,
    )
    .unwrap();
    assert!(host.run(&["star-prompt", "on"]).status.success());
    let status = host.run(&["status"]);
    let all = format!(
        "{}{}",
        shown(&status),
        String::from_utf8_lossy(&status.stderr)
    );
    assert!(!all.to_lowercase().contains("star"), "{all}");
}
