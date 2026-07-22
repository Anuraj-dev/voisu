//! The `voisu setup` wizard.
//!
//! The flow — prompt for each provider's key one at a time, validate it live
//! before saving, and let a re-run keep or replace what is already stored — is a
//! pure function over three injected seams ([`WizardIo`], [`voisu_core::SecretStore`],
//! and [`KeyValidator`]), so the whole thing is unit-tested without a real
//! terminal, network, or keyring. The thin production glue ([`StdioWizard`],
//! [`LiveKeyValidator`]) lives at the bottom of this file.

use voisu_core::{
    Credential, KeyDiagnosis, KeyLocation, Provider, ProviderKeyStatus, SecretStore,
    provider_free_tier_hint,
};

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

    // An informed keep/replace: say WHERE the key lives so keeping a plaintext or
    // env-override key is a deliberate choice, and migrate a plaintext key back
    // into the keyring on keep when the keyring is available again.
    match store.diagnose(provider) {
        KeyDiagnosis::Found { location: KeyLocation::EnvOverride, .. } => {
            io.writeln(&format!(
                "Note: {name} is set via the {} environment variable, which wins at runtime — \
                 unset it to use a stored key.",
                provider.environment_variable()
            ));
            if ask_yes_no(io, &format!("Keep the {name} environment key?"), true) {
                return ProviderOutcome::Kept;
            }
        }
        KeyDiagnosis::Found { location: KeyLocation::Keyring, .. } => {
            if ask_yes_no(
                io,
                &format!("A {name} key is already stored in your keyring. Keep it?"),
                true,
            ) {
                io.writeln(&format!("Keeping the existing {name} key."));
                return ProviderOutcome::Kept;
            }
        }
        KeyDiagnosis::Found { location: KeyLocation::PlaintextFile, credential } => {
            io.writeln(&format!(
                "A {name} key is stored in the plaintext fallback file (saved while the keyring \
                 was unavailable)."
            ));
            if ask_yes_no(io, "Keep it, migrating into your keyring if available?", true) {
                match store.replace(provider, credential) {
                    Ok(()) => io.writeln(&format!(
                        "Kept the {name} key (migrated into the keyring if it was available)."
                    )),
                    Err(error) => io.writeln(&format!(
                        "Kept the {name} key: {}",
                        error.public_message()
                    )),
                }
                return ProviderOutcome::Kept;
            }
        }
        // The notice fires on the PRESENCE of the override, not on its
        // parseability: a present-but-malformed value (empty, stray newline)
        // still wins at runtime and breaks dictation, so it must be named
        // before prompting for a key to store for after the fix.
        KeyDiagnosis::EnvOverrideInvalid => {
            let variable = provider.environment_variable();
            io.writeln(&format!(
                "Note: the {variable} environment variable is set but is not a usable key \
                 (empty or contains a line break), and it overrides any stored key at runtime — \
                 unset or fix {variable} before dictating."
            ));
        }
        // Absent, or the keyring could not be consulted (locked/unavailable/tool
        // missing): fall through and prompt for a key.
        _ => {}
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
        // Non-TTY (pipe/CI/scripting) is deliberately untouched: a plain,
        // unmasked line read so `printf 'key\n' | voisu setup …` keeps working
        // byte-for-byte.
        if !std::io::stdin().is_terminal() {
            print!("{prompt}");
            let _ = std::io::stdout().flush();
            return read_line();
        }
        // TTY: masked echo (one `*` per character) plus a post-entry
        // confirmation revealing at most first-4 + last-4, so a doubled or
        // failed paste is visible immediately rather than surfacing later as an
        // opaque auth failure.
        loop {
            print!("{prompt}");
            let _ = std::io::stdout().flush();
            match read_masked_line() {
                MaskedOutcome::Entered(line) => {
                    // The Enter was swallowed (never echoed), so the cursor is
                    // still parked after the last `*`; advance it ourselves.
                    println!();
                    let shown = line.trim();
                    if !shown.is_empty() {
                        println!("\u{2713} captured   {}", mask_key_reveal(shown));
                    }
                    return Some(line);
                }
                MaskedOutcome::Eof => {
                    println!();
                    return None;
                }
                MaskedOutcome::TooLong => {
                    println!();
                    println!(
                        "That entry exceeds the {MASKED_INPUT_CAP}-character limit and was not \
                         captured — paste the key once and try again."
                    );
                    // Re-prompt on the next loop iteration.
                }
                // The terminal could not be put in masked mode; a plain,
                // already-echoed line was read as a graceful fallback.
                MaskedOutcome::Plain(line) => return line,
            }
        }
    }
}

/// Maximum number of *characters* accepted for a single masked entry. A runaway
/// paste must not allocate without bound; provider keys are well under this, so
/// a legitimate key never approaches the cap.
const MASKED_INPUT_CAP: usize = 512;

