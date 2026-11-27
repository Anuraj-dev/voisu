# Security Policy

## Reporting a vulnerability

Please report security issues privately through GitHub's private vulnerability
reporting on the [`Anuraj-Dev/voisu`](https://github.com/Anuraj-Dev/voisu)
repository: **Security tab → Report a vulnerability**. There is no dedicated
security email. Do not open a public issue for a suspected vulnerability.

We will acknowledge your report and keep you updated on remediation.

## Security surface

- **Provider API keys on Linux** are stored in the Secret Service keyring via
  `secret-tool` (`libsecret`). If storing a key there fails, Voisu warns you and
  saves it in a plaintext file, mode `0600`, at
  `$XDG_CONFIG_HOME/voisu/credentials`.
- **Provider API keys on Windows and macOS** are stored in Credential Manager and
  the Keychain.
- **Cloud mode** sends data to Groq and Deepgram over TLS. Deepgram receives
  audio while you record (unless you turn it off, or have no Deepgram key). Groq
  receives the Recording's audio for transcription, and Transcript text for
  reconciliation and cleanup. A Recording past the full-audio limit reaches Groq
  in chunks while you are still recording; otherwise Groq gets it at stop.
- **Local mode** (Linux and Windows, opt-in) transcribes on your computer. The
  Recording's audio and text are not sent to a transcription provider.
- **Logs and diagnostics are local-only.** Nothing is uploaded unless the user
  explicitly exports it.
- **Release packages** are built from tagged commits through the CI release
  pipeline. The apt repository signs its release metadata, which covers the
  package hashes. The signing key fingerprint is published in
  [`packaging/apt/README.md`](packaging/apt/README.md) and
  [`docs/linux.md`](docs/linux.md).
- **The Linux systemd user unit runs sandboxed** (`ProtectSystem=strict` and
  related hardening directives).
- **The Trigger Key never needs raw input-device or `uinput` access** on Linux.
  Fedora KDE Plasma and GNOME use the Global Shortcuts portal, Ubuntu 26.04 GNOME
  uses Voisu's Shell extension, and Hyprland setup adds a binding to your
  Hyprland config.

## Scope

On Linux, Voisu runs as an unprivileged systemd **user** service. It does not
require root and does not request raw input-device access on the normal supported
path.
