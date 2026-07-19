//! The 0600 file fallback for API credentials.
//!
//! This store is **never** the default path. It is reached only after the
//! desktop Secret Service retry budget is exhausted (see
//! [`crate::system::SecretToolStore`]), and every write is announced loudly on
//! stderr — gh's silent keyring fallback is the anti-pattern we refuse to
//! repeat. The file lives beside `config.toml` under the `voisu` config
//! directory, is created with `0600`, and its parent directory with `0700`, so
//! a credential is never world- or group-readable.

use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use voisu_core::{BoundaryError, BoundaryKind, Credential, Provider};

/// The credential fallback file: `$XDG_CONFIG_HOME/voisu/credentials`.
pub fn default_path() -> PathBuf {
    crate::config::config_dir().join("credentials")
}

/// A minimal, line-oriented `0600` credential file. Each provider occupies one
/// `deepgram=<key>` / `groq=<key>` line; writing one provider preserves the
/// other, mirroring the config writer's both-key contract.
pub struct FileSecretStore {
    path: PathBuf,
}

impl FileSecretStore {
    /// The default fallback file beside `config.toml`.
    pub fn at_default() -> Self {
        Self { path: default_path() }
    }

    /// A fallback file at an explicit path (tests point this at a tempdir).
    pub fn at(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Persists a credential, creating the `0700` parent directory and the
    /// `0600` file if needed and preserving any other provider's line.
    pub fn store(&self, provider: Provider, credential: &Credential) -> Result<(), BoundaryError> {
        let parent = self.path.parent().ok_or_else(|| {
            BoundaryError::new(BoundaryKind::SecretStorage, "credential fallback path has no parent")
        })?;
        std::fs::create_dir_all(parent).map_err(|_| {
            BoundaryError::new(BoundaryKind::SecretStorage, "cannot create credential directory")
        })?;
        // Tighten the directory to owner-only; ignore failures on filesystems
        // that do not honour Unix modes rather than refusing to store.
        let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));

        let existing = self.read_lines()?;
        let rendered = merge_line(&existing, provider, credential.expose_to_boundary());
        self.write_atomic(parent, &rendered)
    }

    /// Reads a credential, returning `None` when the file or the provider's line
    /// is absent (a definitively missing key, not an error).
    pub fn read(&self, provider: Provider) -> Result<Option<Credential>, BoundaryError> {
        let contents = match std::fs::read_to_string(&self.path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => {
                return Err(BoundaryError::new(
                    BoundaryKind::SecretStorage,
                    "cannot read the credential fallback file",
                ));
            }
        };
        match find_line(&contents, provider) {
            Some(value) => Credential::new(value.to_owned()).map(Some),
            None => Ok(None),
        }
    }

    /// Removes a provider's line, deleting the file entirely once it holds no
    /// more credentials. Returns whether a line was actually removed. Used to
    /// migrate a key out of the plaintext file when the keyring accepts it.
    pub fn remove(&self, provider: Provider) -> Result<bool, BoundaryError> {
        let existing = match std::fs::read_to_string(&self.path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(_) => {
                return Err(BoundaryError::new(
                    BoundaryKind::SecretStorage,
                    "cannot read the credential fallback file before pruning",
                ));
            }
        };
        let key = provider_key(provider);
        let mut kept: Vec<&str> = Vec::new();
        let mut removed = false;
        for line in existing.lines() {
            if line == HEADER.trim_end() {
                continue;
            }
            match line.split_once('=') {
                Some((name, _)) if name.trim() == key => removed = true,
                _ => kept.push(line),
            }
        }
        if !removed {
            return Ok(false);
        }
        // Delete the file outright when no credentials remain, so a migrated
        // fallback leaves nothing plaintext behind.
        if kept.iter().all(|line| line.trim().is_empty()) {
            match std::fs::remove_file(&self.path) {
                Ok(()) => return Ok(true),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(true),
                Err(_) => {
                    return Err(BoundaryError::new(
                        BoundaryKind::SecretStorage,
                        "cannot delete the emptied credential fallback file",
                    ));
                }
            }
        }
        let parent = self.path.parent().ok_or_else(|| {
            BoundaryError::new(BoundaryKind::SecretStorage, "credential fallback path has no parent")
        })?;
        let mut body = String::from(HEADER);
        for line in kept {
            body.push_str(line);
            body.push('\n');
        }
        self.write_atomic(parent, &body)?;
        Ok(true)
    }

    fn read_lines(&self) -> Result<String, BoundaryError> {
        match std::fs::read_to_string(&self.path) {
            Ok(contents) => Ok(contents),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
            Err(_) => Err(BoundaryError::new(
                BoundaryKind::SecretStorage,
                "cannot read the credential fallback file before writing",
            )),
        }
    }

    fn write_atomic(&self, parent: &Path, contents: &str) -> Result<(), BoundaryError> {
        // Create the temp file 0600 from the outset so the secret is never
        // briefly world-readable between write and chmod.
        let mut builder = tempfile::Builder::new();
        builder.prefix(".credentials.");
        let file = builder
            .make_in(parent, |path| {
                std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(path)
            })
            .map_err(|_| {
                BoundaryError::new(BoundaryKind::SecretStorage, "cannot stage credential write")
            })?;
        file.as_file()
            .write_all(contents.as_bytes())
            .and_then(|()| file.as_file().sync_all())
            .map_err(|_| {
                BoundaryError::new(BoundaryKind::SecretStorage, "cannot write the credential fallback file")
            })?;
        file.persist(&self.path).map_err(|_| {
            BoundaryError::new(BoundaryKind::SecretStorage, "cannot persist the credential fallback file")
        })?;
        // Reassert 0600 in case an inherited umask or prior file loosened it.
        let _ = std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o600));
        Ok(())
    }
}