/// Renders the post-entry confirmation reveal, e.g. `gsk_••••1a2b   (56 chars)`.
/// Shows the first four and last four characters with the middle masked, plus
/// the character count so a doubled paste is obvious at a glance. Keys shorter
/// than twelve characters are masked entirely rather than exposing most of a
/// short secret; at most eight characters of the key are ever revealed.
fn mask_key_reveal(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    let count = chars.len();
    let noun = if count == 1 { "char" } else { "chars" };
    let reveal = if count >= 12 {
        let first: String = chars[..4].iter().collect();
        let last: String = chars[count - 4..].iter().collect();
        format!("{first}\u{2022}\u{2022}\u{2022}\u{2022}{last}")
    } else {
        // Fully masked; the count (already shown) is the only length signal.
        "\u{2022}".repeat(count)
    };
    format!("{reveal}   ({count} {noun})")
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

/// Published so the SIGINT/SIGQUIT handler can restore the terminal from an
/// async-signal context without touching any lock or allocation.
static ECHO_GUARD_FD: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);
static ECHO_GUARD_LFLAG: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
/// The original `VMIN`/`VTIME` control characters. Masked entry clears `ICANON`,
/// which makes these two active, so the handler must restore them alongside the
/// local flags — otherwise an interrupt could leave the tty in a byte-at-a-time
/// mode with the wrong read timing.
static ECHO_GUARD_VMIN: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
static ECHO_GUARD_VTIME: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Restores the terminal's local flags — both `ECHO` and `ICANON`, since masked
/// entry clears both — and the `VMIN`/`VTIME` read controls those two govern,
/// then re-raises the signal with its default disposition so the process still
/// dies as the user intended, now with the shell's echo and line discipline
/// intact rather than silently off. Only calls async-signal-safe functions
/// (`tcgetattr`/`tcsetattr`/`signal`/`raise`) reading values published before
/// the handler was installed.
extern "C" fn restore_echo_on_signal(signal: libc::c_int) {
    use std::sync::atomic::Ordering;
    let fd = ECHO_GUARD_FD.load(Ordering::SeqCst);
    if fd >= 0 {
        // SAFETY: async-signal-safe termios calls on a valid fd; the flags and
        // control characters to restore were published before the handler was
        // installed.
        unsafe {
            let mut termios: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(fd, &mut termios) == 0 {
                termios.c_lflag = ECHO_GUARD_LFLAG.load(Ordering::SeqCst) as libc::tcflag_t;
                termios.c_cc[libc::VMIN] = ECHO_GUARD_VMIN.load(Ordering::SeqCst) as libc::cc_t;
                termios.c_cc[libc::VTIME] = ECHO_GUARD_VTIME.load(Ordering::SeqCst) as libc::cc_t;
                libc::tcsetattr(fd, libc::TCSANOW, &termios);
            }
        }
    }
    // SAFETY: restoring the default disposition and re-raising are AS-safe.
    unsafe {
        libc::signal(signal, libc::SIG_DFL);
        libc::raise(signal);
    }
}

/// Restores the terminal and the previous signal handlers on every exit path
/// (normal return, `?`, or a panic), so a hidden read can never leave the shell
/// with echo off.
struct EchoGuard {
    fd: i32,
    original: libc::termios,
    old_sigint: libc::sigaction,
    old_sigquit: libc::sigaction,
}

impl Drop for EchoGuard {
    fn drop(&mut self) {
        use std::sync::atomic::Ordering;
        // SAFETY: restores the saved termios and sigactions captured on entry.
        unsafe {
            libc::tcsetattr(self.fd, libc::TCSANOW, &self.original);
            libc::sigaction(libc::SIGINT, &self.old_sigint, std::ptr::null_mut());
            libc::sigaction(libc::SIGQUIT, &self.old_sigquit, std::ptr::null_mut());
        }
        ECHO_GUARD_FD.store(-1, Ordering::SeqCst);
    }
}

/// The local-flag mask for masked entry: clear `ECHO` and `ICANON` so we read
/// byte-by-byte and echo `*` ourselves, but keep every other flag — crucially
/// `ISIG`, so Ctrl-C/Ctrl-\ still raise signals that the restorer handles.
/// Pure and total, so the flag arithmetic is unit-tested without a terminal.
fn masked_lflag(original: libc::tcflag_t) -> libc::tcflag_t {
    original & !(libc::ECHO | libc::ICANON)
}

/// The result of a single masked-line read.
enum MaskedOutcome {
    /// Enter (CR or LF) terminated a line; the string excludes the terminator.
    Entered(String),
    /// End of input on an empty buffer (Ctrl-D or a closed stream) — `None`,
    /// matching the pre-existing contract.
    Eof,
    /// Input exceeded [`MASKED_INPUT_CAP`]; nothing is returned rather than a
    /// silently truncated key.
    TooLong,
    /// The terminal could not be reconfigured; a plain (already-echoed) line was
    /// read as a graceful fallback.
    Plain(Option<String>),
}

/// Byte-stream line editor for masked entry. Pure and deterministic: it holds
/// the accumulated `buffer` and just the *pending* screen bytes for the current
/// step (`*`, `\b \b`, …), plus enough state to reassemble UTF-8 characters and
/// swallow escape sequences. The caller drains [`take_out`](Self::take_out)
/// after each step and writes it to the terminal, so no full-session transcript
/// is retained — memory stays bounded even under an unbounded stream of
/// character-then-backspace. Tests feed a byte slice and accumulate the drained
/// fragments externally. Nothing here depends on the speed at which bytes arrive.
#[derive(Default)]
struct MaskedLineEditor {
    buffer: String,
    /// Screen bytes produced by the most recent step(s), not yet drained. Bounded
    /// by the cap (a kill-line erases at most `MASKED_INPUT_CAP` stars) and reset
    /// to empty by every `take_out`.
    out: String,
    char_count: usize,
    /// Bytes of a partially-read multi-byte UTF-8 character.
    utf8_pending: Vec<u8>,
    utf8_expected: usize,
    escape: EscapeState,
    /// Set once the cap is hit; further characters are swallowed, not truncated
    /// into the buffer, so the caller can report rather than store garbage.
    capped: bool,
}

