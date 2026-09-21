# Require a native Overlay for Ubuntu GNOME

The Ubuntu GNOME Setup Profile is complete only when Voisu can show its capsule
as an Overlay. Desktop notifications do not satisfy this contract. Because
GNOME does not support the layer-shell protocol used by the GTK Overlay, Voisu
will use a GNOME Shell extension for the Ubuntu capsule rather than pretending
the notification fallback provides the same experience. The extension owns an
explicit Voisu Trigger Key setting whose default is `Ctrl+Space`; users may
change that setting without modifying GNOME's general custom-shortcut list.
Ubuntu setup deliberately gives Voisu ownership of `Ctrl+Space`, replacing or
shadowing an existing binding for that accelerator. A user who wants the prior
binding must change Voisu's extension-owned Trigger Key afterward.
