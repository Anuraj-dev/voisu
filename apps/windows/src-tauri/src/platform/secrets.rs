//! Provider keys in Windows Credential Manager (the login Keychain on
//! macOS), behind voisu-core's `SecretStore` seam. Keys never touch disk, logs or the webview.
//!
//! Errors carry fixed diagnostics only: a keyring error is never formatted,
//! so nothing it might echo back can reach a log.

use keyring::Entry;
use voisu_core::{BoundaryError, BoundaryKind, Credential, Provider, SecretStore};

/// Credential Manager target is `<account>.<SERVICE>`, e.g. `groq-api-key.Voisu`;
/// on macOS a generic password with service `Voisu` and that account.
const SERVICE: &str = "Voisu";

fn account(provider: Provider) -> &'static str {
    match provider {
        Provider::Groq => "groq-api-key",
        Provider::Deepgram => "deepgram-api-key",
    }
}

fn entry(provider: Provider) -> Result<Entry, BoundaryError> {
    Entry::new(SERVICE, account(provider)).map_err(|_| {
        BoundaryError::new(
            BoundaryKind::SecretStorage,
            "credential store entry unavailable",
        )
    })
}

#[derive(Default)]
pub struct CredentialManagerStore;

impl CredentialManagerStore {
    /// Whether a usable key is stored for `provider`.
    pub fn has_key(&mut self, provider: Provider) -> bool {
        self.load(provider).is_ok()
    }
}

impl SecretStore for CredentialManagerStore {
    fn replace(&mut self, provider: Provider, credential: Credential) -> Result<(), BoundaryError> {
        entry(provider)?
            .set_password(credential.expose_to_boundary())
            .map_err(|_| {
                BoundaryError::new(BoundaryKind::SecretStorage, "credential store write failed")
            })
    }

    fn load(&mut self, provider: Provider) -> Result<Credential, BoundaryError> {
        let secret = entry(provider)?.get_password().map_err(|error| {
            let diagnostic = match error {
                keyring::Error::NoEntry => "no stored credential",
                _ => "credential store read failed",
            };
            BoundaryError::new(BoundaryKind::SecretStorage, diagnostic)
        })?;
        Credential::new(secret)
    }
}
