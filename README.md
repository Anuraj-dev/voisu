# Voisu

Voice dictation for Linux and Windows, written in Rust. Hold or press a key,
speak, and a clean Transcript lands in the app you were typing in: a reply, a
doc, a terminal. If it can't land there, Voisu keeps it on the clipboard
instead of dropping it.

**Project page, install guide and user docs: https://anuraj-dev.github.io/voisu/**

| | Windows | Linux |
| --- | --- | --- |
| Systems | Windows 10 / 11, x64 | Fedora, Arch, Debian/Ubuntu (KDE Plasma, GNOME, Hyprland, X11 desktops) |
| Trigger Key | Hold `Ctrl`+`Win` (or your own key) to talk; double-tap for hands-free; `Esc` cancels | Press your Trigger Key to start, press again to stop |
| Transcription | Cloud (Deepgram + Groq) | Cloud (Deepgram + Groq), or Local on your own GPU or CPU |
| Runs as | Tray app with a settings window | systemd **user** services, the `voisu` terminal interface, and the `voisu` commands |
| Install | Per-user installer, self-updating | COPR, AUR, signed apt repository |

In Cloud mode, Deepgram streams while you talk and Groq transcribes the whole
Recording when you stop. Voisu reconciles the two into one Transcript. You use
your own Groq and Deepgram API keys: Voisu has no account, server or analytics
of its own, and keys live in your desktop keyring or Windows Credential
Manager. Logs and history stay on your machine.

