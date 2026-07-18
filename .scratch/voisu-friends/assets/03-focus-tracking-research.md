# Focus-tracking research — KDE + Hyprland (map ticket 03)

_Sonnet 5 scout, 2026-07-18. Read-only research; saved verbatim by the driver._

## Summary

- **KDE Plasma 6:** use the KWin scripting API (`workspace.activeWindow`, `windowActivated`), identity = `internalId` UUID (stable across title changes; never use caption). Requires materializing a script under `~/.local/share/kwin/scripts/` and driving it via `org.kde.kwin.Scripting` D-Bus (loadScript/start) — NOT portal-mediated, D-Bus-loaded scripts do not survive KWin restart. `org.kde.plasma.window-management` is a single-binder global plasmashell already owns (unavailable). KWin explicitly REJECTED wlr-foreign-toplevel-management (KDE bug 502647, RESOLVED INTENTIONAL).
- **Hyprland:** bounded `hyprctl activewindow -j` poll (or raw `.socket.sock`), identity = `address` (stable per mapped window) + corroborating `class`/`pid`; reuses the existing run_restricted bounded-subprocess machinery. Event socket (`activewindowv2>>ADDRESS`) unnecessary — guard needs two point-in-time snapshots, not a stream.
- **Wayland-native:** `ext-foreign-toplevel-list-v1` is implemented by both KWin and Hyprland but carries NO focus signal — enumeration/identity only; do not build a cross-compositor protocol path for this ticket.
- **GNOME (reference only):** no unprivileged focus query; needs a third-party Shell extension (e.g. "Focused Window D-Bus"); GNOME 50 removes X11 fallbacks. Guarded mode on GNOME = NullFocusProbe → always fails closed.

## Comparison table

| Mechanism | Identity fields | Stability | Permissions | Subscribe/poll | Maturity |
|---|---|---|---|---|---|
| KWin scripting API | internalId (UUID), resourceClass, pid, caption | internalId + resourceClass+pid stable; caption NOT | Full KWin script trust; script file on disk + org.kde.kwin.Scripting D-Bus; no portal dialog | Event (windowActivated) or poll (workspace.activeWindow) | Mature, official, Plasma 6 QJSEngine |
| org.kde.KWin D-Bus bridge | same, indirect | same | session D-Bus, same trust | via loaded script only | no bare "active window" call exists |
| kde-plasma-window-management protocol | window events, title, app id | good | SINGLE-CLIENT global; plasmashell already binds it | events | unavailable to a second consumer in practice |
| ext-foreign-toplevel-list-v1 | identifier (opaque, stable, never reused), app_id, title | identifier stable | regular global, no portal | events; NO focus signal | KWin + Hyprland implement; enumeration only |
| wlr-foreign-toplevel-management | identity + control verbs + "activated" state | similar | privileged-restrictable; KWin refuses to implement | events | Hyprland yes, KWin never |
| hyprctl activewindow -j | address, class, pid, title, workspace | address stable per mapped window | user-readable Unix socket only | poll | mature, first-class |
| Hyprland .socket2 events | activewindowv2>>ADDRESS | address stable | same socket | events | mature |

## Recommended seam (voisu-core trait + voisu-app adapters)

```rust
pub struct WindowIdentity {
    /// Opaque compositor-defined stable token: KWin internalId UUID,
    /// Hyprland window address. Never a title/caption.
    pub stable_id: String,
    /// Corroborating fields for diagnostics only — never the comparison key.
    pub process_id: Option<u32>,
    pub app_id: Option<String>, // resourceClass / class
}

pub trait FocusProbe: Send {
    /// None = focus cannot be determined; callers MUST fail closed
    /// (guard unsatisfied), never treat as "unchanged".
    fn current(&mut self) -> BoundaryFuture<'_, Option<WindowIdentity>>;
}
```

- `KwinFocusProbe`: owns a loaded KWin script + a channel back to the daemon (Unix socket or session-bus service the script calls); current() reads last-known value, deadline-bounded like portal round trips.
- `HyprlandFocusProbe`: bounded `hyprctl activewindow -j` subprocess via existing run_restricted machinery — cheapest adapter.
- `NullFocusProbe`: always Ok(None) → guarded delivery fails closed on unsupported compositors (GNOME today).
- Runtime detection once at daemon startup: `$HYPRLAND_INSTANCE_SIGNATURE` → Hyprland; else `busctl --user status org.kde.KWin` succeeds → KWin; else Null. (Same env-allowlist idiom restricted_command already uses.)

## Open risks

1. KWin script deployment footprint: no portal consent path; script file on disk; D-Bus-loaded scripts do NOT persist across KWin restart; packaging question (ship in RPM? write at runtime?) needs its own review in ticket 04.
2. plasma-window-management single-binder behavior worth one smoke test on the target Plasma point release before final burial.
3. KWin script → daemon return channel is UNDESIGNED — sub-decision needed in ticket 04 before implementation.
4. Hyprland address-reuse semantics not independently verified (wiki says unique per toplevel) — spot-check close/reopen within one session.
5. Same-app-different-window (two Firefox windows) is a PRODUCT decision: stable_id changes → strict refuse, or allow same app_id? Ticket 04 must decide.
6. GNOME: guarded mode = always fails closed (NullFocusProbe); flag as a product-facing gap, not a footnote.

## Sources

- https://develop.kde.org/docs/plasma/kwin/api/ (internalId, resourceClass, windowActivated, workspace.activeWindow)
- https://develop.kde.org/docs/plasma/kwin/ (Plasma 6 QJSEngine, loadScript/start/stopScript, persistence caveat)
- https://slicker.me/kde/kwin-scripting.html
- https://bugs.kde.org/show_bug.cgi?id=502647 (KWin declines wlr-foreign-toplevel-management, RESOLVED INTENTIONAL)
- https://wayland.app/protocols/kde-plasma-window-management (single-client bind)
- https://wayland.app/protocols/ext-foreign-toplevel-list-v1 (identifier stability, compositor list)
- https://wayland.app/protocols/wlr-foreign-toplevel-management-unstable-v1
- https://github.com/swaywm/wlr-protocols/blob/master/unstable/wlr-foreign-toplevel-management-unstable-v1.xml
- https://wiki.hypr.land/IPC/ (sockets, HYPRLAND_INSTANCE_SIGNATURE, event format)
- https://wiki.hypr.land/Configuring/Advanced-and-Cool/Using-hyprctl/
- https://github.com/hyprwm/Hyprland/issues/8942 (activewindowv2 example)
- https://extensions.gnome.org/extension/5592/focused-window-d-bus/ (GNOME needs an extension)
- https://mutter.gnome.org/ (GNOME 50 X11 removal)

Local file skimmed read-only: crates/voisu-app/src/system.rs (adapter conventions: FedoraShortcutPortal, FedoraReadiness, run_restricted/command_finding, env allowlist, deadline-bounded portal round trips).
