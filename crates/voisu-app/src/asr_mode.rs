//! Explicit Local/Cloud ASR mode: admission, marker, persistence, and CLI IPC.
//!
//! Local ASR stays unavailable before capture until later phases. This module
//! owns mode transactions and the typed Cloud/Local resource choice; it does
//! not download models or load FFI.

use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::os::fd::{FromRawFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use voisu_core::{
    ASR_MODE_V1, AsrMode, AsrModeStatus, Command, DaemonState, LocalReadiness, PROTOCOL_VERSION,
    Request, Response, VersionEnvelope, socket_path,
};

use crate::config::{self, ASR_MODE_KEY, ConfigLock, is_asr_mode_key};
use crate::daemon_lock;

const MODE_INITIALIZED_NAME: &str = "mode-initialized";
const CONFIG_REVISION_NAME: &str = "config-revision";
const IO_DEADLINE: Duration = Duration::from_secs(2);
const MAX_MODE_RESPONSE_BYTES: usize = 64 * 1024;
const LOCAL_UNAVAILABLE: &str = "Local selected; model unavailable";
const LOCAL_START_REFUSED: &str = "Local ASR is unavailable; Start refused before capture";
const LOCAL_REPLAY_REFUSED: &str = "Local ASR is unavailable; Replay refused before capture";

/// Owned Cloud vs Local execution resources. Choose this before constructing
/// provider clients. L1 never builds Local transcription.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AsrPathResources {
    Cloud,
    Local { readiness: LocalReadiness },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AsrModeCommit {
    pub mode: AsrMode,
    pub revision: u64,
    pub path: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PersistError {
    Intact(String),
    Indeterminate(String),
}

impl PersistError {
    pub fn message(&self) -> &str {
        match self {
            Self::Intact(message) | Self::Indeterminate(message) => message,
        }
    }

    pub fn is_indeterminate(&self) -> bool {
        matches!(self, Self::Indeterminate(_))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AdmissionError {
    Access(String),
    Blocked(String),
}

impl AdmissionError {
    pub fn message(&self) -> &str {
        match self {
            Self::Access(message) | Self::Blocked(message) => message,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RecordingAdmission {
    Cloud { mode: AsrMode, revision: u64 },
    LocalUnavailable { revision: u64, error: String },
}

/// Start and Replay share one admission helper so Local cannot construct providers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CaptureKind {
    Start,
    Replay,
}

impl CaptureKind {
    fn refused_message(self) -> &'static str {
        match self {
            Self::Start => LOCAL_START_REFUSED,
            Self::Replay => LOCAL_REPLAY_REFUSED,
        }
    }
}

#[derive(Debug)]
pub struct CliModeError {
    pub exit_code: u8,
    pub message: String,
}

impl CliModeError {
    fn new(exit_code: u8, message: impl Into<String>) -> Self {
        Self {
            exit_code,
            message: message.into(),
        }
    }

    fn from_connect(error: String) -> Self {
        Self::new(3, error)
    }
}

pub fn ensure_private_state_dir() -> Result<PathBuf, String> {
    ensure_private_state_dir_at(&voisu_core::state_dir()?)
}

pub fn ensure_private_state_dir_at(dir: &Path) -> Result<PathBuf, String> {
    let root = dir
        .parent()
        .ok_or_else(|| "state directory has no parent".to_owned())?;
    fs::create_dir_all(root)
        .map_err(|error| format!("cannot create state root {}: {error}", root.display()))?;
    match fs::symlink_metadata(dir) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(format!("unsafe state path: {}", dir.display()));
            }
            // SAFETY: geteuid has no preconditions and does not mutate memory.
            if metadata.uid() != unsafe { libc::geteuid() } {
                return Err(format!(
                    "state directory is not owned by the current user: {}",
                    dir.display()
                ));
            }
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
                .map_err(|error| format!("cannot secure state directory: {error}"))?;
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {
            fs::DirBuilder::new()
                .mode(0o700)
                .create(dir)
                .map_err(|error| format!("cannot create private state directory: {error}"))?;
        }
        Err(error) => return Err(format!("cannot inspect state directory: {error}")),
    }
    Ok(dir.to_path_buf())
}

pub fn persist_asr_mode(mode: AsrMode) -> Result<AsrModeCommit, PersistError> {
    let state = ensure_private_state_dir().map_err(PersistError::Intact)?;
    persist_asr_mode_at(&config::config_path(), &state, mode)
}

pub fn persist_asr_mode_at(
    config_path: &Path,
    state_dir: &Path,
    mode: AsrMode,
) -> Result<AsrModeCommit, PersistError> {
    persist_asr_mode_at_with(config_path, state_dir, mode, commit_revision)
}

fn persist_asr_mode_at_with(
    config_path: &Path,
    state_dir: &Path,
    mode: AsrMode,
    commit_revision: impl FnOnce(&Path, u64) -> Result<u64, PersistError>,
) -> Result<AsrModeCommit, PersistError> {
    let state_dir = ensure_private_state_dir_at(state_dir).map_err(PersistError::Intact)?;
    let _lock =
        ConfigLock::acquire_bounded(config_path, IO_DEADLINE).map_err(PersistError::Intact)?;
    // Validate revision before replacing config; a later revision failure is
    // indeterminate because the mode write already happened.
    let next = read_revision_at(&state_dir)
        .map_err(|error| PersistError::Intact(error.message().to_owned()))?
        .saturating_add(1);
    write_mode_initialized_marker(&state_dir)?;
    config::write_asr_mode_unlocked(config_path, mode).map_err(map_write_error)?;
    let revision = commit_revision(&state_dir, next)
        .map_err(|error| PersistError::Indeterminate(error.message().to_owned()))?;
    Ok(AsrModeCommit {
        mode,
        revision,
        path: config_path.to_path_buf(),
    })
}

fn map_write_error(message: String) -> PersistError {
    if message.starts_with("indeterminate:") {
        PersistError::Indeterminate(message)
    } else {
        PersistError::Intact(message)
    }
}

pub fn mode_initialized_path(state_dir: &Path) -> PathBuf {
    state_dir.join(MODE_INITIALIZED_NAME)
}

pub fn revision_path(state_dir: &Path) -> PathBuf {
    state_dir.join(CONFIG_REVISION_NAME)
}

fn write_mode_initialized_marker(state_dir: &Path) -> Result<(), PersistError> {
    let path = mode_initialized_path(state_dir);
    match fs::symlink_metadata(&path) {
        Ok(_) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => {
            durable_replace(&path, b"initialized\n")
        }
        Err(error) => Err(PersistError::Intact(format!(
            "cannot inspect mode-initialized marker {}: {error}",
            path.display()
        ))),
    }
}

fn marker_present(state_dir: &Path) -> Result<bool, AdmissionError> {
    match fs::symlink_metadata(mode_initialized_path(state_dir)) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
        Err(error) => Err(AdmissionError::Access(format!(
            "cannot inspect mode-initialized marker: {error}"
        ))),
    }
}

fn commit_revision(state_dir: &Path, next: u64) -> Result<u64, PersistError> {
    durable_replace(&revision_path(state_dir), format!("{next}\n").as_bytes())?;
    Ok(next)
}

fn read_revision_at(state_dir: &Path) -> Result<u64, AdmissionError> {
    match fs::read_to_string(revision_path(state_dir)) {
        Ok(contents) => contents
            .trim()
            .parse()
            .map_err(|_| AdmissionError::Blocked("config revision is unreadable".to_owned())),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(0),
        Err(error) => Err(AdmissionError::Access(format!(
            "cannot read config revision: {error}"
        ))),
    }
}

fn durable_replace(path: &Path, contents: &[u8]) -> Result<(), PersistError> {
    let parent = path
        .parent()
        .ok_or_else(|| PersistError::Intact(format!("path has no parent: {}", path.display())))?;
    fs::create_dir_all(parent).map_err(|error| {
        PersistError::Intact(format!(
            "cannot create directory {}: {error}",
            parent.display()
        ))
    })?;
    let mut file = tempfile::Builder::new()
        .prefix(".asr-mode.")
        .tempfile_in(parent)
        .map_err(|error| {
            PersistError::Intact(format!(
                "cannot stage write in {}: {error}",
                parent.display()
            ))
        })?;
    file.write_all(contents)
        .and_then(|()| file.as_file().sync_all())
        .map_err(|error| {
            PersistError::Intact(format!("cannot write {}: {error}", path.display()))
        })?;
    file.persist(path).map_err(|error| {
        PersistError::Intact(format!(
            "cannot persist {}: {}",
            path.display(),
            error.error
        ))
    })?;
    File::open(path)
        .and_then(|file| file.sync_all())
        .map_err(|error| {
            PersistError::Indeterminate(format!(
                "indeterminate: replaced {} but file sync failed: {error}",
                path.display()
            ))
        })?;
    File::open(parent)
        .and_then(|dir| dir.sync_all())
        .map_err(|error| {
            PersistError::Indeterminate(format!(
                "indeterminate: replaced {} but directory sync failed: {error}",
                path.display()
            ))
        })?;
    Ok(())
}

pub fn load_mode(config_path: &Path, state_dir: &Path) -> Result<(AsrMode, u64), AdmissionError> {
    // Same exclusive lock persist holds, so marker/config/revision cannot tear.
    let _lock =
        ConfigLock::acquire_bounded(config_path, IO_DEADLINE).map_err(AdmissionError::Access)?;
    let marker = marker_present(state_dir)?;
    let revision = read_revision_at(state_dir)?;
    match fs::read_to_string(config_path) {
        Ok(contents) => match parse_asr_mode_document(&contents)? {
            Some(mode) => Ok((mode, revision)),
            None if marker => Err(AdmissionError::Blocked(
                "ASR mode is missing after an explicit selection; admission blocked".to_owned(),
            )),
            None => Ok((AsrMode::Cloud, revision)),
        },
        Err(error) if error.kind() == ErrorKind::NotFound => {
            if marker {
                Err(AdmissionError::Blocked(
                    "ASR mode config is missing after an explicit selection; admission blocked"
                        .to_owned(),
                ))
            } else {
                Ok((AsrMode::Cloud, revision))
            }
        }
        Err(error) => Err(AdmissionError::Access(format!(
            "cannot read ASR mode config {}: {error}",
            config_path.display()
        ))),
    }
}

fn parse_asr_mode_document(contents: &str) -> Result<Option<AsrMode>, AdmissionError> {
    let mut in_root = true;
    let mut seen = HashSet::new();
    let mut asr_mode = None;
    for raw in contents.lines() {
        let line = strip_comment(raw).trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') {
            in_root = false;
            continue;
        }
        if !in_root {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err(AdmissionError::Blocked(
                "malformed TOML in daemon config; admission blocked".to_owned(),
            ));
        };
        let key = key.trim();
        if key.is_empty() {
            return Err(AdmissionError::Blocked(
                "malformed TOML in daemon config; admission blocked".to_owned(),
            ));
        }
        // Escapes are not decoded; treating them as a missing key would default
        // to Cloud and route an explicit Local file through capture.
        if key.contains('\\') {
            return Err(AdmissionError::Blocked(
                "unsupported TOML key syntax in daemon config; admission blocked".to_owned(),
            ));
        }
        let seen_key = if is_asr_mode_key(key) {
            ASR_MODE_KEY
        } else {
            key
        };
        if !seen.insert(seen_key.to_owned()) {
            return Err(AdmissionError::Blocked(format!(
                "duplicate root key {key} in daemon config; admission blocked"
            )));
        }
        if !is_asr_mode_key(key) {
            continue;
        }
        let value = value.trim();
        asr_mode = Some(match value {
            "\"cloud\"" | "'cloud'" => AsrMode::Cloud,
            "\"local\"" | "'local'" => AsrMode::Local,
            _ => {
                return Err(AdmissionError::Blocked(
                    "unknown asr_mode in daemon config; admission blocked".to_owned(),
                ));
            }
        });
    }
    Ok(asr_mode)
}

fn strip_comment(line: &str) -> &str {
    line.split('#').next().unwrap_or(line)
}

pub fn admit_recording() -> Result<RecordingAdmission, AdmissionError> {
    admit_recording_at(&config::config_path(), &state_dir_for_admission()?)
}

fn state_dir_for_admission() -> Result<PathBuf, AdmissionError> {
    voisu_core::state_dir().map_err(AdmissionError::Access)
}

pub fn admit_recording_at(
    config_path: &Path,
    state_dir: &Path,
) -> Result<RecordingAdmission, AdmissionError> {
    let (mode, revision) = load_mode(config_path, state_dir)?;
    match select_resources(mode) {
        AsrPathResources::Cloud => Ok(RecordingAdmission::Cloud { mode, revision }),
        AsrPathResources::Local { readiness } => Ok(RecordingAdmission::LocalUnavailable {
            revision,
            error: match readiness {
                LocalReadiness::Unavailable { error } => error,
                _ => LOCAL_UNAVAILABLE.to_owned(),
            },
        }),
    }
}

/// Choose Cloud or Local resources before constructing provider dependencies.
pub fn select_resources(mode: AsrMode) -> AsrPathResources {
    match mode {
        AsrMode::Cloud => AsrPathResources::Cloud,
        AsrMode::Local => AsrPathResources::Local {
            readiness: production_local_readiness(),
        },
    }
}

fn production_local_readiness() -> LocalReadiness {
    let _ = crate::local_model::observe_for_admission();
    LocalReadiness::Unavailable {
        error: LOCAL_UNAVAILABLE.to_owned(),
    }
}

pub fn status_report(active: Option<AsrMode>) -> AsrModeStatus {
    status_report_at(
        &config::config_path(),
        &voisu_core::state_dir().ok(),
        active,
    )
}

pub fn status_report_at(
    config_path: &Path,
    state_dir: &Option<PathBuf>,
    active: Option<AsrMode>,
) -> AsrModeStatus {
    let Some(state_dir) = state_dir else {
        return blocked_status(
            active,
            "cannot resolve Voisu state directory; admission blocked",
        );
    };
    match load_mode(config_path, state_dir) {
        Ok((mode, revision)) => AsrModeStatus {
            pending: Some(mode),
            active,
            revision: Some(revision),
            local_readiness: match mode {
                AsrMode::Cloud => LocalReadiness::Absent,
                AsrMode::Local => production_local_readiness(),
            },
            admission_error: None,
        },
        Err(error) => blocked_status(active, error.message()),
    }
}

fn blocked_status(active: Option<AsrMode>, error: impl Into<String>) -> AsrModeStatus {
    let error = error.into();
    AsrModeStatus {
        pending: None,
        active,
        revision: None,
        local_readiness: LocalReadiness::Unavailable {
            error: error.clone(),
        },
        admission_error: Some(error),
    }
}

pub fn attach_status(response: &mut Response, active: Option<AsrMode>) {
    response.capabilities = vec![ASR_MODE_V1.to_owned()];
    response.asr_mode = Some(status_report(active));
}

/// Historic first line, then pending/active/revision/readiness when advertised.
pub fn write_cli_status(message: &str, asr: Option<&AsrModeStatus>) {
    println!("{message}");
    let Some(asr) = asr else {
        return;
    };
    match asr.pending {
        Some(mode) => println!("asr mode pending: {}", mode.as_str()),
        None => println!("asr mode pending: unknown"),
    }
    if let Some(active) = asr.active {
        println!("asr mode active: {}", active.as_str());
    }
    match asr.revision {
        Some(revision) => println!("config revision: {revision}"),
        None => println!("config revision: unknown"),
    }
    println!(
        "local readiness: {}",
        format_local_readiness(&asr.local_readiness)
    );
    if let Some(error) = &asr.admission_error {
        println!("asr admission: {error}");
    }
}

fn format_local_readiness(readiness: &LocalReadiness) -> String {
    match readiness {
        LocalReadiness::Absent => "absent".to_owned(),
        LocalReadiness::Verifying => "verifying".to_owned(),
        LocalReadiness::Loading => "loading".to_owned(),
        LocalReadiness::Ready {
            model_identity: None,
        } => "ready".to_owned(),
        LocalReadiness::Ready {
            model_identity: Some(identity),
        } => format!("ready ({identity})"),
        LocalReadiness::Busy => "busy".to_owned(),
        LocalReadiness::Stopping => "stopping".to_owned(),
        LocalReadiness::Unavailable { error } => format!("unavailable ({error})"),
    }
}

/// Snapshot admission, then Cloud resources or a rejection before capture.
pub fn admit_cloud_capture(
    kind: CaptureKind,
    daemon_state: DaemonState,
    active: Option<AsrMode>,
) -> Result<AsrMode, Box<Response>> {
    match admit_recording() {
        Ok(RecordingAdmission::Cloud { mode, .. }) => match select_resources(mode) {
            AsrPathResources::Cloud => Ok(mode),
            AsrPathResources::Local { .. } => Err(Box::new(reject_capture(
                kind.refused_message(),
                daemon_state,
                active,
            ))),
        },
        Ok(RecordingAdmission::LocalUnavailable { .. }) => Err(Box::new(reject_capture(
            kind.refused_message(),
            daemon_state,
            active,
        ))),
        Err(error) => Err(Box::new(reject_capture(
            error.message(),
            daemon_state,
            active,
        ))),
    }
}

fn reject_capture(
    message: impl Into<String>,
    daemon_state: DaemonState,
    active: Option<AsrMode>,
) -> Response {
    let mut response = Response::rejected(Some(daemon_state), message);
    attach_status(&mut response, active);
    response
}

pub fn set_asr_mode_response(
    persist: Result<AsrModeCommit, PersistError>,
    daemon_state: DaemonState,
    active: Option<AsrMode>,
) -> Response {
    match persist {
        Ok(commit) => {
            let mut response = Response::success(
                daemon_state,
                format!("ASR mode set to {}", commit.mode.as_str()),
            );
            attach_status(&mut response, active);
            if let Some(status) = response.asr_mode.as_mut() {
                status.pending = Some(commit.mode);
                status.revision = Some(commit.revision);
            }
            response
        }
        Err(error) => {
            let mut response = Response::rejected(Some(daemon_state), error.message());
            attach_status(&mut response, active);
            response
        }
    }
}

pub fn apply_cli_mode(mode: AsrMode) -> Result<String, CliModeError> {
    match try_connect_daemon() {
        Ok(stream) => set_mode_over_ipc(stream, mode),
        Err(_) => set_mode_offline(mode),
    }
}

fn try_connect_daemon() -> Result<UnixStream, String> {
    let path = socket_path()?;
    connect_unix_bounded(&path, IO_DEADLINE)
}

fn set_mode_over_ipc(stream: UnixStream, mode: AsrMode) -> Result<String, CliModeError> {
    // The daemon serves one command per connection, matching `voisu` itself.
    let status = send_request(stream, Command::Status)?;
    if !status
        .capabilities
        .iter()
        .any(|capability| capability == ASR_MODE_V1)
    {
        return Err(CliModeError::new(
            3,
            "daemon does not support ASR mode; upgrade and restart the daemon",
        ));
    }
    let response = send_request(
        try_connect_daemon().map_err(CliModeError::from_connect)?,
        Command::SetAsrMode(mode),
    )?;
    if !response.ok {
        return Err(CliModeError::new(4, response.message));
    }
    Ok(response.message)
}

fn set_mode_offline(mode: AsrMode) -> Result<String, CliModeError> {
    let socket = socket_path().map_err(|error| CliModeError::new(3, error))?;
    match daemon_lock::try_acquire_lifetime_lock() {
        Ok(lock) => match connect_unix_bounded(&socket, IO_DEADLINE) {
            Ok(stream) => {
                drop(lock);
                set_mode_over_ipc(stream, mode)
            }
            Err(_) => {
                let commit = persist_asr_mode(mode)
                    .map_err(|error| CliModeError::new(4, error.message().to_owned()))?;
                Ok(format!(
                    "ASR mode set to {}; it applies at the next supported daemon start",
                    commit.mode.as_str()
                ))
            }
        },
        Err(_) => match connect_unix_bounded(&socket, IO_DEADLINE) {
            Ok(stream) => set_mode_over_ipc(stream, mode),
            Err(_) => Err(CliModeError::new(
                3,
                "daemon lock is held but IPC is unavailable; mode left untouched",
            )),
        },
    }
}

fn connect_unix_bounded(path: &Path, timeout: Duration) -> Result<UnixStream, String> {
    let bytes = path.as_os_str().as_bytes();
    // SAFETY: sockaddr_un is a C struct; zeroing is the documented connect setup.
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    if bytes.len() >= addr.sun_path.len() {
        return Err("daemon socket path is too long".to_owned());
    }
    for (index, byte) in bytes.iter().enumerate() {
        addr.sun_path[index] = *byte as libc::c_char;
    }
    // SAFETY: a fresh socket fd is owned until transferred to UnixStream or closed.
    let fd = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            0,
        )
    };
    if fd < 0 {
        return Err(format!(
            "cannot open daemon socket: {}",
            std::io::Error::last_os_error()
        ));
    }
    let connected = unsafe {
        libc::connect(
            fd,
            std::ptr::addr_of!(addr).cast::<libc::sockaddr>(),
            std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t,
        )
    };
    if connected != 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EINPROGRESS) && error.kind() != ErrorKind::WouldBlock
        {
            unsafe { libc::close(fd) };
            return Err(error.to_string());
        }
        let started = Instant::now();
        loop {
            let remaining = timeout
                .checked_sub(started.elapsed())
                .filter(|remaining| !remaining.is_zero())
                .ok_or_else(|| {
                    unsafe { libc::close(fd) };
                    "daemon connection deadline elapsed".to_owned()
                })?;
            let mut pollfd = libc::pollfd {
                fd,
                events: libc::POLLOUT,
                revents: 0,
            };
            let millis = i32::try_from(remaining.as_millis()).unwrap_or(i32::MAX);
            let polled = unsafe { libc::poll(&mut pollfd, 1, millis) };
            if polled == 0 {
                unsafe { libc::close(fd) };
                return Err("daemon connection deadline elapsed".to_owned());
            }
            if polled < 0 {
                let poll_error = std::io::Error::last_os_error();
                if poll_error.kind() == ErrorKind::Interrupted {
                    continue;
                }
                unsafe { libc::close(fd) };
                return Err(poll_error.to_string());
            }
            let mut so_error: libc::c_int = 0;
            let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
            let option = unsafe {
                libc::getsockopt(
                    fd,
                    libc::SOL_SOCKET,
                    libc::SO_ERROR,
                    std::ptr::addr_of_mut!(so_error).cast(),
                    &mut len,
                )
            };
            if option != 0 {
                let sock_error = std::io::Error::last_os_error();
                unsafe { libc::close(fd) };
                return Err(sock_error.to_string());
            }
            if so_error != 0 {
                unsafe { libc::close(fd) };
                return Err(std::io::Error::from_raw_os_error(so_error).to_string());
            }
            break;
        }
    }
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL, 0) };
    if flags >= 0 {
        unsafe {
            libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK);
        }
    }
    // SAFETY: fd is a connected socket we uniquely own.
    let stream = unsafe { UnixStream::from_raw_fd(fd as RawFd) };
    stream
        .set_read_timeout(Some(timeout))
        .and_then(|()| stream.set_write_timeout(Some(timeout)))
        .map_err(|error| error.to_string())?;
    Ok(stream)
}

