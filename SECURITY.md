# Security Policy

## Reporting a vulnerability

Please report security issues privately through GitHub's private vulnerability
reporting on the [`Anuraj-Dev/voisu`](https://github.com/Anuraj-Dev/voisu)
repository: **Security tab → Report a vulnerability**. There is no dedicated
security email. Do not open a public issue for a suspected vulnerability.

We will acknowledge your report and keep you updated on remediation.

## Security surface

- **Provider API keys** are stored in the Secret Service keyring via
  `secret-tool` (Secret Service / `libsecret`). They are never written to disk
  in plaintext by Voisu.
- **Audio** is sent to the user's chosen cloud provider (Groq or Deepgram) over
  TLS, and only during an active Recording. Voisu is cloud-first by design;
  transcription happens at the provider you select.
- **Logs and diagnostics are local-only.** Nothing is uploaded unless the user
  explicitly exports it.
- **Release packages** are built from tagged commits through the CI release
  pipeline. The apt repository and its packages are GPG-signed; the signing key
  fingerprint is published in `README.md` and `packaging/apt/README.md`.
- **The systemd user unit runs sandboxed** (`ProtectSystem=strict` and related
  hardening directives), and the daemon uses desktop portals rather than
  privileged input-device or `uinput` access.

## Scope

Voisu runs as an unprivileged systemd **user** service. It does not require root
and does not request raw input-device access on the normal supported path.
