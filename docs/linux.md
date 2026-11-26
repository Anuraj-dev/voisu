# Voisu on Linux

Fedora, Arch and Ubuntu 26.04 LTS, on KDE Plasma, GNOME, Hyprland (including Omarchy) and X11 desktops. Voisu runs as systemd **user** services with the `voisu` terminal interface and `voisu` commands. Transcription is Cloud (Deepgram and Groq), or Local on your own GPU or CPU.

Every command is listed in the [command reference](commands.md).

## Install

**Fedora (COPR)**

```sh
sudo dnf copr enable anuraj-dev/voisu
sudo dnf install voisu
```

Fedora KDE Plasma on Wayland is the primary target. GNOME on Wayland works too.

**Arch (AUR)**

```sh
yay -S voisu-bin
```

`voisu-bin` installs prebuilt binaries, so nothing compiles on your machine. To build from source instead, install `voisu` (`yay -S voisu`). The two packages conflict, so pick one.

**Ubuntu (apt)**, for Ubuntu 26.04 LTS, amd64:

```sh
# 1. Add Voisu's signing key.
sudo install -d -m 0755 /etc/apt/keyrings
curl -fsSL https://anuraj-dev.github.io/voisu/voisu-archive-keyring.asc \
  | sudo tee /etc/apt/keyrings/voisu-archive-keyring.asc >/dev/null

# 2. Add the repository.
echo 'deb [signed-by=/etc/apt/keyrings/voisu-archive-keyring.asc arch=amd64] https://anuraj-dev.github.io/voisu stable main' \
  | sudo tee /etc/apt/sources.list.d/voisu.list

# 3. Install.
sudo apt-get update && sudo apt-get install -y voisu
```

Signing key fingerprint: `4149 EE38 68B3 6B60 0759 2966 D08B CFDC 3412 5B28`. For the fingerprint-verified path, see [`packaging/apt/README.md`](../packaging/apt/README.md).

## Start Voisu

Voisu ships as systemd **user** services and does not auto-start. Enable it for your user:

```sh
systemctl --user enable --now voisu.service
voisu                 # opens the interface; a fresh install opens on setup
# Optional on-screen Overlay (on Fedora, install voisu-overlay first):
systemctl --user enable --now voisu-overlay.service
```

## Use the interface

Run `voisu` with no arguments in a terminal. It is a full-screen, keyboard-only interface:

| Keys | Action |
| --- | --- |
| `←` `→` or `1`–`4` | Switch tabs |
| `↑` `↓` | Move |
| `↵` | Act |
| `?` | List every key on the current tab |
| `q` | Quit |

- **Home** shows what matters now: ready to dictate, or the one problem and the single key that fixes it. `d` runs every `voisu doctor` check.
- **Settings** groups its rows into five sections: **Dictation** (Engine, Trigger Key, Delivery, Overlay), **Transcript** (Writing, Layout, Tone), **Cloud** (your keys and Deepgram), **Local** (Local model, device and unload) and **Voisu** (Run setup, Update check, Star prompt, Star on GitHub). Overlay's Move drags the pill anywhere on screen, with ✓ to save and ✗ to cancel; Reset puts it back at the bottom centre. Each choice is explained beside it, and Settings offers a restart when the service needs one.
- **Dictionary** holds the words Voisu should spell your way. `a` adds, `d` removes. Local mode on the Whisper-based model also hands your words to the model as a hint before it transcribes, and corrects by sound: a name it wrote as it heard it ("voice you") becomes your entry ("Voisu") when the entry has two or more syllables. Cloud mode corrects exact spellings only.
- **History** lists recent Recordings. `c` copies one.

If nothing is set up yet, `voisu` opens on the setup screens: Cloud or Local, your Groq and Deepgram keys (checked as you save them; paste with `Ctrl+V`), and the Trigger Key. `Esc` leaves setup, and Home offers it again. `voisu setup` opens the same screens directly.

Keys pause while you dictate, so words that land in the terminal never trigger an action.

### Every Settings row

On the Settings tab, rows sit under section headings in the order below. `↑` `↓` moves from row to row, across sections, and `↵` opens its choices (or turns a switch on and off); `↵` again picks one. Each row has a CLI command that does the same thing.

