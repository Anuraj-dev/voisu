# Use desktop portals instead of privileged input access

Voisu will request the Trigger Key through the XDG Global Shortcuts portal and
perform automatic Delivery through the XDG Remote Desktop portal with libei.
CLI shortcut binding and clipboard Delivery remain fallbacks, avoiding raw
input-device access and a privileged `/dev/uinput` helper on the normal Fedora
path.

