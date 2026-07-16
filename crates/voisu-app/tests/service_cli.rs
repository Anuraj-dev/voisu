use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::thread;
use std::time::{Duration, Instant};

use tempfile::TempDir;

struct ServiceFixture {
    root: TempDir,
    cli: PathBuf,
    source_daemon: PathBuf,
    runtime: PathBuf,
    config: PathBuf,
    data: PathBuf,
    systemctl_log: PathBuf,
    systemctl_state: PathBuf,
    packaged_unit_dir: PathBuf,
    packaged_daemon: PathBuf,
}

impl ServiceFixture {
    fn new(source_daemon: &Path) -> Self {
        let root = TempDir::new().unwrap();
        let bin = root.path().join("source");
        fs::create_dir(&bin).unwrap();
        let cli = bin.join("voisu");
        fs::copy(env!("CARGO_BIN_EXE_voisu"), &cli).unwrap();
        fs::set_permissions(&cli, fs::Permissions::from_mode(0o700)).unwrap();
        let installed_source = bin.join("voisu-daemon");
        fs::copy(source_daemon, &installed_source).unwrap();
        fs::set_permissions(&installed_source, fs::Permissions::from_mode(0o700)).unwrap();

        let runtime = root.path().join("runtime");
        let config = root.path().join("config");
        let data = root.path().join("data");
        let fake_bin = root.path().join("fake-bin");
        fs::create_dir(&runtime).unwrap();
        fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700)).unwrap();
        fs::create_dir(&fake_bin).unwrap();
        let systemctl_log = root.path().join("systemctl.log");
        let systemctl_state = root.path().join("systemctl.state");
        let packaged_unit_dir = root.path().join("usr/lib/systemd/user");
        let packaged_daemon = root.path().join("usr/bin/voisu-daemon");
        fs::create_dir_all(&packaged_unit_dir).unwrap();
        write_systemctl(&fake_bin.join("systemctl"));

        Self {
            root,
            cli,
            source_daemon: installed_source,
            runtime,
            config,
            data,
            systemctl_log,
            systemctl_state,
            packaged_unit_dir,
            packaged_daemon,
        }
    }

    fn command(&self, arguments: &[&str]) -> Command {
        let mut command = Command::new(&self.cli);
        command
            .args(arguments)
            .env("HOME", self.root.path())
            .env("XDG_RUNTIME_DIR", &self.runtime)
            .env("XDG_CONFIG_HOME", &self.config)
            .env("XDG_DATA_HOME", &self.data)
            .env("PATH", format!("{}/fake-bin:/usr/bin:/bin", self.root.path().display()))
            .env("FAKE_SYSTEMCTL_LOG", &self.systemctl_log)
            .env("FAKE_SYSTEMCTL_STATE", &self.systemctl_state)
            .env("VOISU_PACKAGED_UNIT_DIR", &self.packaged_unit_dir)
            .env("VOISU_PACKAGED_DAEMON_PATH", &self.packaged_daemon)
            .env("VOISU_DISABLE_SHORTCUTS", "1")
            .env("VOISU_DISABLE_DIRECT_DELIVERY", "1")
            .env("VOISU_TEST_MODE", "controlled");
        command
    }

    fn run(&self, arguments: &[&str]) -> Output {
        self.command(arguments).output().unwrap()
    }

    fn unit_path(&self) -> PathBuf {
        self.config.join("systemd/user/voisu.service")
    }

    fn installed_daemon(&self) -> PathBuf {
        self.data.join("voisu/bin/voisu-daemon")
    }

    fn packaged_unit_file(&self) -> PathBuf {
        self.packaged_unit_dir.join("voisu.service")
    }

    fn set_show_state(&self, key: &str, value: &str) {
        let prefix = format!("{key}=");
        let mut lines: Vec<String> = fs::read_to_string(&self.systemctl_state)
            .unwrap_or_default()
            .lines()
            .filter(|line| !line.starts_with(&prefix))
            .map(str::to_owned)
            .collect();
        lines.push(format!("{key}={value}"));
        let mut body = lines.join("\n");
        body.push('\n');
        fs::write(&self.systemctl_state, body).unwrap();
    }

    /// Override the effective unit's ExecStart command binaries as `systemctl
    /// show` would report them — e.g. an administrator /etc drop-in that changes
    /// or adds commands. Multiple commands are validated independently.
    fn override_effective_execs(&self, execs: &[&Path]) {
        let joined = execs
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join("|");
        self.set_show_state("execs", &joined);
    }

    /// Override the LoadState the fake `systemctl show` reports for the effective
    /// unit (e.g. an error/bad-setting unit file).
    fn override_effective_load_state(&self, load_state: &str) {
        self.set_show_state("loadstate", load_state);
    }

    fn install_packaged_unit(&self) {
        fs::create_dir_all(self.packaged_daemon.parent().unwrap()).unwrap();
        fs::copy(&self.source_daemon, &self.packaged_daemon).unwrap();
        fs::set_permissions(&self.packaged_daemon, fs::Permissions::from_mode(0o700)).unwrap();
        self.write_packaged_unit_file();
    }

    fn install_packaged_unit_without_daemon(&self) {
        self.write_packaged_unit_file();
    }

    fn write_packaged_unit_file(&self) {
        fs::write(
            self.packaged_unit_file(),
            format!(
                "[Unit]\nDescription=Packaged Voisu dictation daemon\n\n[Service]\nExecStart={} --systemd\n",
                self.packaged_daemon.display()
            ),
        )
        .unwrap();
    }

    fn use_real_managed_daemon(&self) {
        fs::write(
            &self.systemctl_state,
            format!("daemon={}\n", self.installed_daemon().display()),
        )
        .unwrap();
    }
}

