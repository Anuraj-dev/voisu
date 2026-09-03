// Minimal Grammar capability types shared across credential prep and delivery.
//
// Split out of system.rs as a pure move; module-global items come from `super`.

use super::*;

impl GrammarUnavailableReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Cancelled => "cancelled",
            Self::WorkDeadlineExceeded => "work_deadline_exceeded",
            Self::NoCredential => "no_credential",
            Self::KeyringLocked => "keyring_locked",
            Self::KeyringUnavailable => "keyring_unavailable",
            Self::ToolMissing => "tool_missing",
            Self::InvalidCredential => "invalid_credential",
        }
    }
}

/// Explicit pre-validation grammar capability — never a lazy loader.
/// Only terminal `Ready` / `Unavailable` may reach Validation (Architecture A).
#[derive(Clone, Debug)]
pub enum GrammarCapability {
    Ready(ReadyGrammarCapability),
    Unavailable(GrammarUnavailableReason),
}

impl GrammarCapability {
    pub fn is_ready(&self) -> bool {
        matches!(self, Self::Ready(_))
    }

    pub fn is_unavailable(&self) -> bool {
        matches!(self, Self::Unavailable(_))
    }
}

/// Credential material resolved before Validation. The production async HTTP
/// client is attached by SW5/SW10; this owner only proves credential ownership
/// and terminal cleanup. `Credential` has no `Debug`.
#[derive(Clone)]
pub struct ReadyGrammarCapability {
    credential: Credential,
}

impl std::fmt::Debug for ReadyGrammarCapability {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ReadyGrammarCapability { credential: <redacted> }")
    }
}

impl ReadyGrammarCapability {
    pub fn new(credential: Credential) -> Self {
        Self { credential }
    }

    pub fn credential(&self) -> &Credential {
        &self.credential
    }

    pub fn into_credential(self) -> Credential {
        self.credential
    }
}

/// Observable phase of a credential cleanup entry (Architecture A state machine).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CredentialEntryPhase {
    Registered,
    Running,
    CancelRequested,
    Terminal,
    Deregistered,
}
