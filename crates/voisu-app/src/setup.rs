//! The `voisu setup` wizard.
//!
//! The flow — prompt for each provider's key one at a time, validate it live
//! before saving, and let a re-run keep or replace what is already stored — is a
//! pure function over three injected seams ([`WizardIo`], [`voisu_core::SecretStore`],
//! and [`KeyValidator`]), so the whole thing is unit-tested without a real
//! terminal, network, or keyring. The thin production glue ([`StdioWizard`],
//! [`LiveKeyValidator`]) lives at the bottom of this file.

use voisu_core::{Credential, Provider, ProviderKeyStatus, SecretStore, provider_free_tier_hint};

/// Injected terminal IO. Prompts and messages both flow through here so a test
/// can script every keystroke and capture every line.
pub trait WizardIo {
    /// Emits one line of guidance/output.
    fn writeln(&mut self, line: &str);
    /// Reads a visible line (yes/no answers). `None` signals end of input.
    fn prompt_line(&mut self, prompt: &str) -> Option<String>;
    /// Reads a secret (an API key), hiding the echo when the input is a TTY.
    /// `None` signals end of input.
    fn prompt_secret(&mut self, prompt: &str) -> Option<String>;
}

/// Injected live key validation. The real implementation performs the cheapest
/// authenticated round trip per provider; tests script the classification.
pub trait KeyValidator {
    fn validate(&mut self, provider: Provider, credential: &Credential) -> ProviderKeyStatus;
}

/// What happened to one provider's key during the wizard.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderOutcome {
    /// A freshly entered key validated and was stored.
    Stored,
    /// An already-stored key was kept unchanged.
    Kept,
    /// The provider was skipped (blank entry or end of input), nothing stored.
    Skipped,
    /// The key could not be validated (a transient provider condition) but the
    /// user chose to store it anyway.
    StoredUnverified,
}

/// The disposition of both provider keys after a wizard run.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SetupOutcome {
    pub deepgram: ProviderOutcome,
    pub groq: ProviderOutcome,
}

/// Runs the interactive wizard to completion and reports what became of each
/// key. Deepgram is configured first (it is on by default), then Groq.
pub fn run_setup(
    io: &mut dyn WizardIo,
    store: &mut dyn SecretStore,
    validator: &mut dyn KeyValidator,
) -> SetupOutcome {
    io.writeln("Voisu setup — configure your cloud dictation keys.");
    io.writeln("Each key is checked with a live request before it is saved.");
    io.writeln("");
    let deepgram = configure_provider(io, store, validator, Provider::Deepgram);
    io.writeln("");
    let groq = configure_provider(io, store, validator, Provider::Groq);
    io.writeln("");
    io.writeln("Setup complete. Run `voisu doctor` to re-check your keys and desktop.");
    if deepgram == ProviderOutcome::Skipped {
        io.writeln(
            "No Deepgram key configured — run `voisu deepgram off` for the faster Groq-only path.",
        );
    }
    SetupOutcome { deepgram, groq }
}

