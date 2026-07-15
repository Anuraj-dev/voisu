# 09 — Own the daemon through a systemd user service

**Parent:** [Build reliable cloud-first dictation for Fedora Wayland](../../voisu-spec/issues/01-fedora-cloud-dictation.md)

**What to build:** Idempotent user-service management that gives one clear
daemon owner across install, login, restart, upgrade, and removal.

**Blocked by:** 03 — Dictate through PipeWire and Groq into the clipboard.

**Status:** ready-for-agent

- [ ] Service install creates or updates one user unit pointing at the intended Voisu build.
- [ ] Service start, stop, restart, and status report actual systemd ownership and daemon IPC state.
- [ ] Login starts the service only after the user session provides required XDG services.
- [ ] The unit does not bake stale display, Wayland socket, authorization, or checkout paths into its environment.
- [ ] A manually running daemon is detected instead of causing a systemd crash loop.
- [ ] Upgrade replaces the executable safely and removal leaves no enabled stale service or runtime socket.

