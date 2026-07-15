# Separate the reliable daemon from the optional Overlay

The daemon and GTK4 Overlay will be separate processes connected through a
versioned Unix socket. The complete Trigger Key to Delivery path must work and
remain observable without GTK; an Overlay crash must never terminate or corrupt
a Recording, cloud request, or Delivery.