fn configure_provider(
    io: &mut dyn WizardIo,
    store: &mut dyn SecretStore,
    validator: &mut dyn KeyValidator,
    provider: Provider,
) -> ProviderOutcome {
    let name = provider.cli_label();
    io.writeln(&format!("== {name} =="));
    io.writeln(provider_free_tier_hint(provider));

    if store.load(provider).is_ok()
        && ask_yes_no(io, &format!("A {name} key is already configured. Keep it?"), true)
    {
        io.writeln(&format!("Keeping the existing {name} key."));
        return ProviderOutcome::Kept;
    }

    loop {
        let entered = io.prompt_secret(&format!("Enter your {name} API key (leave blank to skip): "));
        let key = match entered {
            Some(key) => key.trim().to_owned(),
            None => {
                io.writeln(&format!("Skipping {name}."));
                return ProviderOutcome::Skipped;
            }
        };
        if key.is_empty() {
            io.writeln(&format!("Skipping {name}."));
            return ProviderOutcome::Skipped;
        }
        let credential = match Credential::new(key) {
            Ok(credential) => credential,
            Err(_) => {
                io.writeln("That key contains a line break; paste the key on one line and try again.");
                continue;
            }
        };

        io.writeln(&format!("Validating the {name} key..."));
        match validator.validate(provider, &credential) {
            ProviderKeyStatus::Valid => {
                return match store.replace(provider, credential) {
                    Ok(()) => {
                        io.writeln(&format!("{name} key validated and stored."));
                        ProviderOutcome::Stored
                    }
                    Err(error) => {
                        io.writeln(&format!(
                            "Could not store the {name} key: {}",
                            error.public_message()
                        ));
                        ProviderOutcome::Skipped
                    }
                };
            }
            ProviderKeyStatus::InvalidKey => {
                io.writeln(&format!("{name} rejected that key ({}).", ProviderKeyStatus::InvalidKey.headline()));
                io.writeln(provider_free_tier_hint(provider));
                if ask_yes_no(io, "Try a different key?", true) {
                    continue;
                }
                return ProviderOutcome::Skipped;
            }
            transient => {
                io.writeln(&format!(
                    "Could not validate the {name} key right now ({}).",
                    transient.headline()
                ));
                if ask_yes_no(io, "Save it anyway?", false) {
                    return match store.replace(provider, credential) {
                        Ok(()) => {
                            io.writeln(&format!("Saved the {name} key without validation."));
                            ProviderOutcome::StoredUnverified
                        }
                        Err(error) => {
                            io.writeln(&format!(
                                "Could not store the {name} key: {}",
                                error.public_message()
                            ));
                            ProviderOutcome::Skipped
                        }
                    };
                }
                if ask_yes_no(io, "Try a different key?", true) {
                    continue;
                }
                return ProviderOutcome::Skipped;
            }
        }
    }
}

