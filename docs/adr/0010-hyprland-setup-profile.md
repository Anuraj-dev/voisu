# Use one Hyprland setup profile with verified paste actions

Hyprland and Omarchy use one Hyprland Setup profile. `voisu setup` supports the
current Lua configuration format, configures a Caps Lock Trigger Key (`code:66`)
with Right Alt (`code:108`) as the fallback, and selects `clipboard` Delivery.
After writing the Clipboard Transcript, Voisu uses a Paste Action only when
setup can verify the user's Hyprland binding; otherwise it preserves the
Transcript on the clipboard and reports clipboard-only behavior.

Fedora KDE and GNOME keep the Global Shortcuts portal. Those users choose a
Trigger Key in the desktop dialog. Setup does not install Hyprland compositor
binds or prompt for Caps Lock on that path.

The shipped daemon and Overlay units are corrected at package level so setup
does not need to leave a hidden user-unit replacement behind. Existing exact
key bindings are never overwritten automatically, and a failed Hyprland reload
restores the previous configuration.
