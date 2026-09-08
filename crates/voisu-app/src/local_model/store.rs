//! `$XDG_DATA_HOME/voisu/models` with version/hash directories and leases.

use std::fs::{self, File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Component, Path, PathBuf};

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
            && overlaps_protected(path, &lease.artifact_dir)
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
            if overlaps_protected(path, &active_dir) {
                return Err(StoreError::Leased);
            }
        }
        let Some(artifact) = validated_artifact_dir(&self.root, path) else {
            return Err(StoreError::Path(
                "cleanup target is not an artifact directory".into(),
            ));
        };
        let _ = fs::remove_dir_all(artifact);
        Ok(())
    }

    pub fn commit_receipt(&self, receipt: &ActiveReceipt) -> Result<(), StoreError> {
        receipt::retain_current(&self.root).map_err(StoreError::Receipt)?;
        receipt::store_atomic(&self.root, receipt).map_err(StoreError::Receipt)
    }
}

fn overlaps_protected(target: &Path, protected: &Path) -> bool {
    // `target.starts_with(protected)` misses ancestors: deleting the revision,
    // catalog, or store root still recursively removes the leased/active tree.
    target.starts_with(protected) || protected.starts_with(target)
}

fn validated_artifact_dir<'a>(root: &Path, path: &'a Path) -> Option<&'a Path> {
    let rel = path.strip_prefix(root.join("artifacts")).ok()?;
    let mut depth = 0;
    for component in rel.components() {
        match component {
            Component::Normal(_) => depth += 1,
            _ => return None,
        }
    }
    (depth == 3).then_some(path)
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

    fn store_with_active_artifact() -> (tempfile::TempDir, ModelStore, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let store = ModelStore::open(temp.path().to_path_buf()).unwrap();
        let entry = crate::local_model::catalog::ci_fixture_entry();
        let artifact_id = "cafef00d";
        let dir = store.artifact_dir(entry, artifact_id);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("model.bin"), b"keep").unwrap();
        store
            .commit_receipt(&receipt::from_entry(entry, artifact_id))
            .unwrap();
        (temp, store, dir)
    }

    #[test]
    fn ancestor_cleanup_is_leased_while_receipt_is_active() {
        let (_temp, store, artifact) = store_with_active_artifact();
        let revision = artifact.parent().unwrap();
        let catalog = revision.parent().unwrap();
        let marker = artifact.join("model.bin");

        assert_eq!(store.remove_unleased(revision), Err(StoreError::Leased));
        assert!(marker.exists());
        assert_eq!(store.remove_unleased(catalog), Err(StoreError::Leased));
        assert!(marker.exists());
        assert_eq!(store.remove_unleased(store.root()), Err(StoreError::Leased));
        assert!(marker.exists());
        assert_eq!(store.remove_unleased(&artifact), Err(StoreError::Leased));
        assert!(marker.exists());
        assert_eq!(
            store.load_active().unwrap().unwrap().artifact_id,
            "cafef00d"
        );
    }

    #[test]
    fn unleased_artifact_dir_can_be_removed() {
        let (_temp, store, active) = store_with_active_artifact();
        let stale = store.artifact_dir(crate::local_model::catalog::ci_fixture_entry(), "deadbeef");
        fs::create_dir_all(&stale).unwrap();
        fs::write(stale.join("old.bin"), b"drop").unwrap();

        store.remove_unleased(&stale).unwrap();
        assert!(!stale.exists());
        assert!(active.join("model.bin").exists());
    }

    #[test]
    fn lease_protects_artifact_without_active_receipt() {
        let temp = tempfile::tempdir().unwrap();
        let mut store = ModelStore::open(temp.path().to_path_buf()).unwrap();
        let entry = crate::local_model::catalog::ci_fixture_entry();
        let dir = store.artifact_dir(entry, "leased0nly");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("model.bin"), b"keep").unwrap();
        assert!(store.load_active().unwrap().is_none());

        store.acquire_lease(&receipt::from_entry(entry, "leased0nly"), dir.clone());
        assert_eq!(store.remove_unleased(&dir), Err(StoreError::Leased));
        assert!(dir.join("model.bin").exists());

        store.release_lease();
        store.remove_unleased(&dir).unwrap();
        assert!(!dir.exists());
    }

    #[test]
    fn lease_protects_while_receipt_is_active() {
        let (_temp, mut store, artifact) = store_with_active_artifact();
        let receipt = store.load_active().unwrap().unwrap();
        store.acquire_lease(&receipt, artifact.clone());
        assert_eq!(store.remove_unleased(&artifact), Err(StoreError::Leased));
        assert!(artifact.join("model.bin").exists());
        store.release_lease();
        assert_eq!(store.remove_unleased(&artifact), Err(StoreError::Leased));
        assert!(artifact.join("model.bin").exists());
    }

    #[test]
    fn descendant_of_active_artifact_is_leased() {
        let (_temp, store, artifact) = store_with_active_artifact();
        let nested = artifact.join("model.bin");
        assert_eq!(store.remove_unleased(&nested), Err(StoreError::Leased));
        assert!(nested.exists());
    }

    #[test]
    fn non_artifact_cleanup_targets_are_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let store = ModelStore::open(temp.path().to_path_buf()).unwrap();
        assert!(store.load_active().unwrap().is_none());
        let not_artifact = StoreError::Path("cleanup target is not an artifact directory".into());

        assert_eq!(
            store.remove_unleased(store.root()),
            Err(not_artifact.clone())
        );

        let scratch = store.root().join("scratch.bin");
        fs::write(&scratch, b"x").unwrap();
        assert_eq!(store.remove_unleased(&scratch), Err(not_artifact.clone()));
        assert!(scratch.exists());

        let other = store.root().join("other");
        fs::create_dir(&other).unwrap();
        assert_eq!(store.remove_unleased(&other), Err(not_artifact.clone()));
        assert!(other.exists());

        let staging = store.staging_dir("token");
        fs::create_dir(&staging).unwrap();
        assert_eq!(store.remove_unleased(&staging), Err(not_artifact.clone()));
        assert!(staging.exists());

        let shallow = store.root().join("artifacts").join("id").join("rev");
        fs::create_dir_all(&shallow).unwrap();
        assert_eq!(store.remove_unleased(&shallow), Err(not_artifact.clone()));
        assert!(shallow.exists());

        let stale = store.artifact_dir(crate::local_model::catalog::ci_fixture_entry(), "deadbeef");
        fs::create_dir_all(&stale).unwrap();
        let nested = stale.join("old.bin");
        fs::write(&nested, b"drop").unwrap();
        assert_eq!(store.remove_unleased(&nested), Err(not_artifact));
        assert!(nested.exists());
        store.remove_unleased(&stale).unwrap();
        assert!(!stale.exists());
    }
}
