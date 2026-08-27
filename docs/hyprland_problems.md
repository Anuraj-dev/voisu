# Hyprland problems and intended fixes

> Confirmed on Raja's Omarchy/Hyprland host. This document defines the product fixes required before Voisu can claim out-of-box Hyprland support.

## Summary

Voisu can record and transcribe on Hyprland, but its default setup is not reliable. The packaged service can lose its login-start job, start without a usable Wayland environment, select an unsupported Delivery mode, and retry a Trigger Key portal flow that Hyprland does not provide.

The intended user experience is one `voisu setup` run. Voisu should detect Hyprland, install session-correct systemd integration, select clipboard Delivery, offer a compositor keybinding, and verify the running daemon.

## Confirmed problems

### P0: packaged service creates an ordering cycle

The packaged daemon unit has:

```ini
After=xdg-desktop-portal.service
Wants=xdg-desktop-portal.service
PartOf=graphical-session.target
WantedBy=graphical-session.target
```

On Omarchy/UWSM, the portal starts after `graphical-session.target`. This produces a cycle:

```text
voisu.service
  after xdg-desktop-portal.service
  after graphical-session.target
  after voisu.service
```

Systemd breaks the cycle by deleting a start job. On the observed cold login, it deleted `voisu.service/start`, so the Overlay ran while the daemon remained inactive.

#### Intended fix

- The desktop session owns the portal. Voisu must not add `After=` or `Wants=` dependencies on `xdg-desktop-portal.service`.
- On Omarchy/UWSM, start Voisu after `wayland-session-waitenv.service` and use `ConditionEnvironment=WAYLAND_DISPLAY`. The service remains enabled by `graphical-session.target` but must not also order itself after that target, which would create a cycle.
- Keep `PartOf=graphical-session.target` so Voisu stops with the compositor.
- Keep login enablement through `WantedBy=graphical-session.target`.
- Replace the complete packaged dependency set. Systemd cannot remove `After=` or `Wants=` dependencies from a drop-in by assigning an empty value.
- Verify the installed unit with `systemd-analyze --user verify` and a real cold-login journal.

The target shape is:

```ini
[Unit]
After=wayland-session-waitenv.service dbus.socket pipewire.service
Wants=dbus.socket pipewire.service
PartOf=graphical-session.target
ConditionEnvironment=WAYLAND_DISPLAY

[Install]
WantedBy=graphical-session.target
```

### P0: daemon can start without Wayland

The failed cold-login daemon had none of these variables:

- `WAYLAND_DISPLAY`
- `DISPLAY`
- `XDG_SESSION_TYPE`
- `XDG_CURRENT_DESKTOP`
- `HYPRLAND_INSTANCE_SIGNATURE`

It could capture audio and produce a Transcript, but clipboard Delivery failed with `no working clipboard backend`. Restarting the service later worked because the user manager had received the Hyprland environment by then.

#### Intended fix

- Order startup after the session environment readiness boundary. Omarchy/UWSM provides `wayland-session-waitenv.service`, which imports Wayland variables before `graphical-session.target` completes.
- Require `WAYLAND_DISPLAY` for graphical startup rather than starting a permanently degraded daemon.
- Make the daemon rediscover the active Wayland socket and session metadata after compositor changes. It must not trust its initial process environment forever.
- Recover after a compositor or portal restart without requiring `voisu service restart`.

### P1: the portal Trigger Key does not work on Hyprland

The daemon reports:

```text
portal CreateSession failed: org.freedesktop.portal.Error.NotAllowed: An app id is required
```

Hyprland does not provide the Global Shortcuts flow Voisu currently expects. Retrying the same portal request does not create a working Trigger Key.

#### Intended fix

- Detect Hyprland during setup and stop retrying the unsupported portal path.
- Offer a compositor keybinding that runs `voisu toggle`.
- Ask to use Caps Lock (`code:66`) first, default yes. Fall back to Right Alt (`code:108`) if the user declines or Caps Lock has an exact unmanaged binding. Do not auto-install Left Alt.
- Never overwrite a binding without explicit approval.
- When Caps Lock is accepted, disable lock-toggle on that key with a marked `kb_options` change (`caps:none,shift:both_capslock_cancel`) and skip that rewrite for Right Alt.
- Verify the installed binding through Hyprland after configuration reload.

Omarchy example for Caps Lock:

```lua
o.bind("code:66", "Voisu dictation", "voisu toggle")
```

### P1: the default Delivery mode is unsupported

Voisu currently defaults to `type`. Type Delivery requires a RemoteDesktop portal implementation that is unavailable on this Hyprland setup.

#### Intended fix

- During Hyprland setup, select `clipboard` Delivery by default.
- Explain that clipboard mode preserves the final Transcript on the clipboard first. If a verified Hyprland Paste Action exists, Voisu then emits that shortcut once. Unverified or failed paste stays clipboard-only.
- Keep the setting persistent across service restarts and package upgrades.
- Do not silently fall back to simulated typing or an unverified paste.

Equivalent command:

```bash
voisu delivery clipboard
```

### P1: `voisu doctor` can report a false green

