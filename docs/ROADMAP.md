# Voisu Roadmap

Voisu is a cloud-first Linux desktop dictation daemon. Press the Trigger Key,
speak, and a validated Transcript is delivered into the focused application.
First supported target: Fedora KDE Plasma on Wayland.

This document tracks what works today, what is coming next, and the known
limitations you may hit. For install instructions see the
[project page](https://anuraj-dev.github.io/voisu/) and `README.md`.

## Now

Shipped and working in the current release:

- **Daemon** — runs as a systemd **user** service. Manages Recording lifecycle,
  the Trigger Key, provider streaming, and Delivery. Works without GTK.
- **CLI** (`voisu`) — `voisu setup` guided wizard to validate and store provider
  keys, `voisu delivery <mode>`, `voisu doctor` diagnostics, plus service
  control through `systemctl --user`.
- **Cloud providers** — Groq and Deepgram. Voisu streams to both concurrently
  and reconciles their Source Transcripts within a bounded Provider Deadline,
  using the valid result already available.
- **Trigger Key via desktop portals** — no raw input-device or privileged
  `uinput` access on the normal path. On KDE Plasma / GNOME a system dialog
  appears once and the choice persists.
- **Delivery modes** — direct text insertion into the focused application via
  the RemoteDesktop portal, with clipboard preservation as the fallback
  (`voisu delivery clipboard`).
- **Overlay** — optional, separate on-screen status surface (GTK4) reflecting
  daemon state. Runs as its own user service; on Fedora it is a separate
  `voisu-overlay` package.
- **Spoken formatting** — `dash dash`, `slash`, `dot`, `colon slash slash`,
  `quote` / `unquote`, `new line` / `new paragraph`, and spoken
  `first` / `second` / `third` steps become real marks and numbered lines.
  Grocery comma lists and “the first time…” stay ordinary sentences. Voisu
  does not invent a `Goal:` heading from the word “goal”. Extra cloud
  polish stays off in the package.
- **Packaging** — four channels, all live:
  - Fedora / COPR (`anuraj-dev/voisu`)
  - Arch / AUR (`voisu` from source, `voisu-bin` prebuilt)
  - Debian / Ubuntu apt repo (GPG-signed, self-hosted on GitHub Pages;
    targets Ubuntu 26.04 LTS amd64)
- GitHub Releases (`.deb`, tarball, `SHA256SUMS`)

## Current host status

As of 2026-08-24, the installed `voisu-bin` `0.35.2-1` works on Raja's
Omarchy/Hyprland host. Both user services are enabled and active. `voisu doctor`
passes every check. Caps Lock starts and stops Recordings, and clipboard Delivery
works. No active runtime issue is known on this configured host.

The remaining work is release follow-through and transcript-quality experiments.
It is not a current installation failure.

## Next

- Finish the `v0.35.2` host replay for the section-loss fix: one controlled
  prefix-loss case and one natural multi-cue Recording.
- Implement complete-source selection so a shorter but coherent provider result
  cannot beat a materially fuller safe result.
- Add the host-only Intent Reconstruction path, then run the six-Recording pilot.
  Run the 30–50 Recording trial only if that pilot passes.
- Tune Raja's private vocabulary and use the measured results to decide whether
  Intent Reconstruction should stay experimental or become ordinary behavior.
- Move the working Hyprland service setup into the package/setup flow and run the
  cold-login gate.

## Known limitations

- **systemd user-service sandboxing.** The daemon unit runs sandboxed
  (`ProtectSystem=strict` and related). Config and state directories are
  provisioned by the unit (`ConfigurationDirectory` / `StateDirectory`); a
  hand-rolled unit that omits these can hit namespace/permission errors on a
  fresh home.
- **Portal behavior differs by desktop:**
  - **KDE Plasma / GNOME** — Trigger Key dialog appears on first daemon start
    and persists with a user-chosen key. Text-insertion Delivery works via RemoteDesktop.
  - **Hyprland** — no shortcut dialog. `voisu setup` installs Caps Lock
    (`code:66`) after a default-yes prompt, or Right Alt (`code:108`) on
    decline or conflict. **Type Delivery does not work on Hyprland**: its portal
    implements no RemoteDesktop interface, so setup selects `clipboard`.
  - **Plain wlroots** portals do not implement GlobalShortcuts, so the Trigger
    Key cannot bind without `xdg-desktop-portal-hyprland`.
  - Run `voisu doctor` if the Trigger Key does not respond — it reports a portal
    without a usable GlobalShortcuts interface.
- **PipeWire capture.** Audio capture is via PipeWire; `pw-record` ships in the
  `pipewire-utils` package (Fedora) and `wpctl` in `wireplumber`, which some minimal
  installs lack.
- **COPR builds only from tags.** The COPR source script self-pins to the
  highest `v*` tag, not `HEAD`. Changes merged to `main` do not reach COPR (or
  any channel) until a new version tag is pushed.
- **Test flakiness under contended CI.** The `daemon_cli_lifecycle` and
  `service_cli` integration tests are timing-sensitive and can flake on heavily
  contended runners; work to eliminate the remaining flakes is ongoing.
- **No local/offline transcription.** Transcription happens at the cloud
  provider you select (cloud-first by design).
