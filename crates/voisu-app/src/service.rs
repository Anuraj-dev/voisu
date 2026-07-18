use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use voisu_core::{
    Command, PROTOCOL_VERSION, Request, Response, VersionEnvelope, socket_path,
};

use crate::process::guard_external_child;

const UNIT_NAME: &str = "voisu.service";
const OVERLAY_UNIT_NAME: &str = "voisu-overlay.service";
const OVERLAY_EXECUTABLE: &str = "/usr/bin/voisu-overlay";
const SYSTEMCTL_DEADLINE: Duration = Duration::from_secs(5);
const SERVICE_TRANSITION_DEADLINE: Duration = Duration::from_secs(3);
// Startup readiness gets a longer bound than stop: 3 s proved too tight on
// loaded machines (CI's parallel flake gate timed out twice in one day on the
// restart path). Readiness normally lands in well under a second, so the
// longer bound only delays reporting genuine failures, while stop keeps the
// short deadline the stuck-stop path (and its test) relies on.
const SERVICE_READY_DEADLINE: Duration = Duration::from_secs(15);
const IPC_DEADLINE: Duration = Duration::from_millis(300);
const MAX_SYSTEMCTL_OUTPUT: u64 = 16 * 1024;
// systemd precedence among packaged locations: an administrator unit under /etc
// overrides the packaged unit under /usr/lib. (A user unit under XDG config
// outranks both, which is why a stale Ticket 09 shadow must be detected on disk
// rather than trusted as the effective unit.)
const PACKAGED_UNIT_DIRS: &[&str] = &["/etc/systemd/user", "/usr/lib/systemd/user"];
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UserServiceAction {
    Install,
    Start,
    Stop,
    Restart,
    Status,
    Uninstall,
}

pub struct UserServiceReport {
    pub message: String,
    pub exit_code: u8,
}

impl UserServiceReport {
    fn success(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            exit_code: 0,
        }
    }
}

struct ServicePaths {
    source_daemon: PathBuf,
    installed_daemon: PathBuf,
    unit: PathBuf,
    packaged_unit: Option<PathBuf>,
    packaged_fallback: Option<String>,
}

struct PackagedUnitDetection {
    path: Option<PathBuf>,
    fallback_reason: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum DaemonIpc {
    Available(String),
    ProtocolMismatch,
    Unavailable,
}

#[derive(Clone, Copy)]
enum OptionalOverlayAction {
    Enable,
    Disable,
}

pub fn manage_user_service(action: UserServiceAction) -> Result<UserServiceReport, String> {
    match action {
        UserServiceAction::Install => {
            let report = install()?;
            Ok(append_optional_overlay_report(
                report,
                manage_optional_overlay(OptionalOverlayAction::Enable),
            ))
        }
        UserServiceAction::Start => start(),
        UserServiceAction::Stop => stop(),
        UserServiceAction::Restart => restart(),
        UserServiceAction::Status => status(),
        UserServiceAction::Uninstall => {
            let overlay_message = manage_optional_overlay(OptionalOverlayAction::Disable);
            let report = uninstall()?;
            Ok(append_optional_overlay_report(report, overlay_message))
        }
    }
}

fn manage_optional_overlay(action: OptionalOverlayAction) -> Option<String> {
    if !packaged_overlay_unit_exists() {
        return None;
    }
    if let Err(reason) = validate_effective_overlay_unit() {
        return Some(format!(
            "warning: optional Overlay service was not managed: {reason}"
        ));
    }

    let (arguments, success_message, failure_prefix): (&[&str], &str, &str) = match action {
        OptionalOverlayAction::Enable => (
            &["enable", "--now", OVERLAY_UNIT_NAME],
            "optional Overlay service enabled and started",
            "optional Overlay service was not enabled",
        ),
        OptionalOverlayAction::Disable => (
            &["disable", "--now", OVERLAY_UNIT_NAME],
            "optional Overlay service disabled and stopped",
            "optional Overlay service was not disabled",
        ),
    };

    Some(match systemctl_required(arguments) {
        Ok(()) => success_message.to_owned(),
        Err(error) => format!("warning: {failure_prefix}: {error}"),
    })
}

fn packaged_overlay_unit_exists() -> bool {
    packaged_unit_dirs().into_iter().any(|directory| {
        let path = directory.join(OVERLAY_UNIT_NAME);
        fs::symlink_metadata(path)
            .is_ok_and(|metadata| metadata.is_file() && !metadata.file_type().is_symlink())
    })
}

fn validate_effective_overlay_unit() -> Result<(), String> {
    let output = systemctl(&[
        "show",
        OVERLAY_UNIT_NAME,
        "-p",
        "LoadState",
        "-p",
        "FragmentPath",
        "-p",
        "ExecStart",
    ])?;
    if !output.success {
        return Err("systemd could not resolve the effective unit".to_owned());
    }

    let mut load_state = None;
    let mut fragment = None;
    let mut exec_lines = Vec::new();
    for line in output.stdout.lines() {
        if let Some(value) = line.strip_prefix("LoadState=") {
            load_state.get_or_insert_with(|| value.trim().to_owned());
        } else if let Some(value) = line.strip_prefix("FragmentPath=") {
            fragment.get_or_insert_with(|| value.trim().to_owned());
        } else if let Some(value) = line.strip_prefix("ExecStart=") {
            exec_lines.push(value.to_owned());
        }
    }

    if load_state.as_deref() != Some("loaded") {
        return Err(format!(
            "effective unit is not cleanly loaded (LoadState={})",
            load_state.as_deref().unwrap_or("unknown")
        ));
    }
    let fragment = fragment
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| "effective unit has no fragment".to_owned())?;
    if !is_packaged_fragment(&fragment) {
        return Err(format!(
            "effective unit {} is not packaged",
            fragment.display()
        ));
    }

