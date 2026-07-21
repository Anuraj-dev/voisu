//! The released binaries must answer the standard `--version`/`--help` probes
//! with exit 0, so packaging smoke gates (and users) can identify them without
//! accidentally launching the daemon or printing a usage error. Invalid
//! arguments still fail with the usage text and exit 2.

use std::process::{Command, Output};

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn run(bin: &str, arguments: &[&str]) -> Output {
    Command::new(bin)
        .args(arguments)
        .output()
        .expect("binary under test should run")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn voisu_version_flag_prints_name_and_version() {
    for flag in ["--version", "-V"] {
        let output = run(env!("CARGO_BIN_EXE_voisu"), &[flag]);
        assert!(output.status.success(), "{flag}: {output:?}");
        assert_eq!(stdout(&output), format!("voisu {VERSION}\n"), "{flag}");
    }
}

#[test]
fn voisu_help_flag_prints_usage_and_exits_zero() {
    for flag in ["--help", "-h", "help"] {
        let output = run(env!("CARGO_BIN_EXE_voisu"), &[flag]);
        assert!(output.status.success(), "{flag}: {output:?}");
        assert!(stdout(&output).contains("usage: voisu"), "{flag}");
    }
}

#[test]
fn voisu_invalid_arguments_still_fail_with_usage_and_exit_two() {
    let output = run(env!("CARGO_BIN_EXE_voisu"), &["definitely-not-a-command"]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("usage: voisu"),
        "{output:?}"
    );
}

#[test]
fn voisu_daemon_version_flag_prints_name_and_version() {
    for flag in ["--version", "-V"] {
        let output = run(env!("CARGO_BIN_EXE_voisu-daemon"), &[flag]);
        assert!(output.status.success(), "{flag}: {output:?}");
        assert_eq!(stdout(&output), format!("voisu-daemon {VERSION}\n"), "{flag}");
    }
}

#[test]
fn voisu_daemon_help_flag_describes_the_daemon_and_exits_zero() {
    for flag in ["--help", "-h"] {
        let output = run(env!("CARGO_BIN_EXE_voisu-daemon"), &[flag]);
        assert!(output.status.success(), "{flag}: {output:?}");
        assert!(stdout(&output).contains("voisu-daemon"), "{flag}");
    }
}
