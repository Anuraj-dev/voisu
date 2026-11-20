# Voisu on Windows

Windows 10 and 11, x64. A tray app with a settings window, Cloud transcription (Deepgram and Groq).

1. Download **[Voisu-Setup-x64.exe](https://github.com/Anuraj-dev/voisu/releases/latest/download/Voisu-Setup-x64.exe)** and run it. It installs for your user account, with no admin rights needed.
2. On first launch, paste your Groq and Deepgram keys and choose *Save and start*.
3. Hold `Ctrl`+`Win`, speak, and let go.

The installer isn't code-signed yet, so SmartScreen may warn on first run. Choose *More info*, then *Run anyway*.

- **Settings** (tray, then *Settings…*): provider keys, Trigger Key, *Paste automatically* (off means clipboard only), overlay position (Move or Reset), boost words, start at login, and the last Transcript with a Copy button.
- **Trigger Key** defaults to `Ctrl`+`Win`. Presets: `Ctrl`+`` ` ``, Right `Ctrl`, Caps Lock, or `Ctrl`+`Shift`+`Space`.
- **Tray menu**: *Preview overlay*, *Test paste (in 3 s)*, *Start at login*, *Update to v…*, *Quit Voisu*. Voisu updates itself from GitHub Releases.
- **Delivery**: text is pasted into the window that had focus when you started, even if you clicked away. Your clipboard is restored afterwards. Terminals get their own paste chord.
- **Pages**: the window has a sidebar with Home, Dictation, Words, API keys, General and What's new. *Informal tone* (Dictation) uses chat abbreviations like btw and drops the final full stop.
- **Overlay**: a small pill with sound bars shows while you dictate, at the bottom centre by default. *Move overlay* (Dictation) lets you drag it anywhere; click ✓ on the pill or press Enter to save, ✗ or Esc to cancel. *Reset position* puts it back.
- **Recording**: the microphone starts faster, and letting go of the Trigger Key or pressing `Esc` works even while it is still starting.
- **Updates**: when an update is ready, a banner offers to restart and install it. After an update, Voisu shows what changed, and *What's new* lists every release.
- **Star prompt**: after steady use, Voisu may ask for a GitHub star. Turn it off with *Star prompt* on the General page.

Windows is Cloud-only for now.

---

[Back to the README](../README.md)