    let execs = parse_show_execstart_binaries(&exec_lines);
    if execs.as_slice() != [PathBuf::from(OVERLAY_EXECUTABLE)] {
        return Err("effective unit does not run only /usr/bin/voisu-overlay".to_owned());
    }
    Ok(())
}

fn append_optional_overlay_report(
    mut report: UserServiceReport,
    overlay_message: Option<String>,
) -> UserServiceReport {
    if let Some(message) = overlay_message {
        report.message.push_str("; ");
        report.message.push_str(&message);
    }
    report
}

fn install() -> Result<UserServiceReport, String> {
    let paths = service_paths()?;
    if paths.packaged_unit.is_some() {
        return install_packaged(&paths);
    }
    install_executable(&paths.source_daemon, &paths.installed_daemon)?;
    atomic_write(
        &paths.unit,
        service_unit(&paths.installed_daemon)?.as_bytes(),
        0o600,
    )?;
    systemctl_required(&["daemon-reload"])?;
    systemctl_required(&["enable", UNIT_NAME])?;

    // An upgrade swaps the executable inode atomically. Restart only a service
    // systemd already owns; a manual daemon must never be displaced or turned
    // into a restart loop by installation.
    if systemd_is_active()? {
        systemctl_required(&["restart", UNIT_NAME])?;
        wait_for_managed_daemon()?;
        return Ok(UserServiceReport::success(install_message(
            &paths,
            "systemd user service updated, enabled, and restarted",
        )));
    }
    Ok(UserServiceReport::success(install_message(
        &paths,
        "systemd user service installed and enabled",
    )))
}

fn install_message(paths: &ServicePaths, message: &str) -> String {
    match &paths.packaged_fallback {
        Some(reason) => format!(
            "{message} via Ticket 09 user-data path; packaged unit was ignored: {reason}"
        ),
        None => message.to_owned(),
    }
}

fn install_packaged(paths: &ServicePaths) -> Result<UserServiceReport, String> {
    let has_user_shadow = paths.unit.exists() || paths.installed_daemon.exists();
    let was_active = systemd_is_active()?;

    if has_user_shadow {
        // The XDG user unit has higher precedence than /usr/lib/systemd/user.
        // Stop and remove our old copy before reloading systemd, otherwise an
        // upgrade would silently continue running the stale daemon.
        systemctl_required(&["disable", "--now", UNIT_NAME])?;
        wait_for_service_stop()?;
        remove_if_file(&paths.unit)?;
        remove_if_file(&paths.installed_daemon)?;
        remove_stale_runtime_socket()?;
    }

    systemctl_required(&["daemon-reload"])?;
    systemctl_required(&["enable", UNIT_NAME])?;
    if was_active {
        systemctl_required(&["restart", UNIT_NAME])?;
        wait_for_managed_daemon()?;
    }
    Ok(UserServiceReport::success(
        "packaged systemd user service selected, enabled, and migrated",
    ))
}

