# Voisu on Linux

Fedora, Arch, Debian and Ubuntu, on KDE Plasma, GNOME, Hyprland and X11 desktops. Voisu runs as systemd **user** services with the `voisu` terminal interface and `voisu` commands. Transcription is Cloud (Deepgram and Groq), or Local on your own GPU or CPU.

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

**Debian / Ubuntu (apt)**, for Ubuntu 26.04 LTS, amd64:

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
- **Settings** covers Engine, Delivery, Writing, Layout, Tone, Deepgram and your keys. Each choice is explained beside it, and Settings offers a restart when the service needs one.
- **Dictionary** holds the words Voisu should spell your way. `a` adds, `d` removes.
- **History** lists recent Recordings. `c` copies one.

If nothing is set up yet, `voisu` opens on the setup screens: Cloud or Local, your Groq and Deepgram keys (checked as you save them; paste with `Ctrl+V`), and the Trigger Key. `Esc` leaves setup, and Home offers it again. `voisu setup` opens the same screens directly.

Keys pause while you dictate, so words that land in the terminal never trigger an action.

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
| Debian / Ubuntu apt | `sudo apt-get update`, then `sudo apt-get install --only-upgrade voisu` |

```sh
voisu update --check     # installed version vs the latest GitHub Release; changes nothing
voisu update --dry-run   # print the commands without running them
voisu update
```

A binary no package owns (for example a `cargo` build) is not updated.

## Update check

Once a day Voisu downloads a small version file, `latest-linux.json`, from GitHub Releases, so it can show *Update available* on the interface's Home screen and in `voisu status`. Voisu sends no identifier, no settings and no usage; the request carries only Voisu's version in its user agent. GitHub sees it like any other download, with your IP address and the time. GitHub publicly counts downloads of each release file, and that count is how Voisu estimates how many installs are active. `voisu status` never contacts the network; it reads the last answer.

Turn it off with `voisu update-check off`, or *Settings → Update check* in the interface. Voisu then never checks by itself. `voisu update --check` still works when you ask.

## Star prompt

After 20 dictations spread over at least 3 days, Voisu sends one desktop notification asking whether you'd star it on GitHub, and the interface's Home screen shows *Enjoying Voisu?* with a key to star and a key to close. Closing it hides it for 30 days; it then comes back once, with no second notification, and a second close hides it for good. Closing or ignoring the notification changes nothing. Starring from anywhere ends it for good. The count lives only on this computer and is never sent anywhere.

`voisu star` and *Settings → Star on GitHub* open the GitHub page at any time. Turn the request off with `voisu star-prompt off`, or *Settings → Star prompt* in the interface; the Settings entry stays.

## Trigger Key by desktop

- **KDE Plasma and GNOME:** a system dialog appears the first time the daemon starts. Choose a Trigger Key there, and it persists.
- **Hyprland:** there is no Global Shortcuts dialog. `voisu setup` installs a compositor bind: Caps Lock (`code:66`) after a default-yes prompt, or Right Alt (`code:108`) if you decline or Caps Lock is already bound. It never auto-installs Left Alt and never overwrites an exact unmanaged binding. Caps Lock as the Trigger Key also disables lock-toggle on that key.

  Hyprland's portal has no RemoteDesktop interface, so Paste puts the Transcript on the clipboard and asks Hyprland to press the paste shortcut from your config:

  ```sh
  voisu delivery paste
  ```

- **Cinnamon and X11** (Linux Mint, Ubuntu on X11): X11 has no portal dialog, so bind the key yourself. In *System Settings → Keyboard → Shortcuts → Custom Shortcuts*, add a shortcut that runs `voisu toggle`.

  The X11 clipboard backend is `xclip`. The `.deb` depends on `wl-clipboard | xclip`, so either satisfies it. On other distributions, install `xclip` if `voisu doctor` asks. Feedback arrives as desktop notifications rather than the on-screen Overlay, which needs Wayland layer-shell.

If the Trigger Key does not respond, run `voisu doctor`. It reports a portal without a usable GlobalShortcuts interface. Add `--verbose` for the reasoning behind each check.

## Local transcription (Consented Install)

`voisu setup` shows Cloud and Local on one screen. Local recommends a model for your computer, with development evidence from one host and one speaker. Cloud stays selected until you pick Local. Local is English only. Hinglish stays on Cloud.

- **NVIDIA GPU with at least 3584 MiB VRAM:** `distil-large-v3.5-int8` on the GPU (about 2.98 GB, MIT licence).
- **Any other computer:** `parakeet-tdt-0.6b-v3` on CPU (about 770 MB, CC-BY-4.0 licence, no GPU needed).
- If the GPU probe fails, setup says "GPU status unknown" and shows the CPU model. It never switches silently.

Nothing downloads until you agree to the size, the licence, the English-only limit, and that this is development evidence rather than a Bakeoff Winner. Once installed, the model runs offline in a sandboxed worker. Local has no automatic GPU, CPU or Cloud fallback, and it fails closed until the model is Ready.

Remove it with `voisu local remove`, or `voisu local remove --yes` without a terminal.

## Ubuntu GNOME troubleshooting

- **Voisu is not running after login.** The daemon waits up to 20 seconds for GNOME to export `WAYLAND_DISPLAY` and `DISPLAY`. If `voisu service status` still shows inactive or failed, run `voisu service restart` once the desktop is up, and check `journalctl --user -u voisu.service`.
- **Paste goes to whichever window has focus.** On GNOME, Voisu cannot check which window has focus. To paste by hand, run `voisu delivery copy`.
- **The Overlay is the GNOME Shell extension `overlay@voisu.app`.** `voisu-overlay.service` (the GTK observer) being inactive is expected there.
- **Paste uses the RemoteDesktop portal and libei.** Unless the compositor's input seat offers text input, Voisu sends a Ctrl+V keystroke. A window that pastes with a different shortcut (a terminal using Ctrl+Shift+V) receives nothing. In that case, paste manually. The Transcript is on the clipboard.
- **Reset Voisu's RemoteDesktop grant.** This clears only the grant Voisu saved itself. GNOME may ask again when the daemon restarts or at the next Delivery. A permission GNOME remembers on its own is not cleared by this.

  ```sh
  rm ~/.local/state/voisu/remote-desktop.restore-token
  systemctl --user restart voisu.service
  ```

---

[Back to the README](../README.md)
