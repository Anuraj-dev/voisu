//! Lifetime single-instance lock for the daemon.
//!
//! The lock, not the socket path, is proof of absence. A missing socket can
//! race with a starting or old daemon that still holds this lock.

use std::fs::{self, File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use voisu_core::{PROTOCOL_VERSION, socket_path};

pub struct SingleInstance(File);

impl SingleInstance {
    pub fn acquire(path: &Path) -> Result<Self, String> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(path)
            .map_err(|error| format!("cannot open daemon lock: {error}"))?;
        // SAFETY: flock only reads the valid file descriptor and flags.
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if result != 0 {
            return Err("voisu-daemon is already running".to_owned());
        }
        Ok(Self(file))
    }
}

impl Drop for SingleInstance {
    fn drop(&mut self) {
        // SAFETY: this instance owns a valid open descriptor until Drop completes.
        let _ = unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

pub fn lock_path() -> Result<PathBuf, String> {
    let socket = socket_path()?;
    let parent = socket
        .parent()
        .ok_or_else(|| "daemon socket has no parent directory".to_owned())?;
    Ok(parent.join("daemon.lock"))
}

pub fn create_private_runtime_dirs(parent: &Path) -> Result<(), String> {
    let runtime = voisu_core::runtime_dir()?;
    let mut current = runtime;
    for component in ["voisu".to_owned(), format!("v{PROTOCOL_VERSION}")] {
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    return Err(format!(
                        "unsafe runtime path component: {}",
                        current.display()
                    ));
                }
                if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o777 != 0o700
                {
                    return Err(format!(
                        "runtime directory is not private: {}",
                        current.display()
                    ));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::DirBuilder::new()
                    .mode(0o700)
                    .create(&current)
                    .map_err(|error| format!("cannot create private runtime directory: {error}"))?;
            }
            Err(error) => return Err(format!("cannot inspect runtime directory: {error}")),
        }
    }
    if current != parent {
        return Err("unexpected daemon runtime directory".to_owned());
    }
    Ok(())
}

/// Acquire the daemon lifetime lock, creating the private runtime directory
/// first. Success means no other daemon holds the lock.
pub fn try_acquire_lifetime_lock() -> Result<SingleInstance, String> {
    let path = lock_path()?;
    let parent = path
        .parent()
        .ok_or_else(|| "daemon lock has no parent directory".to_owned())?;
    create_private_runtime_dirs(parent)?;
    SingleInstance::acquire(&path)
}
