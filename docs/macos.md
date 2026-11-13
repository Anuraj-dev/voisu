# Voisu on macOS

A menu bar app with a settings window, Cloud transcription (Deepgram and Groq).

Apple Silicon (M1 or newer), macOS 13 Ventura or newer.

1. Download **[Voisu-macOS-arm64.dmg](https://github.com/Anuraj-dev/voisu/releases/latest/download/Voisu-macOS-arm64.dmg)**, open it and drag Voisu onto *Applications*.
2. Open Voisu. It isn't notarized yet, so macOS blocks the first open: choose *Done* (not *Move to Trash*), then *System Settings → Privacy & Security → Open Anyway*. You do this once.
3. Allow **Microphone**, **Accessibility** (so Voisu can paste) and **Input Monitoring** (for the Trigger Key) in *Privacy & Security*, then quit and reopen Voisu once. Its window lists any permission still missing.
4. Paste your Groq and Deepgram keys and choose *Save and start*.
5. Hold `Control`+`Option`, speak, and let go.

- Voisu lives in the menu bar, not the Dock. Keys are stored in the macOS Keychain.
- Without Accessibility, Voisu copies the text and shows *Copied · ⌘V* so you can paste it yourself.
- Voisu updates itself from GitHub Releases, and your permissions carry over.
- macOS is Cloud-only. The full walkthrough is the [Mac setup guide](https://anuraj-dev.github.io/voisu/docs/macos-setup.html).

---

[Back to the README](../README.md)