/// The file key for a provider: the same lowercase token used on the Secret
/// Service attribute, so the two stores stay legible together.
fn provider_key(provider: Provider) -> &'static str {
    provider.secret_service_value()
}

/// Finds a provider's stored value in the file body, or `None`.
fn find_line(contents: &str, provider: Provider) -> Option<&str> {
    let key = provider_key(provider);
    contents.lines().find_map(|line| {
        let (name, value) = line.split_once('=')?;
        (name.trim() == key).then(|| value.trim())
    })
}

/// Produces the new file body with `provider`'s line set to `value`, every other
/// line preserved verbatim, and the managed header emitted once.
fn merge_line(existing: &str, provider: Provider, value: &str) -> String {
    let key = provider_key(provider);
    let mut out = String::from(HEADER);
    let mut wrote = false;
    for line in existing.lines() {
        if line == HEADER.trim_end() {
            continue;
        }
        match line.split_once('=') {
            Some((name, _)) if name.trim() == key => {
                out.push_str(key);
                out.push('=');
                out.push_str(value);
                out.push('\n');
                wrote = true;
            }
            _ => {
                out.push_str(line);
                out.push('\n');
            }
        }
    }
    if !wrote {
        out.push_str(key);
        out.push('=');
        out.push_str(value);
        out.push('\n');
    }
    out
}

const HEADER: &str = "# Voisu credential fallback (Secret Service was unavailable). Mode 0600.\n";

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store() -> (tempfile::TempDir, FileSecretStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = FileSecretStore::at(dir.path().join("voisu").join("credentials"));
        (dir, store)
    }

    #[test]
    fn stores_and_reads_a_credential_round_trip() {
        let (_dir, store) = temp_store();
        let cred = Credential::new("groq-secret".to_owned()).unwrap();
        store.store(Provider::Groq, &cred).unwrap();
        let read = store.read(Provider::Groq).unwrap().unwrap();
        assert_eq!(read.expose_to_boundary(), "groq-secret");
    }

    #[test]
    fn a_missing_file_reads_as_none_not_an_error() {
        let (_dir, store) = temp_store();
        assert!(store.read(Provider::Deepgram).unwrap().is_none());
    }

    #[test]
    fn the_file_is_created_0600_and_the_directory_0700() {
        let (_dir, store) = temp_store();
        let cred = Credential::new("secret".to_owned()).unwrap();
        store.store(Provider::Deepgram, &cred).unwrap();
        let file_mode = std::fs::metadata(store.path()).unwrap().permissions().mode() & 0o777;
        assert_eq!(file_mode, 0o600, "credential file must be owner-only");
        let dir_mode = std::fs::metadata(store.path().parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dir_mode, 0o700, "credential directory must be owner-only");
    }

    #[test]
    fn writing_one_provider_preserves_the_other() {
        let (_dir, store) = temp_store();
        store
            .store(Provider::Groq, &Credential::new("groq-key".to_owned()).unwrap())
            .unwrap();
        store
            .store(Provider::Deepgram, &Credential::new("deepgram-key".to_owned()).unwrap())
            .unwrap();
        assert_eq!(
            store.read(Provider::Groq).unwrap().unwrap().expose_to_boundary(),
            "groq-key"
        );
        assert_eq!(
            store.read(Provider::Deepgram).unwrap().unwrap().expose_to_boundary(),
            "deepgram-key"
        );
        // Exactly one line per provider, header emitted once.
        let body = std::fs::read_to_string(store.path()).unwrap();
        assert_eq!(body.matches("groq=").count(), 1, "{body}");
        assert_eq!(body.matches("deepgram=").count(), 1, "{body}");
        assert_eq!(body.matches("# Voisu credential fallback").count(), 1, "{body}");
    }

    #[test]
    fn removing_the_last_provider_deletes_the_file() {
        let (_dir, store) = temp_store();
        store
            .store(Provider::Groq, &Credential::new("groq-key".to_owned()).unwrap())
            .unwrap();
        assert!(store.remove(Provider::Groq).unwrap(), "a stored key is removed");
        assert!(!store.path().exists(), "an emptied fallback file is deleted");
        // Removing an absent provider is a no-op, not an error.
        assert!(!store.remove(Provider::Groq).unwrap());
    }

    #[test]
    fn removing_one_provider_preserves_the_other() {
        let (_dir, store) = temp_store();
        store
            .store(Provider::Groq, &Credential::new("groq-key".to_owned()).unwrap())
            .unwrap();
        store
            .store(Provider::Deepgram, &Credential::new("deepgram-key".to_owned()).unwrap())
            .unwrap();
        assert!(store.remove(Provider::Groq).unwrap());
        assert!(store.read(Provider::Groq).unwrap().is_none());
        assert_eq!(
            store.read(Provider::Deepgram).unwrap().unwrap().expose_to_boundary(),
            "deepgram-key"
        );
        assert!(store.path().exists(), "the file survives while a key remains");
    }

    #[test]
    fn replacing_a_provider_key_rewrites_it_once() {
        let (_dir, store) = temp_store();
        store
            .store(Provider::Groq, &Credential::new("old".to_owned()).unwrap())
            .unwrap();
        store
            .store(Provider::Groq, &Credential::new("new".to_owned()).unwrap())
            .unwrap();
        assert_eq!(
            store.read(Provider::Groq).unwrap().unwrap().expose_to_boundary(),
            "new"
        );
        let body = std::fs::read_to_string(store.path()).unwrap();
        assert_eq!(body.matches("groq=").count(), 1, "{body}");
    }
}
