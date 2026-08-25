# Packaged Hyprland release gate

This is the final gate for claiming out-of-box Hyprland support. It must run on
a clean user account with the exact packaged Voisu and Overlay artifacts. A
configured developer desktop is useful for debugging, but it is not release
evidence.

The gate has two parts:

1. An operator performs the state-changing checks below and records each result.
2. `packaging/hyprland-release-gate.sh --check` collects read-only host evidence
   and refuses a pass until every manual check is marked `PASS` or explicitly
   `WAIVED` with a reason.

## Procedure

Build the exact release artifact, then use a clean account with Hyprland and
the required Overlay package available. Install the package through the
documented Fedora path and run only:

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
failed or unavailable paste leaves the same Transcript available on the
clipboard. Restart Hyprland and its portal, then repeat the readiness and
clipboard checks. Deliberately restart or otherwise stale the daemon's session
environment and verify `voisu doctor` reports the mismatch as a failure.

Finally upgrade and reinstall the exact package. Verify that credentials,
unrelated Hyprland bindings, Voisu's managed binding, Delivery mode, and both
service behaviors survive. Record package NEVRA, binary versions, commands,
journals, and observed results in the evidence directory.

The runner does not install, reboot, restart, edit compositor configuration, or
upgrade the host. Those actions are intentionally operator-owned release gates.

## Evidence markers

Create one marker per manual gate in the evidence directory. A `.pass` marker
means the operator recorded the command output and result in the evidence
directory. A `.waived` marker is allowed only when it contains a concise reason
and the release decision explicitly accepts that waiver. Missing markers block
the runner.

| Marker | Required evidence |
| --- | --- |
| `clean-account-install.pass` | exact Voisu and Overlay packages installed on a clean account |
| `trigger-key-conflict.pass` | Left Alt/Right Alt conflict behavior and `hyprctl binds -j` |
| `cold-login.pass` | reboot/login result; both services started without manual restart |
| `daemon-wayland.pass` | daemon-owned `WAYLAND_DISPLAY` matches the active session |
| `controlled-recording.pass` | exactly one final Transcript on the clipboard |
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
