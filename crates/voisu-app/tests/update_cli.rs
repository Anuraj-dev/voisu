//! `voisu update` end to end through the real CLI, against a fake host: fake
//! package-manager, AUR-helper, systemctl and curl scripts on `PATH`, and a
//! fake installed `voisu` binary named by the `VOISU_UPDATE_BINARY` seam. No
//! test touches the real package manager, services, or network.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output};

use tempfile::TempDir;

struct FakeHost {
    root: TempDir,
    bin: PathBuf,
    log: PathBuf,
    installed: PathBuf,
}

impl FakeHost {
    fn new() -> Self {
        let root = TempDir::new().unwrap();
        let bin = root.path().join("fake-bin");
        fs::create_dir(&bin).unwrap();
        let log = root.path().join("calls.log");
        fs::write(&log, "").unwrap();
        let installed = root.path().join("usr/bin/voisu");
        fs::create_dir_all(installed.parent().unwrap()).unwrap();
        let host = Self {
            root,
            bin,
            log,
            installed,
        };
        write_script(&host.installed, "echo \"voisu 9.9.9\"");
        // systemctl: voisu.service is active, the Overlay service is not.
        host.tool(
            "systemctl",
            &format!(
                "echo \"systemctl $*\" >> '{log}'\n\
                 case \"$*\" in\n\
                 \"--user is-active --quiet voisu.service\") exit 0 ;;\n\
                 \"--user is-active --quiet voisu-overlay.service\") exit 3 ;;\n\
                 esac\n\
                 exit 0",
                log = host.log.display()
            ),
        );
        host
    }

    fn tool(&self, name: &str, body: &str) {
        write_script(&self.bin.join(name), body);
    }

    /// A tool that records its argv and exits with `status`.
    fn logging_tool(&self, name: &str, status: u8) {
        self.tool(
            name,
            &format!(
                "echo \"{name} $*\" >> '{}'\nexit {status}",
                self.log.display()
            ),
        );
    }

    /// `pacman -Qqo <path>` answers that `package` owns the installed binary.
    fn pacman_owner(&self, package: &str) {
        self.tool(
            "pacman",
            &format!(
                "if [ \"$1\" = \"-Qqo\" ] && [ \"$2\" = '{}' ]; then echo {package}; exit 0; fi\n\
                 echo \"error: No package owns $2\" >&2\nexit 1",
                self.installed.display()
            ),
        );
    }

    fn voisu(&self, arguments: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_voisu"))
            .args(arguments)
            .env_clear()
            .env("HOME", self.root.path())
            .env("XDG_RUNTIME_DIR", self.root.path())
            .env("PATH", &self.bin)
            .env("VOISU_UPDATE_BINARY", &self.installed)
            .output()
            .expect("voisu should run")
    }

    fn calls(&self) -> String {
        fs::read_to_string(&self.log).unwrap()
    }
}