The interactive CLI inherited a healthy Wayland environment and passed its clipboard round trip while the already-running daemon had stale environment variables and could not deliver.

#### Intended fix

- Add daemon IPC fields for the daemon's session type, display variables, selected Delivery mode, and usable Delivery backend.
- Make `voisu doctor` compare the CLI environment with the running daemon's reported readiness.
- Distinguish "clipboard tool installed" from "clipboard usable by the daemon."
- Fail or warn when the daemon started without a display or holds stale session metadata.

### P1: Overlay can start before Wayland

The Overlay previously checked for a display once, selected journal-only feedback, and stayed degraded. One supervised GTK/Wayland child also crashed and restarted; the available stack did not identify whether Voisu, GTK, Mesa, or the driver caused it.

#### Intended fix

- Apply the same graphical-session ordering and `ConditionEnvironment=WAYLAND_DISPLAY` used by the daemon.
- Retry display discovery after compositor changes instead of degrading permanently after one check.
- Keep supervision so a child crash does not affect Recording or Transcript Delivery.
- Investigate the GTK crash only if it repeats with a comparable or symbolized stack.

## Intended `voisu setup` flow

For a Hyprland session, `voisu setup` should:

1. Detect Hyprland and the session manager, including Omarchy/UWSM when present.
2. Install or select a service definition without a portal ownership dependency.
3. Enable the daemon and optional Overlay for the graphical session.
4. Select clipboard Delivery and explain that the Transcript is preserved on the clipboard before any verified Paste Action.
5. Ask to use Caps Lock as the Trigger Key (default yes), then install Caps Lock or Right Alt. Never overwrite an exact unmanaged bind.
6. Reload Hyprland and systemd, then start both services.
7. Query the running daemon and report its real capture, provider, display, and Delivery readiness.
8. Print one clear recovery command only when a step fails.

Setup must be re-runnable. It must preserve provider credentials, dictionary data, diagnostics, Delivery preference, unrelated Hyprland bindings, and other user units.

## Current host configuration

Raja's host currently uses:

- `voisu-bin` 0.35.2-1.
- Clipboard Delivery.
- Caps Lock mapped to `voisu toggle` in `~/.config/hypr/bindings.lua`.
- `~/.config/systemd/user/voisu.service`, modeled on Omarchy's UWSM graphical-service ordering.
- `~/.config/systemd/user/voisu-overlay.service.d/override.conf`, which adds graphical readiness to the packaged Overlay unit.
- Both user units enabled and active.

The effective daemon graph has no `xdg-desktop-portal.service` dependency. `systemd-analyze --user verify`, Hyprland configuration validation, and every current `voisu doctor` row passed after service restart. This is host evidence, not proof that the packaged out-of-box flow is fixed.

Running `voisu service install` may replace or migrate the host-only daemon unit. Until the package implements the intended unit, inspect `FragmentPath`, `After`, and `Wants` after rerunning setup.

## Release gates

The executable collector and operator-owned evidence procedure live in
[`docs/hyprland-release-gate.md`](hyprland-release-gate.md) and
`packaging/hyprland-release-gate.sh`. The collector is fail-closed: it never
turns an unrun cold-login, recovery, stale-daemon, or upgrade check into a
passing release claim.

Do not claim Hyprland support until a packaged installation passes all of these on a clean user account:

1. Install Voisu and run only the documented setup flow.
2. Confirm the requested Trigger Key was not already owned before Voisu changes it.
3. Reboot and log into Hyprland.
4. Confirm both services start without an ordering cycle or manual restart.
5. Confirm the daemon process has the active `WAYLAND_DISPLAY` before the first Recording.
6. Press Caps Lock (or the selected Trigger Key) to start and stop a controlled Recording.
7. Confirm exactly one final Transcript reaches the clipboard, then at most one verified Paste Action; paste failure must leave the Transcript on the clipboard.
8. Confirm the Overlay shows Recording, Processing, and terminal feedback, and record that result in the release evidence.
9. Restart Hyprland or its portal and confirm both Voisu processes recover.
10. Deliberately create a stale-daemon condition and confirm `voisu doctor` detects it.
11. Upgrade and reinstall the package, then confirm the settings, keybinding, credentials, and service behavior remain intact.

## Not confirmed as general Hyprland failures

- `wl-clipboard` works on this host. The observed failure came from the daemon's missing Wayland environment, not from a missing clipboard package.
- A quiet Voisu D-Bus monitor is expected. The Overlay reads daemon state through its Unix socket.
- The single Overlay crash does not identify a faulty component without a repeatable trigger or symbolized frame.

## Latest host result

On 2026-08-24, Raja validated the installed `voisu-bin` `0.35.2-1` on the
Omarchy/Hyprland host. The daemon and Overlay were enabled and active, the
daemon had the live Wayland environment, `voisu doctor` passed every check,
Caps Lock started and stopped Recordings, and clipboard Delivery worked.

There is no active runtime issue on this configured host. The remaining items in
this report are package and setup hardening: preserve the session-correct unit,
avoid the unsupported portal retry, and prove the behavior through a cold-login
gate before claiming out-of-box Hyprland support.
