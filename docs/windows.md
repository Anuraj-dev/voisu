# Voisu on Windows

Windows 10 and 11, x64. A tray app with a settings window. Cloud transcription (Deepgram and Groq) by default, or opt-in Local transcription on your own PC.

1. Download **[Voisu-Setup-x64.exe](https://github.com/Anuraj-dev/voisu/releases/latest/download/Voisu-Setup-x64.exe)** and run it. It installs for your user account, with no admin rights needed.
2. On first launch, paste your Groq and Deepgram keys and choose *Save and start*.
3. Hold `Ctrl`+`Win`, speak, and let go.

The installer isn't code-signed yet, so SmartScreen may warn on first run. Choose *More info*, then *Run anyway*.

- **Settings** (tray, then *Settings…*): provider keys, Trigger Key, *Paste automatically* (off means clipboard only), overlay position (Move or Reset), boost words, start at login, and the last Transcript with a Copy button.
- **Trigger Key** defaults to `Ctrl`+`Win`. Presets: `Ctrl`+`` ` ``, Right `Ctrl`, Caps Lock, or `Ctrl`+`Shift`+`Space`.
- **Tray menu**: *Preview overlay*, *Test paste (in 3 s)*, *Start at login*, *Update to v…*, *Quit Voisu*. Voisu updates itself from GitHub Releases.
- **Delivery**: text is pasted into the window that had focus when you started, even if you clicked away. The Transcript stays on the clipboard so you can paste it again, as on Linux. Terminals get their own paste chord.
- **Pages**: the window has a sidebar with Home, Dictation, Words, API keys, General and What's new. *Informal tone* (Dictation) uses chat abbreviations like btw and drops the final full stop.
- **Overlay**: a small pill with sound bars shows while you dictate, at the bottom centre by default. *Move overlay* (Dictation) lets you drag it anywhere; click ✓ on the pill or press Enter to save, ✗ or Esc to cancel. *Reset position* puts it back.
- **Recording**: the microphone starts faster, and letting go of the Trigger Key or pressing `Esc` works even while it is still starting.
- **Updates**: when an update is ready, a banner offers to restart and install it. After an update, Voisu shows what changed, and *What's new* lists every release.
- **Star prompt**: after steady use, Voisu may ask for a GitHub star. Turn it off with *Star prompt* on the General page.

## Local mode (opt-in)

Local transcribes on this PC with a whisper.cpp model. Nothing you say leaves the machine, and it needs no API keys.

1. Open *Settings…*, then **Dictation**, and find **Transcription**.
2. Choose *Download 265 MB*. Nothing downloads until you do. The model goes to `%LOCALAPPDATA%\Voisu\models`, is checked against its pinned SHA-256 before use, and a download that stops picks up where it left off. *Cancel* stops it.
3. When the download finishes, Voisu switches to **Local**. Choose **Cloud** any time to switch back; *Remove* deletes the model.

What to expect:

- **One model: Fast** (whisper small, English only, 8-bit). Windows runs it on the CPU; there is no GPU support here.
- **Speed**: the Transcript arrives after you stop, not while you speak. The same model takes about 9 seconds for a minute of speech on a recent 4-core CPU under Linux; Windows has not been measured yet and may be slower. Short Recordings take a few seconds, and older or low-power CPUs are slower. Each Recording loads the model again while you speak. Cloud stays faster.
- **Hardware**: an x64 CPU with AVX2 (most Intel and AMD CPUs from 2015 on). An older CPU gets a clear error and stays on Cloud. Windows on ARM is not supported.
- **Spoken punctuation**, boost words and *Informal tone* work as in Cloud.
- **If something goes wrong** (model missing or damaged, the CPU unsupported, the model failing to load, or a Recording that takes too long), the overlay says so, Settings opens on the error, and *Switch to Cloud* takes you back. A Recording that takes longer than its time limit (30 seconds plus its own length) is stopped rather than left hanging. The worker's log is `%LOCALAPPDATA%\Voisu\logs\local-asr.log`.

Local mode is Windows only; the macOS app is Cloud-only.

---

[Back to the README](../README.md)