/// Where we are within an ANSI escape sequence (arrow keys, Home/End, bracketed
/// paste markers). Such sequences are swallowed whole so a single arrow key
/// never paints three stray stars.
#[derive(Default, PartialEq)]
enum EscapeState {
    #[default]
    None,
    /// Saw `ESC`; awaiting `[`/`O` (CSI) or a lone final byte (Meta/Alt).
    Esc,
    /// Inside a CSI sequence; swallow until a final byte in `0x40..=0x7E`.
    Csi,
}

/// What the reader should do after one byte.
enum Step {
    /// Keep reading; any screen output is pending in `out` for the caller to
    /// drain.
    Continue,
    /// Enter seen — terminate with the current buffer.
    Done,
    /// EOF on an empty buffer (Ctrl-D) — terminate with no line.
    Eof,
}

impl MaskedLineEditor {
    /// Feeds one input byte, updating `buffer` and appending any screen bytes to
    /// `out`, and reports whether the line is finished.
    fn step(&mut self, byte: u8) -> Step {
        // Escape-sequence swallowing has top priority; no stars are emitted.
        if self.escape != EscapeState::None {
            self.consume_escape(byte);
            return Step::Continue;
        }
        // Mid-character UTF-8 assembly: only continuation bytes are expected.
        if !self.utf8_pending.is_empty() {
            if byte & 0b1100_0000 == 0b1000_0000 {
                self.utf8_pending.push(byte);
                if self.utf8_pending.len() >= self.utf8_expected {
                    self.flush_utf8();
                }
                return Step::Continue;
            }
            // Malformed sequence: drop the partial character and re-handle this
            // byte as a fresh one below.
            self.utf8_pending.clear();
            self.utf8_expected = 0;
        }
        match byte {
            b'\n' | b'\r' => Step::Done,
            0x08 | 0x7F => {
                self.backspace();
                Step::Continue
            }
            0x15 => {
                // Ctrl-U: kill the whole line.
                self.kill_line();
                Step::Continue
            }
            0x04 => {
                // Ctrl-D: EOF on an empty buffer, otherwise submit what is held
                // (matching canonical-mode end-of-file semantics).
                if self.buffer.is_empty() {
                    Step::Eof
                } else {
                    Step::Done
                }
            }
            0x1B => {
                self.escape = EscapeState::Esc;
                Step::Continue
            }
            b if b < 0x20 => Step::Continue, // other control bytes: swallow
            b if b < 0x80 => {
                self.append_char(b as char);
                Step::Continue
            }
            b if (0xC0..0xF8).contains(&b) => {
                self.utf8_expected = match b {
                    0xC0..=0xDF => 2,
                    0xE0..=0xEF => 3,
                    _ => 4,
                };
                self.utf8_pending.push(b);
                Step::Continue
            }
            _ => Step::Continue, // stray continuation / invalid lead: swallow
        }
    }

    fn consume_escape(&mut self, byte: u8) {
        match self.escape {
            EscapeState::Esc => {
                if byte == b'[' || byte == b'O' {
                    self.escape = EscapeState::Csi;
                } else {
                    // A two-byte ESC sequence (Meta/Alt) or a lone ESC: done.
                    self.escape = EscapeState::None;
                }
            }
            EscapeState::Csi => {
                // Parameter/intermediate bytes keep the sequence open; the final
                // byte (0x40..=0x7E) closes it.
                if (0x40..=0x7E).contains(&byte) {
                    self.escape = EscapeState::None;
                }
            }
            EscapeState::None => {}
        }
    }

    fn flush_utf8(&mut self) {
        let bytes = std::mem::take(&mut self.utf8_pending);
        self.utf8_expected = 0;
        if let Ok(text) = std::str::from_utf8(&bytes) {
            if let Some(ch) = text.chars().next() {
                self.append_char(ch);
            }
        }
        // Invalid bytes are dropped: no buffer entry, no star.
    }

    /// Drains and returns the pending screen bytes, leaving `out` empty. Called
    /// once per step by the reader (and by tests, which accumulate the pieces).
    fn take_out(&mut self) -> String {
        std::mem::take(&mut self.out)
    }

    fn append_char(&mut self, ch: char) {
        if self.char_count >= MASKED_INPUT_CAP {
            // Over the cap: stop accepting, do not truncate silently.
            self.capped = true;
            return;
        }
        self.buffer.push(ch);
        self.char_count += 1;
        self.out.push('*');
    }

    fn backspace(&mut self) {
        if self.buffer.pop().is_some() {
            self.char_count -= 1;
            // Erase one `*`: back over it, overwrite with a space, back again.
            self.out.push_str("\u{8} \u{8}");
        }
    }

    fn kill_line(&mut self) {
        for _ in 0..self.char_count {
            self.out.push_str("\u{8} \u{8}");
        }
        self.buffer.clear();
        self.char_count = 0;
        // A killed line is a clean slate: an earlier over-cap paste must not
        // keep poisoning a fresh, valid key entered afterward.
        self.capped = false;
    }

    fn finish_entered(self) -> MaskedOutcome {
        if self.capped {
            MaskedOutcome::TooLong
        } else {
            MaskedOutcome::Entered(self.buffer)
        }
    }

    fn finish_at_eof(self) -> MaskedOutcome {
        if self.capped {
            MaskedOutcome::TooLong
        } else if self.buffer.is_empty() {
            MaskedOutcome::Eof
        } else {
            MaskedOutcome::Entered(self.buffer)
        }
    }
}

