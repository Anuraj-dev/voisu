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
        lock_exclusive(path)
            .map_err(|error| format!("cannot open daemon lock: {error}"))?
            .map(Self)
            .ok_or_else(|| "voisu-daemon is already running".to_owned())
    }
}

/// Opens (creating, mode 0600) and takes an exclusive `flock` on `path`
/// without waiting. `None` means another process holds it. The lock lasts as
/// long as the returned file stays open; the file itself is never removed, so
/// two processes can never lock different files at the same path.
pub(crate) fn lock_exclusive(path: &Path) -> std::io::Result<Option<File>> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)?;
    // SAFETY: flock only reads the valid file descriptor and flags.
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result == 0 {
        return Ok(Some(file));
    }
    let error = std::io::Error::last_os_error();
    if error.kind() == std::io::ErrorKind::WouldBlock {
        Ok(None)
    } else {
        Err(error)
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

/// Creates `$XDG_RUNTIME_DIR/voisu/v{PROTOCOL_VERSION}` (which must be
/// `parent`), each level owned by us with mode 0700. The daemon, the Overlay
/// and setup may all race here at first login: a level another process just
/// created counts as success, once it passes the same checks.
pub fn create_private_runtime_dirs(parent: &Path) -> Result<(), String> {
    create_private_dirs_under(voisu_core::runtime_dir()?, parent)
}

fn create_private_dirs_under(runtime: PathBuf, parent: &Path) -> Result<(), String> {
    let mut current = runtime;
    for component in ["voisu".to_owned(), format!("v{PROTOCOL_VERSION}")] {
        current.push(component);
        match fs::DirBuilder::new().mode(0o700).create(&current) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(format!("cannot create private runtime directory: {error}"));
            }
        }
        let metadata = fs::symlink_metadata(&current)
            .map_err(|error| format!("cannot inspect runtime directory: {error}"))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(format!(
                "unsafe runtime path component: {}",
                current.display()
            ));
        }
        if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o777 != 0o700 {
            return Err(format!(
                "runtime directory is not private: {}",
                current.display()
            ));
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::{Arc, Barrier};

    fn private_dir(runtime: &Path) -> PathBuf {
        runtime.join("voisu").join(format!("v{PROTOCOL_VERSION}"))
    }

    #[test]
    fn racing_creators_all_succeed_on_the_same_private_dirs() {
        let runtime = tempfile::tempdir().unwrap();
        let parent = private_dir(runtime.path());
        let start = Arc::new(Barrier::new(8));
        let racers: Vec<_> = (0..8)
            .map(|_| {
                let (runtime, parent) = (runtime.path().to_owned(), parent.clone());
                let start = Arc::clone(&start);
                std::thread::spawn(move || {
                    start.wait();
                    create_private_dirs_under(runtime, &parent)
                })
            })
            .collect();
        for racer in racers {
            assert_eq!(racer.join().unwrap(), Ok(()));
        }
        let mode = fs::metadata(&parent).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
    }

    #[test]
    fn an_existing_dir_others_can_read_is_still_refused() {
        let runtime = tempfile::tempdir().unwrap();
        let voisu = runtime.path().join("voisu");
        fs::create_dir(&voisu).unwrap();
        fs::set_permissions(&voisu, fs::Permissions::from_mode(0o755)).unwrap();
        let error =
            create_private_dirs_under(runtime.path().to_owned(), &private_dir(runtime.path()))
                .unwrap_err();
        assert!(error.contains("not private"), "{error}");
    }

    #[test]
    fn a_held_lock_is_reported_as_held_not_as_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.lock");
        let held = lock_exclusive(&path).unwrap();
        assert!(held.is_some());
        assert!(lock_exclusive(&path).unwrap().is_none());
        drop(held);
        assert!(lock_exclusive(&path).unwrap().is_some());
    }
}
