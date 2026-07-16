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
const SYSTEMCTL_DEADLINE: Duration = Duration::from_secs(5);
const SERVICE_TRANSITION_DEADLINE: Duration = Duration::from_secs(3);
const IPC_DEADLINE: Duration = Duration::from_millis(300);
const MAX_SYSTEMCTL_OUTPUT: u64 = 16 * 1024;
const PACKAGED_UNIT_DIRS: &[&str] = &["/usr/lib/systemd/user", "/etc/systemd/user"];
const PACKAGED_DAEMON_PATH: &str = "/usr/bin/voisu-daemon";
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

pub fn manage_user_service(action: UserServiceAction) -> Result<UserServiceReport, String> {
    match action {
        UserServiceAction::Install => install(),
        UserServiceAction::Start => start(),
        UserServiceAction::Stop => stop(),
        UserServiceAction::Restart => restart(),
        UserServiceAction::Status => status(),
        UserServiceAction::Uninstall => uninstall(),
    }
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

fn packaged_unit_path() -> PackagedUnitDetection {
    // Tests provide a private directory so the public CLI tests never inspect
    // or modify the host's systemd unit directories. Production uses Fedora's
    // system and administrator user-unit locations.
    let configured = std::env::var_os("VOISU_PACKAGED_UNIT_DIR");
    let directories = configured
        .as_deref()
        .map(|directory| vec![PathBuf::from(directory)])
        .unwrap_or_else(|| PACKAGED_UNIT_DIRS.iter().map(PathBuf::from).collect());
    let mut fallback_reason = None;
    for directory in directories {
        let path = directory.join(UNIT_NAME);
        let is_regular_file = fs::symlink_metadata(&path).is_ok_and(|metadata| {
            metadata.is_file() && !metadata.file_type().is_symlink()
        });
        if !is_regular_file {
            continue;
        }
        match validate_packaged_unit(&path) {
            Ok(()) => {
                return PackagedUnitDetection {
                    path: Some(path),
                    fallback_reason: None,
                };
            }
            Err(reason) => {
                fallback_reason.get_or_insert(reason);
            }
        }
    }
    PackagedUnitDetection {
        path: None,
        fallback_reason,
    }
}

fn validate_packaged_unit(unit: &Path) -> Result<(), String> {
    let daemon = packaged_daemon_path();
    let metadata = fs::symlink_metadata(&daemon).map_err(|_| {
        format!(
            "packaged daemon binary {} is missing",
            daemon.display()
        )
    })?;
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.mode() & 0o111 == 0
    {
        return Err(format!(
            "packaged daemon binary {} is not a trusted executable",
            daemon.display()
        ));
    }
    let contents = fs::read_to_string(unit)
        .map_err(|_| format!("cannot read packaged unit {}", unit.display()))?;
    let expected = format!("ExecStart={} --systemd", daemon.display());
    if !contents.lines().any(|line| line.trim() == expected) {
        return Err(format!(
            "packaged unit {} does not reference {}",
            unit.display(),
            daemon.display()
        ));
    }
    Ok(())
}

fn packaged_daemon_path() -> PathBuf {
    std::env::var_os("VOISU_PACKAGED_DAEMON_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(PACKAGED_DAEMON_PATH))
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
    let deadline = Instant::now() + SERVICE_TRANSITION_DEADLINE;
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
