# Ship Windows as a cloud-only Tauri app

Raja decided on 2026-11-02 that Voisu ships on Windows without Local mode. A Windows Recording goes to Deepgram and Groq with the user's own keys, the same Cloud path as Linux. The `local_*` code stays Linux-only.

The Windows shell is Tauri. It owns the tray, the settings and setup window, the Overlay window, the installer and the updater. Tauri uses the system WebView2 and bundles no Chromium, so ADR 0007 already names it as the only acceptable web-tech surface. That ADR's GTK layer-shell argument is a Wayland constraint and does not apply to Windows. Linux keeps the GTK Overlay. The Recording → Transcript pipeline is not forked. Windows implements the `voisu-core` seams (`AudioCapture`, `DeliveryAdapter`, `SecretStore`, Trigger Key) and reuses the shared cloud crate. Windows runs one process: ADR 0003's daemon/Overlay split protects a GTK client on Wayland, and Windows has no such risk.

The default Trigger Key is Ctrl+Win: hold to talk, double-tap for hands-free, Esc to cancel. A modifier-only chord with key-up needs a `WH_KEYBOARD_LL` hook. ADR 0004 bans raw device access on Linux, not this documented user-mode Windows API. The hook callback only enqueues, because Windows silently removes a hook that exceeds `LowLevelHooksTimeout`.

Distribution is free. The installer is a per-user NSIS setup and carries no Authenticode signature, so SmartScreen warns until reputation builds; the install page explains the "Run anyway" step. Updates are still signed, with the free Tauri updater key. Rejected: paid Azure Artifact Signing, which is not free; SignPath OSS, which needs project recognition Voisu does not yet have; a pure-Rust native UI, which needs more installer and updater work; and a terminal setup, which is foreign to Windows users.

Consequences: a keyboard hook in an unsigned binary invites antivirus false positives, so each release is submitted for Defender review. Real-host behavior (hotkeys, paste, microphone) is proven only on Raja's Windows 11 machine, not in CI. Plan: `internal/specs/2026-11-02-windows-cloud-port.md`.