/// Reads one line from stdin with masked echo (one `*` per character) and
/// explicit line editing. A RAII guard restores the terminal on every ordinary
/// exit, and a temporary SIGINT/SIGQUIT handler restores it when the user
/// interrupts mid-entry — so no exit path (including Ctrl-C) leaves the shell
/// with echo or the line discipline silently off. Falls back to a plain read if
/// the terminal cannot be reconfigured.
fn read_masked_line() -> MaskedOutcome {
    use std::sync::atomic::Ordering;
    // SAFETY: termios/sigaction calls on the stdin fd with local storage; every
    // path either installs the RAII guard or returns before altering the tty.
    unsafe {
        let fd = libc::STDIN_FILENO;
        let mut original: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(fd, &mut original) != 0 {
            return MaskedOutcome::Plain(read_line());
        }

        // Publish the original lflag and the VMIN/VTIME read controls the
        // handler restores (ECHO, ICANON, and their timing both change for
        // masked entry).
        ECHO_GUARD_LFLAG.store(original.c_lflag as u32, Ordering::SeqCst);
        ECHO_GUARD_VMIN.store(original.c_cc[libc::VMIN] as u32, Ordering::SeqCst);
        ECHO_GUARD_VTIME.store(original.c_cc[libc::VTIME] as u32, Ordering::SeqCst);
        ECHO_GUARD_FD.store(fd, Ordering::SeqCst);

        // Install the interrupt-time restorers and capture the previous ones.
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = restore_echo_on_signal as *const () as libc::sighandler_t;
        libc::sigemptyset(&mut action.sa_mask);
        action.sa_flags = 0;
        let mut old_sigint: libc::sigaction = std::mem::zeroed();
        let mut old_sigquit: libc::sigaction = std::mem::zeroed();
        libc::sigaction(libc::SIGINT, &action, &mut old_sigint);
        libc::sigaction(libc::SIGQUIT, &action, &mut old_sigquit);

        let _guard = EchoGuard {
            fd,
            original,
            old_sigint,
            old_sigquit,
        };

        // Clear ECHO and ICANON but keep ISIG on, so Ctrl-C/Ctrl-\ still raise
        // signals rather than arriving as bytes — the handler above is what
        // makes an interrupted prompt safe, and going fully raw would make it
        // dead code. With ICANON off, VMIN/VTIME govern reads: set VMIN=1,
        // VTIME=0 so each read blocks for exactly one byte and never returns 0
        // spuriously (which the driver would misread as EOF, accepting a
        // half-typed key).
        let mut hidden = original;
        hidden.c_lflag = masked_lflag(original.c_lflag);
        hidden.c_cc[libc::VMIN] = 1;
        hidden.c_cc[libc::VTIME] = 0;
        if libc::tcsetattr(fd, libc::TCSANOW, &hidden) != 0 {
            // The guard still restores handlers/termios on drop.
            return MaskedOutcome::Plain(read_line());
        }
        // Masked mode is now live and the interrupt-time restorer is armed. In
        // tests this releases a subprocess barrier so a PTY harness can deliver
        // SIGINT/SIGQUIT knowing restoration is in scope; it is a no-op (and
        // compiled out) otherwise.
        #[cfg(test)]
        tests::signal_masked_ready();
        // Guard stays alive across the read and drops (restoring the terminal)
        // only after the outcome is computed.
        drive_masked_editor()
    }
}

