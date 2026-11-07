#!/bin/sh
# Install a downloaded Voisu build on an Apple Silicon Mac.
#
#   sh install.sh ~/Downloads/Voisu-macOS-arm64.dmg
#   sh install.sh ~/Downloads/Voisu-macOS-arm64.app.tar.gz
#
# Quits a running Voisu, replaces /Applications/Voisu.app, clears the
# download quarantine flag (the build is self-signed, not notarized, so
# Gatekeeper would otherwise refuse to open it) and launches the app.
set -eu

APP_NAME="Voisu.app"
DEST="/Applications/$APP_NAME"
BUNDLE_ID="app.voisu.desktop"

say() { printf '==> %s\n' "$*"; }
die() { printf 'Error: %s\n' "$*" >&2; exit 1; }

[ "$#" -eq 1 ] || die "give one file: the Voisu .dmg or .app.tar.gz you downloaded.
Example: sh install.sh ~/Downloads/Voisu-macOS-arm64.dmg"
SRC="$1"
[ -f "$SRC" ] || die "file not found: $SRC"
[ "$(uname -s)" = "Darwin" ] || die "this installer only runs on macOS."
[ "$(uname -m)" = "arm64" ] || die "this build is for Apple Silicon Macs (M1 or newer) only."

WORK="$(mktemp -d)"
MOUNT=""
cleanup() {
  if [ -n "$MOUNT" ]; then
    hdiutil detach "$MOUNT" -quiet >/dev/null 2>&1 || hdiutil detach "$MOUNT" -force -quiet >/dev/null 2>&1 || true
  fi
  rm -rf "$WORK"
}
trap cleanup EXIT INT TERM

# 1. Find Voisu.app inside the download.
case "$SRC" in
  *.dmg)
    say "Opening the disk image"
    MOUNT="$WORK/mnt"
    mkdir -p "$MOUNT"
    hdiutil attach -nobrowse -readonly -noautoopen -mountpoint "$MOUNT" "$SRC" >/dev/null \
      || { MOUNT=""; die "could not open $SRC. Is the download complete?"; }
    APP_SRC="$MOUNT/$APP_NAME"
    ;;
  *.tar.gz | *.tgz)
    say "Unpacking the archive"
    tar -xzf "$SRC" -C "$WORK" || die "could not unpack $SRC. Is the download complete?"
    APP_SRC="$WORK/$APP_NAME"
    ;;
  *)
    die "expected a .dmg or .app.tar.gz file, got: $SRC"
    ;;
esac
[ -d "$APP_SRC" ] || die "no $APP_NAME found inside $SRC."

# 2. Quit a running Voisu so its files can be replaced.
if pgrep -x Voisu >/dev/null 2>&1; then
  say "Quitting the running Voisu"
  osascript -e "tell application id \"$BUNDLE_ID\" to quit" >/dev/null 2>&1 || true
  i=0
  while pgrep -x Voisu >/dev/null 2>&1 && [ "$i" -lt 10 ]; do
    sleep 1
    i=$((i + 1))
  done
  if pgrep -x Voisu >/dev/null 2>&1; then
    pkill -x Voisu || true
    sleep 1
  fi
fi

# 3. Copy into /Applications (asks for your Mac password only if needed).
SUDO=""
if [ ! -w /Applications ] || { [ -e "$DEST" ] && [ ! -w "$DEST" ]; }; then
  say "Your Mac password is needed to write to /Applications"
  SUDO="sudo"
fi
say "Installing to $DEST"
$SUDO rm -rf "$DEST"
$SUDO ditto "$APP_SRC" "$DEST"

# 4. Clear the quarantine flag the download added.
say "Allowing the app to open"
$SUDO xattr -dr com.apple.quarantine "$DEST" 2>/dev/null || true
if xattr -r "$DEST" 2>/dev/null | grep -q com.apple.quarantine; then
  die "could not clear the quarantine flag on $DEST."
fi

# 5. Launch.
say "Starting Voisu"
open "$DEST"
say "Done. If macOS asks for Microphone, Accessibility or Input Monitoring access, allow it."