impl Drop for ServiceFixture {
    fn drop(&mut self) {
        let _ = self.run(&["service", "stop"]);
    }
}

fn write_systemctl(path: &Path) {
    fs::write(
        path,
        r#"#!/bin/sh
set -eu
log=${FAKE_SYSTEMCTL_LOG:?}
state=${FAKE_SYSTEMCTL_STATE:?}
printf '%s\n' "$*" >> "$log"
command=${2:-}
pid_file="${state}.pid"
daemon=$(sed -n 's/^daemon=//p' "$state" 2>/dev/null || true)
forced=$(sed -n 's/^forced=//p' "$state" 2>/dev/null || true)
stuck_stop=$(sed -n 's/^stuck_stop=//p' "$state" 2>/dev/null || true)
active() { test -f "$pid_file" && kill -0 "$(cat "$pid_file")" 2>/dev/null; }
case "$command" in
  show)
    # Model systemd precedence honestly: a user unit under XDG config shadows
    # any packaged unit. Report whichever unit systemd would actually run.
    xdg_unit="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/voisu.service"
    pkg_unit="${VOISU_PACKAGED_UNIT_DIR:-}/voisu.service"
    loadstate=$(sed -n 's/^loadstate=//p' "$state" 2>/dev/null || true)
    execs=$(sed -n 's/^execs=//p' "$state" 2>/dev/null || true)
    if test -f "$xdg_unit"; then
      frag="$xdg_unit"; unit_file="$xdg_unit"
    elif test -n "${VOISU_PACKAGED_UNIT_DIR:-}" && test -f "$pkg_unit"; then
      frag="$pkg_unit"; unit_file="$pkg_unit"
    else
      frag=""
    fi
    if test -z "$frag"; then
      printf 'LoadState=not-found\nFragmentPath=\nExecStart=\n'
      exit 0
    fi
    test -n "$loadstate" || loadstate=loaded
    printf 'LoadState=%s\n' "$loadstate"
    printf 'FragmentPath=%s\n' "$frag"
    # ExecStart binaries: an explicit "execs=" override (pipe-separated for
    # multiple commands, e.g. an /etc drop-in) else parse the unit file.
    if test -z "$execs"; then
      execs=$(sed -n 's/^ExecStart=\(.*\) --systemd$/\1/p' "$unit_file" | head -1)
    fi
    old_ifs=$IFS
    IFS='|'
    for e in $execs; do
      printf 'ExecStart={ path=%s ; argv[]=%s --systemd ; ignore_errors=no }\n' "$e" "$e"
    done
    IFS=$old_ifs
    exit 0
    ;;
  is-active)
    if test -n "$forced"; then printf '%s\n' "$forced"; exit 3; fi
    if active; then printf 'active\n'; exit 0; fi
    printf 'inactive\n'; exit 3
    ;;
  start)
    if ! active; then
      "$daemon" >/dev/null 2>&1 &
      printf '%s\n' "$!" > "$pid_file"
    fi
    ;;
  restart)
    if active; then kill "$(cat "$pid_file")"; fi
    rm -f "$pid_file"
    "$daemon" >/dev/null 2>&1 &
    printf '%s\n' "$!" > "$pid_file"
    ;;
  stop)
    if test "$stuck_stop" = "1"; then exit 0; fi
    if active; then
      kill "$(cat "$pid_file")"
      i=0
      while active && test "$i" -lt 100; do i=$((i + 1)); sleep 0.01; done
    fi
    rm -f "$pid_file"
    ;;
  disable)
    if test "${3:-}" = "--now" && active; then
      kill "$(cat "$pid_file")"
      i=0
      while active && test "$i" -lt 100; do i=$((i + 1)); sleep 0.01; done
      rm -f "$pid_file"
    fi
    ;;
  daemon-reload|enable|reset-failed) ;;
  *) printf 'unexpected systemctl command: %s\n' "$*" >&2; exit 2 ;;
