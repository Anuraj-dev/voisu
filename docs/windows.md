# Voisu on Windows

Windows 10 and 11, x64. A tray app with a settings window, Cloud transcription (Deepgram and Groq).

1. Download **[Voisu-Setup-x64.exe](https://github.com/Anuraj-dev/voisu/releases/latest/download/Voisu-Setup-x64.exe)** and run it. It installs for your user account, with no admin rights needed.
2. On first launch, paste your Groq and Deepgram keys and choose *Save and start*.
3. Hold `Ctrl`+`Win`, speak, and let go.

The installer isn't code-signed yet, so SmartScreen may warn on first run. Choose *More info*, then *Run anyway*.

- **Settings** (tray, then *Settings…*): provider keys, Trigger Key, *Paste automatically* (off means clipboard only), overlay size, boost words, start at login, and the last Transcript with a Copy button.
- **Trigger Key** defaults to `Ctrl`+`Win`. Presets: `Ctrl`+`` ` ``, Right `Ctrl`, Caps Lock, or `Ctrl`+`Shift`+`Space`.
- **Tray menu**: *Preview overlay*, *Test paste (in 3 s)*, *Start at login*, *Update to v…*, *Quit Voisu*. Voisu updates itself from GitHub Releases.
- **Delivery**: text is pasted into the window that had focus when you started, even if you clicked away. Your clipboard is restored afterwards. Terminals get their own paste chord.

Windows is Cloud-only for now.

---

[Back to the README](../README.md)