/// Asks a yes/no question, defaulting on a blank answer or end of input.
fn ask_yes_no(io: &mut dyn WizardIo, question: &str, default_yes: bool) -> bool {
    let suffix = if default_yes { " [Y/n]" } else { " [y/N]" };
    loop {
        match io.prompt_line(&format!("{question}{suffix} ")) {
            None => return default_yes,
            Some(answer) => match answer.trim().to_ascii_lowercase().as_str() {
                "" => return default_yes,
                "y" | "yes" => return true,
                "n" | "no" => return false,
                _ => io.writeln("Please answer y or n."),
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Production glue: real stdio (with hidden key entry) and a live validator.
// ---------------------------------------------------------------------------

/// Reads from stdin / writes to stdout, hiding the echo while a key is typed at
/// a TTY so the secret never appears on screen.
pub struct StdioWizard;

impl WizardIo for StdioWizard {
    fn writeln(&mut self, line: &str) {
        println!("{line}");
    }

    fn prompt_line(&mut self, prompt: &str) -> Option<String> {
        use std::io::Write;
        print!("{prompt}");
        let _ = std::io::stdout().flush();
        read_line()
    }

    fn prompt_secret(&mut self, prompt: &str) -> Option<String> {
        use std::io::{IsTerminal, Write};
        print!("{prompt}");
        let _ = std::io::stdout().flush();
        if std::io::stdin().is_terminal() {
            let line = read_line_without_echo();
            // The suppressed Enter left the cursor on the prompt line; advance it.
            println!();
            line
        } else {
            read_line()
        }
    }
}

/// Reads one line from stdin, trimming the trailing newline. `None` at EOF.
fn read_line() -> Option<String> {
    use std::io::BufRead;
    let mut line = String::new();
    match std::io::stdin().lock().read_line(&mut line) {
        Ok(0) => None,
        Ok(_) => Some(line.trim_end_matches(['\n', '\r']).to_owned()),
        Err(_) => None,
    }
}

/// Reads one line from stdin with terminal echo disabled, restoring the terminal
/// afterwards. Falls back to a plain read if the terminal cannot be reconfigured.
fn read_line_without_echo() -> Option<String> {
    // SAFETY: `tcgetattr`/`tcsetattr` on the stdin fd only read and write a
    // local `termios`; on any failure we restore nothing and fall back to a
    // plain read.
    unsafe {
        let fd = libc::STDIN_FILENO;
        let mut original: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(fd, &mut original) != 0 {
            return read_line();
        }
        let mut hidden = original;
        hidden.c_lflag &= !libc::ECHO;
        if libc::tcsetattr(fd, libc::TCSANOW, &hidden) != 0 {
            return read_line();
        }
        let line = read_line();
        libc::tcsetattr(fd, libc::TCSANOW, &original);
        line
    }
}

/// Validates keys by performing the real per-provider round trip on a private
/// current-thread runtime. A runtime that fails to build reports `Unreachable`,
/// so a validation failure never masquerades as a wrong key.
pub struct LiveKeyValidator;

impl KeyValidator for LiveKeyValidator {
    fn validate(&mut self, provider: Provider, credential: &Credential) -> ProviderKeyStatus {
        let client = crate::system::ProviderHttpClient;
        let credential = credential.clone();
        match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime.block_on(client.check(provider, credential)),
            Err(_) => ProviderKeyStatus::Unreachable,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use voisu_core::{BoundaryError, BoundaryKind};

    /// A scripted terminal: pops queued key/line answers and records output.
    struct FakeIo {
        secrets: Vec<Option<String>>,
        lines: Vec<Option<String>>,
        output: Vec<String>,
    }

    impl FakeIo {
        fn new(secrets: Vec<Option<&str>>, lines: Vec<Option<&str>>) -> Self {
            Self {
                secrets: secrets.into_iter().map(|s| s.map(str::to_owned)).collect(),
                lines: lines.into_iter().map(|s| s.map(str::to_owned)).collect(),
                output: Vec::new(),
            }
        }

        fn transcript(&self) -> String {
            self.output.join("\n")
        }
    }

    impl WizardIo for FakeIo {
        fn writeln(&mut self, line: &str) {
            self.output.push(line.to_owned());
        }
        fn prompt_line(&mut self, prompt: &str) -> Option<String> {
            self.output.push(format!("PROMPT {prompt}"));
            if self.lines.is_empty() {
                None
            } else {
                self.lines.remove(0)
            }
        }
        fn prompt_secret(&mut self, prompt: &str) -> Option<String> {
            self.output.push(format!("SECRET {prompt}"));
            if self.secrets.is_empty() {
                None
            } else {
                self.secrets.remove(0)
            }
        }
    }

    /// An in-memory secret store.
    #[derive(Default)]
    struct FakeStore {
        keys: HashMap<&'static str, String>,
        fail_replace: bool,
    }

    impl SecretStore for FakeStore {
        fn replace(&mut self, provider: Provider, credential: Credential) -> Result<(), BoundaryError> {
            if self.fail_replace {
                return Err(BoundaryError::new(BoundaryKind::SecretStorage, "store failed"));
            }
            self.keys
                .insert(provider.secret_service_value(), credential.expose_to_boundary().to_owned());
            Ok(())
        }
        fn load(&mut self, provider: Provider) -> Result<Credential, BoundaryError> {
            match self.keys.get(provider.secret_service_value()) {
                Some(value) => Credential::new(value.clone()),
                None => Err(BoundaryError::new(BoundaryKind::SecretStorage, "absent")),
            }
        }
    }

    /// A validator that returns queued statuses in order.
    struct FakeValidator {
        statuses: Vec<ProviderKeyStatus>,
    }

    impl KeyValidator for FakeValidator {
        fn validate(&mut self, _provider: Provider, _credential: &Credential) -> ProviderKeyStatus {
            if self.statuses.is_empty() {
                ProviderKeyStatus::Unreachable
            } else {
                self.statuses.remove(0)
            }
        }
    }

    #[test]
    fn both_valid_keys_are_validated_then_stored() {
        let mut io = FakeIo::new(
            vec![Some("deepgram-key"), Some("groq-key")],
            vec![],
        );
        let mut store = FakeStore::default();
        let mut validator = FakeValidator {
            statuses: vec![ProviderKeyStatus::Valid, ProviderKeyStatus::Valid],
        };
        let outcome = run_setup(&mut io, &mut store, &mut validator);
        assert_eq!(outcome.deepgram, ProviderOutcome::Stored);
        assert_eq!(outcome.groq, ProviderOutcome::Stored);
        assert_eq!(store.keys.get("deepgram").map(String::as_str), Some("deepgram-key"));
        assert_eq!(store.keys.get("groq").map(String::as_str), Some("groq-key"));
    }

    #[test]
    fn an_invalid_key_is_not_stored_and_the_user_can_retry() {
        // First Deepgram key is rejected, user retries (default yes), second is
        // valid. Groq entered valid straight away.
        let mut io = FakeIo::new(
            vec![Some("bad-key"), Some("good-key"), Some("groq-key")],
            vec![Some("y")], // "Try a different key?" → yes
        );
        let mut store = FakeStore::default();
        let mut validator = FakeValidator {
            statuses: vec![
                ProviderKeyStatus::InvalidKey,
                ProviderKeyStatus::Valid,
                ProviderKeyStatus::Valid,
            ],
        };
        let outcome = run_setup(&mut io, &mut store, &mut validator);
        assert_eq!(outcome.deepgram, ProviderOutcome::Stored);
        assert_eq!(store.keys.get("deepgram").map(String::as_str), Some("good-key"));
        assert!(io.transcript().contains("run `voisu setup`"), "{}", io.transcript());
    }

    #[test]
    fn a_blank_entry_skips_the_provider() {
        let mut io = FakeIo::new(vec![Some(""), Some("groq-key")], vec![]);
        let mut store = FakeStore::default();
        let mut validator = FakeValidator {
            statuses: vec![ProviderKeyStatus::Valid],
        };
        let outcome = run_setup(&mut io, &mut store, &mut validator);
        assert_eq!(outcome.deepgram, ProviderOutcome::Skipped);
        assert_eq!(outcome.groq, ProviderOutcome::Stored);
        assert!(!store.keys.contains_key("deepgram"));
        assert!(
            io.transcript().contains("voisu deepgram off"),
            "a skipped Deepgram should suggest the Groq-only path: {}",
            io.transcript()
        );
    }

    #[test]
    fn an_already_stored_key_can_be_kept_without_re_entering() {
        let mut store = FakeStore::default();
        store.keys.insert("deepgram", "existing-deepgram".to_owned());
        // Keep Deepgram (yes), then enter a Groq key.
        let mut io = FakeIo::new(vec![Some("groq-key")], vec![Some("y")]);
        let mut validator = FakeValidator {
            statuses: vec![ProviderKeyStatus::Valid],
        };
        let outcome = run_setup(&mut io, &mut store, &mut validator);
        assert_eq!(outcome.deepgram, ProviderOutcome::Kept);
        // The stored key is untouched.
        assert_eq!(store.keys.get("deepgram").map(String::as_str), Some("existing-deepgram"));
        assert_eq!(outcome.groq, ProviderOutcome::Stored);
    }

    #[test]
    fn a_transient_failure_can_be_saved_anyway() {
        // Deepgram: rate-limited, user saves anyway. Groq: blank/skip.
        let mut io = FakeIo::new(
            vec![Some("deepgram-key"), Some("")],
            vec![Some("y")], // "Save it anyway?" → yes
        );
        let mut store = FakeStore::default();
        let mut validator = FakeValidator {
            statuses: vec![ProviderKeyStatus::RateLimited],
        };
        let outcome = run_setup(&mut io, &mut store, &mut validator);
        assert_eq!(outcome.deepgram, ProviderOutcome::StoredUnverified);
        assert_eq!(store.keys.get("deepgram").map(String::as_str), Some("deepgram-key"));
        assert_eq!(outcome.groq, ProviderOutcome::Skipped);
    }

    #[test]
    fn declining_to_retry_an_invalid_key_skips_the_provider() {
        let mut io = FakeIo::new(
            vec![Some("bad-key"), Some("")],
            vec![Some("n")], // "Try a different key?" → no
        );
        let mut store = FakeStore::default();
        let mut validator = FakeValidator {
            statuses: vec![ProviderKeyStatus::InvalidKey],
        };
        let outcome = run_setup(&mut io, &mut store, &mut validator);
        assert_eq!(outcome.deepgram, ProviderOutcome::Skipped);
        assert!(!store.keys.contains_key("deepgram"));
    }
}