| Section | Row | What it does | Choices | Command |
| --- | --- | --- | --- | --- |
| Dictation | Engine | Where your speech becomes text | Cloud (needs internet), Local (stays on this computer) | `voisu engine cloud\|local` |
| | Trigger Key | Shows your shortcut. Change it in your desktop (see [Trigger Key by desktop](#trigger-key-by-desktop)) | | `voisu shortcut` |
| | Delivery | How your words reach your window | Paste, Copy (you press Ctrl+V) | `voisu delivery paste\|copy` |
| | Overlay | Where the on-screen pill sits | Move, Reset | `voisu overlay-position move\|reset\|<x,y>` |
| Transcript | Writing | How much Voisu tidies your words | Clean, Exact | `voisu writing clean\|exact` |
| | Layout | How your words are laid out | Plain, Auto, Sections | `voisu layout plain\|auto\|sections` |
| | Tone | How casual your words read | Formal, Informal (btw, no final full stop) | `voisu tone formal\|informal` |
| Cloud | Groq key, Deepgram key | `↵` enters a new key, checked before it is saved | | `voisu key set groq\|deepgram` |
| | Deepgram | Uses Deepgram alongside Groq, for technical words. Off uses Groq alone | On, Off | `voisu deepgram on\|off` |
| Local | Local model | Which Local model runs | Auto, Accurate, Fast | `voisu local-model auto\|accurate\|fast` |
| | Local device | Where the Local model runs | Auto, GPU, CPU | `voisu local-device auto\|gpu\|cpu` |
| | Local unload | When an idle Local model frees its memory | Never, After 30 min, After 1 hour | `voisu local-unload off\|SECONDS` |
| Voisu | Run setup | Walks through your keys, Engine and Trigger Key again. Safe to re-run | | `voisu setup` |
| | Update check | Reads the newest version number from GitHub once a day | On, Off | `voisu update-check on\|off` |
| | Star prompt | Lets Voisu ask for a GitHub star after a few days of use | On, Off | `voisu star-prompt on\|off` |
| | Star on GitHub | Opens Voisu's GitHub page | | `voisu star` |

A few changes apply only when Voisu restarts. The row says so, and `r` restarts Voisu.

### Switch to Local, step by step

These steps are for Linux. Windows has its own opt-in Local mode (see the [Windows guide](windows.md#local-mode-opt-in)); the macOS app uses Cloud.

1. Run `voisu` and open **Settings** (`2`).
2. If Local is already installed, pick **Engine → Local** and you are done. If not, Voisu says *Local isn't ready yet. Choose Run setup to install it.*
3. Pick **Run setup**. On *Where should your speech become text?* choose **Local**.
4. *Install Local?* shows how much Voisu downloads, once. Without a GPU it adds *No GPU found, so it runs on the CPU, more slowly.* Choose **Install** to download, or **Not now** to keep using Cloud. Nothing downloads until you choose Install.
5. *Installing Local* shows the progress, then *Checking it works*. Keep the window open; it can take a few minutes. If the download stops, `↵` tries again and carries on where it stopped.

Setup installs Accurate when it finds a GPU and Fast when it does not.

### Pick the Local model and where it runs

- **Local model:** **Auto** runs Accurate on a GPU and Fast on the CPU. **Accurate** (distil-large-v3.5) is the most accurate, best on a GPU. **Fast** (small.en) is a third of the size and quick on the CPU. Picking a model that is not installed asks first, for example *Download Fast (small.en), 252.6 MiB? y/n*. `y` downloads it, and it replaces the installed model at your next Recording. Pressing `q` during a download asks you to press it again before quitting.
- **Local device:** **Auto** uses your best GPU; without one, Local stops with an error. **GPU** uses a GPU only, never the CPU. **CPU** works anywhere but is several times slower. The change applies the next time the Local model loads.

The same from a script: `voisu local-model fast`, `voisu local-device cpu`, `voisu engine local`. See [Local transcription](#local-transcription-consented-install) for the details.

## Overlay

The Overlay is a small pill with white sound bars that shows while you dictate, then a short result such as *Pasted* or *Copied*. It sits at the bottom centre by default. On KDE Plasma and Hyprland it shows on the monitor you are working on.

To move it, choose *Settings → Overlay → Move* in the interface, or run `voisu overlay-position move`. Drag the pill anywhere, then click ✓ (or press Enter) to save, or ✗ (or Esc) to cancel. *Reset*, or `voisu overlay-position reset`, puts it back at the bottom centre. `voisu overlay-position 0.250,0.100` sets the spot directly, as fractions of the screen across and down.

Moving the pill needs KDE Plasma or Hyprland. On GNOME the Overlay is a Shell extension that stays in place. X11 desktops have no Overlay and use desktop notifications instead.

## Set up from a script or an agent

Every setting the interface changes is also a plain command. Keys are read from standard input, never from the command line:

```sh
voisu key set groq       # paste the key, then Ctrl+D
voisu key set deepgram
voisu engine cloud
voisu delivery paste
voisu service install
voisu doctor            # every check, with the fix for anything that fails
```

A coding agent can install Voisu, change settings and check it with these commands. The [agent install guide](https://anuraj-dev.github.io/voisu/agent-install.md) walks through each desktop.

Your API keys never go through the agent. Type them into `voisu` in a terminal of your own. To diagnose a problem, give the agent `voisu doctor --verbose`, `voisu history --verbose` and `journalctl --user -u voisu.service`. Without a terminal, `voisu setup` runs the step-by-step text version.

## Update

`voisu update` upgrades Voisu through the package manager that installed it, then restarts the running Voisu user services:

| Installed from | `voisu update` runs |
| --- | --- |
| AUR `voisu-bin` | `yay -S --needed voisu-bin` (or `paru`). Without an AUR helper it prints the manual `makepkg` steps |
| AUR `voisu` (source) | Nothing by default. It explains that this package compiles on your machine. `voisu update --switch-to-bin` replaces it with `voisu-bin` |
| Fedora COPR | `sudo dnf upgrade --refresh voisu` |
| Ubuntu apt | `sudo apt-get update`, then `sudo apt-get install --only-upgrade voisu` |

```sh
voisu update --check     # installed version vs the latest GitHub Release; changes nothing
voisu update --dry-run   # print the commands without running them
voisu update
```

A binary no package owns (for example a `cargo` build) is not updated.

## Update check

Once a day Voisu downloads a small version file, `latest-linux.json`, from GitHub Releases, so it can show *Update available* on the interface's Home screen and in `voisu status`. Voisu sends no identifier, no settings and no usage; the request carries only Voisu's version in its user agent. GitHub sees it like any other download, with your IP address and the time. GitHub publicly counts downloads of each release file, and that count is how Voisu estimates how many installs are active. `voisu status` never contacts the network; it reads the last answer.

Turn it off with `voisu update-check off`, or *Settings → Update check* in the interface. Voisu then never checks by itself. A value Voisu cannot read, such as `update_check = "no"`, also keeps it off. `voisu update --check` still works when you ask.

## Star prompt

After 20 dictations spread over at least 3 days, Voisu sends one desktop notification asking whether you'd star it on GitHub, and the interface's Home screen shows *Enjoying Voisu?* with a key to star and a key to close. Closing it hides it for 30 days; it then comes back once, with no second notification, and a second close hides it for good. Closing or ignoring the notification changes nothing. Starring from anywhere ends it for good. The count lives only on this computer and is never sent anywhere.

`voisu star` and *Settings → Star on GitHub* open the GitHub page at any time. Turn the request off with `voisu star-prompt off`, or *Settings → Star prompt* in the interface; the Settings entry stays.

## Trigger Key by desktop

- **KDE Plasma and GNOME:** a system dialog appears the first time the daemon starts. Choose a Trigger Key there, and it persists.
- **Hyprland and Omarchy:** there is no Global Shortcuts dialog. `voisu setup` installs a compositor bind: Caps Lock (`code:66`) after a default-yes prompt, or Right Alt (`code:108`) if you decline or Caps Lock is already bound. It never auto-installs Left Alt and never overwrites an exact unmanaged binding. Caps Lock as the Trigger Key also disables lock-toggle on that key.

  Hyprland's portal has no RemoteDesktop interface, so Paste puts the Transcript on the clipboard and asks Hyprland to press the paste shortcut from your config:

  ```sh
  voisu delivery paste
  ```

- **Cinnamon and X11** (Linux Mint, Ubuntu on X11): X11 has no portal dialog, so bind the key yourself. In *System Settings → Keyboard → Shortcuts → Custom Shortcuts*, add a shortcut that runs `voisu toggle`.

  The X11 clipboard backend is `xclip`. The `.deb` depends on `wl-clipboard | xclip`, so either satisfies it. On other distributions, install `xclip` if `voisu doctor` asks. Feedback arrives as desktop notifications rather than the on-screen Overlay, which needs Wayland layer-shell.

If the Trigger Key does not respond, run `voisu doctor`. It reports a portal without a usable GlobalShortcuts interface. Add `--verbose` for the reasoning behind each check.

## Local transcription (Consented Install)

`voisu setup` and the interface's setup show Cloud and Local on one screen. Cloud stays selected until you pick Local. Local is English only. Hinglish stays on Cloud.

Local installs one of two whisper.cpp models (MIT licence). See [Local model](#local-model) for how to choose. Where it runs follows `voisu local-device` (*Settings → Local device*):

- **`auto` (default) or `gpu`:** on the GPU through Vulkan, NVIDIA, AMD or Intel. Setup checks the GPU before it downloads anything.
- **`auto` with no usable GPU:** setup notes that it runs on the CPU, more slowly, and offers the Fast model. Installing sets `voisu local-device cpu`.
- **`gpu` with no usable GPU:** setup stops before the download and says how to choose the CPU. It never switches silently.
- **`cpu`:** on the processor.

Nothing downloads until you agree. The prompt shows the size and says when no GPU was found. This is development evidence, not a Bakeoff Winner. Installs of the earlier `distil-large-v3.5-int8` and `parakeet-tdt-0.6b-v3` models keep working; setup no longer offers them. Once installed, the model runs offline in a sandboxed worker. Local has no automatic GPU, CPU or Cloud fallback, and it fails closed until the model is Ready.

Remove it with `voisu local remove`, or `voisu local remove --yes` without a terminal.

### Local model

Local has two models. Voisu shows them by name; `voisu doctor` adds the model id and its pinned checksum.

- **Accurate:** distil-large-v3.5, about 780 MB. The most accurate, and recommended on a GPU. On the CPU it is much slower.
- **Fast:** small.en, about 265 MB. Made for computers without a GPU, a little less accurate.

`voisu local-model` shows the setting and the model it picks. In the interface it is **Local model** under Settings. `auto` (default) picks Accurate on a GPU and Fast on the CPU. `voisu local-model accurate` or `fast` picks one. `voisu status` and the interface's Home show the model in use, for example `Fast (small.en, CPU)`.

One Local model is installed at a time. Choosing the installed one changes nothing else. Choosing the other shows its size and downloads it only after you agree. Without a terminal, `voisu local-model` never downloads. The switch takes effect once Voisu is idle, at your next Recording; a Recording in progress finishes on the old model. Voisu then removes the old model. With `local_device = auto`, Voisu checks for a GPU before the download, and saves `local_device = cpu` if it finds none. Switching back downloads the first model again.

### Local unload

By default the Local model stays loaded, so every Recording starts at once. A loaded model holds about 1-2 GB of GPU memory or RAM. If you need that memory back, set Voisu to unload the model after an idle spell; the first Recording after an unload then waits about 15 seconds while the model reloads. If the model is still loading when you stop, Voisu waits for it; a load that does not finish within 60 seconds fails the Recording, with no Cloud fallback. A Recording in progress is never unloaded.

`voisu local-unload` shows the setting. `voisu local-unload 1800` unloads after 30 minutes (30 to 86400 seconds), and `voisu local-unload off` keeps the model loaded (the default). A model already unloaded stays unloaded until your next Recording. In the interface it is **Local unload** under Settings: Never, after 30 minutes, or after 1 hour. A change applies within 30 seconds, without a restart.

## Ubuntu GNOME troubleshooting

- **Voisu is not running after login.** The daemon waits up to 20 seconds for GNOME to export `WAYLAND_DISPLAY` and `DISPLAY`. If `voisu service status` still shows inactive or failed, run `voisu service restart` once the desktop is up, and check `journalctl --user -u voisu.service`.
- **Paste goes to whichever window has focus.** On GNOME, Voisu cannot check which window has focus. To paste by hand, run `voisu delivery copy`.
- **The Overlay is the GNOME Shell extension `overlay@voisu.app`.** `voisu-overlay.service` (the GTK observer) being inactive is expected there. The extension's pill can't be moved: `voisu overlay-position move` and Settings' Move say so instead.
- **Paste uses the RemoteDesktop portal and libei.** Unless the compositor's input seat offers text input, Voisu sends a Ctrl+V keystroke. A window that pastes with a different shortcut (a terminal using Ctrl+Shift+V) receives nothing. In that case, paste manually. The Transcript is on the clipboard.
- **Reset Voisu's RemoteDesktop grant.** This clears only the grant Voisu saved itself. GNOME may ask again when the daemon restarts or at the next Delivery. A permission GNOME remembers on its own is not cleared by this.

  ```sh
  rm ~/.local/state/voisu/remote-desktop.restore-token
  systemctl --user restart voisu.service
  ```

---

[Back to the README](../README.md)
