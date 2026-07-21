# Voisu

Voisu is a cloud-first Linux desktop dictation application. Its first supported
environment is Fedora KDE Plasma on Wayland. It runs as a set of systemd **user**
services: press the Trigger Key, speak, and a validated Transcript is inserted
into the focused application.

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
  starts — choose the key once and it persists.
- **Hyprland:** there is no dialog by design. Install
  `xdg-desktop-portal-hyprland`, then declare the bind in `hyprland.conf`:

  ```conf
  bind = SUPER, D, global, voisu:voisu-toggle
  ```

  `SUPER, D` is only an example key. After the comma, `voisu` is the app id and
  `voisu-toggle` is the Trigger Key's shortcut id — keep those exact. Plain
  wlroots portals do not implement GlobalShortcuts, so the Trigger Key cannot
  bind without `xdg-desktop-portal-hyprland`.

Run `voisu doctor` if the Trigger Key does not respond — it reports a portal
without a usable GlobalShortcuts interface.

## License

Voisu is licensed under the [MIT License](LICENSE).

## Development docs

- [Domain language](CONTEXT.md)
- [Platform research](docs/research/linux-platform.md)
- [Architectural decisions](docs/adr/)
- [Wayfinder map](.scratch/voisu-wayfinder/map.md)
- [Approved specification](.scratch/voisu-spec/issues/01-fedora-cloud-dictation.md)
- [Implementation tickets](.scratch/voisu-implementation/issues/)
