# 03 — Dictate through PipeWire and Groq into the clipboard

**Parent:** [Build reliable cloud-first dictation for Fedora Wayland](../../voisu-spec/issues/01-fedora-cloud-dictation.md)

**What to build:** The first usable Fedora workflow: capture a real microphone
Recording, transcribe it through Groq, validate it, and preserve the final
Transcript on the clipboard.

**Blocked by:** 02 — Verify Fedora readiness and store cloud credentials safely.

**Status:** ready-for-agent

- [ ] Start captures the configured PipeWire microphone without blocking CLI status requests.
- [ ] Stop includes the final audio frames and submits a valid provider audio request.
- [ ] A valid Groq Source Transcript becomes one validated final Transcript on the clipboard.
- [ ] Empty, too-short, silent, and over-deadline Recordings produce distinct observable outcomes.
- [ ] Capture or provider failure returns the daemon to a state that accepts the next Recording.
- [ ] Standard tests use deterministic audio and a local provider server; an opt-in smoke test exercises the real Fedora microphone and Groq.

