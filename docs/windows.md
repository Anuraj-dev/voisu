# Voisu on Windows

Windows 10 and 11, x64. A tray app with a settings window. Cloud transcription (Deepgram and Groq) by default, or opt-in Local transcription on your own PC.

1. Download **[Voisu-Setup-x64.exe](https://github.com/Anuraj-dev/voisu/releases/latest/download/Voisu-Setup-x64.exe)** and run it. It installs for your user account, with no admin rights needed.
2. On first launch, paste your Groq and Deepgram keys and choose *Save and start*. To skip the keys and transcribe on this PC instead, choose *No keys? Transcribe on this PC instead* and follow [Local mode](#local-mode-opt-in).
3. Hold `Ctrl`+`Win`, speak, and let go.

The installer isn't code-signed yet, so SmartScreen may warn on first run. Choose *More info*, then *Run anyway*.

- **Settings** (tray, then *Settings…*): provider keys, Trigger Key, *Paste automatically* (off means clipboard only), overlay position (Move or Reset), boost words, start at login, and the last Transcript with a Copy button.
- **Trigger Key** defaults to `Ctrl`+`Win`. Presets: `Ctrl`+`` ` ``, Right `Ctrl`, Caps Lock, or `Ctrl`+`Shift`+`Space`.
- **Tray menu**: *Preview overlay*, *Test paste (in 3 s)*, *Start at login*, *Update to v…*, *Quit Voisu*. Voisu updates itself from GitHub Releases.
- **Delivery**: text is pasted into the window that had focus when you started, even if you clicked away. The Transcript stays on the clipboard so you can paste it again, as on Linux. Terminals get their own paste chord.
- **Pages**: the window has a sidebar with Home, Dictation, Words, API keys, General and What's new. *Informal tone* (Dictation) uses chat abbreviations like btw and drops the final full stop.
- **Overlay**: a small pill with sound bars shows while you dictate, at the bottom centre by default. *Move overlay* (Dictation) lets you drag it anywhere; click ✓ on the pill or press Enter to save, ✗ or Esc to cancel. *Reset position* puts it back.
- **Recording**: the microphone starts faster, and letting go of the Trigger Key or pressing `Esc` works even while it is still starting.
- **Microphone** (Dictation): choose how long Voisu keeps the microphone on. *Always on* (the default): starts the instant you press; Windows shows the mic icon while Voisu runs. *On for 5 minutes after dictating*: instant for 5 minutes after each dictation. *Only while dictating*: mic on only while you dictate; starts about 0.8 s later. Voisu sends nothing anywhere until you press the Trigger Key: it holds only the last half second of sound, in memory, and keeps overwriting it. Turning microphone access off in Windows privacy settings releases the microphone.
- **Updates**: when an update is ready, a banner offers to restart and install it. After an update, Voisu shows what changed, and *What's new* lists every release.
- **Star prompt**: after steady use, Voisu may ask for a GitHub star. Turn it off with *Star prompt* on the General page.

## Local mode (opt-in)

Local transcribes on this PC with a whisper.cpp model. Nothing you say leaves the machine, and it needs no API keys.

1. Open *Settings…*, then **Dictation**, and find **Transcription**. On first launch, *No keys? Transcribe on this PC instead* (under *Save and start*) opens it without asking for keys, and names your GPU if Voisu found one.
2. **Local model** shows the GPU Voisu found, or that it runs on the processor. Pick **Auto** (the default), **Fast** or **Accurate**, then choose *Download*, which shows the size it will fetch. Nothing downloads until you do. The model goes to `%LOCALAPPDATA%\Voisu\models`, is checked against its pinned SHA-256 before use, and a download that stops picks up where it left off. *Cancel* stops it.
3. When the first download finishes, Voisu switches to **Local**, unless you chose **Cloud** while it downloaded. If you choose **Local** while it downloads, Voisu says *Using Cloud until the model finishes* and switches when it is done. Choose **Cloud** any time to switch back; *Remove* deletes the downloaded models.

What to expect:

- **Two models.** **Fast** (whisper small, 265 MB) suits the processor; **Accurate** (distil-large-v3.5, 818 MB) is more accurate and suits a GPU. **Auto** picks Accurate when Voisu finds a GPU and Fast otherwise. If you already have Fast and Voisu finds a GPU, Fast keeps working until you download Accurate. Both are English only and 8-bit.
- **GPU**: Voisu uses your GPU through Vulkan, which current NVIDIA, AMD and Intel drivers include. Without one, or if the GPU fails, it runs on the processor. Accurate on the processor is much slower; Settings warns you.
- **Speed**: the Transcript arrives after you stop, not while you speak. The model stays loaded while Local is on, so a Recording only waits for the transcription. On a recent 4-core CPU under Linux, Fast takes about 9 seconds for a minute of speech; Windows has not been measured yet and may be slower. A GPU is much faster. Cloud stays fastest without one.
- **Hardware**: an x64 PC. Running on the processor needs an x86-64-v3 CPU (SSE4.2, AVX, AVX2, FMA, F16C and BMI2; most Intel and AMD CPUs from 2015 on); without it, and without a usable GPU, you get a clear error and can switch back to Cloud. Windows on ARM is not supported. The installer carries everything Local needs, so no Visual C++ Redistributable or Vulkan SDK is required.
- **Home** shows how you transcribe: Cloud, or Local with its model and device.
- **Spoken punctuation**, boost words and *Informal tone* work as in Cloud. A silent Recording is skipped.
- **If something goes wrong** (model missing or damaged, the CPU unsupported, the model failing to load, the Local engine stopping repeatedly, or a Recording that takes too long), the overlay says so, Settings opens on the errors you need to act on, and *Switch to Cloud* takes you back. A Recording that takes longer than its time limit (30 seconds plus its own length, and another 90 seconds when the Local engine is still loading its model) is stopped rather than left hanging. Voisu's log is `%LOCALAPPDATA%\Voisu\logs\voisu.*.log` (the GPU found, the model loaded and on which device, each transcription's time); the Local engine's own log is `local-asr.log` beside it.

The macOS app is Cloud-only. Local mode is also available on [Linux](linux.md).

---

[Back to the README](../README.md)
