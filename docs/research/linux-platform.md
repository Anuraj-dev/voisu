# Linux platform research

## Research target

The first acceptance environment is Fedora Linux 43, KDE Plasma, Wayland, on an
AMD Ryzen 5 5500U laptop with 16 GB-class memory. Voisu is cloud-first because
local speech inference would compete with the user's normal workload.

## Findings

### Trigger Key

Use the XDG Global Shortcuts portal. It lets applications request shortcuts
that activate regardless of focused window and emits activation events. Toggle
Recording only requires activation, which is simpler than depending on a
reliable key-release signal. Provide `voisu start`, `voisu stop`, and
`voisu toggle` as desktop-bindable fallbacks.

Source: <https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.GlobalShortcuts.html>

### Audio capture

Use PipeWire as the primary audio boundary. WirePlumber links capture streams
to microphone sources, and PipeWire exposes a native low-latency streaming API.
Keep capture behind an adapter so tests do not require a real microphone.

Sources:

- <https://pipewire.pages.freedesktop.org/pipewire/audio-capture_8c-example.html>
- <https://pipewire.pages.freedesktop.org/wireplumber/policies/linking.html>

### Cloud transcription

Deepgram should receive continuous PCM frames over its streaming connection.
Groq should receive bounded overlapping audio chunks concurrently. On Recording
stop, Voisu sends the final tail, observes a Provider Deadline, and reconciles
two valid Source Transcripts when both are available. One valid Source
Transcript is sufficient after the deadline.

Model identifiers and deadlines must be configuration, not compiled policy, so
real latency and quality evidence can tune them safely.

### Delivery on Wayland

Use the XDG Remote Desktop portal and libei for compositor-authorized keyboard
emulation. The portal advertises keyboard capability and can provide an EIS
connection. If permission is denied or the desktop lacks support, preserve the
Transcript on the clipboard and report the fallback clearly.

Avoid `ydotool` and raw `/dev/uinput` for the standard path because they require
a privileged helper and expand the security surface.

Sources:

- <https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.RemoteDesktop.html>
- <https://libinput.pages.freedesktop.org/libei/api/index.html>

### Overlay

Build the Overlay only after the daemon path is reliable. GTK4 with GTK4 Layer
Shell supports KDE Plasma and wlroots/Smithay-based compositors. It does not
support GNOME Wayland or X11, so the Overlay needs capability detection and a
regular-window or desktop-notification fallback.

The preferred visual direction is a small bottom-centre voice capsule: hidden
while idle, voice-reactive during Recording, compact during processing, and
brief for success or failure. It must use the system font, remain unfocusable,
respect reduced motion, and stop animation work while hidden.

Sources:

- <https://github.com/wmww/gtk4-layer-shell>
- <https://packages.fedoraproject.org/pkgs/gtk4-layer-shell/>

### Service and filesystem layout

Run the daemon as a systemd user service on Fedora. Store durable configuration,
history, and logs under the XDG base directories. Store the versioned Unix
socket and other disposable runtime state under `XDG_RUNTIME_DIR`.

Source: <https://specifications.freedesktop.org/basedir/>

### HyprVox relationship

HyprVox demonstrates useful patterns: a daemon state machine, parallel cloud
providers, transcript reconciliation, quality recovery, history, structured
events, and a separate Overlay. Voisu is not a fork. It will implement its own
Rust boundaries and retain attribution for any MIT-licensed algorithm or code
that is deliberately adapted.

## Risks requiring implementation evidence

- Portal permission persistence and behavior across KDE login/restart.
- Whether libei text capability or key events give reliable Unicode Delivery
  across browsers, terminals, Electron applications, and native GTK/Qt apps.
- Groq chunk size and overlap needed to avoid duplicated or missing words.
- Provider Deadline that balances quality against release-to-text latency.
- GTK4 Layer Shell behavior across scaling, multiple monitors, and compositor
  capability changes.
- Secret storage and headless fallback behavior.

