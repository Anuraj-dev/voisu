//! Which compositor to ask for the focused output before a fresh pill is
//! mapped, and which output its answer names. The asking itself is in
//! [`super::hyprland`] and, over D-Bus, in the Overlay binary.

use std::time::Duration;

use super::placement::Size;

/// Who can name the focused output before the pill is mapped.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FocusSource {
    /// Hyprland's IPC socket ([`super::hyprland`]).
    Hyprland,
    /// KWin's `org.kde.KWin.activeOutputName` on the session bus.
    KWin,
}

impl FocusSource {
    /// The name the trace gives it.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Hyprland => "hyprland",
            Self::KWin => "kwin",
        }
    }
}

/// KWin's D-Bus name, object, interface and method that name its active
/// output, as a `(s)` reply. KWin 5.27 and 6 have it.
pub const KWIN_BUS: &str = "org.kde.KWin";
pub const KWIN_PATH: &str = "/KWin";
pub const KWIN_INTERFACE: &str = "org.kde.KWin";
pub const KWIN_ACTIVE_OUTPUT: &str = "activeOutputName";

/// The longest a fresh map waits on KWin before mapping without it.
pub const KWIN_DEADLINE: Duration = Duration::from_millis(25);

/// The source for this session: Hyprland when its instance signature is set,
/// KWin when `XDG_CURRENT_DESKTOP` lists KDE, otherwise none (the Overlay then
/// learns the output from the surface's enter, after the map).
#[must_use]
pub fn focus_source(hyprland_instance: bool, current_desktop: Option<&str>) -> Option<FocusSource> {
    if hyprland_instance {
        return Some(FocusSource::Hyprland);
    }
    current_desktop
        .is_some_and(|desktops| {
            desktops
                .split(':')
                .any(|desktop| desktop.trim().eq_ignore_ascii_case("KDE"))
        })
        .then_some(FocusSource::KWin)
}

/// Which of `outputs` (each its connector name, and its logical size while
/// connected) the compositor's answer `name` is: the exact connector, on an
/// output still connected with a real size.
#[must_use]
pub fn output_named(name: &str, outputs: &[(Option<&str>, Option<Size>)]) -> Option<usize> {
    if name.is_empty() {
        return None;
    }
    outputs.iter().position(|(connector, size)| {
        *connector == Some(name) && size.is_some_and(|(width, height)| width > 0 && height > 0)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hyprland_comes_first_then_a_kde_desktop() {
        assert_eq!(focus_source(true, Some("KDE")), Some(FocusSource::Hyprland));
        assert_eq!(focus_source(true, None), Some(FocusSource::Hyprland));
        assert_eq!(focus_source(false, Some("KDE")), Some(FocusSource::KWin));
        assert_eq!(focus_source(false, Some("kde")), Some(FocusSource::KWin));
        assert_eq!(
            focus_source(false, Some("X-Fedora:KDE")),
            Some(FocusSource::KWin)
        );
        assert_eq!(focus_source(false, Some("GNOME")), None);
        assert_eq!(focus_source(false, Some("KDEish:sway")), None);
        assert_eq!(focus_source(false, Some("")), None);
        assert_eq!(focus_source(false, None), None);
    }

    #[test]
    fn the_answer_names_an_exact_connected_connector() {
        let outputs = [
            (Some("eDP-1"), Some((1536, 960))),
            (Some("HDMI-A-1"), Some((1920, 1080))),
            (Some("DP-2"), None),
            (Some("DP-3"), Some((0, 1080))),
            (None, Some((1920, 1080))),
        ];
        assert_eq!(output_named("HDMI-A-1", &outputs), Some(1));
        assert_eq!(output_named("eDP-1", &outputs), Some(0));
        assert_eq!(output_named("hdmi-a-1", &outputs), None, "exact only");
        assert_eq!(output_named("HDMI-A", &outputs), None);
        assert_eq!(output_named("DP-2", &outputs), None, "unplugged");
        assert_eq!(output_named("DP-3", &outputs), None, "no real size");
        assert_eq!(output_named("", &outputs), None);
        assert_eq!(output_named("DP-9", &outputs), None);
    }
}