fn send_request(mut stream: UnixStream, command: Command) -> Result<Response, CliModeError> {
    stream
        .set_write_timeout(Some(IO_DEADLINE))
        .map_err(|_| CliModeError::new(3, "failed to configure daemon connection deadline"))?;
    let request = Request::new(command);
    if serde_json::to_writer(&mut stream, &request).is_err() || stream.write_all(b"\n").is_err() {
        return Err(CliModeError::new(
            3,
            "ASR mode result is unknown; the daemon did not acknowledge the change",
        ));
    }
    let mut reader = BufReader::new(stream);
    let frame = read_frame(&mut reader).map_err(|message| CliModeError::new(3, message))?;
    let envelope: VersionEnvelope = serde_json::from_str(&frame)
        .map_err(|_| CliModeError::new(3, "daemon returned an invalid ASR mode response"))?;
    if envelope.version != PROTOCOL_VERSION {
        return Err(CliModeError::new(
            3,
            format!(
                "IPC protocol mismatch: daemon uses {}, CLI uses {PROTOCOL_VERSION}",
                envelope.version
            ),
        ));
    }
    serde_json::from_str(&frame)
        .map_err(|_| CliModeError::new(3, "daemon returned an invalid ASR mode response"))
}

fn read_frame(stream: &mut BufReader<UnixStream>) -> Result<String, String> {
    let started = Instant::now();
    let mut response = Vec::new();
    loop {
        let remaining = IO_DEADLINE
            .checked_sub(started.elapsed())
            .filter(|remaining| !remaining.is_zero())
            .ok_or_else(|| {
                "ASR mode result is unknown; the daemon did not acknowledge the change".to_owned()
            })?;
        stream
            .get_ref()
            .set_read_timeout(Some(remaining))
            .map_err(|_| "failed to configure daemon connection deadline".to_owned())?;
        match stream.fill_buf() {
            Ok([]) => {
                return Err(
                    "ASR mode result is unknown; the daemon did not acknowledge the change"
                        .to_owned(),
                );
            }
            Ok(available) => {
                let frame_end = available.iter().position(|byte| *byte == b'\n');
                let consumed = frame_end.map_or(available.len(), |position| position + 1);
                response.extend_from_slice(&available[..consumed]);
                stream.consume(consumed);
                if response.len() > MAX_MODE_RESPONSE_BYTES {
                    return Err("daemon response frame is too large".to_owned());
                }
                if frame_end.is_some() {
                    return String::from_utf8(response)
                        .map_err(|_| "daemon returned an invalid ASR mode response".to_owned());
                }
            }
            Err(error) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                return Err(
                    "ASR mode result is unknown; the daemon did not acknowledge the change"
                        .to_owned(),
                );
            }
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(_) => {
                return Err(
                    "ASR mode result is unknown; the daemon did not acknowledge the change"
                        .to_owned(),
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_mode_without_marker_is_legacy_cloud() {
        let home = tempfile::tempdir().unwrap();
        let config = home.path().join("config.toml");
        let state = home.path().join("state");
        fs::create_dir_all(&state).unwrap();
        assert_eq!(load_mode(&config, &state).unwrap(), (AsrMode::Cloud, 0));
    }

    #[test]
    fn marker_without_mode_never_becomes_cloud() {
        let home = tempfile::tempdir().unwrap();
        let config = home.path().join("config.toml");
        let state = home.path().join("state");
        fs::create_dir_all(&state).unwrap();
        durable_replace(&mode_initialized_path(&state), b"initialized\n").unwrap();
        let error = load_mode(&config, &state).unwrap_err();
        assert!(
            !error.message().to_ascii_lowercase().contains("cloud"),
            "{}",
            error.message()
        );
        assert!(matches!(error, AdmissionError::Blocked(_)));
    }

    #[test]
    fn quoted_double_asr_mode_key_loads_local_without_marker() {
        let home = tempfile::tempdir().unwrap();
        let config = home.path().join("config.toml");
        let state = home.path().join("state");
        fs::create_dir_all(&state).unwrap();
        fs::write(&config, "\"asr_mode\" = \"local\"\n").unwrap();
        let (mode, _) = load_mode(&config, &state).unwrap();
        assert_eq!(mode, AsrMode::Local);
        assert_ne!(mode, AsrMode::Cloud);
    }

    #[test]
    fn quoted_single_asr_mode_key_loads_local_without_marker() {
        let home = tempfile::tempdir().unwrap();
        let config = home.path().join("config.toml");
        let state = home.path().join("state");
        fs::create_dir_all(&state).unwrap();
        fs::write(&config, "'asr_mode' = 'local'\n").unwrap();
        let (mode, _) = load_mode(&config, &state).unwrap();
        assert_eq!(mode, AsrMode::Local);
        assert_ne!(mode, AsrMode::Cloud);
    }

    #[test]
    fn unicode_escaped_asr_mode_key_is_blocked_not_cloud() {
        let home = tempfile::tempdir().unwrap();
        let config = home.path().join("config.toml");
        let state = home.path().join("state");
        fs::create_dir_all(&state).unwrap();
        fs::write(&config, "\"\\u0061sr_mode\" = \"local\"\n").unwrap();
        let error = load_mode(&config, &state).unwrap_err();
        assert!(matches!(error, AdmissionError::Blocked(_)));
        assert!(
            error.message().contains("unsupported TOML key syntax"),
            "{}",
            error.message()
        );
        assert!(
            !error.message().to_ascii_lowercase().contains("cloud"),
            "{}",
            error.message()
        );
    }

    #[test]
    fn unknown_mode_never_becomes_cloud() {
        let contents = "asr_mode = \"hybrid\"\n";
        let error = parse_asr_mode_document(contents).unwrap_err();
        assert!(error.message().contains("unknown asr_mode"));
    }

    #[test]
    fn duplicate_root_keys_block_admission() {
        let contents = "asr_mode = \"local\"\nasr_mode = \"cloud\"\n";
        let error = parse_asr_mode_document(contents).unwrap_err();
        assert!(error.message().contains("duplicate"));
    }

    #[test]
    fn quoted_and_bare_asr_mode_keys_are_duplicates() {
        let contents = "\"asr_mode\" = \"local\"\nasr_mode = \"cloud\"\n";
        let error = parse_asr_mode_document(contents).unwrap_err();
        assert!(error.message().contains("duplicate"));
    }

    #[test]
    fn malformed_toml_blocks_admission() {
        let error = parse_asr_mode_document("not toml\n").unwrap_err();
        assert!(error.message().contains("malformed"));
    }

    #[test]
    fn explicit_local_does_not_admit_capture() {
        let home = tempfile::tempdir().unwrap();
        let config = home.path().join("voisu").join("config.toml");
        let state = home.path().join("state");
        persist_asr_mode_at(&config, &state, AsrMode::Local).unwrap();
        match admit_recording_at(&config, &state).unwrap() {
            RecordingAdmission::LocalUnavailable { error, .. } => {
                assert_eq!(error, LOCAL_UNAVAILABLE);
            }
            RecordingAdmission::Cloud { .. } => panic!("Local must not admit as Cloud"),
        }
        assert!(!matches!(
            select_resources(AsrMode::Local),
            AsrPathResources::Cloud
        ));
    }

    #[test]
    fn first_explicit_commit_creates_marker_without_a_second_mode_value() {
        let home = tempfile::tempdir().unwrap();
        let config = home.path().join("config.toml");
        let state = home.path().join("state");
        persist_asr_mode_at(&config, &state, AsrMode::Local).unwrap();
        let marker = fs::read_to_string(mode_initialized_path(&state)).unwrap();
        assert!(!marker.contains("local"));
        assert!(!marker.contains("cloud"));
        let contents = fs::read_to_string(&config).unwrap();
        assert!(contents.contains("asr_mode = \"local\""));
        assert_eq!(contents.matches("asr_mode").count(), 1);
    }

    #[test]
    fn unreadable_revision_before_replace_is_intact() {
        let home = tempfile::tempdir().unwrap();
        let config = home.path().join("config.toml");
        let state = home.path().join("state");
        fs::create_dir_all(&state).unwrap();
        fs::write(&config, "custom = 1\n").unwrap();
        fs::write(revision_path(&state), "not-a-number\n").unwrap();
        let error = persist_asr_mode_at(&config, &state, AsrMode::Local).unwrap_err();
        assert!(
            !error.is_indeterminate(),
            "revision was invalid before replace: {}",
            error.message()
        );
        assert_eq!(fs::read_to_string(&config).unwrap(), "custom = 1\n");
    }

    #[test]
    fn revision_write_failure_after_config_replace_is_indeterminate() {
        let home = tempfile::tempdir().unwrap();
        let config = home.path().join("config.toml");
        let state = home.path().join("state");
        let error = persist_asr_mode_at_with(&config, &state, AsrMode::Local, |_state, _next| {
            Err(PersistError::Intact("cannot stage revision".to_owned()))
        })
        .unwrap_err();
        assert!(
            error.is_indeterminate(),
            "config already changed: {}",
            error.message()
        );
        let contents = fs::read_to_string(&config).unwrap();
        assert!(contents.contains("asr_mode = \"local\""), "{contents}");
    }

    #[test]
    fn persist_preserves_unrelated_settings() {
        let home = tempfile::tempdir().unwrap();
        let config = home.path().join("voisu").join("config.toml");
        let state = home.path().join("state");
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        fs::write(&config, "delivery_mode = \"clipboard\"\ncustom = 7\n").unwrap();
        persist_asr_mode_at(&config, &state, AsrMode::Cloud).unwrap();
        let contents = fs::read_to_string(&config).unwrap();
        assert!(
            contents.contains("delivery_mode = \"clipboard\""),
            "{contents}"
        );
        assert!(contents.contains("custom = 7"), "{contents}");
        assert!(contents.contains("asr_mode = \"cloud\""), "{contents}");
    }

    #[test]
    fn persist_replaces_quoted_asr_mode_key() {
        let home = tempfile::tempdir().unwrap();
        let config = home.path().join("config.toml");
        let state = home.path().join("state");
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        fs::write(&config, "\"asr_mode\" = \"local\"\ncustom = 1\n").unwrap();
        persist_asr_mode_at(&config, &state, AsrMode::Cloud).unwrap();
        let contents = fs::read_to_string(&config).unwrap();
        assert_eq!(contents.matches("asr_mode").count(), 1, "{contents}");
        assert!(contents.contains("asr_mode = \"cloud\""), "{contents}");
        assert!(!contents.contains("\"asr_mode\""), "{contents}");
        let (mode, _) = load_mode(&config, &state).unwrap();
        assert_eq!(mode, AsrMode::Cloud);
    }

    #[test]
    fn admission_does_not_wait_unbounded_for_config_lock() {
        let home = tempfile::tempdir().unwrap();
        let config = home.path().join("config.toml");
        let state = home.path().join("state");
        fs::create_dir_all(&state).unwrap();
        fs::write(&config, "asr_mode = \"cloud\"\n").unwrap();
        let _held = ConfigLock::acquire(&config).unwrap();
        let started = Instant::now();
        let error = load_mode(&config, &state).unwrap_err();
        assert!(
            started.elapsed() < IO_DEADLINE + Duration::from_millis(500),
            "bounded lock waited {:?}",
            started.elapsed()
        );
        assert!(matches!(error, AdmissionError::Access(_)));
        assert!(
            error.message().contains("deadline elapsed"),
            "{}",
            error.message()
        );
    }

    #[test]
    fn concurrent_setters_do_not_drop_updates() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("config.toml");
        let state = home.path().join("state");
        fs::create_dir_all(&state).unwrap();
        let workers = (0..8)
            .map(|index| {
                let path = path.clone();
                let state = state.clone();
                std::thread::spawn(move || {
                    let mode = if index % 2 == 0 {
                        AsrMode::Local
                    } else {
                        AsrMode::Cloud
                    };
                    persist_asr_mode_at(&path, &state, mode).unwrap();
                })
            })
            .collect::<Vec<_>>();
        for worker in workers {
            worker.join().unwrap();
        }
        let contents = fs::read_to_string(&path).unwrap();
        assert_eq!(contents.matches("asr_mode").count(), 1, "{contents}");
        let (mode, revision) = load_mode(&path, &state).unwrap();
        assert!(matches!(mode, AsrMode::Cloud | AsrMode::Local));
        assert!(revision >= 1);
    }

    #[test]
    fn concurrent_mode_and_deepgram_setters_keep_both_keys() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("config.toml");
        let state = home.path().join("state");
        fs::create_dir_all(&state).unwrap();
        let workers = (0..8)
            .map(|index| {
                let path = path.clone();
                let state = state.clone();
                std::thread::spawn(move || {
                    if index % 2 == 0 {
                        persist_asr_mode_at(&path, &state, AsrMode::Local).unwrap();
                    } else {
                        crate::config::set_deepgram_enabled_at(&path, false).unwrap();
                    }
                })
            })
            .collect::<Vec<_>>();
        for worker in workers {
            worker.join().unwrap();
        }
        let contents = fs::read_to_string(&path).unwrap();
        assert_eq!(contents.matches("asr_mode").count(), 1, "{contents}");
        assert_eq!(
            contents.matches("deepgram_enabled").count(),
            1,
            "{contents}"
        );
        assert!(contents.contains("asr_mode = \"local\""), "{contents}");
        assert!(contents.contains("deepgram_enabled = false"), "{contents}");
    }
}