fn start() -> Result<UserServiceReport, String> {
    if !systemd_is_active()? {
        match probe_daemon() {
            DaemonIpc::Available(_) => {
                return Ok(UserServiceReport::success(
                    "daemon running outside systemd; service not started",
                ));
            }
            DaemonIpc::ProtocolMismatch => return status(),
            DaemonIpc::Unavailable => {}
        }
    }
    systemctl_required(&["start", UNIT_NAME])?;
    wait_for_managed_daemon()?;
    status()
}

fn stop() -> Result<UserServiceReport, String> {
    let was_active = systemd_is_active()?;
    systemctl_required(&["stop", UNIT_NAME])?;
    if was_active {
        let deadline = Instant::now() + SERVICE_TRANSITION_DEADLINE;
        while Instant::now() < deadline {
            if !systemd_is_active()? && matches!(probe_daemon(), DaemonIpc::Unavailable) {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }
    if systemd_is_active()? {
        return Err("systemd user service did not stop before the deadline".to_owned());
    }
    let mut report = status()?;
    // The requested stop itself succeeded. Preserve the actual ownership/IPC
    // text while keeping stop idempotently successful when the final state is
    // the expected inactive/unavailable pair.
    if report.message == "systemd user service inactive; daemon IPC unavailable" {
        report.exit_code = 0;
    }
    Ok(report)
}

fn wait_for_service_stop() -> Result<(), String> {
    let deadline = Instant::now() + SERVICE_TRANSITION_DEADLINE;
    while Instant::now() < deadline {
        if !systemd_is_active()? && matches!(probe_daemon(), DaemonIpc::Unavailable) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    if systemd_is_active()? {
        return Err("systemd user service did not stop before the deadline".to_owned());
    }
    match probe_daemon() {
        DaemonIpc::Unavailable => Ok(()),
        DaemonIpc::Available(_) => Err(
            "daemon is running outside systemd; stop it before migrating to the packaged service"
                .to_owned(),
        ),
        DaemonIpc::ProtocolMismatch => Err(
            "an incompatible daemon is running outside systemd; stop it before migrating to the packaged service"
                .to_owned(),
        ),
    }
}

fn restart() -> Result<UserServiceReport, String> {
    let active = systemd_is_active()?;
    if !active {
        match probe_daemon() {
            DaemonIpc::Available(_) => {
                return Err(
                    "daemon is running outside systemd; stop it before restarting the service"
                        .to_owned(),
                );
            }
            DaemonIpc::ProtocolMismatch => {
                return Err(
                    "an incompatible daemon is running outside systemd; stop it before \
                     restarting the service"
                        .to_owned(),
                );
            }
            DaemonIpc::Unavailable => {}
        }
    }
    systemctl_required(&["restart", UNIT_NAME])?;
    wait_for_managed_daemon()?;
    status()
}

fn status() -> Result<UserServiceReport, String> {
    let systemd_state = systemd_state()?;
    let active = systemd_state == "active";
    let report = match (active, probe_daemon()) {
        (true, DaemonIpc::Available(state)) => UserServiceReport::success(format!(
            "systemd user service active; daemon IPC {state}"
        )),
        (false, DaemonIpc::Available(state)) => {
            let ownership = if systemd_state == "inactive" {
                "daemon running outside systemd".to_owned()
            } else {
                format!("daemon running outside systemd; systemd user service {systemd_state}")
            };
            UserServiceReport::success(format!("{ownership}; daemon IPC {state}"))
        }
        (true, DaemonIpc::ProtocolMismatch) => UserServiceReport {
            message: "systemd user service active; daemon IPC protocol mismatch".to_owned(),
            exit_code: 5,
        },
        (false, DaemonIpc::ProtocolMismatch) => UserServiceReport {
            message: format!(
                "daemon running outside systemd; systemd user service {systemd_state}; \
                 daemon IPC protocol mismatch"
            ),
            exit_code: 5,
        },
        (true, DaemonIpc::Unavailable) => UserServiceReport {
            message: "systemd user service active; daemon IPC unavailable".to_owned(),
            exit_code: 4,
        },
        (false, DaemonIpc::Unavailable) => UserServiceReport {
            message: format!("systemd user service {systemd_state}; daemon IPC unavailable"),
            exit_code: if systemd_state == "inactive" { 3 } else { 4 },
        },
    };
    Ok(report)
}

fn uninstall() -> Result<UserServiceReport, String> {
    let paths = service_paths()?;
    if paths.packaged_unit.is_some() {
        return uninstall_packaged(&paths);
    }
    // Disable first so no future graphical session can start the old unit.
    // A missing unit is already disabled, so tolerate that one idempotent edge;
    // an existing unit must be disabled successfully or removal would leave a
    // stale enablement symlink behind while falsely reporting success.
    if paths.unit.exists() {
        systemctl_required(&["disable", "--now", UNIT_NAME])?;
    } else {
        let _ = systemctl(&["disable", "--now", UNIT_NAME])?;
    }
    let deadline = Instant::now() + SERVICE_TRANSITION_DEADLINE;
    while Instant::now() < deadline {
        if !systemd_is_active()? && matches!(probe_daemon(), DaemonIpc::Unavailable) {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    if systemd_is_active()? {
        return Err("systemd user service did not stop before the uninstall deadline".to_owned());
    }
    match probe_daemon() {
        DaemonIpc::Available(_) => {
            return Err(
                "daemon is running outside systemd; stop it before uninstalling the service"
                    .to_owned(),
            );
        }
        DaemonIpc::ProtocolMismatch => {
            return Err(
                "an incompatible daemon is running outside systemd; stop it before \
                 uninstalling the service"
                    .to_owned(),
            );
        }
        DaemonIpc::Unavailable => {}
    }

    remove_if_file(&paths.unit)?;
    remove_if_file(&paths.installed_daemon)?;
    remove_stale_runtime_socket()?;
    systemctl_required(&["daemon-reload"])?;
    let _ = systemctl(&["reset-failed", UNIT_NAME])?;
    Ok(UserServiceReport::success(
        "systemd user service disabled and removed",
    ))
}

fn uninstall_packaged(paths: &ServicePaths) -> Result<UserServiceReport, String> {
    // RPM owns the packaged unit and binaries. The CLI may disable the user
    // service and clean a stale pre-RPM shadow, but must never remove files
    // owned by the package or user configuration/credentials.
    systemctl_required(&["disable", "--now", UNIT_NAME])?;
    wait_for_service_stop()?;
    remove_if_file(&paths.unit)?;
    remove_if_file(&paths.installed_daemon)?;
    remove_stale_runtime_socket()?;
    systemctl_required(&["daemon-reload"])?;
    let _ = systemctl(&["reset-failed", UNIT_NAME])?;
    Ok(UserServiceReport::success(
        "packaged systemd user service disabled; run this before removing the RPM to remove packaged artifacts",
    ))
}

fn service_paths() -> Result<ServicePaths, String> {
    let current = std::env::current_exe()
        .map_err(|_| "cannot locate the Voisu executable".to_owned())?;
    let source_daemon = current
        .parent()
        .ok_or_else(|| "cannot locate voisu-daemon beside the Voisu CLI".to_owned())?
        .join("voisu-daemon");
    let data = xdg_home("XDG_DATA_HOME", ".local/share")?;
    let config = xdg_home("XDG_CONFIG_HOME", ".config")?;
    let packaged = packaged_unit_path();
    Ok(ServicePaths {
        source_daemon,
        installed_daemon: data.join("voisu/bin/voisu-daemon"),
        unit: config.join("systemd/user/voisu.service"),
        packaged_unit: packaged.path,
        packaged_fallback: packaged.fallback_reason,
    })
}

enum EffectiveLookup {
    /// A packaged unit that systemd will run; `execs` holds every effective
    /// ExecStart command binary that must validate.
    Packaged {
        fragment: PathBuf,
        execs: Vec<PathBuf>,
    },
    /// A packaged unit is the effective unit but is not cleanly loaded
    /// (LoadState is error/bad-setting/masked/…); it must not be migrated to.
    Unloadable {
        fragment: PathBuf,
        load_state: String,
    },
    /// A packaged unit file exists on disk but its ExecStart could not be
    /// faithfully read; migration must never trust a unit it cannot parse.
    Unparseable { fragment: PathBuf, reason: String },
    /// There is no packaged unit to consider.
    NoPackagedUnit,
}

fn packaged_unit_path() -> PackagedUnitDetection {
    // Ask systemd which unit it would actually run. `systemctl --user show`
    // reflects administrator /etc overrides and drop-ins, so the effective
    // ExecStart is validated rather than the text of a file at a guessed path.
    match effective_packaged_unit() {
        EffectiveLookup::Packaged { fragment, execs } => {
            match validate_packaged_execs(&fragment, &execs) {
                Ok(()) => PackagedUnitDetection {
                    path: Some(fragment),
                    fallback_reason: None,
                },
                Err(reason) => PackagedUnitDetection {
                    path: None,
                    fallback_reason: Some(reason),
                },
            }
        }
        EffectiveLookup::Unloadable {
            fragment,
            load_state,
        } => PackagedUnitDetection {
            path: None,
            fallback_reason: Some(format!(
                "packaged unit {} is not cleanly loaded (LoadState={load_state})",
                fragment.display()
            )),
        },
        EffectiveLookup::Unparseable { fragment, reason } => PackagedUnitDetection {
            path: None,
            fallback_reason: Some(format!(
                "packaged unit {} was not trusted: {reason}",
                fragment.display()
            )),
        },
        EffectiveLookup::NoPackagedUnit => PackagedUnitDetection {
            path: None,
            fallback_reason: None,
        },
    }
}

fn effective_packaged_unit() -> EffectiveLookup {
    let output = systemctl(&[
        "show",
        UNIT_NAME,
        "-p",
        "LoadState",
        "-p",
        "FragmentPath",
        "-p",
        "ExecStart",
    ])
    .ok()
    .filter(|output| output.success);

    if let Some(output) = output {
        let mut load_state = None;
        let mut fragment = None;
        let mut exec_lines: Vec<String> = Vec::new();
        for line in output.stdout.lines() {
            if let Some(value) = line.strip_prefix("LoadState=") {
                load_state.get_or_insert_with(|| value.trim().to_owned());
            } else if let Some(value) = line.strip_prefix("FragmentPath=") {
                fragment.get_or_insert_with(|| value.trim().to_owned());
            } else if let Some(value) = line.strip_prefix("ExecStart=") {
                exec_lines.push(value.to_owned());
            }
        }

        let fragment = fragment.filter(|value| !value.is_empty()).map(PathBuf::from);
        // Case A: systemd's effective unit is itself a packaged unit (possibly
        // via an /etc override or drop-in). Trust exactly what systemd will run.
        if let Some(fragment) = fragment.filter(|fragment| is_packaged_fragment(fragment)) {
            return match load_state.as_deref() {
                Some("loaded") => EffectiveLookup::Packaged {
                    fragment,
                    execs: parse_show_execstart_binaries(&exec_lines),
                },
                other => EffectiveLookup::Unloadable {
                    fragment,
                    load_state: other.unwrap_or("unknown").to_owned(),
                },
            };
        }
        // Otherwise the effective unit is the user-owned Ticket 09 unit (systemd
        // ranks XDG config above the packaged dirs) or nothing is loaded. A
        // packaged unit may still exist on disk, shadowed — fall through to the
        // on-disk detection below, which is exactly the stale-shadow migration
        // case the migration exists for.
    }

    // systemctl could not answer, or the effective unit is a shadowing XDG user
    // unit. Detect a packaged unit directly on disk and validate its ExecStart
    // from the unit file (the best available signal when systemd is not showing
    // it).
    match disk_packaged_unit() {
        Some((fragment, Ok(execs))) => EffectiveLookup::Packaged { fragment, execs },
        Some((fragment, Err(reason))) => EffectiveLookup::Unparseable { fragment, reason },
        None => EffectiveLookup::NoPackagedUnit,
    }
}

fn validate_packaged_execs(fragment: &Path, execs: &[PathBuf]) -> Result<(), String> {
    if execs.is_empty() {
        return Err(format!(
            "packaged unit {} has no resolvable ExecStart",
            fragment.display()
        ));
    }
    // Every command systemd would run must be trusted; a valid first command
    // does not excuse a missing or untrusted later one.
    for exec in execs {
        validate_effective_daemon(exec)?;
    }
    Ok(())
}

fn parse_show_execstart_binaries(lines: &[String]) -> Vec<PathBuf> {
    // systemd renders each command as `{ path=/bin/x ; argv[]=/bin/x --f ; … }`
    // and joins multiple commands with `} ; {`. The command binary is only the
    // `path=` field OPENING a block — at the start of the value or right after
    // a `} ; ` block close. A `{ path=` sequence inside an argv[] argument
    // (e.g. `--config-path=/tmp`, or a literal `{ path=/tmp` argument) must
    // never be collected as an executable.
    let mut binaries = Vec::new();
    for line in lines {
        let value = line.trim_start();
        let mut offset = 0;
        while let Some(found) = value[offset..].find("{ path=") {
            let index = offset + found;
            let opens_block = index == 0 || value[..index].ends_with("} ; ");
            let after = &value[index + "{ path=".len()..];
            let end = after.find(" ;").unwrap_or(after.len());
            if opens_block {
                let path = after[..end].trim();
                if !path.is_empty() {
                    binaries.push(PathBuf::from(path));
                }
            }
            offset = index + "{ path=".len() + end;
        }
    }
    binaries
}

fn parse_unit_file_execstart_binaries(contents: &str) -> Result<Vec<PathBuf>, String> {
    // Conservative unit-file reading for the shadowed-migration decision only:
    // accept exactly the syntax Voisu ships — an absolute, unquoted executable,
    // optionally with execute prefixes (`@-:+!`) attached directly to it — and
    // honor systemd's empty-assignment reset. Everything else (quoting, line
    // continuations, a prefix separated from its executable, relative paths) is
    // rejected with a reason so migration never trusts a unit this parser
    // cannot faithfully read; the install then stays on the Ticket 09 path.
    if contents.lines().any(|line| line.trim_end().ends_with('\\')) {
        return Err("unit file uses line continuations".to_owned());
    }
    let mut binaries = Vec::new();
    let mut in_service_section = false;
    for line in contents.lines() {
        let line = line.trim();
        if line.starts_with('[') && line.ends_with(']') {
            // Only [Service] assignments are systemd commands; an ExecStart=
            // under any other section must never collect into — or reset — the
            // command list.
            in_service_section = line == "[Service]";
            continue;
        }
        if !in_service_section {
            continue;
        }
        // systemd trims whitespace around the key, so `ExecStart =` also counts.
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim_end() != "ExecStart" {
            continue;
        }
        let value = value.trim();
        if value.is_empty() {
            // systemd reset semantics: an empty assignment clears the list.
            binaries.clear();
            continue;
        }
        let token = value
            .split_whitespace()
            .next()
            .expect("non-empty value has a first token");
        let executable = token.trim_start_matches(['@', '-', ':', '+', '!']);
        if executable.is_empty() {
            return Err(format!(
                "ExecStart prefix \"{token}\" is separated from its executable"
            ));
        }
        if value.starts_with(['"', '\'']) || executable.contains(['"', '\'']) {
            return Err("quoted ExecStart executables are not supported".to_owned());
        }
        if !executable.starts_with('/') {
            return Err(format!(
                "ExecStart executable \"{executable}\" is not an absolute path"
            ));
        }
        binaries.push(PathBuf::from(executable));
    }
    Ok(binaries)
}

fn disk_packaged_unit() -> Option<(PathBuf, Result<Vec<PathBuf>, String>)> {
    // Search the packaged directories in systemd precedence order (/etc before
    // /usr/lib) for a regular unit file, and parse its ExecStart commands.
    for directory in packaged_unit_dirs() {
        let path = directory.join(UNIT_NAME);
        let is_regular_file = fs::symlink_metadata(&path)
            .is_ok_and(|metadata| metadata.is_file() && !metadata.file_type().is_symlink());
        if !is_regular_file {
            continue;
        }
        let execs = fs::read_to_string(&path)
            .map_err(|error| format!("unit file is unreadable ({error})"))
            .and_then(|contents| parse_unit_file_execstart_binaries(&contents));
        return Some((path, execs));
    }
    None
}

fn is_packaged_fragment(fragment: &Path) -> bool {
    fragment
        .parent()
        .is_some_and(|parent| packaged_unit_dirs().iter().any(|dir| dir == parent))
}

fn packaged_unit_dirs() -> Vec<PathBuf> {
    // Tests provide a private directory so the public CLI tests never inspect
    // or modify the host's systemd unit directories. Production uses Fedora's
    // administrator and system user-unit locations.
    match std::env::var_os("VOISU_PACKAGED_UNIT_DIR") {
        Some(directory) => vec![PathBuf::from(directory)],
        None => PACKAGED_UNIT_DIRS.iter().map(PathBuf::from).collect(),
    }
}

fn validate_effective_daemon(daemon: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(daemon).map_err(|_| {
        format!(
            "packaged unit ExecStart binary {} is missing",
            daemon.display()
        )
    })?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.mode() & 0o111 == 0 {
        return Err(format!(
            "packaged unit ExecStart binary {} is not a trusted executable",
            daemon.display()
        ));
    }
    Ok(())
}

fn xdg_home(variable: &str, fallback: &str) -> Result<PathBuf, String> {
    if let Some(value) = std::env::var_os(variable).filter(|value| !value.is_empty()) {
        let path = PathBuf::from(value);
        return path
            .is_absolute()
            .then_some(path)
            .ok_or_else(|| format!("{variable} must be absolute"));
    }
    let home = PathBuf::from(
        std::env::var_os("HOME")
            .filter(|value| !value.is_empty())
            .ok_or_else(|| "HOME is not set".to_owned())?,
    );
    if !home.is_absolute() {
        return Err("HOME must be absolute".to_owned());
    }
    Ok(home.join(fallback))
}

fn install_executable(source: &Path, destination: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(source)
        .map_err(|_| "voisu-daemon is not installed beside the Voisu CLI".to_owned())?;
    let owner = metadata.uid();
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || (owner != 0 && owner != unsafe { libc::geteuid() })
        || metadata.mode() & 0o022 != 0
        || metadata.mode() & 0o111 == 0
    {
        return Err("voisu-daemon source is not a trusted executable".to_owned());
    }
    let bytes = fs::read(source).map_err(|_| "cannot read voisu-daemon".to_owned())?;
    atomic_write(destination, &bytes, 0o700)
}

fn atomic_write(path: &Path, bytes: &[u8], mode: u32) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "installation path has no parent directory".to_owned())?;
    fs::create_dir_all(parent)
        .map_err(|_| format!("cannot create installation directory {}", parent.display()))?;
    let parent_metadata = fs::symlink_metadata(parent)
        .map_err(|_| format!("cannot inspect installation directory {}", parent.display()))?;
    if !parent_metadata.is_dir()
        || parent_metadata.file_type().is_symlink()
        || parent_metadata.uid() != unsafe { libc::geteuid() }
    {
        return Err(format!("unsafe installation directory {}", parent.display()));
    }

    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temp = parent.join(format!(
        ".voisu-install.{}.{}.tmp",
        std::process::id(),
        sequence
    ));
    let result = (|| -> std::io::Result<()> {
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&temp)?;
        output.write_all(bytes)?;
        output.sync_all()?;
        fs::set_permissions(&temp, fs::Permissions::from_mode(mode))?;
        fs::rename(&temp, path)?;
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
        return Err(format!("cannot atomically install {}", path.display()));
    }
    Ok(())
}

fn service_unit(executable: &Path) -> Result<String, String> {
    let executable = quote_systemd_path(executable)?;
    Ok(format!(concat!(
        "[Unit]\n",
        "Description=Voisu dictation daemon\n",
        "After=dbus.socket pipewire.service xdg-desktop-portal.service\n",
        "Wants=dbus.socket pipewire.service xdg-desktop-portal.service\n",
        "PartOf=graphical-session.target\n",
        "StartLimitIntervalSec=30s\n",
        "StartLimitBurst=3\n\n",
        "[Service]\n",
        "Type=simple\n",
        "ExecStart={} --systemd\n",
        "Restart=on-failure\n",
        "RestartSec=2s\n",
        // Graceful shutdown stops an active Recording, processes it to
        // completion, joins the actor, and drains retained provider cleanup;
        // that internal budget peaks near 37 seconds, so the stop bound is set
        // explicitly and comfortably above it instead of relying on the
        // distribution's default.
        "TimeoutStopSec=60s\n\n",
        "[Install]\n",
        "WantedBy=graphical-session.target\n",
    ), executable))
}

fn quote_systemd_path(path: &Path) -> Result<String, String> {
    let value = path
        .to_str()
        .ok_or_else(|| "service executable path is not valid UTF-8".to_owned())?;
    if value.chars().any(char::is_control) {
        return Err("service executable path contains control characters".to_owned());
    }
    let escaped = value
        .replace('%', "%%")
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    Ok(format!("\"{escaped}\""))
}

struct ProcessOutput {
    success: bool,
    stdout: String,
}

fn systemctl_required(arguments: &[&str]) -> Result<(), String> {
    let output = systemctl(arguments)?;
    if output.success {
        Ok(())
    } else {
        Err(format!(
            "systemctl --user {} failed",
            arguments.join(" ")
        ))
    }
}

fn systemctl(arguments: &[&str]) -> Result<ProcessOutput, String> {
    let mut command = ProcessCommand::new("systemctl");
    command
        .arg("--user")
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    guard_external_child(&mut command);
    let mut child = command
        .spawn()
        .map_err(|_| "systemctl is unavailable".to_owned())?;
    let deadline = Instant::now() + SYSTEMCTL_DEADLINE;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("systemctl --user deadline elapsed".to_owned());
            }
            Err(_) => return Err("cannot inspect systemctl --user".to_owned()),
        }
    }
    let output = child
        .wait_with_output()
        .map_err(|_| "cannot collect systemctl --user output".to_owned())?;
    let stdout = bounded_utf8(output.stdout)?;
    let _ = bounded_utf8(output.stderr)?;
    Ok(ProcessOutput {
        success: output.status.success(),
        stdout,
    })
}

