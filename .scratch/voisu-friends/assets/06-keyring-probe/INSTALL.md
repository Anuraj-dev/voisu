# Installing the ticket-06 keyring probe (next-reboot capture)

Not installed by this diagnostics pass — these are the exact commands to run
by hand (HITL) when Raja wants a real reboot-confirmed measurement. The `-`
prefix on `ExecStartPre` means a probe failure never blocks `voisu.service`
from starting.

## Install (4 commands)

```bash
mkdir -p ~/.local/bin
cp /home/raja/Anuraj-Dev/voisu/.scratch/voisu-friends/assets/06-keyring-probe/voisu-keyring-probe.sh ~/.local/bin/voisu-keyring-probe.sh
chmod +x ~/.local/bin/voisu-keyring-probe.sh
mkdir -p ~/.config/systemd/user/voisu.service.d
cp /home/raja/Anuraj-Dev/voisu/.scratch/voisu-friends/assets/06-keyring-probe/keyring-probe.conf ~/.config/systemd/user/voisu.service.d/keyring-probe.conf
systemctl --user daemon-reload
```

Then reboot (or at minimum log out/in) so `voisu.service` starts fresh at
login and the probe runs before it. After logging back in:

```bash
journalctl --user -t voisu-keyring-probe -b --no-pager
```

Compare the first `epoch_ms=` line's timestamp against
`systemctl --user show voisu.service -p InactiveExitTimestamp` to confirm
whether the secrets service was owned/unlocked before voisu.service's own
start.

## Cleanup (remove both, 3 commands)

```bash
rm -f ~/.config/systemd/user/voisu.service.d/keyring-probe.conf
rmdir --ignore-fail-on-non-empty ~/.config/systemd/user/voisu.service.d
rm -f ~/.local/bin/voisu-keyring-probe.sh
systemctl --user daemon-reload
```
