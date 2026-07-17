# Overlay Login Start Design

**Date:** 2026-07-17
**Status:** Approved

## Goal

After the optional `voisu-overlay` RPM subpackage is installed and the desktop user runs `voisu service install`, `voisu-overlay --supervise` starts immediately and on subsequent Fedora KDE/Wayland graphical logins. The Overlay remains a separate, disposable, observer-only process. The daemon never spawns, signals, waits on, or depends on it.

## Confirmed root cause

The installed `voisu-overlay` subpackage owns `/usr/bin/voisu-overlay` but ships no systemd user unit or XDG autostart entry. The binary is healthy and reports `backend=layer-shell degradation=none`, but no process launches it.

## Architecture

Ship a dedicated systemd user unit in the optional Overlay subpackage:

```ini
[Unit]
Description=Voisu overlay observer
PartOf=graphical-session.target
After=voisu.service

[Service]
Type=simple
ExecStart=/usr/bin/voisu-overlay --supervise
Restart=on-failure

[Install]
WantedBy=graphical-session.target
```

`After=voisu.service` is ordering only. The unit has no `Wants=`, `Requires=`, or other dependency on the daemon. Both units belong independently to `graphical-session.target`. The Overlay continues polling the daemon's read-only `OverlayStatus`; unavailable daemon IPC is an observer condition, not a lifecycle failure.

## RPM packaging

- Add `packaging/voisu-overlay.service`.
- Install it under `%{_userunitdir}`.
- Include it in `%files overlay` with `/usr/bin/voisu-overlay`.
- Add Overlay-subpackage `%systemd_user_post`, `%systemd_user_preun`, and `%systemd_user_postun` scriptlets.
- Extend the exact-commit source-archive check in `packaging/build-rpm.sh` to require the new unit.
- Keep the base RPM and default workspace build GTK-free. Overlay compilation remains opt-in through `--features overlay`.

The RPM scriptlets register package installation/removal with systemd but do not silently opt every desktop user into the optional Overlay. The existing explicit desktop-user setup command remains the enable path.

## Public lifecycle

Keep the existing public interface:

```text
voisu service install
voisu service uninstall
```

When a trusted packaged `voisu-overlay.service` is present:

- A successful `voisu service install` attempts `systemctl --user enable --now voisu-overlay.service`.
- `voisu service uninstall` attempts `systemctl --user disable --now voisu-overlay.service` as an independent cleanup action.
- The CLI reports when the optional Overlay service was enabled or disabled, and appends a warning when it could not be managed. If the optional unit is absent, existing daemon-only output remains unchanged.
- Failure to inspect, enable, start, disable, or stop the Overlay never changes the daemon install/uninstall result into a failure.

`voisu service start`, `stop`, and `restart` continue managing only `voisu.service`. This avoids turning an optional presentation observer into part of the daemon lifecycle.

Detection requires both an on-disk unit in a trusted packaged user-unit directory and a successful systemd lookup whose effective `FragmentPath` remains packaged and whose `ExecStart` runs only `/usr/bin/voisu-overlay`. An arbitrary user-owned Overlay unit or command override is not automatically trusted or managed.

## Error handling

Daemon service management remains required and authoritative. Overlay management is best-effort:

- Daemon install failure returns failure as before and does not claim successful setup.
- After daemon install succeeds, Overlay enable/start failure is appended as a warning to a successful report.
- Overlay disable/stop is attempted before daemon uninstall and any failure is retained as a warning. Daemon uninstall then proceeds normally; if daemon uninstall itself fails, that required failure remains the command result.
- Missing optional Overlay package is a normal case and does not add a systemctl failure or warning.

The unit combines the Overlay's bounded internal `--supervise` policy with conservative `Restart=on-failure`. Neither mechanism can restart or affect `voisu.service`.

## Test strategy

Work in vertical RED → GREEN → REFACTOR cycles through the public `voisu service` CLI and the existing fake-systemd process boundary:

1. Packaged Overlay unit present: `service install` enables and starts `voisu-overlay.service`.
2. Overlay enable/start failure: daemon install remains successful and its required systemctl operations still occur.
3. `service uninstall` disables and stops the packaged Overlay service.
4. Overlay disable/stop failure: daemon uninstall still succeeds and performs its required cleanup.
5. Missing Overlay unit: existing daemon-only behavior remains unchanged.

Packaging verification checks that the source archive contains the unit, the Overlay RPM owns both binary and unit, the scriptlets are attached to the Overlay subpackage, and the rendered unit has the approved lifecycle contract.

Automated gates:

- Targeted `service_cli` tests during each cycle.
- `cargo test --workspace`.
- `cargo check -p voisu-app --features overlay` where the host has the existing Overlay build dependencies.
- Rebuilt RPM `%check`, file manifest, dependency, install, upgrade, and uninstall verification on the Fedora host.

## Live Fedora KDE/Wayland acceptance

With Raja observing and host commands logged using `|& tee /tmp/...log`:

1. Install the rebuilt base and optional Overlay RPMs, then rerun `voisu service install`.
2. Verify `voisu-overlay.service` is enabled and active and `pgrep -a -x voisu-overlay` shows the supervisor and capsule process.
3. Log out and back in; verify the Overlay starts from `graphical-session.target` without manual launch.
4. Start a Recording and visually confirm Recording → Processing → Success/Failure, followed by hidden Idle.
5. Kill the Overlay during a Recording; verify `voisu.service` stays active, Recording and Delivery complete, and supervision respawns the Overlay.
6. Confirm `voisu-overlay --report-backend` still reports Layer Shell with no degradation and that KWin accepts and renders the real capsule.
7. Run `voisu service uninstall` before RPM removal; verify both user units are disabled/stopped and package-owned artifacts disappear while supported user data remains.

## Documentation

- Update `docs/packaging-fedora.md` with optional Overlay installation, enablement, login-start, upgrade, and removal steps.
- Update `docs/release-evidence.md` with automated proof and retain the real-login row as pending until observed.
- Append the systemd user-unit and integrated best-effort enable-path choice to `docs/decisions.md`.
- Run the repository checkpoint workflow at session end.