fn bounded_utf8(bytes: Vec<u8>) -> Result<String, String> {
    if bytes.len() as u64 > MAX_SYSTEMCTL_OUTPUT {
        return Err("systemctl --user output is too large".to_owned());
    }
    String::from_utf8(bytes).map_err(|_| "systemctl --user returned invalid output".to_owned())
}

fn systemd_is_active() -> Result<bool, String> {
    Ok(systemd_state()? == "active")
}

fn systemd_state() -> Result<String, String> {
    let output = systemctl(&["is-active", UNIT_NAME])?;
    let state = output.stdout.trim();
    if state.is_empty() || state.chars().any(char::is_control) {
        return Err("systemctl --user returned an invalid service state".to_owned());
    }
    Ok(state.to_owned())
}

fn wait_for_managed_daemon() -> Result<(), String> {
    let deadline = Instant::now() + SERVICE_READY_DEADLINE;
    while Instant::now() < deadline {
        if systemd_is_active()? && matches!(probe_daemon(), DaemonIpc::Available(_)) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    let report = status()?;
    Err(format!("service did not become ready: {}", report.message))
}

fn probe_daemon() -> DaemonIpc {
    let Ok(path) = socket_path() else {
        return DaemonIpc::Unavailable;
    };
    let Ok(mut stream) = UnixStream::connect(path) else {
        return DaemonIpc::Unavailable;
    };
    if stream.set_read_timeout(Some(IPC_DEADLINE)).is_err()
        || stream.set_write_timeout(Some(IPC_DEADLINE)).is_err()
    {
        return DaemonIpc::Unavailable;
    }
    let request = Request {
        version: PROTOCOL_VERSION,
        command: Command::Status,
    };
    if serde_json::to_writer(&mut stream, &request).is_err() || stream.write_all(b"\n").is_err() {
        return DaemonIpc::Unavailable;
    }
    let mut response = Vec::new();
    if stream
        .take(64 * 1024)
        .read_to_end(&mut response)
        .is_err()
        || !response.ends_with(b"\n")
    {
        return DaemonIpc::Unavailable;
    }
    let Ok(envelope) = serde_json::from_slice::<VersionEnvelope>(&response) else {
        return DaemonIpc::Unavailable;
    };
    if envelope.version != PROTOCOL_VERSION {
        return DaemonIpc::ProtocolMismatch;
    }
    let Ok(response) = serde_json::from_slice::<Response>(&response) else {
        return DaemonIpc::Unavailable;
    };
    response
        .state
        .map(|state| DaemonIpc::Available(state.cli_label().to_owned()))
        .unwrap_or(DaemonIpc::Unavailable)
}

fn remove_if_file(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() || metadata.file_type().is_symlink() => {
            fs::remove_file(path).map_err(|_| format!("cannot remove {}", path.display()))
        }
        Ok(_) => Err(format!("refusing to remove non-file {}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(format!("cannot inspect {}", path.display())),
    }
}

fn remove_stale_runtime_socket() -> Result<(), String> {
    use std::os::unix::fs::FileTypeExt;

    let Ok(path) = socket_path() else {
        return Ok(());
    };
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err("cannot inspect daemon runtime socket".to_owned()),
    };
    if !metadata.file_type().is_socket() || metadata.uid() != unsafe { libc::geteuid() } {
        return Err("refusing to remove unsafe daemon runtime socket".to_owned());
    }
    if UnixStream::connect(&path).is_ok() {
        return Err("daemon runtime socket is still active".to_owned());
    }
    fs::remove_file(path).map_err(|_| "cannot remove stale daemon runtime socket".to_owned())
}
