# Packaged Hyprland release gate

This is the final gate for claiming out-of-box Hyprland support. It must run on
a clean user account with the exact packaged `voisu` or `voisu-bin` artifact. A
configured developer desktop is useful for debugging, but it is not release
evidence.

The gate has two parts:

1. An operator performs the state-changing checks below and records each result.
2. `packaging/hyprland-release-gate.sh --check` collects read-only host evidence
   and refuses a pass until every manual check is marked `PASS` or explicitly
   `WAIVED` with a non-whitespace reason.

Build the exact Arch package first from either `packaging/aur/voisu` or
`packaging/aur/voisu-bin`. Install that package file with `pacman -U` (or use
`makepkg -si`, which invokes `pacman` for the install). For a normal AUR
install, `yay -S voisu` or `yay -S voisu-bin` is the user-facing path. The
release gate binds evidence to a locally built package artifact, so copy the
installed package file to `package-artifact` in the evidence directory before
the first `--check` run. The collector records its `pkgname`, `pkgver`,
`pkgrel`, architecture, and SHA-256 in `tested-release`, verifies the
artifact's `.MTREE` manifest and SHA-256 digests against the installed payload,
then rejects later checks if that identity changes. Use a new evidence directory
for a new package release.

From the selected PKGBUILD directory, the release flow is:

```sh
pkgfile=$(makepkg --packagelist | head -n 1)
makepkg --syncdeps --noconfirm
sudo pacman -U "$pkgfile"
cp "$pkgfile" /path/to/hyprland-gate-evidence/package-artifact
```

Start with a clean build directory so `pkgfile` names the package just built.

## Procedure

Build and install the exact `voisu` or `voisu-bin` package artifact through the
Arch path above, then use a clean account with Hyprland. Both Arch packages
ship the daemon, CLI, Overlay binary, and user units. Run only:

```sh
voisu setup
```

Do not repair the installation with an undocumented manual unit, binding, or
environment override. Before changing a trigger binding, record whether Left
Alt and Right Alt are already owned and preserve the relevant `hyprctl binds -j`
output.

Reboot and log into Hyprland. Without manually restarting Voisu, record:

- the active `voisu.service` and `voisu-overlay.service` states;
- `WAYLAND_DISPLAY` from the session and from the daemon's systemd environment;
- the setup output, `voisu doctor --verbose`, and both user journals.

Run one controlled Recording. Verify exactly one final Transcript reaches the
clipboard, that the verified Paste Action inserts it when enabled, and that a
failed or unavailable paste leaves that same Transcript available on the
clipboard. Also verify that the Overlay shows Recording, Processing, and
terminal feedback. Restart Hyprland and its portal, then repeat the readiness
and clipboard checks. Deliberately restart or otherwise stale the daemon's
session environment and verify `voisu doctor` reports the mismatch as a
failure.

Finally upgrade and reinstall the exact package. Verify that credentials,
unrelated Hyprland bindings, Voisu's managed binding, Delivery mode, and both
service behaviors survive. Record Arch package identity, artifact SHA-256,
binary versions, commands, journals, and observed results in the evidence
directory.

The runner does not install, reboot, restart, edit compositor configuration, or
upgrade the host. Those actions are intentionally operator-owned release gates.

## Evidence markers

Create one marker per manual gate in the evidence directory. A `.pass` marker
means the operator recorded the command output and result in the evidence
directory. A `.waived` marker is allowed only when it contains a concise
non-whitespace reason and the release decision explicitly accepts that waiver.
Missing markers block the runner.

| Marker | Required evidence |
| --- | --- |
| `clean-account-install.pass` | exact `voisu` or `voisu-bin` package installed on a clean account |
| `trigger-key-conflict.pass` | Left Alt/Right Alt conflict behavior and `hyprctl binds -j` |
| `cold-login.pass` | reboot/login result; both services started without manual restart |
| `daemon-wayland.pass` | daemon-owned `WAYLAND_DISPLAY` matches the active session |
| `controlled-recording.pass` | exactly one final Transcript on the clipboard |
| `overlay-feedback.pass` | Overlay shows Recording, Processing, and terminal feedback |
| `verified-paste.pass` | verified Paste Action inserts the Transcript |
| `clipboard-fallback.pass` | paste failure/unavailability preserves the Transcript |
| `compositor-recovery.pass` | Hyprland/portal restart and Voisu recovery |
| `stale-daemon-doctor.pass` | stale daemon condition detected by `voisu doctor` |
| `upgrade-reinstall.pass` | credentials, bindings, Delivery, and service behavior preserved |

Run the collector from the active Hyprland session:

```sh
packaging/hyprland-release-gate.sh --plan
packaging/hyprland-release-gate.sh --check /path/to/hyprland-gate-evidence
```

Exit 0 is the only release-gate pass. Exit 4 means an automated probe failed or
evidence is incomplete. Review logs for secrets before attaching them to a
release or issue; the collector is local-only and does not upload evidence.

## Release decision

Do not claim out-of-box Hyprland support from the existing host note in
[`docs/hyprland_problems.md`](hyprland_problems.md). The issue is complete only
when the clean-account evidence directory is archived with the release record,
all automated probes pass, and every manual gate is `PASS` or an explicitly
accepted `WAIVED` item.
