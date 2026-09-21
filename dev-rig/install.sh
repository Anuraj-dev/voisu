#!/bin/sh
# Installs the Voisu dev rig for the Narilabs third-provider test (NOT committed).
# Idempotent: re-runs only refresh the unit file and wrapper; seeded config,
# credentials, and history are left alone.
set -eu

RIG_DIR=$(cd "$(dirname "$0")" && pwd)
BIN_DIR="$RIG_DIR/../target/release"
DAEMON="$BIN_DIR/voisu-daemon"
CLI="$BIN_DIR/voisu"

[ -x "$DAEMON" ] && [ -x "$CLI" ] || {
    echo "error: release binaries not built yet." >&2
    echo "  cd $RIG_DIR/.. && cargo build --release" >&2
    exit 1
}

DEV_CONFIG="$HOME/.config/voisu-dev/voisu"
DEV_STATE="$HOME/.local/state/voisu-dev/voisu"
mkdir -p "$DEV_CONFIG" "$DEV_STATE" "$HOME/.local/share/voisu-dev" "$HOME/.local/bin"
# The unit sets ProtectSystem=strict with ReadWritePaths on these dirs; they
# must exist before the unit starts or namespace setup fails (status=226).

# Paste-action discovery resolves the Hyprland config as
# $XDG_CONFIG_HOME/hypr/hyprland.lua (hyprland_bindings::discover_live_paste_action).
# The unit overrides XDG_CONFIG_HOME for isolation, so expose the real hypr tree
# inside the dev root or clipboard delivery silently loses its auto-paste action.
if [ -d "$HOME/.config/hypr" ] && [ ! -e "$HOME/.config/voisu-dev/hypr" ]; then
    ln -s "$HOME/.config/hypr" "$HOME/.config/voisu-dev/hypr"
    echo "linked $HOME/.config/voisu-dev/hypr -> $HOME/.config/hypr (paste-action discovery)"
fi

if [ ! -f "$DEV_CONFIG/config.toml" ] && [ -f "$HOME/.config/voisu/config.toml" ]; then
    cp "$HOME/.config/voisu/config.toml" "$DEV_CONFIG/config.toml"
    echo "seeded $DEV_CONFIG/config.toml from the managed install"
fi
for f in credentials credentials.lock dictionary.txt dictionary.txt.lock; do
    if [ ! -f "$DEV_CONFIG/$f" ] && [ -f "$HOME/.config/voisu/$f" ]; then
        cp "$HOME/.config/voisu/$f" "$DEV_CONFIG/$f"
        echo "seeded $DEV_CONFIG/$f from the managed install"
    fi
done

install -m 644 "$RIG_DIR/voisu-dev.service" "$HOME/.config/systemd/user/voisu-dev.service"
install -m 755 "$RIG_DIR/voisu-dev-wrapper.sh" "$HOME/.local/bin/voisu-dev"
systemctl --user daemon-reload

echo
echo "dev rig installed. Next steps (in order):"
echo "  1. ~/.local/bin/voisu-dev auth set narilabs      # paste the Nari Labs key"
echo "  2. ~/.local/bin/voisu-dev mode cloud             # dev instance is cloud-ASR"
echo "  3. ~/.local/bin/voisu-dev narilabs on            # opt the new provider in"
echo "     (isolate one provider with: voisu-dev groq off / voisu-dev deepgram off)"
echo "  4. systemctl --user restart voisu-dev.service    # daemon reads flags at start"
echo "     (starting it also stops voisu.service; your hotkey then hits the dev daemon)"
echo "  5. dictate, then compare providers:"
echo "     journalctl --user -u voisu-dev.service | grep provider-comparison"
echo
echo "switch back to the managed install:"
echo "  systemctl --user stop voisu-dev.service && systemctl --user start voisu.service"
