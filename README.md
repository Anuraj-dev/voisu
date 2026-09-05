# Voisu

Voisu is a cloud-first Linux desktop dictation application. Its first supported
environment is Fedora KDE Plasma on Wayland. It runs as a set of systemd **user**
services: press the Trigger Key, speak, and a validated Transcript is inserted
into the focused application.

Current development host: Arch Linux (Omarchy) with Hyprland. The Fedora
statement above is the product's first-support target, not the current host.

Project page and full install docs: **https://anuraj-dev.github.io/voisu/**

## Product promise

Press the Trigger Key once, speak naturally, press it again, and receive one
validated Transcript in the focused application. If direct insertion is not
available, the Transcript remains available on the clipboard.

## Installation

### Fedora (COPR)

```sh
sudo dnf copr enable anuraj-dev/voisu
sudo dnf install voisu
```

### Arch (AUR)

```sh
yay -S voisu-bin      # prebuilt
yay -S voisu          # build from source (pick one; they conflict)
```

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
voisu setup   # guided wizard: validate and store your provider keys
# optional on-screen Overlay (on Fedora, install voisu-overlay first):
systemctl --user enable --now voisu-overlay.service
```

### Trigger Key by desktop

How you pick the Trigger Key depends on your desktop's portal:

- **KDE Plasma / GNOME:** a system dialog appears the first time the daemon
  starts — choose a Trigger Key in that dialog, and it persists.
- **Hyprland:** there is no Global Shortcuts dialog. `voisu setup` installs a
  compositor bind: Caps Lock (`code:66`) after a default-yes prompt, or Right
  Alt (`code:108`) if you decline or Caps Lock is already bound. It does not
  auto-install Left Alt and never overwrites an exact unmanaged binding. Caps
  Lock as the Trigger Key also disables lock-toggle on that key.

  Type Delivery does not work on Hyprland: its portal implements no
  RemoteDesktop interface, which is Voisu's only text-injection path. Setup
  selects clipboard Delivery:

  ```sh
  voisu delivery clipboard
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

## Command reference

`voisu` controls the daemon (`voisu-daemon`). All history and diagnostics stay
local to your machine.

| Command | Purpose |
| --- | --- |
| `voisu start` / `stop` / `toggle` / `status` | Control and inspect the daemon |
| `voisu shortcut` | Show the desktop-approved Trigger Key |
| `voisu setup` | Guided wizard: Trigger Key, Delivery, provider keys |
| `voisu auth set` / `auth verify <groq\|deepgram>` | Store or verify a provider key (key on stdin) |
| `voisu deepgram on` / `off` | Enable or disable Deepgram streaming |
| `voisu delivery [type\|clipboard\|guarded]` | Show or set the Delivery mode |
| `voisu writing [smart\|literal]` | Show or set the writing mode |
| `voisu rendering [natural\|adaptive\|structured]` | Show or set the rendering policy |
| `voisu dictionary add` / `remove <term>` / `list` | Personal pronunciation dictionary |
| `voisu history [--json]` | Recent local diagnostic history |
| `voisu export <correlation-id>` | Redacted diagnostic export for one Recording |
| `voisu replay <path>` | Replay a captured fixture through the pipeline |
| `voisu doctor [--verbose]` | Host readiness diagnostics |
| `voisu service install` / `start` / `stop` / `restart` / `status` / `uninstall` | Manage the user services |

## License

Voisu is licensed under the [MIT License](LICENSE).

## Development docs

- [Roadmap and known limitations](docs/ROADMAP.md)
- [Contributing](CONTRIBUTING.md)
- [Domain language](CONTEXT.md)
- [Architectural decisions](docs/adr/)
- [Platform research](docs/research/linux-platform.md)
- [Fedora packaging](docs/packaging-fedora.md)