/// Drives a [`MaskedLineEditor`] over stdin, flushing each step's star/erase
/// bytes to the screen as they are produced. Only the pending fragment is held
/// at a time (drained via `take_out`), so memory stays bounded. Kept separate
/// from the terminal setup so the [`EchoGuard`] restores state after this
/// returns, on every path.
fn drive_masked_editor() -> MaskedOutcome {
    use std::io::{Read, Write};
    let mut editor = MaskedLineEditor::default();
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let mut byte = [0u8; 1];
    loop {
        match input.read(&mut byte) {
            Ok(0) => return editor.finish_at_eof(),
            Ok(_) => {
                let step = editor.step(byte[0]);
                let fragment = editor.take_out();
                if !fragment.is_empty() {
                    let mut out = std::io::stdout().lock();
                    let _ = out.write_all(fragment.as_bytes());
                    let _ = out.flush();
                }
                match step {
                    Step::Continue => {}
                    Step::Done => return editor.finish_entered(),
                    Step::Eof => return MaskedOutcome::Eof,
                }
            }
            Err(ref error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return editor.finish_at_eof(),
        }
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

    /// An in-memory secret store. `locations` lets a test pin where a key
    /// "lives" (keyring by default) so the wizard's informed keep/replace and
    /// migration prompts can be exercised.
    #[derive(Default)]
    struct FakeStore {
        keys: HashMap<&'static str, String>,
        locations: HashMap<&'static str, KeyLocation>,
        /// Providers whose env override is present but cannot form a
        /// credential (empty or multi-line) — diagnosed before any stored key.
        invalid_env: Vec<&'static str>,
        fail_replace: bool,
    }

    impl SecretStore for FakeStore {
        fn replace(&mut self, provider: Provider, credential: Credential) -> Result<(), BoundaryError> {
            if self.fail_replace {
                return Err(BoundaryError::new(BoundaryKind::SecretStorage, "store failed"));
            }
            self.keys
                .insert(provider.secret_service_value(), credential.expose_to_boundary().to_owned());
            // A successful store migrates a key into the keyring.
            self.locations.insert(provider.secret_service_value(), KeyLocation::Keyring);
            Ok(())
        }
        fn load(&mut self, provider: Provider) -> Result<Credential, BoundaryError> {
            match self.keys.get(provider.secret_service_value()) {
                Some(value) => Credential::new(value.clone()),
                None => Err(BoundaryError::new(BoundaryKind::SecretStorage, "absent")),
            }
        }
        fn diagnose(&mut self, provider: Provider) -> KeyDiagnosis {
            if self.invalid_env.contains(&provider.secret_service_value()) {
                return KeyDiagnosis::EnvOverrideInvalid;
            }
            match self.keys.get(provider.secret_service_value()) {
                Some(value) => KeyDiagnosis::Found {
                    location: self
                        .locations
                        .get(provider.secret_service_value())
                        .copied()
                        .unwrap_or(KeyLocation::Keyring),
                    credential: Credential::new(value.clone()).unwrap(),
                },
                None => KeyDiagnosis::Absent,
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
    fn an_env_override_is_flagged_before_keeping() {
        // A key present via an env override must be announced (env wins at
        // runtime) so keeping it is an informed choice.
        let mut store = FakeStore::default();
        store.keys.insert("deepgram", "env-deepgram".to_owned());
        store.locations.insert("deepgram", KeyLocation::EnvOverride);
        let mut io = FakeIo::new(vec![Some("groq-key")], vec![Some("y")]);
        let mut validator = FakeValidator {
            statuses: vec![ProviderKeyStatus::Valid],
        };
        let outcome = run_setup(&mut io, &mut store, &mut validator);
        assert_eq!(outcome.deepgram, ProviderOutcome::Kept);
        assert!(
            io.transcript().contains("environment variable, which wins at runtime"),
            "{}",
            io.transcript()
        );
    }

    #[test]
    fn a_present_but_invalid_env_override_is_flagged_before_prompting() {
        // The env notice must fire on the PRESENCE of the variable, not on its
        // parseability: an empty/multi-line override still wins at runtime and
        // silently breaks dictation, so the wizard must name it before
        // prompting for a key to store.
        let mut store = FakeStore::default();
        store.invalid_env.push("deepgram");
        let mut io = FakeIo::new(vec![Some("deepgram-key"), Some("")], vec![]);
        let mut validator = FakeValidator {
            statuses: vec![ProviderKeyStatus::Valid],
        };
        let outcome = run_setup(&mut io, &mut store, &mut validator);
        assert!(
            io.transcript().contains("VOISU_DEEPGRAM_API_KEY"),
            "the broken override must be named: {}",
            io.transcript()
        );
        assert!(
            io.transcript().contains("unset or fix"),
            "the remedy must be stated: {}",
            io.transcript()
        );
        // The wizard still prompts so a key can be stored for after the fix.
        assert_eq!(outcome.deepgram, ProviderOutcome::Stored);
        assert_eq!(store.keys.get("deepgram").map(String::as_str), Some("deepgram-key"));
    }

    #[test]
    fn a_plaintext_key_is_offered_migration_on_keep() {
        // A key living only in the plaintext fallback is surfaced as such and,
        // on keep, re-stored (migrated) through replace.
        let mut store = FakeStore::default();
        store.keys.insert("deepgram", "file-deepgram".to_owned());
        store.locations.insert("deepgram", KeyLocation::PlaintextFile);
        // Keep+migrate Deepgram (yes), then skip Groq.
        let mut io = FakeIo::new(vec![Some("")], vec![Some("y")]);
        let mut validator = FakeValidator { statuses: vec![] };
        let outcome = run_setup(&mut io, &mut store, &mut validator);
        assert_eq!(outcome.deepgram, ProviderOutcome::Kept);
        assert!(io.transcript().contains("plaintext fallback file"), "{}", io.transcript());
        // The migration re-stored it, flipping its location to the keyring.
        assert_eq!(store.locations.get("deepgram"), Some(&KeyLocation::Keyring));
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

    // -----------------------------------------------------------------------
    // Masked-echo line editor. Driven purely over a byte slice — no terminal,
    // and no dependence on the speed at which bytes arrive.
    // -----------------------------------------------------------------------

    #[derive(Debug, PartialEq)]
    enum End {
        Continue,
        Done,
        Eof,
    }

    /// Feeds a byte slice to a fresh editor, draining the screen output after
    /// every step (exactly as the real reader does), and returns the editor, the
    /// externally accumulated screen bytes, and how it ended. The editor never
    /// retains a full transcript, so tests read the accumulation here.
    fn feed(bytes: &[u8]) -> (MaskedLineEditor, String, End) {
        let mut editor = MaskedLineEditor::default();
        let mut screen = String::new();
        let mut end = End::Continue;
        for &byte in bytes {
            let step = editor.step(byte);
            screen.push_str(&editor.take_out());
            match step {
                Step::Continue => {}
                Step::Done => {
                    end = End::Done;
                    break;
                }
                Step::Eof => {
                    end = End::Eof;
                    break;
                }
            }
        }
        (editor, screen, end)
    }

    fn stars(screen: &str) -> usize {
        screen.matches('*').count()
    }

    #[test]
    fn printable_ascii_buffers_and_prints_one_star_each() {
        let (editor, screen, end) = feed(b"abcd");
        assert_eq!(end, End::Continue);
        assert_eq!(editor.buffer, "abcd");
        assert_eq!(screen, "****");
    }

    #[test]
    fn star_count_matches_key_length_exactly() {
        let key = "gsk_abcdefghijklmnopqrstuvwxyz0123456789";
        let (editor, screen, _) = feed(key.as_bytes());
        assert_eq!(stars(&screen), key.len());
        assert_eq!(editor.buffer, key);
    }

    #[test]
    fn enter_terminates_on_both_lf_and_cr() {
        let (editor, _, end) = feed(b"abc\n");
        assert_eq!(end, End::Done);
        assert_eq!(editor.buffer, "abc");

        let (editor, _, end) = feed(b"abc\r");
        assert_eq!(end, End::Done);
        assert_eq!(editor.buffer, "abc");
    }

    #[test]
    fn backspace_removes_one_character_and_erases_one_star() {
        let (editor, screen, _) = feed(b"abc\x7f");
        assert_eq!(editor.buffer, "ab");
        assert_eq!(screen, "***\u{8} \u{8}");
        // 0x08 (BS) behaves identically to 0x7F (DEL).
        let (editor, _, _) = feed(b"abc\x08");
        assert_eq!(editor.buffer, "ab");
    }

    #[test]
    fn backspace_on_empty_buffer_is_a_no_op() {
        let (editor, screen, _) = feed(b"\x7f");
        assert_eq!(editor.buffer, "");
        assert_eq!(screen, "");
    }

    #[test]
    fn ctrl_u_clears_buffer_and_erases_all_stars() {
        let (editor, screen, _) = feed(b"abcde\x15");
        assert_eq!(editor.buffer, "");
        let erases = screen.matches("\u{8} \u{8}").count();
        assert_eq!(erases, 5, "one erase per star: {screen:?}");
    }

    #[test]
    fn ctrl_u_recovers_from_an_over_cap_paste_so_a_fresh_key_succeeds() {
        // An over-cap paste sets `capped`; Ctrl-U must clear that flag so a
        // valid key entered afterward is accepted rather than still reported
        // TooLong.
        let mut bytes = vec![b'a'; MASKED_INPUT_CAP + 8];
        bytes.push(0x15); // Ctrl-U
        bytes.extend_from_slice(b"good-key\n");
        let (editor, _, end) = feed(&bytes);
        assert_eq!(end, End::Done);
        assert!(!editor.capped, "Ctrl-U must clear the over-cap state");
        assert_eq!(editor.buffer, "good-key");
        assert!(
            matches!(editor.finish_entered(), MaskedOutcome::Entered(line) if line == "good-key"),
            "a valid key after Ctrl-U must be accepted"
        );
    }

    #[test]
    fn character_then_backspace_churn_never_retains_a_transcript() {
        // Repeated char+backspace keeps char_count at zero but would grow an
        // accumulated screen without bound. Draining per step must keep the
        // editor's own retained state tiny regardless of how many cycles run.
        let mut editor = MaskedLineEditor::default();
        for _ in 0..100_000 {
            editor.step(b'a');
            let _ = editor.take_out();
            editor.step(0x7f);
            let _ = editor.take_out();
        }
        assert_eq!(editor.buffer, "");
        assert_eq!(editor.char_count, 0);
        // The only per-step buffer is `out`, drained above; nothing accumulates.
        assert!(editor.out.is_empty());
        assert!(editor.out.capacity() <= 8, "pending output stays bounded");
    }

    #[test]
    fn ctrl_d_on_empty_buffer_is_eof_but_submits_a_pending_line() {
        let (editor, _, end) = feed(b"\x04");
        assert_eq!(end, End::Eof);
        assert_eq!(editor.buffer, "");

        let (editor, _, end) = feed(b"ab\x04");
        assert_eq!(end, End::Done);
        assert_eq!(editor.buffer, "ab");
    }

    #[test]
    fn arrow_and_home_end_keys_print_no_stars() {
        // Right arrow, then Home, then End (CSI and SS3 forms), then a real char.
        let (editor, screen, _) = feed(b"\x1b[C\x1b[H\x1b[F\x1bOFa");
        assert_eq!(editor.buffer, "a");
        assert_eq!(stars(&screen), 1);
    }

    #[test]
    fn bracketed_paste_markers_are_swallowed_leaving_only_the_key_stars() {
        // A paste wrapped in ESC[200~ … ESC[201~ must not inject stray stars.
        let (editor, screen, _) = feed(b"\x1b[200~key\x1b[201~");
        assert_eq!(editor.buffer, "key");
        assert_eq!(stars(&screen), 3);
    }

    #[test]
    fn multibyte_utf8_prints_one_star_per_character() {
        // 2-byte é, 3-byte €, 4-byte 😀.
        let input = "é€😀";
        let (editor, screen, _) = feed(input.as_bytes());
        assert_eq!(editor.buffer, input);
        assert_eq!(stars(&screen), 3, "one star per character, not per byte");
    }

    #[test]
    fn backspace_removes_a_whole_multibyte_character() {
        let (editor, _, _) = feed("é".as_bytes());
        assert_eq!(editor.buffer, "é");
        let (editor, screen, _) = feed(b"\xc3\xa9\x7f"); // é then DEL
        assert_eq!(editor.buffer, "", "backspace must not split a character");
        assert_eq!(screen, "*\u{8} \u{8}");
    }

    #[test]
    fn over_long_input_is_capped_and_reported_not_truncated() {
        let mut bytes = vec![b'a'; MASKED_INPUT_CAP + 16];
        bytes.push(b'\n');
        let (editor, screen, end) = feed(&bytes);
        assert_eq!(end, End::Done);
        assert_eq!(editor.buffer.chars().count(), MASKED_INPUT_CAP);
        assert_eq!(stars(&screen), MASKED_INPUT_CAP);
        assert!(editor.capped);
        assert!(
            matches!(editor.finish_entered(), MaskedOutcome::TooLong),
            "a capped entry reports TooLong rather than a truncated key"
        );
    }

    #[test]
    fn finish_at_eof_returns_a_pending_non_empty_line() {
        let (editor, _, _) = feed(b"partial");
        assert!(matches!(editor.finish_at_eof(), MaskedOutcome::Entered(line) if line == "partial"));
    }

    // -----------------------------------------------------------------------
    // Confirmation reveal.
    // -----------------------------------------------------------------------

    #[test]
    fn reveal_shows_first_four_and_last_four_with_the_count() {
        assert_eq!(mask_key_reveal("gsk_1234567890abcd"), "gsk_••••abcd   (18 chars)");
        // Exactly at the twelve-character threshold.
        assert_eq!(mask_key_reveal("abcdefghijkl"), "abcd••••ijkl   (12 chars)");
    }

    #[test]
    fn short_keys_are_masked_entirely() {
        // Eleven characters: below the threshold, nothing is revealed.
        assert_eq!(mask_key_reveal("abcdefghijk"), "•••••••••••   (11 chars)");
        assert_eq!(mask_key_reveal(""), "   (0 chars)");
        assert_eq!(mask_key_reveal("x"), "•   (1 char)");
    }

    #[test]
    fn reveal_never_exposes_more_than_eight_characters_of_the_key() {
        // A long key of all distinct positions; count the non-mask, non-count
        // glyphs before the "   (" separator.
        let key: String = ('a'..='z').chain('A'..='Z').collect(); // 52 chars
        let reveal = mask_key_reveal(&key);
        let head = reveal.split("   (").next().unwrap();
        let exposed = head.chars().filter(|c| *c != '•').count();
        assert!(exposed <= 8, "revealed {exposed} characters: {reveal}");
    }

    // -----------------------------------------------------------------------
    // Terminal-state arithmetic (pure; the real EchoGuard restores wholesale).
    // -----------------------------------------------------------------------

    #[test]
    fn masked_lflag_clears_echo_and_icanon_but_keeps_isig() {
        let original = libc::ECHO | libc::ICANON | libc::ISIG;
        let masked = masked_lflag(original);
        assert_eq!(masked & libc::ECHO, 0, "ECHO must be cleared");
        assert_eq!(masked & libc::ICANON, 0, "ICANON must be cleared");
        assert_eq!(masked & libc::ISIG, libc::ISIG, "ISIG must survive so Ctrl-C still signals");
    }

    // -----------------------------------------------------------------------
    // PTY-backed terminal restoration. The pure tests above prove the flag
    // arithmetic; these prove the effect on a real tty — that after a normal
    // return (EchoGuard::drop) and after an interrupt (the async-signal-safe
    // handler), ECHO, ICANON, and VMIN/VTIME are all put back. Synchronization
    // is by pipe and by blocking wait/read — never by sleeping or by assuming
    // how fast bytes arrive.
    // -----------------------------------------------------------------------

    /// Readiness barrier written to `fd 3` when the parent wired one, releasing
    /// the harness only once masked mode is live and the restorer is armed.
    /// A no-op in-process and compiled out of non-test builds.
    static MASKED_READY_FD: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);

    pub(super) fn signal_masked_ready() {
        use std::sync::atomic::Ordering;
        let fd = MASKED_READY_FD.load(Ordering::SeqCst);
        if fd >= 0 {
            let byte = [1u8];
            // SAFETY: fd is a pipe write-end wired by the parent test; a 1-byte
            // write is async-signal-safe and any error is irrelevant here.
            unsafe {
                libc::write(fd, byte.as_ptr() as *const libc::c_void, 1);
            }
        }
    }

    /// The child half of the PTY tests. Re-exec'd by [`spawn_pty_child`] with
    /// its stdin bound to a pty slave and `fd 3` wired to a readiness pipe; a
    /// plain `cargo test` run (no env marker) makes it a no-op.
    #[test]
    fn pty_child_entry() {
        let mode = match std::env::var("VOISU_MASKED_PTY_CHILD") {
            Ok(mode) => mode,
            Err(_) => return,
        };
        // Watchdog: if anything wedges (e.g. the read never completes), die so
        // the parent unblocks and fails loudly instead of hanging CI. Never
        // fires on the success path, which completes in microseconds.
        unsafe { libc::alarm(20) };
        MASKED_READY_FD.store(3, std::sync::atomic::Ordering::SeqCst);
        let outcome = read_masked_line();
        // Reached only on the normal path; the signal path dies mid-read.
        let code = match (mode.as_str(), &outcome) {
            ("normal", MaskedOutcome::Entered(line)) if line == "hello" => 0,
            _ => 7,
        };
        std::process::exit(code);
    }

    struct PtyChild {
        child: std::process::Child,
        master: libc::c_int,
        /// A parent-held slave fd for reading the tty termios after restoration.
        probe: libc::c_int,
        /// Read-end of the child's masked-mode readiness pipe.
        ready: libc::c_int,
    }

    /// Creates a pty stamped with a distinctive original termios, then re-execs
    /// this test binary running only `pty_child_entry` with stdin = the slave
    /// and a readiness pipe on `fd 3`.
    fn spawn_pty_child(mode: &str) -> PtyChild {
        use std::os::fd::{FromRawFd, OwnedFd};
        use std::os::unix::process::CommandExt;
        use std::process::{Command, Stdio};

        let mut master: libc::c_int = -1;
        let mut slave: libc::c_int = -1;
        let rc = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
            )
        };
        assert_eq!(rc, 0, "openpty failed");

        // Stamp distinctive original settings so restoration is provable: ECHO
        // and ICANON on, and VMIN/VTIME at values the masked path overwrites.
        let mut original: libc::termios = unsafe { std::mem::zeroed() };
        assert_eq!(unsafe { libc::tcgetattr(slave, &mut original) }, 0);
        original.c_lflag |= libc::ECHO | libc::ICANON | libc::ISIG;
        original.c_cc[libc::VMIN] = 7;
        original.c_cc[libc::VTIME] = 13;
        assert_eq!(unsafe { libc::tcsetattr(slave, libc::TCSANOW, &original) }, 0);

        // A parent-held slave fd to read the termios back after the child exits.
        let probe = unsafe { libc::dup(slave) };
        assert!(probe >= 0, "dup slave failed");

        // Readiness pipe: the child writes one byte once masked mode is live.
        let mut pipe_fds = [0 as libc::c_int; 2];
        assert_eq!(unsafe { libc::pipe(pipe_fds.as_mut_ptr()) }, 0, "pipe failed");
        let (ready_read, ready_write) = (pipe_fds[0], pipe_fds[1]);

        let exe = std::env::current_exe().expect("current_exe");
        let slave_stdin = unsafe { OwnedFd::from_raw_fd(slave) };
        let mut cmd = Command::new(exe);
        cmd.args(["setup::tests::pty_child_entry", "--exact", "--test-threads=1", "-q"]);
        cmd.env("VOISU_MASKED_PTY_CHILD", mode);
        cmd.stdin(Stdio::from(slave_stdin));
        cmd.stdout(Stdio::null());
        cmd.stderr(Stdio::null());
        // SAFETY: only async-signal-safe libc calls between fork and exec.
        unsafe {
            cmd.pre_exec(move || {
                // No core dumps from the SIGQUIT path.
                let limit = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
                libc::setrlimit(libc::RLIMIT_CORE, &limit);
                // Wire the readiness pipe write-end to fd 3 for the child.
                if libc::dup2(ready_write, 3) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = cmd.spawn().expect("spawn pty child");

        // Parent drops the write-end so a child that dies early yields EOF on
        // the read-end rather than blocking the parent forever.
        unsafe { libc::close(ready_write) };
        PtyChild { child, master, probe, ready: ready_read }
    }

    fn termios_of(fd: libc::c_int) -> libc::termios {
        let mut termios: libc::termios = unsafe { std::mem::zeroed() };
        assert_eq!(unsafe { libc::tcgetattr(fd, &mut termios) }, 0, "tcgetattr failed");
        termios
    }

    fn write_all_fd(fd: libc::c_int, data: &[u8]) {
        let mut offset = 0;
        while offset < data.len() {
            let written = unsafe {
                libc::write(
                    fd,
                    data[offset..].as_ptr() as *const libc::c_void,
                    data.len() - offset,
                )
            };
            assert!(written > 0, "write to pty failed");
            offset += written as usize;
        }
    }

    /// Blocks until the child signals masked mode is live. Returns false if the
    /// pipe reached EOF first (the child died before getting there).
    fn wait_ready(fd: libc::c_int) -> bool {
        let mut byte = [0u8; 1];
        let read = unsafe { libc::read(fd, byte.as_mut_ptr() as *mut libc::c_void, 1) };
        read == 1
    }

    fn assert_terminal_restored(termios: &libc::termios, context: &str) {
        assert_ne!(termios.c_lflag & libc::ECHO, 0, "ECHO restored ({context})");
        assert_ne!(termios.c_lflag & libc::ICANON, 0, "ICANON restored ({context})");
        assert_eq!(termios.c_cc[libc::VMIN], 7, "VMIN restored ({context})");
        assert_eq!(termios.c_cc[libc::VTIME], 13, "VTIME restored ({context})");
    }

    fn close_pty(pty: &PtyChild) {
        unsafe {
            libc::close(pty.master);
            libc::close(pty.probe);
            libc::close(pty.ready);
        }
    }

    #[test]
    fn pty_normal_return_restores_terminal_state() {
        let mut pty = spawn_pty_child("normal");
        assert!(wait_ready(pty.ready), "child never reached masked mode");
        // Complete the read; the child returns and EchoGuard::drop restores.
        write_all_fd(pty.master, b"hello\n");
        let status = pty.child.wait().expect("wait child");
        assert!(status.success(), "child exit status: {status:?}");
        assert_terminal_restored(&termios_of(pty.probe), "normal return");
        close_pty(&pty);
    }

    fn run_signal_restoration(signal: libc::c_int, name: &str) {
        use std::os::unix::process::ExitStatusExt;
        let mut pty = spawn_pty_child("signal");
        assert!(wait_ready(pty.ready), "child never reached masked mode ({name})");
        // Interrupt while the child is parked in the masked read.
        let killed = unsafe { libc::kill(pty.child.id() as libc::pid_t, signal) };
        assert_eq!(killed, 0, "kill {name} failed");
        let status = pty.child.wait().expect("wait child");
        assert_eq!(status.signal(), Some(signal), "child should die from {name}: {status:?}");
        // The async-signal-safe handler must have restored the tty before death.
        assert_terminal_restored(&termios_of(pty.probe), name);
        close_pty(&pty);
    }

    #[test]
    fn pty_sigint_mid_prompt_restores_terminal_state() {
        run_signal_restoration(libc::SIGINT, "SIGINT");
    }

    #[test]
    fn pty_sigquit_mid_prompt_restores_terminal_state() {
        run_signal_restoration(libc::SIGQUIT, "SIGQUIT");
    }
}