fn write_script(path: &std::path::Path, body: &str) {
    fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn dry_run_prints_the_upgrade_and_restart_without_running_them() {
    let host = FakeHost::new();
    host.pacman_owner("voisu-bin");
    host.logging_tool("yay", 0);

    let output = host.voisu(&["update", "--dry-run"]);

    assert!(output.status.success(), "{output:?}");
    let out = stdout(&output);
    assert!(out.contains("voisu-bin"), "{out}");
    assert!(
        out.contains("would run: yay -S --needed voisu-bin"),
        "{out}"
    );
    assert!(
        out.contains(
            "would run: systemctl --user daemon-reload\n\
             would run: systemctl --user try-restart voisu.service"
        ),
        "{out}"
    );
    assert!(
        !out.contains("voisu-overlay.service"),
        "inactive unit: {out}"
    );
    assert!(!host.calls().contains("yay"), "dry run ran yay");
    assert!(!host.calls().contains("try-restart"), "dry run restarted");
}

#[test]
fn update_runs_the_helper_restarts_active_services_and_reports_the_version() {
    let host = FakeHost::new();
    host.pacman_owner("voisu-bin");
    // The fake helper "installs" a newer release over the installed binary.
    host.tool(
        "yay",
        &format!(
            "echo \"yay $*\" >> '{log}'\n\
             printf '#!/bin/sh\\necho \"voisu 10.0.0\"\\n' > '{installed}'",
            log = host.log.display(),
            installed = host.installed.display()
        ),
    );

    let output = host.voisu(&["update"]);

    assert!(output.status.success(), "{output:?}");
    let calls = host.calls();
    assert!(calls.contains("yay -S --needed voisu-bin\n"), "{calls}");
    assert!(
        calls.contains(
            "systemctl --user daemon-reload\n\
             systemctl --user try-restart voisu.service\n"
        ),
        "{calls}"
    );
    assert!(!calls.contains("try-restart voisu-overlay"), "{calls}");
    assert!(
        stdout(&output).contains("Now installed: voisu 10.0.0"),
        "{output:?}"
    );
}

#[test]
fn an_upgrade_that_installs_nothing_newer_says_so() {
    let host = FakeHost::new();
    host.pacman_owner("voisu-bin");
    host.logging_tool("yay", 0);

    let output = host.voisu(&["update"]);

    assert!(output.status.success(), "{output:?}");
    let out = stdout(&output);
    assert!(out.contains("voisu is still 9.9.9"), "{out}");
    assert!(!out.contains("Now installed"), "{out}");
}

#[test]
fn a_failed_upgrade_exits_non_zero_and_restarts_nothing() {
    let host = FakeHost::new();
    host.pacman_owner("voisu-bin");
    host.logging_tool("yay", 1);

    let output = host.voisu(&["update"]);

    assert_eq!(output.status.code(), Some(4), "{output:?}");
    assert!(
        stderr(&output).contains("yay -S --needed voisu-bin"),
        "{output:?}"
    );
    assert!(!host.calls().contains("try-restart"), "{}", host.calls());
}

#[test]
fn the_source_package_needs_an_explicit_switch_to_prebuilt() {
    let host = FakeHost::new();
    host.pacman_owner("voisu");
    host.logging_tool("yay", 0);

    let refused = host.voisu(&["update"]);
    assert_eq!(refused.status.code(), Some(1), "{refused:?}");
    let message = stdout(&refused);
    assert!(message.contains("compiles"), "{message}");
    assert!(
        message.contains("voisu update --switch-to-bin"),
        "{message}"
    );
    assert!(!host.calls().contains("yay"), "ran yay without consent");

    let switched = host.voisu(&["update", "--switch-to-bin", "--dry-run"]);
    assert!(switched.status.success(), "{switched:?}");
    assert!(
        stdout(&switched).contains("would run: yay -S voisu-bin"),
        "{switched:?}"
    );
    assert!(!host.calls().contains("yay"), "dry run ran yay");
}

#[test]
fn a_binary_no_package_owns_points_at_the_install_docs() {
    let host = FakeHost::new();

    let output = host.voisu(&["update", "--dry-run"]);

    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let message = stdout(&output);
    assert!(message.contains("not owned by a package"), "{message}");
    assert!(message.contains("#installation"), "{message}");
}

#[test]
fn check_compares_the_installed_version_with_the_latest_release() {
    let host = FakeHost::new();
    host.pacman_owner("voisu-bin");
    host.tool(
        "curl",
        &format!(
            "echo \"curl $*\" >> '{}'\nprintf '%s' '{{\"version\":\"99.0.0\"}}'",
            host.log.display()
        ),
    );
    host.logging_tool("yay", 0);

    let output = host.voisu(&["update", "--check"]);

    assert!(output.status.success(), "{output:?}");
    let out = stdout(&output);
    assert!(
        out.contains("Installed: 9.9.9 (Arch package voisu-bin, prebuilt)"),
        "{out}"
    );
    assert!(out.contains("Latest release: 99.0.0"), "{out}");
    assert!(out.contains("Update available"), "{out}");
    let calls = host.calls();
    assert!(!calls.contains("yay"), "check ran yay");
    // The one request is a plain download of the release's version file.
    assert!(
        calls.contains(
            "https://github.com/Anuraj-dev/voisu/releases/latest/download/latest-linux.json"
        ),
        "{calls}"
    );
    assert!(!calls.contains("api.github.com"), "{calls}");
}

#[test]
fn check_still_works_with_the_daily_check_turned_off() {
    let host = FakeHost::new();
    host.pacman_owner("voisu-bin");
    host.tool("curl", "printf '%s' '{\"version\":\"99.0.0\"}'");
    assert!(host.voisu(&["update-check", "off"]).status.success());

    let output = host.voisu(&["update", "--check"]);

    assert!(output.status.success(), "{output:?}");
    assert!(
        stdout(&output).contains("Latest release: 99.0.0"),
        "{output:?}"
    );
}

#[test]
fn a_missing_or_junk_version_file_is_reported_not_trusted() {
    let host = FakeHost::new();
    host.pacman_owner("voisu-bin");
    host.tool("curl", "printf '%s' '<html>Not Found</html>'");

    let output = host.voisu(&["update", "--check"]);

    assert!(!output.status.success(), "{output:?}");
    assert!(!stdout(&output).contains("Update available"), "{output:?}");
    assert!(!stdout(&output).contains("Latest release"), "{output:?}");
}

#[test]
fn check_points_the_source_package_at_the_prebuilt_switch() {
    let host = FakeHost::new();
    host.pacman_owner("voisu");
    host.tool("curl", "printf '%s' '{\"version\":\"99.0.0\"}'");

    let output = host.voisu(&["update", "--check"]);

    assert!(output.status.success(), "{output:?}");
    let out = stdout(&output);
    assert!(out.contains("voisu update --switch-to-bin"), "{out}");
}

#[test]
fn a_check_feeds_the_update_line_in_status_without_another_request() {
    let host = FakeHost::new();
    host.pacman_owner("voisu-bin");
    host.tool("curl", "printf '%s' '{\"version\":\"99.0.0\"}'");
    assert!(host.voisu(&["update", "--check"]).status.success());
    // Any request from `status` would now fail loudly in the log.
    host.tool(
        "curl",
        &format!("echo \"curl $*\" >> '{}'\nexit 6", host.log.display()),
    );

    // No daemon answers here; the line comes all the same, last.
    let output = host.voisu(&["status"]);

    let out = stdout(&output);
    assert!(
        out.ends_with("Update available: 99.0.0, run `voisu update`\n"),
        "{out}"
    );
    assert!(!host.calls().contains("curl"), "status touched the network");
}

#[test]
fn status_says_nothing_about_updates_before_any_check() {
    let host = FakeHost::new();

    let output = host.voisu(&["status"]);

    assert!(!stdout(&output).contains("Update"), "{output:?}");
}

#[test]
fn check_reports_an_unreachable_release_feed() {
    let host = FakeHost::new();
    host.tool(
        "curl",
        "echo 'curl: (6) Could not resolve host' >&2\nexit 6",
    );

    let output = host.voisu(&["update", "--check"]);

    assert_eq!(output.status.code(), Some(4), "{output:?}");
    assert!(stderr(&output).contains("latest release"), "{output:?}");
}

#[test]
fn unknown_update_flags_are_a_usage_error() {
    let host = FakeHost::new();

    let output = host.voisu(&["update", "--bogus"]);

    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(stderr(&output).contains("voisu update"), "{output:?}");
}
