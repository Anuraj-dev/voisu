//! Opt-in Local recovery audio. Off by default; separate from debug capture.

use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::config::{self, ConfigLock};
use crate::local_model::{SafeFsError, ensure_private_dir, relative_file_name};

pub const RECOVERY_KEY: &str = "local_recovery_enabled";
pub const MAX_RECORDINGS: usize = 3;
pub const MAX_TOTAL_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);
pub const HOURLY: Duration = Duration::from_secs(60 * 60);

const CLOCK_NAME: &str = "last-wall-ms";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RecoveryClock {
    pub wall_unix_ms: u64,
    pub boot_ms: Option<u64>,
}

impl RecoveryClock {
    #[must_use]
    pub fn system() -> Self {
        Self {
            wall_unix_ms: unix_ms_now(),
            boot_ms: boot_ms_now(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeliveryState {
    None,
    DeliveryStarted,
    Delivered,
    Failed,
    Ambiguous,
}

impl DeliveryState {
    fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::DeliveryStarted => "delivery_started",
            Self::Delivered => "delivered",
            Self::Failed => "failed",
            Self::Ambiguous => "ambiguous",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "none" => Some(Self::None),
            "delivery_started" => Some(Self::DeliveryStarted),
            "delivered" => Some(Self::Delivered),
            "failed" => Some(Self::Failed),
            "ambiguous" => Some(Self::Ambiguous),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecoveryArtifact {
    pub recording_id: String,
    pub created_unix_ms: u64,
    pub observed_wall_unix_ms: u64,
    pub boot_ms: Option<u64>,
    pub model_receipt_hash: String,
    pub delivery_state: DeliveryState,
    pub origin: Origin,
    pub pcm_bytes: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Origin {
    Local,
}

impl Origin {
    fn as_str(self) -> &'static str {
        "local"
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "local" => Some(Self::Local),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RecoveryError {
    Disabled,
    Quota(String),
    Storage(String),
    Expired,
    CloudSelected,
    Unsafe,
}

impl RecoveryError {
    pub fn message(&self) -> String {
        match self {
            Self::Disabled => "local recovery is disabled".to_owned(),
            Self::Quota(message) | Self::Storage(message) => message.clone(),
            Self::Expired => "recovery artifact has expired".to_owned(),
            Self::CloudSelected => {
                "Local-origin recovery is refused while Cloud is selected".to_owned()
            }
            Self::Unsafe => "recovery path is unsafe".to_owned(),
        }
    }

    #[must_use]
    pub fn is_storage(&self) -> bool {
        matches!(self, Self::Storage(_))
    }
}

#[derive(Clone, Debug)]
pub struct RecoveryStore {
    root: PathBuf,
    clock: RecoveryClock,
}

impl RecoveryStore {
    pub fn open(root: PathBuf, clock: RecoveryClock) -> Result<Self, RecoveryError> {
        reject_symlink(&root)?;
        ensure_private_dir(&root).map_err(map_fs)?;
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).map_err(map_io)?;
        Ok(Self { root, clock })
    }

    pub fn open_default(clock: RecoveryClock) -> Result<Self, RecoveryError> {
        Self::open(recovery_dir()?, clock)
    }

    pub fn open_existing(root: PathBuf, clock: RecoveryClock) -> Result<Self, RecoveryError> {
        reject_symlink(&root)?;
        match fs::symlink_metadata(&root) {
            Ok(metadata) if metadata.is_dir() => Ok(Self { root, clock }),
            Ok(_) => Err(RecoveryError::Unsafe),
            Err(error) if error.kind() == ErrorKind::NotFound => Err(RecoveryError::Disabled),
            Err(error) => Err(map_io(error)),
        }
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn expire(&self) -> Result<(), RecoveryError> {
        self.expire_with(self.clock, false)
    }

    pub fn expire_leftovers(&self) -> Result<(), RecoveryError> {
        self.expire_with(self.clock, true)
    }

    pub fn expire_with(
        &self,
        clock: RecoveryClock,
        promote_started: bool,
    ) -> Result<(), RecoveryError> {
        let previous = self.read_last_wall()?;
        if let Some(previous) = previous
            && clock.wall_unix_ms < previous
        {
            self.expire_all()?;
            self.write_last_wall(clock.wall_unix_ms)?;
            return Ok(());
        }
        self.write_last_wall(clock.wall_unix_ms)?;
        for path in self.entries()? {
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if name == CLOCK_NAME {
                continue;
            }
            if name.starts_with('.') || name.ends_with(".tmp") {
                if self.should_expire_orphan(&path, clock) {
                    let _ = fs::remove_file(&path);
                }
                continue;
            }
            if let Some(stem) = name.strip_suffix(".pcm") {
                let meta = self.root.join(format!("{stem}.meta"));
                if !meta.exists() && self.should_expire_orphan(&path, clock) {
                    let _ = fs::remove_file(&path);
                }
                continue;
            }
            if path.extension().and_then(|ext| ext.to_str()) != Some("meta") {
                continue;
            }
            match self.load_meta(&path) {
                Ok(artifact) if artifact_expired(&artifact, clock) => {
                    self.delete_artifact(&artifact.recording_id)?;
                }
                Ok(artifact)
                    if promote_started
                        && artifact.delivery_state == DeliveryState::DeliveryStarted =>
                {
                    let mut updated = artifact;
                    updated.delivery_state = DeliveryState::Ambiguous;
                    let _ = write_private_file(&path, encode_meta(&updated).as_bytes());
                }
                Ok(_) => {}
                Err(_) => {
                    let _ = fs::remove_file(&path);
                }
            }
        }
        Ok(())
    }

    pub fn quota_before_capture(&self) -> Result<(), RecoveryError> {
        self.expire()?;
        let (count, bytes) = self.usage()?;
        if count >= MAX_RECORDINGS {
            return Err(RecoveryError::Quota(
                "recovery quota: at most 3 recordings".to_owned(),
            ));
        }
        if bytes >= MAX_TOTAL_BYTES {
            return Err(RecoveryError::Quota(
                "recovery quota: at most 64 MiB".to_owned(),
            ));
        }
        Ok(())
    }

    pub fn persist_before_inference(
        &self,
        recording_id: &str,
        pcm: &[u8],
        model_receipt_hash: &str,
    ) -> Result<RecoveryArtifact, RecoveryError> {
        self.expire()?;
        let name = safe_id(recording_id)?;
        let (count, bytes) = self.usage()?;
        if count >= MAX_RECORDINGS {
            return Err(RecoveryError::Quota(
                "recovery quota: at most 3 recordings".to_owned(),
            ));
        }
        let incoming = pcm.len() as u64 + 512;
        if bytes.saturating_add(incoming) > MAX_TOTAL_BYTES {
            return Err(RecoveryError::Quota(
                "recovery quota: at most 64 MiB".to_owned(),
            ));
        }
        let pcm_tmp = self.root.join(format!(".{name}.pcm.tmp"));
        let meta_tmp = self.root.join(format!(".{name}.meta.tmp"));
        write_private_file(&pcm_tmp, pcm)?;
        let artifact = RecoveryArtifact {
            recording_id: recording_id.to_owned(),
            created_unix_ms: self.clock.wall_unix_ms,
            observed_wall_unix_ms: self.clock.wall_unix_ms,
            boot_ms: self.clock.boot_ms,
            model_receipt_hash: model_receipt_hash.to_owned(),
            delivery_state: DeliveryState::None,
            origin: Origin::Local,
            pcm_bytes: pcm.len() as u64,
        };
        write_private_file(&meta_tmp, encode_meta(&artifact).as_bytes())?;
        let pcm_path = self.root.join(format!("{name}.pcm"));
        let meta_path = self.root.join(format!("{name}.meta"));
        fs::rename(&pcm_tmp, &pcm_path).map_err(map_io)?;
        fs::rename(&meta_tmp, &meta_path).map_err(map_io)?;
        sync_dir(&self.root)?;
        Ok(artifact)
    }

    pub fn mark_delivery_started(&self, recording_id: &str) -> Result<(), RecoveryError> {
        self.set_state(recording_id, DeliveryState::DeliveryStarted)
    }

    pub fn mark_delivered(&self, recording_id: &str) -> Result<(), RecoveryError> {
        self.set_state(recording_id, DeliveryState::Delivered)?;
        self.delete_artifact(recording_id)
    }

    pub fn mark_failed(&self, recording_id: &str) -> Result<(), RecoveryError> {
        self.set_state(recording_id, DeliveryState::Failed)
    }

    pub fn load(&self, recording_id: &str) -> Result<RecoveryArtifact, RecoveryError> {
        self.expire()?;
        let name = safe_id(recording_id)?;
        let meta = self.root.join(format!("{name}.meta"));
        let artifact = self.load_meta(&meta)?;
        if artifact_expired(&artifact, self.clock) {
            self.delete_artifact(recording_id)?;
            return Err(RecoveryError::Expired);
        }
        Ok(artifact)
    }

    pub fn pcm_path(&self, recording_id: &str) -> Result<PathBuf, RecoveryError> {
        let artifact = self.load(recording_id)?;
        let name = safe_id(&artifact.recording_id)?;
        Ok(self.root.join(format!("{name}.pcm")))
    }

    pub fn refuse_if_cloud_selected(&self, cloud_selected: bool) -> Result<(), RecoveryError> {
        if cloud_selected {
            Err(RecoveryError::CloudSelected)
        } else {
            Ok(())
        }
    }

    pub fn delete_artifact(&self, recording_id: &str) -> Result<(), RecoveryError> {
        let name = safe_id(recording_id)?;
        for suffix in ["pcm", "meta"] {
            let path = self.root.join(format!("{name}.{suffix}"));
            match fs::remove_file(&path) {
                Ok(()) | Err(_) => {}
            }
        }
        Ok(())
    }

    fn set_state(&self, recording_id: &str, state: DeliveryState) -> Result<(), RecoveryError> {
        let mut artifact = self.load(recording_id)?;
        if artifact.delivery_state == DeliveryState::DeliveryStarted
            && state == DeliveryState::Delivered
        {
            artifact.delivery_state = DeliveryState::Delivered;
        } else if artifact.delivery_state == DeliveryState::DeliveryStarted
            && state != DeliveryState::Delivered
            && state != DeliveryState::Failed
        {
            artifact.delivery_state = DeliveryState::Ambiguous;
        } else {
            artifact.delivery_state = state;
        }
        let name = safe_id(recording_id)?;
        let meta = self.root.join(format!("{name}.meta"));
        write_private_file(&meta, encode_meta(&artifact).as_bytes())
    }

    fn load_meta(&self, path: &Path) -> Result<RecoveryArtifact, RecoveryError> {
        reject_symlink(path)?;
        let text = fs::read_to_string(path).map_err(map_io)?;
        parse_meta(&text).ok_or_else(|| RecoveryError::Storage("invalid recovery metadata".into()))
    }

    fn entries(&self) -> Result<Vec<PathBuf>, RecoveryError> {
        let mut out = Vec::new();
        let dir = match fs::read_dir(&self.root) {
            Ok(dir) => dir,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(out),
            Err(error) => return Err(map_io(error)),
        };
        for entry in dir {
            let entry = entry.map_err(map_io)?;
            let path = entry.path();
            reject_symlink(&path)?;
            out.push(path);
        }
        Ok(out)
    }

    fn usage(&self) -> Result<(usize, u64), RecoveryError> {
        let mut recordings = 0usize;
        let mut bytes = 0u64;
        let mut seen = std::collections::BTreeSet::new();
        for path in self.entries()? {
            let metadata = fs::symlink_metadata(&path).map_err(map_io)?;
            bytes = bytes.saturating_add(metadata.len());
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if name.starts_with('.') || name == CLOCK_NAME {
                continue;
            }
            if let Some(stem) = name
                .strip_suffix(".meta")
                .or_else(|| name.strip_suffix(".pcm"))
                && seen.insert(stem.to_owned())
            {
                recordings += 1;
            }
        }
        Ok((recordings, bytes))
    }

    fn should_expire_orphan(&self, path: &Path, clock: RecoveryClock) -> bool {
        let Ok(metadata) = fs::symlink_metadata(path) else {
            return true;
        };
        let modified = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map(|duration| duration.as_millis() as u64);
        match modified {
            Some(created) if created > clock.wall_unix_ms => true,
            Some(created) => {
                clock.wall_unix_ms.saturating_sub(created) >= MAX_AGE.as_millis() as u64
            }
            None => true,
        }
    }

    fn expire_all(&self) -> Result<(), RecoveryError> {
        for path in self.entries()? {
            if path.file_name().and_then(|name| name.to_str()) == Some(CLOCK_NAME) {
                continue;
            }
            let _ = fs::remove_file(path);
        }
        Ok(())
    }

    fn read_last_wall(&self) -> Result<Option<u64>, RecoveryError> {
        match fs::read_to_string(self.root.join(CLOCK_NAME)) {
            Ok(text) => Ok(text.trim().parse().ok()),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
            Err(error) => Err(map_io(error)),
        }
    }

    fn write_last_wall(&self, wall: u64) -> Result<(), RecoveryError> {
        write_private_file(&self.root.join(CLOCK_NAME), format!("{wall}\n").as_bytes())
    }
}

pub fn enabled() -> bool {
    enabled_at(&config::config_path())
}

pub fn enabled_at(path: &Path) -> bool {
    match fs::read_to_string(path) {
        Ok(contents) => parse_enabled(&contents).unwrap_or(false),
        Err(_) => false,
    }
}

pub fn set_enabled_at(path: &Path, enabled: bool) -> Result<PathBuf, String> {
    let _lock = ConfigLock::acquire(path)?;
    let existing = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == ErrorKind::NotFound => String::new(),
        Err(error) => return Err(format!("cannot read config: {error}")),
    };
    let parent = path
        .parent()
        .ok_or_else(|| format!("config path has no parent: {}", path.display()))?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("cannot create config directory: {error}"))?;
    let body = upsert_bool_key(&existing, enabled);
    let mut file = tempfile::Builder::new()
        .prefix(".config.toml.")
        .tempfile_in(parent)
        .map_err(|error| format!("cannot stage config write: {error}"))?;
    file.write_all(body.as_bytes())
        .and_then(|()| file.as_file().sync_all())
        .map_err(|error| format!("cannot write config: {error}"))?;
    file.persist(path)
        .map_err(|error| format!("cannot persist config: {}", error.error))?;
    Ok(path.to_path_buf())
}

pub fn parse_enabled(contents: &str) -> Option<bool> {
    let mut in_root = true;
    for raw in contents.lines() {
        let line = raw.split('#').next().unwrap_or(raw).trim();
        if line.starts_with('[') {
            in_root = false;
            continue;
        }
        if !in_root || line.is_empty() {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim() != RECOVERY_KEY {
            continue;
        }
        return match value.trim() {
            "true" => Some(true),
            "false" => Some(false),
            _ => Some(false),
        };
    }
    None
}

pub fn audio_retention_label(debug_capture: bool, recovery: bool) -> String {
    match (debug_capture, recovery) {
        (false, false) => "none".to_owned(),
        (true, false) => "debug-capture".to_owned(),
        (false, true) => "local-recovery".to_owned(),
        (true, true) => "debug-capture,local-recovery".to_owned(),
    }
}

pub fn spawn_hourly_expiry() {
    std::thread::Builder::new()
        .name("voisu-local-recovery".to_owned())
        .spawn(|| {
            loop {
                expire_if_present();
                std::thread::sleep(HOURLY);
            }
        })
        .ok();
}

pub fn expire_at_startup() {
    expire_if_present();
}

/// Expiry is independent of `local_recovery_enabled`. Missing dirs stay missing.
pub fn expire_if_present() {
    let Ok(dir) = recovery_dir() else {
        return;
    };
    if let Ok(store) = RecoveryStore::open_existing(dir, RecoveryClock::system()) {
        let _ = store.expire_leftovers();
    }
}

pub fn is_local_origin(recording_id: &str) -> bool {
    let Ok(dir) = recovery_dir() else {
        return false;
    };
    let Ok(store) = RecoveryStore::open_existing(dir, RecoveryClock::system()) else {
        return false;
    };
    matches!(store.load(recording_id), Ok(artifact) if artifact.origin == Origin::Local)
}

pub fn read_pcm(recording_id: &str) -> Result<Vec<u8>, RecoveryError> {
    let dir = recovery_dir()?;
    let store = RecoveryStore::open_existing(dir, RecoveryClock::system())?;
    let path = store.pcm_path(recording_id)?;
    reject_symlink(&path)?;
    fs::read(&path).map_err(map_io)
}

fn upsert_bool_key(existing: &str, enabled: bool) -> String {
    let mut in_root = true;
    let mut replaced = false;
    let mut lines = Vec::new();
    for line in existing.lines() {
        let trimmed = line.split('#').next().unwrap_or(line).trim();
        if trimmed.starts_with('[') {
            in_root = false;
        }
        let is_key = in_root
            && trimmed
                .split_once('=')
                .is_some_and(|(key, _)| key.trim() == RECOVERY_KEY);
        if is_key && !replaced {
            lines.push(format!("{RECOVERY_KEY} = {enabled}"));
            replaced = true;
        } else if !is_key {
            lines.push(line.to_owned());
        }
    }
    if !replaced {
        if !lines.is_empty() && !lines.last().is_some_and(|line| line.is_empty()) {
            lines.push(String::new());
        }
        lines.push(format!("{RECOVERY_KEY} = {enabled}"));
    }
    let mut out = lines.join("\n");
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

fn artifact_expired(artifact: &RecoveryArtifact, clock: RecoveryClock) -> bool {
    if artifact.created_unix_ms > clock.wall_unix_ms {
        return true;
    }
    if artifact.observed_wall_unix_ms > clock.wall_unix_ms {
        return true;
    }
    if artifact.created_unix_ms > artifact.observed_wall_unix_ms {
        return true;
    }
    let wall_age = clock.wall_unix_ms.saturating_sub(artifact.created_unix_ms);
    if wall_age >= MAX_AGE.as_millis() as u64 {
        return true;
    }
    if let (Some(created_boot), Some(now_boot)) = (artifact.boot_ms, clock.boot_ms) {
        if now_boot < created_boot {
            return true;
        }
        if now_boot.saturating_sub(created_boot) >= MAX_AGE.as_millis() as u64 {
            return true;
        }
    }
    false
}

fn encode_meta(artifact: &RecoveryArtifact) -> String {
    format!(
        "recording_id={}\ncreated_unix_ms={}\nobserved_wall_unix_ms={}\nboot_ms={}\nmodel={}\ndelivery_state={}\norigin={}\npcm_bytes={}\nformat=pcm_s16le_mono_16khz\n",
        artifact.recording_id,
        artifact.created_unix_ms,
        artifact.observed_wall_unix_ms,
        artifact
            .boot_ms
            .map(|value| value.to_string())
            .unwrap_or_else(|| "none".to_owned()),
        artifact.model_receipt_hash,
        artifact.delivery_state.as_str(),
        artifact.origin.as_str(),
        artifact.pcm_bytes,
    )
}

fn parse_meta(text: &str) -> Option<RecoveryArtifact> {
    let mut recording_id = None;
    let mut created = None;
    let mut observed = None;
    let mut boot = None;
    let mut model = None;
    let mut state = None;
    let mut origin = None;
    let mut pcm_bytes = None;
    for raw in text.lines() {
        let Some((key, value)) = raw.split_once('=') else {
            continue;
        };
        match key {
            "recording_id" => recording_id = Some(value.to_owned()),
            "created_unix_ms" => created = value.parse().ok(),
            "observed_wall_unix_ms" => observed = value.parse().ok(),
            "boot_ms" => {
                boot = if value == "none" {
                    Some(None)
                } else {
                    Some(Some(value.parse().ok()?))
                };
            }
            "model" => model = Some(value.to_owned()),
            "delivery_state" => state = DeliveryState::parse(value),
            "origin" => origin = Origin::parse(value),
            "pcm_bytes" => pcm_bytes = value.parse().ok(),
            _ => {}
        }
    }
    Some(RecoveryArtifact {
        recording_id: recording_id?,
        created_unix_ms: created?,
        observed_wall_unix_ms: observed?,
        boot_ms: boot.flatten(),
        model_receipt_hash: model?,
        delivery_state: state?,
        origin: origin?,
        pcm_bytes: pcm_bytes?,
    })
}

fn write_private_file(path: &Path, contents: &[u8]) -> Result<(), RecoveryError> {
    reject_symlink(path)?;
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(map_io)?;
    file.write_all(contents)
        .and_then(|()| file.sync_all())
        .map_err(map_io)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(map_io)?;
    Ok(())
}

fn sync_dir(path: &Path) -> Result<(), RecoveryError> {
    File::open(path)
        .and_then(|file| file.sync_all())
        .map_err(map_io)
}

fn reject_symlink(path: &Path) -> Result<(), RecoveryError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(RecoveryError::Unsafe),
        Ok(_) | Err(_) => Ok(()),
    }
}

fn safe_id(recording_id: &str) -> Result<&str, RecoveryError> {
    relative_file_name(recording_id).map_err(|_| RecoveryError::Unsafe)
}

fn recovery_dir() -> Result<PathBuf, RecoveryError> {
    let state = voisu_core::state_dir().map_err(RecoveryError::Storage)?;
    Ok(state.join("local-recovery"))
}

fn unix_ms_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn boot_ms_now() -> Option<u64> {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    let result = unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut ts) };
    if result != 0 {
        return None;
    }
    Some(
        (ts.tv_sec as u64)
            .saturating_mul(1000)
            .saturating_add((ts.tv_nsec as u64) / 1_000_000),
    )
}

fn map_fs(error: SafeFsError) -> RecoveryError {
    RecoveryError::Storage(format!("{error:?}"))
}

fn map_io(error: std::io::Error) -> RecoveryError {
    RecoveryError::Storage(format!("recovery-storage: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn clock(ms: u64) -> RecoveryClock {
        RecoveryClock {
            wall_unix_ms: ms,
            boot_ms: Some(ms),
        }
    }

    fn store(home: &tempfile::TempDir, clock: RecoveryClock) -> RecoveryStore {
        RecoveryStore::open(home.path().join("recovery"), clock).unwrap()
    }

    #[test]
    fn recovery_defaults_off_and_is_separate_from_debug_key() {
        assert_ne!(RECOVERY_KEY, "debug_capture");
        assert!(!enabled_at(Path::new("/no/such/config.toml")));
        assert_eq!(parse_enabled("asr_mode = \"local\"\n"), None);
        assert_eq!(parse_enabled("local_recovery_enabled = true\n"), Some(true));
        assert_eq!(
            parse_enabled("local_recovery_enabled = false\n"),
            Some(false)
        );
    }

    #[test]
    fn setter_preserves_unrelated_keys() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("config.toml");
        fs::write(&path, "asr_mode = \"local\"\ndeepgram_enabled = false\n").unwrap();
        set_enabled_at(&path, true).unwrap();
        let contents = fs::read_to_string(&path).unwrap();
        assert!(contents.contains("asr_mode = \"local\""), "{contents}");
        assert!(contents.contains("deepgram_enabled = false"), "{contents}");
        assert!(
            contents.contains("local_recovery_enabled = true"),
            "{contents}"
        );
    }

    #[test]
    fn quota_and_symlink_and_clock_expiry() {
        let home = tempfile::tempdir().unwrap();
        let store = store(&home, clock(1_000));
        store
            .persist_before_inference("rec-a", &[1, 0], "hash")
            .unwrap();
        store
            .persist_before_inference("rec-b", &[1, 0], "hash")
            .unwrap();
        store
            .persist_before_inference("rec-c", &[1, 0], "hash")
            .unwrap();
        let err = store.quota_before_capture().unwrap_err();
        assert!(matches!(err, RecoveryError::Quota(_)), "{err:?}");

        let later = RecoveryStore::open(
            store.root().to_path_buf(),
            clock(1_000 + MAX_AGE.as_millis() as u64 + 1),
        )
        .unwrap();
        later.expire_leftovers().unwrap();
        later.quota_before_capture().unwrap();

        let future = RecoveryStore::open(store.root().to_path_buf(), clock(50)).unwrap();
        future
            .persist_before_inference("rec-d", &[1, 0], "hash")
            .unwrap();
        // Backwards wall clock expires retained artifacts.
        let back = RecoveryStore::open(store.root().to_path_buf(), clock(10)).unwrap();
        back.expire().unwrap();
        assert!(back.load("rec-d").is_err());

        let linked = home.path().join("link");
        symlink(store.root(), &linked).unwrap();
        assert!(matches!(
            RecoveryStore::open(linked, clock(1)),
            Err(RecoveryError::Unsafe)
        ));
    }

    #[test]
    fn temps_count_toward_quota_and_delivery_deletes() {
        let home = tempfile::tempdir().unwrap();
        let store = store(&home, clock(5_000));
        fs::write(store.root().join(".crash.pcm.tmp"), vec![0u8; 1024]).unwrap();
        let (count, bytes) = store.usage().unwrap();
        assert_eq!(count, 0);
        assert!(
            bytes >= 1024,
            "crash-left temps count toward quota: {bytes}"
        );
        store.quota_before_capture().unwrap();

        store
            .persist_before_inference("rec-live", &[3, 0, 4, 0], "hash")
            .unwrap();
        store.mark_delivery_started("rec-live").unwrap();
        let loaded = store.load("rec-live").unwrap();
        assert_eq!(loaded.delivery_state, DeliveryState::DeliveryStarted);
        store.mark_delivered("rec-live").unwrap();
        assert!(store.load("rec-live").is_err());
    }

    #[test]
    fn crash_between_delivery_events_is_ambiguous_and_never_auto() {
        let home = tempfile::tempdir().unwrap();
        let store = store(&home, clock(9_000));
        store
            .persist_before_inference("rec-x", &[1, 0], "hash")
            .unwrap();
        store.mark_delivery_started("rec-x").unwrap();
        store.expire_leftovers().unwrap();
        let loaded = store.load("rec-x").unwrap();
        assert_eq!(loaded.delivery_state, DeliveryState::Ambiguous);
        store.refuse_if_cloud_selected(true).unwrap_err();
    }

    #[test]
    fn unmatched_pcm_expires_as_crash_left() {
        let home = tempfile::tempdir().unwrap();
        let store = store(&home, clock(5_000));
        fs::write(store.root().join("orphan.pcm"), vec![0u8; 64]).unwrap();
        let (count, bytes) = store.usage().unwrap();
        assert_eq!(count, 1);
        assert!(bytes >= 64);
        store.expire_with(clock(5_000), true).unwrap();
        assert!(
            !store.root().join("orphan.pcm").exists(),
            "future-dated unmatched pcm must expire"
        );
    }

    #[test]
    fn debug_retention_label_is_independent() {
        assert_eq!(audio_retention_label(false, false), "none");
        assert_eq!(audio_retention_label(true, false), "debug-capture");
        assert_eq!(audio_retention_label(false, true), "local-recovery");
        assert_eq!(
            audio_retention_label(true, true),
            "debug-capture,local-recovery"
        );
    }
}