esac
"#,
    )
    .unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

fn write_parent_death_probing_systemctl(path: &Path) {
    fs::write(
        path,
        r#"#!/usr/bin/python3
import ctypes
import signal
import sys

value = ctypes.c_int()
if ctypes.CDLL(None).prctl(2, ctypes.byref(value)) != 0 or value.value != signal.SIGKILL:
    sys.exit(9)
if len(sys.argv) > 2 and sys.argv[2] == "is-active":
    print("inactive")
    sys.exit(3)
sys.exit(0)
"#,
    )
    .unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn wait_for_socket(runtime: &Path, present: bool) {
    let socket = runtime.join("voisu/v1/daemon.sock");
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if socket.exists() == present {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("daemon socket did not reach present={present}: {}", socket.display());
}

#[test]
fn service_manager_guards_its_systemctl_child_with_parent_death_signal() {
    let fixture = ServiceFixture::new(Path::new(env!("CARGO_BIN_EXE_voisu-daemon")));
    write_parent_death_probing_systemctl(&fixture.root.path().join("fake-bin/systemctl"));

    let status = fixture.run(&["service", "status"]);

    assert_eq!(status.status.code(), Some(3), "{}", stderr(&status));
    assert!(stdout(&status).contains("systemd user service inactive"));
}

#[test]
fn install_is_idempotent_atomic_and_free_of_stale_session_or_checkout_values() {
    let fixture = ServiceFixture::new(Path::new(env!("CARGO_BIN_EXE_voisu-daemon")));

    let installed = fixture.run(&["service", "install"]);
    assert!(installed.status.success(), "{}", stderr(&installed));
    let first_inode = fs::metadata(fixture.installed_daemon()).unwrap().ino();
    fs::write(&fixture.source_daemon, b"#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&fixture.source_daemon, fs::Permissions::from_mode(0o700)).unwrap();
    let upgraded = fixture.run(&["service", "install"]);
    assert!(upgraded.status.success(), "{}", stderr(&upgraded));

    let unit = fs::read_to_string(fixture.unit_path()).unwrap();
    assert!(unit.contains("After=dbus.socket pipewire.service xdg-desktop-portal.service"));
    assert!(!unit.contains("After=graphical-session.target"));
    assert!(unit.contains("WantedBy=graphical-session.target"));
    assert!(unit.contains(&format!(
        "ExecStart=\"{}\" --systemd",
        fixture.installed_daemon().display()
    )));
    for stale in [
        "DISPLAY=",
        "WAYLAND_DISPLAY=",
        "DBUS_SESSION_BUS_ADDRESS=",
        "XAUTHORITY=",
        "/target/",
    ] {
        assert!(!unit.contains(stale), "unit baked stale value {stale}: {unit}");
    }
    assert_eq!(
        fs::metadata(fixture.installed_daemon()).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_ne!(first_inode, fs::metadata(fixture.installed_daemon()).unwrap().ino());
    assert_eq!(
        fs::read(&fixture.source_daemon).unwrap(),
        fs::read(fixture.installed_daemon()).unwrap()
    );
    let calls = fs::read_to_string(&fixture.systemctl_log).unwrap();
    assert_eq!(calls.matches("--user daemon-reload").count(), 2);
    assert_eq!(calls.matches("--user enable voisu.service").count(), 2);
}

#[test]
fn packaged_install_migrates_a_stale_user_service_without_shadowing_the_package() {
    let fixture = ServiceFixture::new(Path::new(env!("CARGO_BIN_EXE_voisu-daemon")));

    // A Ticket 09 install first, so a real XDG user unit exists on disk.
    assert!(fixture.run(&["service", "install"]).status.success());
    assert!(fixture.unit_path().exists());
    assert!(fixture.installed_daemon().exists());

    // The RPM then lands the packaged unit. systemd precedence keeps the XDG
    // user unit effective (the fake `systemctl show` models this), so migration
    // must be reached via on-disk packaged-unit detection, not the effective
    // fragment. Without that, install would rewrite the Ticket 09 unit and the
    // stale shadow would keep owning the service.
    fixture.install_packaged_unit();
    let installed = fixture.run(&["service", "install"]);

    assert!(installed.status.success(), "{}", stderr(&installed));
    assert!(stdout(&installed).contains("packaged systemd user service selected"));
    assert!(!fixture.unit_path().exists(), "user unit must not shadow the package");
    assert!(
        !fixture.installed_daemon().exists(),
        "stale XDG user-data daemon must not own the package service"
    );
    assert!(fixture.packaged_unit_dir.join("voisu.service").exists());
    let calls = fs::read_to_string(&fixture.systemctl_log).unwrap();
    assert!(calls.contains("--user daemon-reload"));
    assert!(calls.contains("--user enable voisu.service"));
}

#[test]
fn packaged_unit_without_daemon_binary_falls_back_to_ticket_09_user_data_service() {
    let fixture = ServiceFixture::new(Path::new(env!("CARGO_BIN_EXE_voisu-daemon")));
    fixture.install_packaged_unit_without_daemon();

    let installed = fixture.run(&["service", "install"]);

    assert!(installed.status.success(), "{}", stderr(&installed));
    assert!(
        stdout(&installed).contains("packaged unit was ignored")
            && stdout(&installed).contains("Ticket 09 user-data path")
    );
    assert!(fixture.unit_path().exists());
    assert!(fixture.installed_daemon().exists());
}

#[test]
fn effective_execstart_override_binary_missing_falls_back_to_ticket_09_user_data() {
    let fixture = ServiceFixture::new(Path::new(env!("CARGO_BIN_EXE_voisu-daemon")));
    // The packaged unit file and its packaged daemon on disk are both valid,
    // so an on-disk search would trust them. But the unit systemd would actually
    // run (an administrator /etc override or drop-in) points ExecStart at a
    // binary that is not installed, so the CLI must not migrate to it.
    fixture.install_packaged_unit();
    let overridden = fixture.root.path().join("etc-override/voisu-daemon");
    fixture.override_effective_execs(&[&overridden]);

    let installed = fixture.run(&["service", "install"]);

    assert!(installed.status.success(), "{}", stderr(&installed));
    assert!(
        stdout(&installed).contains("packaged unit was ignored")
            && stdout(&installed).contains("Ticket 09 user-data path"),
        "{}",
        stdout(&installed)
    );
    assert!(fixture.unit_path().exists());
    assert!(fixture.installed_daemon().exists());
}

#[test]
fn effective_execstart_override_selects_packaged_when_the_static_daemon_is_absent() {
    let fixture = ServiceFixture::new(Path::new(env!("CARGO_BIN_EXE_voisu-daemon")));
    // The packaged unit file references a daemon that is not present on disk, so
    // an on-disk search would ignore the package. systemd's effective ExecStart
    // (an administrator override) points at a valid installed binary, so the CLI
    // must select and migrate to the packaged unit.
    fixture.install_packaged_unit_without_daemon();
    fixture.override_effective_execs(&[&fixture.source_daemon]);

    let installed = fixture.run(&["service", "install"]);

    assert!(installed.status.success(), "{}", stderr(&installed));
    assert!(
        stdout(&installed).contains("packaged systemd user service selected"),
        "{}",
        stdout(&installed)
    );
    assert!(!fixture.unit_path().exists(), "user unit must not shadow the package");
}

#[test]
fn an_xdg_user_unit_with_no_packaged_file_is_never_treated_as_packaged() {
    let fixture = ServiceFixture::new(Path::new(env!("CARGO_BIN_EXE_voisu-daemon")));
    // A Ticket 09 install creates a real XDG user unit; `systemctl show` then
    // resolves it as the effective unit. With no packaged unit file on disk, the
    // on-disk detection must find nothing and a re-install must stay on the
    // Ticket 09 path — never fabricate a packaged migration.
    assert!(fixture.run(&["service", "install"]).status.success());
    assert!(fixture.unit_path().exists());

    let reinstalled = fixture.run(&["service", "install"]);

    assert!(reinstalled.status.success(), "{}", stderr(&reinstalled));
    assert!(
        !stdout(&reinstalled).contains("packaged"),
        "{}",
        stdout(&reinstalled)
    );
    assert!(fixture.unit_path().exists());
    assert!(fixture.installed_daemon().exists());
}

#[test]
fn packaged_unit_with_a_non_loaded_load_state_falls_back_to_ticket_09_user_data() {
    let fixture = ServiceFixture::new(Path::new(env!("CARGO_BIN_EXE_voisu-daemon")));
    // The packaged unit is the effective unit but systemd reports it as not
    // cleanly loaded (e.g. bad-setting/error). Any LoadState other than "loaded"
    // must not be migrated to; it falls back to Ticket 09 with an explicit
    // reason instead of silently trusting a broken unit.
    fixture.install_packaged_unit();
    fixture.override_effective_load_state("error");

    let installed = fixture.run(&["service", "install"]);

    assert!(installed.status.success(), "{}", stderr(&installed));
    assert!(
        stdout(&installed).contains("packaged unit was ignored")
            && stdout(&installed).contains("LoadState=error"),
        "{}",
        stdout(&installed)
    );
    assert!(fixture.unit_path().exists());
    assert!(fixture.installed_daemon().exists());
}

#[test]
fn packaged_unit_with_a_missing_later_execstart_command_falls_back_to_ticket_09() {
    let fixture = ServiceFixture::new(Path::new(env!("CARGO_BIN_EXE_voisu-daemon")));
    // A multi-command ExecStart (an /etc drop-in adding a second command) whose
    // first command is valid but whose later command is missing must not be
    // accepted as packaged: every command systemd would run has to validate.
    fixture.install_packaged_unit();
    let missing = fixture.root.path().join("etc-override/second-command");
    fixture.override_effective_execs(&[&fixture.packaged_daemon, &missing]);

    let installed = fixture.run(&["service", "install"]);

    assert!(installed.status.success(), "{}", stderr(&installed));
    assert!(
        stdout(&installed).contains("packaged unit was ignored")
            && stdout(&installed).contains("Ticket 09 user-data path"),
        "{}",
        stdout(&installed)
    );
    assert!(fixture.unit_path().exists());
    assert!(fixture.installed_daemon().exists());
}

#[test]
fn packaged_install_restarts_an_active_service_after_migrating_its_user_shadow() {
    let fixture = ServiceFixture::new(Path::new(env!("CARGO_BIN_EXE_voisu-daemon")));

    assert!(fixture.run(&["service", "install"]).status.success());
    fs::write(
        &fixture.systemctl_state,
        format!("daemon={}\n", fixture.source_daemon.display()),
    )
    .unwrap();
    assert!(fixture.run(&["service", "start"]).status.success());

    fixture.install_packaged_unit();
    let migrated = fixture.run(&["service", "install"]);

    assert!(migrated.status.success(), "{}", stderr(&migrated));
    assert!(stdout(&migrated).contains("packaged systemd user service selected"));
    assert!(stdout(&fixture.run(&["service", "status"])).contains("systemd user service active"));
    let calls = fs::read_to_string(&fixture.systemctl_log).unwrap();
    assert!(calls.contains("--user restart voisu.service"));
}

#[test]
fn installed_service_bounds_repeated_startup_failures() {
    let fixture = ServiceFixture::new(Path::new(env!("CARGO_BIN_EXE_voisu-daemon")));

    let installed = fixture.run(&["service", "install"]);
    assert!(installed.status.success(), "{}", stderr(&installed));

    let unit = fs::read_to_string(fixture.unit_path()).unwrap();
    assert!(unit.contains("Restart=on-failure\n"), "{unit}");
    assert!(unit.contains("StartLimitIntervalSec=30s\n"), "{unit}");
    assert!(unit.contains("StartLimitBurst=3\n"), "{unit}");
    // Graceful shutdown's internal budget (stop, process, join, drain) peaks
    // near 37 seconds; the unit must bound the stop explicitly above it rather
    // than rely on the distribution's default.
    assert!(unit.contains("TimeoutStopSec=60s\n"), "{unit}");
}

#[test]
fn inactive_status_reports_both_systemd_and_ipc_state() {
    let fixture = ServiceFixture::new(Path::new(env!("CARGO_BIN_EXE_voisu-daemon")));

    let status = fixture.run(&["service", "status"]);

    assert_eq!(status.status.code(), Some(3));
    assert!(stdout(&status).contains("systemd user service inactive; daemon IPC unavailable"));
}

#[test]
fn failed_systemd_state_is_not_mislabeled_as_inactive() {
    let fixture = ServiceFixture::new(Path::new(env!("CARGO_BIN_EXE_voisu-daemon")));
    fs::write(&fixture.systemctl_state, "forced=failed\n").unwrap();

    let status = fixture.run(&["service", "status"]);

    assert_eq!(status.status.code(), Some(4));
    assert!(stdout(&status).contains("systemd user service failed; daemon IPC unavailable"));
}

#[test]
fn managed_service_lifecycle_reports_systemd_ownership_and_daemon_ipc() {
    let fixture = ServiceFixture::new(Path::new(env!("CARGO_BIN_EXE_voisu-daemon")));
    assert!(fixture.run(&["service", "install"]).status.success());
    fixture.use_real_managed_daemon();

    let started = fixture.run(&["service", "start"]);
    assert!(started.status.success(), "{}", stderr(&started));
    assert!(stdout(&started).contains("systemd user service active; daemon IPC idle"));
    wait_for_socket(&fixture.runtime, true);

    let status = fixture.run(&["service", "status"]);
    assert!(status.status.success(), "{}", stderr(&status));
    assert!(stdout(&status).contains("systemd user service active; daemon IPC idle"));

    let restarted = fixture.run(&["service", "restart"]);
    assert!(restarted.status.success(), "{}", stderr(&restarted));
    assert!(stdout(&restarted).contains("systemd user service active; daemon IPC idle"));

    let stopped = fixture.run(&["service", "stop"]);
    assert!(stopped.status.success(), "{}", stderr(&stopped));
    assert!(stdout(&stopped).contains("systemd user service inactive; daemon IPC unavailable"));
    wait_for_socket(&fixture.runtime, false);
}

#[test]
fn stop_fails_when_systemd_still_owns_the_daemon_after_the_deadline() {
    let fixture = ServiceFixture::new(Path::new(env!("CARGO_BIN_EXE_voisu-daemon")));
    assert!(fixture.run(&["service", "install"]).status.success());
    fixture.use_real_managed_daemon();
    assert!(fixture.run(&["service", "start"]).status.success());
    fs::write(
        &fixture.systemctl_state,
        format!("daemon={}\nstuck_stop=1\n", fixture.installed_daemon().display()),
    )
    .unwrap();

    let stopped = fixture.run(&["service", "stop"]);

    assert!(!stopped.status.success());
    assert!(stderr(&stopped).contains("did not stop before the deadline"));
    fixture.use_real_managed_daemon();
    assert!(fixture.run(&["service", "stop"]).status.success());
}

#[test]
fn a_manual_daemon_is_reported_and_service_start_does_not_create_a_crash_loop() {
    let fixture = ServiceFixture::new(Path::new(env!("CARGO_BIN_EXE_voisu-daemon")));
    let mut manual = Command::new(env!("CARGO_BIN_EXE_voisu-daemon"));
    manual
        .env("XDG_RUNTIME_DIR", &fixture.runtime)
        .env("VOISU_DISABLE_SHORTCUTS", "1")
        .env("VOISU_DISABLE_DIRECT_DELIVERY", "1")
        .env("VOISU_TEST_MODE", "controlled");
    let mut manual = manual.spawn().unwrap();
    wait_for_socket(&fixture.runtime, true);

    let started = fixture.run(&["service", "start"]);
    assert!(started.status.success(), "{}", stderr(&started));
    assert!(stdout(&started).contains("daemon running outside systemd; service not started"));
    let calls = fs::read_to_string(&fixture.systemctl_log).unwrap();
    assert!(!calls.lines().any(|line| line == "--user start voisu.service"));

    let result = unsafe { libc::kill(manual.id() as libc::pid_t, libc::SIGTERM) };
    assert_eq!(result, 0);
    manual.wait().unwrap();
    wait_for_socket(&fixture.runtime, false);
}

#[test]
fn a_systemd_launched_duplicate_exits_cleanly_while_the_manual_daemon_remains_reachable() {
    let fixture = ServiceFixture::new(Path::new(env!("CARGO_BIN_EXE_voisu-daemon")));
    let mut manual = Command::new(env!("CARGO_BIN_EXE_voisu-daemon"));
    manual
        .env("XDG_RUNTIME_DIR", &fixture.runtime)
        .env("VOISU_DISABLE_SHORTCUTS", "1")
        .env("VOISU_DISABLE_DIRECT_DELIVERY", "1")
        .env("VOISU_TEST_MODE", "controlled");
    let mut manual = manual.spawn().unwrap();
    wait_for_socket(&fixture.runtime, true);

    let duplicate = Command::new(env!("CARGO_BIN_EXE_voisu-daemon"))
        .arg("--systemd")
        .env("XDG_RUNTIME_DIR", &fixture.runtime)
        .output()
        .unwrap();
    assert!(duplicate.status.success(), "{}", stderr(&duplicate));
    let status = fixture.run(&["status"]);
    assert!(status.status.success(), "{}", stderr(&status));

    let result = unsafe { libc::kill(manual.id() as libc::pid_t, libc::SIGTERM) };
    assert_eq!(result, 0);
    manual.wait().unwrap();
    wait_for_socket(&fixture.runtime, false);
}

#[test]
fn uninstall_disables_service_removes_installed_files_and_leaves_no_runtime_socket() {
    let fixture = ServiceFixture::new(Path::new(env!("CARGO_BIN_EXE_voisu-daemon")));
    assert!(fixture.run(&["service", "install"]).status.success());
    fixture.use_real_managed_daemon();
    assert!(fixture.run(&["service", "start"]).status.success());
    wait_for_socket(&fixture.runtime, true);

    let removed = fixture.run(&["service", "uninstall"]);
    assert!(removed.status.success(), "{}", stderr(&removed));
    assert!(!fixture.unit_path().exists());
    assert!(!fixture.installed_daemon().exists());
    wait_for_socket(&fixture.runtime, false);
    let calls = fs::read_to_string(&fixture.systemctl_log).unwrap();
    assert!(calls.contains("--user disable --now voisu.service"));
    assert!(calls.contains("--user daemon-reload"));
    assert!(calls.contains("--user reset-failed voisu.service"));
}

#[test]
fn packaged_uninstall_disables_only_the_service_and_preserves_packaged_unit_and_user_data() {
    let fixture = ServiceFixture::new(Path::new(env!("CARGO_BIN_EXE_voisu-daemon")));
    fixture.install_packaged_unit();
    fs::create_dir_all(fixture.installed_daemon().parent().unwrap()).unwrap();
    fs::write(fixture.installed_daemon(), b"stale user-data daemon").unwrap();

    let removed = fixture.run(&["service", "uninstall"]);

    assert!(removed.status.success(), "{}", stderr(&removed));
    assert!(stdout(&removed).contains("packaged systemd user service disabled"));
    assert!(fixture.packaged_unit_dir.join("voisu.service").exists());
    assert!(!fixture.installed_daemon().exists());
    assert!(!fixture.unit_path().exists());
}
