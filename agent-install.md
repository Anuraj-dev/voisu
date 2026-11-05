# Install Voisu: guide for AI agents

> Voisu is free, MIT-licensed voice dictation for Linux and Windows. The user presses a key, speaks, and the Transcript is pasted where they were typing. It uses the user's own Deepgram and Groq API keys. This file tells a terminal agent (Claude Code, Codex, Cursor or similar) how to install and set up Voisu for the user. The human version is https://anuraj-dev.github.io/voisu/docs/getting-started.html

## Objective

Install Voisu on the user's computer, start it, and hand over to the user at the steps only they can do. You are done when every check in "Done when" passes for their platform.

## Rules for the agent

1. **Never ask for API keys in the chat.** Do not request, accept, read, print, log, or store the user's Deepgram or Groq keys. Do not pass them as command arguments, environment variables or files. The user types them into Voisu's own prompt: `voisu setup` in their own terminal on Linux, or the Voisu settings window on Windows. If the user pastes a key into the chat anyway, don't use it. Tell them to revoke it in the provider's console and create a new one.
2. **Don't run `voisu setup` yourself.** It opens full-screen setup screens that need a real terminal for hidden key entry. Ask the user to run it in a terminal window of their own, not through you.
3. **Show before you run.** Before each `sudo` command, show it and say why it's needed. If `sudo` can't prompt for a password in your shell, ask the user to run that exact command in their own terminal, then carry on.
4. **Stop at every "HUMAN" step.** Tell the user exactly what to do, wait until they say it's done, then verify.
5. **Use only the commands in this file.** Don't edit desktop config files by hand, install other key remappers, or give Voisu root or `uinput` access. Voisu never needs them.
6. **Stop on failure.** If a command fails, stop. Show the error and point the user to https://anuraj-dev.github.io/voisu/docs/troubleshooting.html. Don't improvise a workaround.

## Done when

- **Linux:** `voisu --version` prints a version. `systemctl --user is-active voisu.service` prints `active`. `voisu auth verify groq` and `voisu auth verify deepgram` each print `authentication verified`. The user has made one Recording and its Transcript appeared at their cursor.
- **Windows:** Voisu is installed and in the tray. The user has saved both keys with **Validate & save**, and has made one Recording by holding Ctrl+Win.

## Step 1: Detect the platform

On Linux:

```sh
uname -s
. /etc/os-release && echo "$ID $ID_LIKE $VERSION_ID"
echo "$XDG_CURRENT_DESKTOP $XDG_SESSION_TYPE"
```

- `ID` or `ID_LIKE` contains `fedora`: follow **Fedora**.
- `ID` or `ID_LIKE` contains `arch`: follow **Arch**.
- `ID` or `ID_LIKE` contains `debian` or `ubuntu`: follow **Debian / Ubuntu**. The apt repository is amd64 only (`dpkg --print-architecture` must print `amd64`).
- `XDG_SESSION_TYPE` is `x11`: also follow **X11 desktops** after your distribution's section.

On Windows, in PowerShell:

```powershell
[System.Environment]::OSVersion.Version
$env:PROCESSOR_ARCHITECTURE
```

Windows 10 or 11 on `AMD64` is supported: follow **Windows**. Other platforms (macOS, ARM Linux, other distributions) are not supported. Tell the user, and stop.

## Fedora

Primary target: Fedora KDE Plasma on Wayland. GNOME also works.

```sh
sudo dnf copr enable anuraj-dev/voisu
sudo dnf install voisu
# optional on-screen overlay (the capsule):
sudo dnf install voisu-overlay
```

Start the daemon as a systemd **user** service (no sudo):

```sh
systemctl --user daemon-reload
systemctl --user enable --now voisu.service
# only if voisu-overlay was installed:
systemctl --user enable --now voisu-overlay.service
```

**HUMAN: choose the Trigger Key.** On KDE Plasma and GNOME, a desktop dialog appears the first time the daemon starts. Tell the user to pick a key in it and approve. The choice persists.

**HUMAN: add API keys.** Ask the user to open their own terminal and run `voisu setup`. It asks for the Groq and Deepgram keys and checks each with its provider. Keys come from https://console.groq.com/keys and https://console.deepgram.com. Wait for them to finish.

Then go to **Verify on Linux**.

## Arch

```sh
yay -S voisu-bin
```

`voisu` (build from source) conflicts with `voisu-bin`, so install only one. If `yay` is missing, ask the user which AUR helper they use. Don't install one without asking.

**HUMAN: run setup.** Ask the user to run `voisu setup` in their own terminal. It asks for the API keys and checks them. On Hyprland it also binds the Trigger Key (Caps Lock by default, or Right Alt if they decline or Caps Lock is taken) and starts Voisu's services.

Then go to **Verify on Linux**.

## Debian / Ubuntu

Documented target: Ubuntu 26.04 LTS, GNOME on Wayland, amd64.

Check the signing key's fingerprint before trusting it:

```sh
curl -fsSL https://anuraj-dev.github.io/voisu/voisu-archive-keyring.asc -o /tmp/voisu.asc
gpg --show-keys --with-colons /tmp/voisu.asc | awk -F: '$1=="fpr"{print $10; exit}'
```

The output must be exactly `4149EE3868B36B6007592966D08BCFDC34125B28`. If it isn't, stop and tell the user.

```sh
sudo install -d -m 0755 /etc/apt/keyrings
sudo install -m 0644 /tmp/voisu.asc /etc/apt/keyrings/voisu-archive-keyring.asc
echo 'deb [signed-by=/etc/apt/keyrings/voisu-archive-keyring.asc arch=amd64] https://anuraj-dev.github.io/voisu stable main' \
  | sudo tee /etc/apt/sources.list.d/voisu.list
sudo apt-get update && sudo apt-get install -y voisu
```

**HUMAN: run setup.** Ask the user to run `voisu setup` in their own terminal. It asks for the API keys, enables the daemon and the native GNOME capsule, and sets Ctrl+Space as the Trigger Key by default. If setup says so, the user must log out and back in, or reboot. Wait for them to come back before you verify.

Then go to **Verify on Linux**.

## X11 desktops (Cinnamon, Linux Mint, Ubuntu on X11)

X11 has no shortcut portal. **HUMAN:** ask the user to add a custom shortcut under *System Settings → Keyboard → Shortcuts → Custom Shortcuts* that runs `voisu toggle`. On X11, Voisu uses `xclip` for the clipboard and shows feedback as desktop notifications.

## Verify on Linux

```sh
voisu --version
systemctl --user is-active voisu.service
voisu shortcut
voisu auth verify groq
voisu auth verify deepgram
voisu doctor
```

- `voisu shortcut` shows the Trigger Key the desktop approved.
- `voisu doctor` checks the daemon, portals, Delivery, clipboard and keys, and says what to do about anything that fails. Use `voisu doctor --verbose` for the reasoning behind each check. The non-interactive commands (`voisu status`, `voisu shortcut`, `voisu delivery`, `voisu engine`, `voisu history --verbose`) and `journalctl --user -u voisu.service` are safe for you to run when diagnosing a problem. Bare `voisu` opens a full-screen interface meant for the user; don't run it yourself.
- If the service isn't active, run `voisu service install` and then `voisu service start`. If it still fails, read `journalctl --user -u voisu.service`.

**HUMAN: first Recording.** Ask the user to click into a text field, press the Trigger Key, say a sentence, and press it again. The Transcript should appear at the cursor. Ask them to confirm.

## Windows

Windows 10 or 11, x64. Installs for the current user, with no admin rights needed. Use PowerShell.

```powershell
$installer = Join-Path $env:TEMP 'Voisu-Setup-x64.exe'
Invoke-WebRequest -Uri 'https://github.com/Anuraj-dev/voisu/releases/latest/download/Voisu-Setup-x64.exe' -OutFile $installer
Start-Process -FilePath $installer
```

If the download returns 404, no Windows release has been published yet. Tell the user to watch https://github.com/Anuraj-dev/voisu/releases, and stop.

**HUMAN: SmartScreen.** The installer isn't code-signed yet, so Windows may show "Windows protected your PC". Tell the user to choose **More info**, then **Run anyway**. They finish the installer themselves.

**HUMAN: add API keys.** Voisu opens its settings window on first launch, or from the tray icon's **Settings…** item. The user pastes their Groq and Deepgram keys into that window, not into this chat, and chooses **Validate & save**. Both keys are required. They are stored in Windows Credential Manager.

Check that it's installed:

```powershell
Get-ItemProperty 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\*' |
  Where-Object DisplayName -like 'Voisu*' | Select-Object DisplayName, DisplayVersion
```

**HUMAN: first Recording.** Ask the user to click into a text field, hold Ctrl+Win, speak, and release. The Transcript is pasted where the cursor was. If nothing lands, the tray's **Test paste (in 3 s)** item checks Delivery into one app.

## After setup

Tell the user:

- **Gestures:** on Windows, hold Ctrl+Win to dictate and double-tap for hands-free. On Linux, press the Trigger Key to start and press it again to stop. Esc cancels on Windows.
- **Keys:** they can be changed later with `voisu auth set groq` / `voisu auth set deepgram` on Linux (these read the key from standard input), or in the settings window on Windows.
- **Updates:** Windows updates itself. On Linux, Voisu updates through the package manager.
- **Docs:** https://anuraj-dev.github.io/voisu/docs/

## Links

- Getting started (human version): https://anuraj-dev.github.io/voisu/docs/getting-started.html
- Troubleshooting: https://anuraj-dev.github.io/voisu/docs/troubleshooting.html
- Providers and API keys: https://anuraj-dev.github.io/voisu/docs/providers.html
- Privacy: https://anuraj-dev.github.io/voisu/docs/privacy.html
- Source: https://github.com/Anuraj-dev/voisu
