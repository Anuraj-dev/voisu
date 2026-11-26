# Voisu Roadmap

Voisu is voice dictation for Linux, Windows and macOS. Press the Trigger Key,
speak, and a validated Transcript is delivered into the focused application.
Primary Linux target: Fedora KDE Plasma on Wayland. Windows ships a tray app that
uses Cloud mode by default with opt-in Local mode; the macOS app is Cloud-only.

This document tracks what works today, what is coming next, and the known
limitations you may hit. For install instructions see the
[project page](https://anuraj-dev.github.io/voisu/) and the
[README](../README.md).

## Now

Shipped and working in the current release:

- **Daemon** (Linux): runs as a systemd **user** service. Manages Recording
  lifecycle, the Trigger Key, provider streaming, and Delivery. Works without GTK.
- **CLI** (`voisu`, Linux): guided setup, diagnostics, recording control,
  provider keys, behaviour settings, a personal pronunciation dictionary, Local
  models and user-service management. See the [command reference](commands.md).
- **Cloud mode**: Groq and Deepgram. Deepgram streams the live Recording; Groq
  receives the full audio at stop (a Recording past the full-audio limit is
  transcribed in overlapping 60 s chunks instead). Voisu reconciles the two
  Source Transcripts within a bounded Provider Deadline, using the valid result
  already available.
- **Local mode** (Linux and Windows): whisper.cpp on your own computer after a
  Consented Install, with an Accurate and a Fast tier. English only.
- **Trigger Key** without raw input-device or privileged `uinput` access: the
  Global Shortcuts portal on Fedora KDE Plasma and GNOME, Voisu's Shell extension
  on Ubuntu 26.04 GNOME, and a compositor binding on Hyprland. See
  [Linux](linux.md) for each desktop.
- **Delivery modes**: `paste` puts the Transcript on the clipboard and pastes it
  into the focused application; `copy` only copies it. If pasting fails, the
  Transcript stays on the clipboard.
- **Overlay**: an on-screen status surface. On Linux it is optional (GTK4, its own
  user service, a separate `voisu-overlay` package on Fedora); the Windows and
  macOS apps host their own.
- **Spoken formatting**: `dash dash`, `slash`, `dot`, `colon slash slash`,
  `quote` / `unquote`, `new line` / `new paragraph`, and spoken
  `first` / `second` / `third` steps become real marks and numbered lines.
  Grocery comma lists and “the first time…” stay ordinary sentences. Voisu
  does not invent a `Goal:` heading from the word “goal”. Extra cloud
  polish stays off in the package.
- **Distribution**:
  - Windows installer and macOS (Apple Silicon) `.dmg` from GitHub Releases
  - Fedora / COPR (`anuraj-dev/voisu`)
  - Arch / AUR (`voisu` from source, `voisu-bin` prebuilt)
  - Ubuntu 26.04 LTS amd64 apt repository (signed, self-hosted on GitHub Pages)
  - Linux `.deb`, tarball and `SHA256SUMS` on GitHub Releases

## Next

- Prove out-of-box Hyprland support with the clean-account
  [release gate](hyprland-release-gate.md).
- Keep improving transcript quality with audio-backed evaluation.

## Known limitations

- **systemd user-service sandboxing.** The daemon unit runs sandboxed
  (`ProtectSystem=strict` and related). Config and state directories are
  provisioned by the unit (`ConfigurationDirectory` / `StateDirectory`); a
  hand-rolled unit that omits these can hit namespace/permission errors on a
  fresh home.
- **Portal behavior differs by desktop:**
  - **Fedora KDE Plasma / GNOME**: a Trigger Key dialog appears on first daemon
    start and your choice persists. Text-insertion Delivery works via
    RemoteDesktop.
  - **Ubuntu 26.04 GNOME**: the Trigger Key belongs to Voisu's Shell extension.
  - **Hyprland**: no shortcut dialog. `voisu setup` installs Caps Lock
    (`code:66`) after a default-yes prompt, or Right Alt (`code:108`) on
    decline or conflict. Its portal implements no RemoteDesktop interface, so
    Paste presses the paste shortcut found in the Hyprland config.
  - **Plain wlroots** portals do not implement GlobalShortcuts, so the Trigger
    Key cannot bind without `xdg-desktop-portal-hyprland`.
  - Run `voisu doctor` if the Trigger Key does not respond; it reports a portal
    without a usable GlobalShortcuts interface.
- **PipeWire capture.** Linux audio capture is via PipeWire; `pw-record` ships in
  the `pipewire-utils` package (Fedora) and `wpctl` in `wireplumber`, which some
  minimal installs lack.
- **COPR builds only from tags.** The COPR source script self-pins to the
  highest `v*` tag, not `HEAD`. Changes merged to `main` do not reach COPR (or
  any channel) until a new version tag is pushed.
- **Test flakiness under contended CI.** The `daemon_cli_lifecycle` and
  `service_cli` integration tests are timing-sensitive and can flake on heavily
  contended runners.
- **macOS is Cloud-only**, and Local models are English only.
