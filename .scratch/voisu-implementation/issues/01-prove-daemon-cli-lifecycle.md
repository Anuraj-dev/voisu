# 01 — Prove the daemon lifecycle through the public CLI

**Parent:** [Build reliable cloud-first dictation for Fedora Wayland](../../voisu-spec/issues/01-fedora-cloud-dictation.md)

**What to build:** A Cargo-based daemon and CLI that demonstrate one complete
Recording through the public commands and versioned Unix IPC using controlled
external boundaries.

**Blocked by:** None — can start immediately.

**Status:** ready-for-agent

- [ ] `voisu status` distinguishes an unavailable daemon from an idle running daemon.
- [ ] `voisu start` begins one Recording and rejects a second start without corrupting state.
- [ ] `voisu stop` completes controlled capture, provider, validation, and Delivery behavior and returns to idle.
- [ ] `voisu toggle` produces the same observable transitions as start followed by stop.
- [ ] Runtime files are isolated under an injected XDG runtime directory in tests.
- [ ] The acceptance test drives only CLI and IPC and records a RED then GREEN cycle.

