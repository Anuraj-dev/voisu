//! `$XDG_DATA_HOME/voisu/models` with version/hash directories and leases.

use std::fs::{self, File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use super::catalog::CatalogEntry;
use super::receipt::{self, ActiveReceipt, ReceiptError};
use super::safe_fs::{self, SafeFsError};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StoreError {
    Path(String),
    Busy,
    Receipt(ReceiptError),
    Fs(SafeFsError),
    Leased,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelLease {
    pub receipt_hash: String,
    pub artifact_dir: PathBuf,
    pub catalog_id: String,
}

pub struct StoreLock {
    file: File,
}

impl Drop for StoreLock {
    fn drop(&mut self) {
        let _ = unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
    }
}

pub struct ModelStore {
    root: PathBuf,
    lease: Option<ModelLease>,
}

pub fn models_dir() -> Result<PathBuf, StoreError> {
    models_dir_from(std::env::var_os("XDG_DATA_HOME"), std::env::var_os("HOME"))
}

pub fn models_dir_from(
    xdg_data: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
) -> Result<PathBuf, StoreError> {
    let base = xdg_data
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            home.map(PathBuf::from)
                .filter(|path| path.is_absolute())
                .map(|home| home.join(".local/share"))
        })
        .ok_or_else(|| StoreError::Path("neither XDG_DATA_HOME nor HOME is absolute".into()))?;
    Ok(base.join("voisu").join("models"))
}

impl ModelStore {
    pub fn open(root: PathBuf) -> Result<Self, StoreError> {
        safe_fs::ensure_private_dir(&root).map_err(StoreError::Fs)?;
        safe_fs::ensure_private_dir(&root.join("private")).map_err(StoreError::Fs)?;
        safe_fs::ensure_private_dir(&root.join("artifacts")).map_err(StoreError::Fs)?;
        safe_fs::ensure_private_dir(&root.join("staging")).map_err(StoreError::Fs)?;
        Ok(Self { root, lease: None })
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn lock_exclusive(&self) -> Result<StoreLock, StoreError> {
        let path = self.root.join("store.lock");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&path)
            .map_err(|error| StoreError::Path(error.to_string()))?;
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if result != 0 {
            return Err(StoreError::Busy);
        }
        Ok(StoreLock { file })
    }

    pub fn load_active(&self) -> Result<Option<ActiveReceipt>, StoreError> {
        match receipt::load(&self.root) {
            Ok(receipt) => Ok(Some(receipt)),
            Err(ReceiptError::Missing) => Ok(None),
            Err(error) => Err(StoreError::Receipt(error)),
        }
    }

    pub fn artifact_dir(&self, entry: &CatalogEntry, content_hash: &str) -> PathBuf {
        self.root
            .join("artifacts")
            .join(entry.id)
            .join(entry.revision)
            .join(content_hash)
    }

    pub fn staging_dir(&self, token: &str) -> PathBuf {
        self.root.join("staging").join(token)
    }

    pub fn acquire_lease(&mut self, receipt: &ActiveReceipt, dir: PathBuf) -> ModelLease {
        let lease = ModelLease {
            receipt_hash: receipt.receipt_hash.clone(),
            artifact_dir: dir,
            catalog_id: receipt.catalog_id.clone(),
        };
        self.lease = Some(lease.clone());
        lease
    }

    pub fn release_lease(&mut self) {
        self.lease = None;
    }

    pub fn remove_unleased(&self, path: &Path) -> Result<(), StoreError> {
        if let Some(lease) = &self.lease
            && path.starts_with(&lease.artifact_dir)
        {
            return Err(StoreError::Leased);
        }
        if let Some(active) = self.load_active()? {
            let active_dir = self
                .root
                .join("artifacts")
                .join(&active.catalog_id)
                .join(&active.catalog_revision)
                .join(&active.artifact_id);
            if path.starts_with(&active_dir) {
                return Err(StoreError::Leased);
            }
        }
        if path.starts_with(&self.root) {
            let _ = fs::remove_dir_all(path);
        }
        Ok(())
    }

    pub fn commit_receipt(&self, receipt: &ActiveReceipt) -> Result<(), StoreError> {
        receipt::retain_current(&self.root).map_err(StoreError::Receipt)?;
        receipt::store_atomic(&self.root, receipt).map_err(StoreError::Receipt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xdg_data_home_wins_over_home_fallback() {
        let dir = models_dir_from(Some("/tmp/xdg-data".into()), Some("/home/raja".into())).unwrap();
        assert_eq!(dir, PathBuf::from("/tmp/xdg-data/voisu/models"));
        let fallback = models_dir_from(None, Some("/home/raja".into())).unwrap();
        assert_eq!(
            fallback,
            PathBuf::from("/home/raja/.local/share/voisu/models")
        );
    }

    #[test]
    fn concurrent_lock_is_busy() {
        let temp = tempfile::tempdir().unwrap();
        let first = ModelStore::open(temp.path().to_path_buf()).unwrap();
        let _held = first.lock_exclusive().unwrap();
        let second = ModelStore::open(temp.path().to_path_buf()).unwrap();
        assert!(matches!(second.lock_exclusive(), Err(StoreError::Busy)));
    }
}
