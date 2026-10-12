//! `voisu-daemon --systemd` waits for the session display environment itself
//! instead of relying on a unit start condition (a condition skip is never
//! retried). Late arrival exits 75 so `Restart=on-failure` respawns it with the
//! manager's environment; no arrival exits 78 so it is not restart-looped.
//!
//! The manager environment comes from a fake `systemctl` on PATH and the wait
//! is shrunk through the VOISU_TEST_* seams, so no test waits in real time.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};

use tempfile::TempDir;

/// A fake `systemctl --user show-environment`. It counts calls; the display
/// variables appear from the `arrive_after`-th call on (0 = never).
fn fake_systemctl(dir: &Path, arrive_after: u32) {
    fs::write(dir.join("arrive_after"), arrive_after.to_string()).unwrap();
    let script = dir.join("systemctl");
    fs::write(
        &script,
        r#"#!/bin/sh
dir=$(dirname "$0")
printf '%s\n' "$*" >> "$dir/systemctl.log"
if [ "$1" != "--user" ] || [ "$2" != "show-environment" ]; then exit 0; fi
n=$(cat "$dir/count" 2>/dev/null || echo 0)
n=$((n + 1))
printf '%s' "$n" > "$dir/count"
printf 'LANG=C\n'
arrive=$(cat "$dir/arrive_after")
if [ "$arrive" -gt 0 ] && [ "$n" -ge "$arrive" ]; then
  printf 'WAYLAND_DISPLAY=wayland-0\nDISPLAY=:0\n'
fi
"#,
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
}

fn run_systemd_daemon(bin: &Path, wait_ms: &str, poll_ms: &str) -> Output {
    let runtime = TempDir::new().unwrap();
    fs::set_permissions(runtime.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    Command::new(env!("CARGO_BIN_EXE_voisu-daemon"))
        .arg("--systemd")
        .env("PATH", path)
        .env("XDG_RUNTIME_DIR", runtime.path())
        .env("XDG_STATE_HOME", runtime.path().join("state"))
        .env_remove("WAYLAND_DISPLAY")
        .env_remove("DISPLAY")
        .env("VOISU_TEST_DISPLAY_WAIT_MS", wait_ms)
        .env("VOISU_TEST_DISPLAY_POLL_MS", poll_ms)
        .output()
        .expect("daemon should run")
}

#[test]
fn display_arriving_late_exits_75_so_systemd_respawns_the_daemon() {
    let bin = TempDir::new().unwrap();
    fake_systemctl(bin.path(), 3);

    // A generous wait proves arrival, not the timeout, ends the run; the exit
    // is driven by the third `show-environment` call, not by elapsed time.
    let output = run_systemd_daemon(bin.path(), "60000", "10");

    assert_eq!(output.status.code(), Some(75), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("display environment arrived after start; restarting"),
        "{stderr}"
    );
}

#[test]
fn display_never_arriving_exits_78_naming_the_missing_variables() {
    let bin = TempDir::new().unwrap();
    fake_systemctl(bin.path(), 0);

    let output = run_systemd_daemon(bin.path(), "100", "10");

    assert_eq!(output.status.code(), Some(78), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("WAYLAND_DISPLAY") && stderr.contains("DISPLAY"),
        "{stderr}"
    );
    assert!(stderr.contains("voisu service restart"), "{stderr}");
}
