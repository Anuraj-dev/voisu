#!/bin/sh
# Voisu dev-rig CLI wrapper (NOT shipped, NOT committed).
# Same XDG namespacing as voisu-dev.service, so `voisu-dev ...` reads/writes the
# dev config/state and talks to the dev daemon's control socket.
#
# Do NOT use `voisu-dev service ...` — the service subcommand manages the
# voisu.service unit, not voisu-dev.service. Control the rig with systemctl:
#   systemctl --user start|stop|restart voisu-dev.service

export XDG_CONFIG_HOME="$HOME/.config/voisu-dev"
export XDG_STATE_HOME="$HOME/.local/state/voisu-dev"
export XDG_DATA_HOME="$HOME/.local/share/voisu-dev"
exec /home/raja/Anuraj-dev/voisu-narilabs-test/target/release/voisu "$@"