Spoken punctuation and layout ("new paragraph", "dash dash", "quote …
unquote", "first … second …" lists) are formatted locally on both platforms.

## Windows

Download **[Voisu-Setup-x64.exe](https://github.com/Anuraj-dev/voisu/releases/latest/download/Voisu-Setup-x64.exe)**
and run it. It installs for your user account, with no admin rights needed.
The installer isn't code-signed yet, so SmartScreen may warn on first run:
choose *More info → Run anyway*.

On first launch the settings window opens. Paste your Groq and Deepgram keys
and choose *Validate & save*. Then hold `Ctrl`+`Win`, speak, and let go.

- **Trigger Key:** `Ctrl`+`Win` by default. Or pick one of the presets in
  settings: `Ctrl`+`` ` ``, Right `Ctrl`, Caps Lock or `Ctrl`+`Shift`+`Space`.
- **Settings window** (tray → *Settings…*): provider keys, Trigger Key, *Paste
  automatically* (off = clipboard only), overlay size, boost words, start at
  login, and the last Transcript with a Copy button.
- **Tray menu:** *Preview overlay*, *Test paste (in 3 s)*, *Start at login*,
  *Update to v…*, *Quit Voisu*. Voisu updates itself from GitHub Releases.
- **Delivery:** text is pasted into the window that had focus when you
  started, even if you clicked away, and your clipboard is restored afterwards.
  Terminals get their own paste chord.
- Windows is Cloud-only for now; Local transcription is Linux-only.

## Linux

### Fedora (COPR)

```sh
sudo dnf copr enable anuraj-dev/voisu
sudo dnf install voisu
```

Fedora KDE Plasma on Wayland is the primary Linux target. GNOME on Wayland
works too.

### Arch (AUR)

```sh
yay -S voisu-bin
```

`voisu-bin` installs the prebuilt release binaries, so nothing compiles on your
machine. To build from source instead, install the `voisu` package
(`yay -S voisu`); it compiles Voisu on your machine for every install and
update. The two packages conflict, so pick one.

### Debian / Ubuntu (apt)

Targets **Ubuntu 26.04 LTS, amd64**.

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

Signing key fingerprint: `4149 EE38 68B3 6B60 0759 2966 D08B CFDC 3412 5B28`.
For the fingerprint-verified install path, see
[`packaging/apt/README.md`](packaging/apt/README.md).

### After install

Voisu ships as systemd **user** services and is intentionally not auto-started.
Enable it for your user:

```sh
systemctl --user enable --now voisu.service
voisu         # opens the interface; a fresh install opens straight on setup
# optional on-screen Overlay (on Fedora, install voisu-overlay first):
systemctl --user enable --now voisu-overlay.service
```

### The `voisu` interface

Run `voisu` with no arguments in a terminal to open a full-screen interface.
It is keyboard-only: `←` `→` (or `1`–`4`) switch tabs, `↑` `↓` move, `↵` acts,
`?` lists every key on the current tab, and `q` quits.

- **Home** shows the one thing that matters right now: ready to dictate, or the
  problem and the single key that fixes it (start Voisu, restart it, run setup).
  `d` shows every check `voisu doctor` runs.
- **History** lists recent Recordings; `c` copies one.
- **Dictionary** holds the words Voisu should spell your way; `a` adds, `d`
  removes.
- **Settings** changes Engine, Delivery, Writing, Layout, Deepgram and your
  keys, explains each choice beside it, and offers a restart when the running
  service needs one to pick up a change.

If nothing is set up yet, `voisu` opens straight on the setup screens: Cloud or
Local, your Groq and Deepgram keys (checked as you save them, paste with
`Ctrl+V`), and the Trigger Key. `Esc` leaves setup; Home then offers it again.
`voisu setup` opens the same screens directly.

Keys pause for a moment while you dictate, so words that land in the terminal
never trigger an action.

### Setting up from a script or an agent

Every setting the interface changes is also a plain command, so a script or a
coding agent can set Voisu up end to end without the interface. Keys are read
from standard input, never from the command line:

```sh
printf %s "$GROQ_API_KEY" | voisu key set groq
printf %s "$DEEPGRAM_API_KEY" | voisu key set deepgram
voisu engine cloud
voisu delivery paste
voisu service install
voisu doctor            # every check, with the fix for anything that fails
```

`voisu setup` without a terminal runs the step-by-step text version. When
something goes wrong, `voisu doctor --verbose`, `voisu history --verbose` and
`journalctl --user -u voisu.service` give an agent what it needs to diagnose it.

### Updating

On Linux, `voisu update` upgrades Voisu through the package manager that
installed it, then restarts the Voisu user services that are running:

| Installed from | `voisu update` runs |
| --- | --- |
| AUR `voisu-bin` | `yay -S --needed voisu-bin` (or `paru`); without an AUR helper it prints the manual `makepkg` steps |
| AUR `voisu` (source) | Nothing by default: it explains that this package compiles on your machine. `voisu update --switch-to-bin` replaces it with `voisu-bin` |
| Fedora COPR | `sudo dnf upgrade --refresh voisu` |
| Debian / Ubuntu apt | `sudo apt-get update`, then `sudo apt-get install --only-upgrade voisu` |

```sh
voisu update --check     # installed version vs the latest GitHub Release; changes nothing
voisu update --dry-run   # print the commands without running them
voisu update
```

A binary no package owns (for example a `cargo` build) is not updated; install a
package as described above. Only `voisu update --check` contacts the network
itself (GitHub's release API).

### Trigger Key by desktop

How you pick the Trigger Key depends on your desktop's portal:

- **KDE Plasma / GNOME:** a system dialog appears the first time the daemon
  starts — choose a Trigger Key in that dialog, and it persists.
- **Hyprland:** there is no Global Shortcuts dialog. `voisu setup` installs a
  compositor bind: Caps Lock (`code:66`) after a default-yes prompt, or Right
  Alt (`code:108`) if you decline or Caps Lock is already bound. It does not
  auto-install Left Alt and never overwrites an exact unmanaged binding. Caps
  Lock as the Trigger Key also disables lock-toggle on that key.

  Hyprland's portal implements no RemoteDesktop interface, so Paste puts the
  Transcript on the clipboard and then asks Hyprland to press the paste
  shortcut it found in your config:

  ```sh
  voisu delivery paste
  ```

- **Cinnamon / X11 (Linux Mint, Ubuntu on X11):** X11 sessions have no portal
  GlobalShortcuts dialog, so bind the Trigger Key yourself. In *System Settings
  → Keyboard → Shortcuts → Custom Shortcuts*, add a shortcut that runs:

  ```sh
  voisu toggle
  ```

  On X11 the clipboard backend is `xclip`. The `.deb` depends on
  `wl-clipboard | xclip`, so either satisfies it; on other distributions install
  `xclip` if `voisu doctor` prescribes it. Feedback on X11 arrives as desktop
  notifications rather than the on-screen Overlay, which needs Wayland
  layer-shell.

Run `voisu doctor` if the Trigger Key does not respond — it reports a portal
without a usable GlobalShortcuts interface, and adds `--verbose` for the full
reasoning behind each check.

### Local (Consented Install)

`voisu setup` shows Cloud and Local on one screen. Local shows
**Recommended for this computer: <model>**, with development evidence from one
host and one speaker (not the release gate); Cloud stays selected until you pick
Local. English only; Hinglish stays on Cloud.

- NVIDIA GPU 0 with at least 3584 MiB total VRAM: `distil-large-v3.5-int8` on
  the GPU (about 2.98 GB download, MIT licence).
- Any other computer, including one with a smaller GPU: `parakeet-tdt-0.6b-v3`
  on CPU (about 770 MB download, CC-BY-4.0 licence, no GPU needed).
- If the GPU probe fails, setup says "GPU status unknown" and shows the CPU
  model; it never switches silently.

Nothing is fetched until you agree to the download size, licence, English-only
limits, and that this is development evidence rather than a Bakeoff Winner.
Once installed the model runs offline in a sandboxed worker, with no automatic
GPU, CPU or Cloud fallback; Local fails closed until the model is Ready. Decline
keeps Local selected with no model. Remove it with `voisu local remove`
(non-interactive: `voisu local remove --yes`).

### Ubuntu GNOME troubleshooting

- **Voisu is not running after login.** The daemon waits up to 20 seconds for
  GNOME to export `WAYLAND_DISPLAY`/`DISPLAY`. If `voisu service status` still
  shows inactive or failed, run `voisu service restart` once the desktop is up,
  and check `journalctl --user -u voisu.service`.
- **Paste goes to whichever window has focus.** On GNOME, Voisu cannot check
  which window has focus. To paste by hand instead, run `voisu delivery copy`.
- **The Overlay is the GNOME Shell extension `overlay@voisu.app`.**
  `voisu-overlay.service` (the GTK observer) being inactive is expected there.
- **Paste puts the Transcript on the clipboard and asks GNOME to paste it.** It goes through the RemoteDesktop portal and libei. Unless the
  compositor's input seat offers text input, Voisu submits a Ctrl+V keystroke, so
  a window that pastes with a different shortcut (a terminal using
  Ctrl+Shift+V) receives nothing; paste manually, the Transcript is on the
  clipboard.
- **Reset Voisu's RemoteDesktop grant.** This clears only the grant Voisu saved
  itself. GNOME may ask again when the daemon restarts or at the next Delivery,
  and a permission GNOME remembers on its own is not cleared by this.

  ```sh
  rm ~/.local/state/voisu/remote-desktop.restore-token
  systemctl --user restart voisu.service
  ```

## Linux command reference

`voisu` controls the daemon (`voisu-daemon`). All history and diagnostics stay
local to your machine.

| Command | Purpose |
| --- | --- |
| `voisu` | Open the interface (on a terminal); opens on setup when nothing is set up |
| `voisu start` / `stop` / `toggle` / `status` | Start and stop a Recording, or see what Voisu is doing |
| `voisu shortcut` | Show the desktop-approved Trigger Key |
| `voisu setup` | Set up step by step: Cloud or Local, keys or Consented Install, Trigger Key (screens on a terminal, text otherwise) |
| `voisu engine [cloud\|local]` | Transcribe online with Cloud or on this computer with an installed Local model |
| `voisu local remove [--yes]` | Remove a Consented Install (`--yes` required when stdin is not a terminal) |
| `voisu key set` / `key check <groq\|deepgram>` | Save or check a Groq or Deepgram key (key on stdin) |
| `voisu deepgram on` / `off` | Enable or disable Deepgram streaming |
| `voisu delivery [paste\|copy]` | Paste the Transcript into your window, or only copy it |
| `voisu writing [clean\|exact]` | `clean` fixes punctuation and formatting; `exact` keeps your words as spoken |
| `voisu layout [plain\|auto\|sections]` | Choose how text is laid out: plain, automatic, or sections |
| `voisu dictionary add` / `remove <term>` / `list` | Personal pronunciation dictionary |
| `voisu history [--json\|--verbose]` | List recent Recordings |
| `voisu export <correlation-id>` | Redacted diagnostic export for one Recording |
| `voisu doctor [--verbose]` | Host readiness diagnostics |
| `voisu service install` / `start` / `stop` / `restart` / `status` / `uninstall` | Manage the user services |
| `voisu update [--check\|--dry-run] [--switch-to-bin]` | Update Voisu through its package manager (Linux; see [Updating](#updating)) |

## License

Voisu is licensed under the [MIT License](LICENSE).

## Development docs

- [Roadmap and known limitations](docs/ROADMAP.md)
- [Contributing](CONTRIBUTING.md)
- [Domain language](CONTEXT.md)
- [Architectural decisions](docs/adr/)
- [Linux platform research](docs/research/linux-platform.md)
- [Fedora packaging](docs/packaging-fedora.md)
- Windows app source: [`apps/windows/`](apps/windows/)
