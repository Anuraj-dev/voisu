# Use desktop portals instead of privileged input access

Voisu will request the Trigger Key through the XDG Global Shortcuts portal and
perform automatic Delivery through the XDG Remote Desktop portal with libei.
CLI shortcut binding and clipboard Delivery remain fallbacks, avoiding raw
input-device access and a privileged `/dev/uinput` helper on the normal Fedora
path.

## Update (2026-11-26)

The portal decision covers Fedora KDE Plasma and GNOME. Ubuntu 26.04 GNOME takes
the Trigger Key from Voisu's Shell extension, and Hyprland uses a compositor
binding and the configured paste shortcut. None needs raw input-device access.
