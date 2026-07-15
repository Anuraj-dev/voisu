# 02 — Verify Fedora readiness and store cloud credentials safely

**Parent:** [Build reliable cloud-first dictation for Fedora Wayland](../../voisu-spec/issues/01-fedora-cloud-dictation.md)

**What to build:** A Fedora setup and readiness workflow that verifies required
desktop capabilities, stores cloud credentials securely, and proves provider
authentication before a real Recording.

**Blocked by:** 01 — Prove the daemon lifecycle through the public CLI.

**Status:** ready-for-agent

- [ ] Readiness reports PipeWire, microphone, portal, clipboard, secret-storage, and daemon results as actionable PASS/WARN/FAIL outcomes.
- [ ] The user can store and replace Groq and Deepgram credentials through desktop secret storage.
- [ ] Provider authentication can be verified independently without retaining response content.
- [ ] Credentials never appear in CLI output, logs, errors, or diagnostic exports.
- [ ] Denied or unavailable secret storage produces an explicit supported fallback rather than silently writing plaintext.
- [ ] Standard tests use controlled desktop and provider boundaries without real credentials.

