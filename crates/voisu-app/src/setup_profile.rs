//! Pure discovery of the Setup Profile used by `voisu setup`.
//!
//! The resolver receives facts collected by the thin production adapter. It
//! never reads the environment or filesystem itself, so session and config
//! decisions can be tested without changing the developer's desktop.

use std::path::PathBuf;

use voisu_core::{SessionKind, resolve_session};

/// A supported setup path. Omarchy deliberately resolves to [`Self::Hyprland`]
/// because it is a Hyprland session, not a separate compositor profile.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SetupProfile {
    /// Fedora KDE or GNOME running on Wayland.
    FedoraWayland,
    /// Hyprland, including an Omarchy session.
    Hyprland,
}

impl SetupProfile {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::FedoraWayland => "fedora-wayland",
            Self::Hyprland => "hyprland",
        }
    }
}

/// The Hyprland configuration format discovered without modifying it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HyprlandConfig {
    /// The current Lua entrypoint supported by setup.
    CurrentLua(PathBuf),
    /// The legacy configuration format that setup must not rewrite.
    LegacyConf(PathBuf),
}

/// A successful profile discovery. KDE/GNOME does not need a Hyprland config;
/// Hyprland always carries the discovered config format for later setup steps.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SetupProfileDiscovery {
    pub profile: SetupProfile,
    pub hyprland_config: Option<HyprlandConfig>,
}

/// Facts supplied to the pure Setup Profile resolver.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SetupDiscoveryFacts {
    pub wayland_display: Option<String>,
    pub x11_display: Option<String>,
    pub session_type: Option<String>,
    pub current_desktop: Option<String>,
    pub session_desktop: Option<String>,
    pub distro_id: Option<String>,
    /// Presence of this compositor-owned session variable is the live
    /// Hyprland evidence. A desktop label alone is never sufficient.
    pub hyprland_instance_signature: Option<String>,
    pub current_lua_config: Option<PathBuf>,
    pub legacy_config: Option<PathBuf>,
}

/// Why setup declined to choose a profile. All variants are non-mutating.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SetupDiscoveryError {
    UnknownSession,
    UnsupportedSession {
        session: SessionKind,
        desktop: Option<String>,
    },
    LegacyHyprlandConfig(PathBuf),
    MissingHyprlandLua,
}

impl SetupDiscoveryError {
    /// Actionable, bounded user-facing guidance for the setup command.
    pub fn message(&self) -> String {
        match self {
            Self::UnknownSession => {
                "cannot determine the desktop session; run `voisu setup` from Fedora KDE/GNOME Wayland or Hyprland (including Omarchy)".to_owned()
            }
            Self::UnsupportedSession { session, desktop } => {
                let session = match session {
                    SessionKind::Wayland => "Wayland",
                    SessionKind::X11 => "X11",
                    SessionKind::Unknown => "unknown",
                };
                let desktop = desktop.as_deref().unwrap_or("unidentified desktop");
                format!(
                    "unsupported setup session: {session} ({desktop}); no files were changed"
                )
            }
            Self::LegacyHyprlandConfig(path) => format!(
                "Hyprland is using legacy configuration at {}; update Hyprland to the current Lua configuration, then re-run `voisu setup`; no files were changed",
                path.display()
            ),
            Self::MissingHyprlandLua => {
                "Hyprland's current Lua configuration was not found; create or enable `~/.config/hypr/hyprland.lua`, then re-run `voisu setup`; no files were changed".to_owned()
            }
        }
    }
}

/// Resolve one Setup Profile from injected facts.
pub fn discover_setup_profile(
    facts: &SetupDiscoveryFacts,
) -> Result<SetupProfileDiscovery, SetupDiscoveryError> {
    let session = resolve_session(
        facts.wayland_display.as_deref(),
        facts.x11_display.as_deref(),
        facts.session_type.as_deref(),
    );
    let desktop = facts
        .current_desktop
        .as_deref()
        .or(facts.session_desktop.as_deref());
    let hyprland = session.session == SessionKind::Wayland
        && is_present(facts.hyprland_instance_signature.as_deref());

    if hyprland {
        let hyprland_config = match (
            facts.current_lua_config.as_ref(),
            facts.legacy_config.as_ref(),
        ) {
            (Some(path), _) => HyprlandConfig::CurrentLua(path.clone()),
            (None, Some(path)) => {
                return Err(SetupDiscoveryError::LegacyHyprlandConfig(path.clone()));
            }
            (None, None) => return Err(SetupDiscoveryError::MissingHyprlandLua),
        };
        return Ok(SetupProfileDiscovery {
            profile: SetupProfile::Hyprland,
            hyprland_config: Some(hyprland_config),
        });
    }

    if session.session == SessionKind::Unknown {
        return Err(SetupDiscoveryError::UnknownSession);
    }

    if session.session == SessionKind::Wayland
        && is_fedora(facts.distro_id.as_deref())
        && has_supported_desktop(desktop)
    {
        return Ok(SetupProfileDiscovery {
            profile: SetupProfile::FedoraWayland,
            hyprland_config: None,
        });
    }

    Err(SetupDiscoveryError::UnsupportedSession {
        session: session.session,
        desktop: desktop.map(str::to_owned),
    })
}

/// Read-only production adapter for the pure resolver.
pub fn live_setup_facts() -> SetupDiscoveryFacts {
    // `config_dir()` is Voisu's own `$XDG_CONFIG_HOME/voisu` directory. The
    // compositor keeps its files beside it under `$XDG_CONFIG_HOME/hypr`.
    let hyprland_dir = crate::config::config_dir()
        .parent()
        .map(|path| path.join("hypr"))
        .unwrap_or_else(|| PathBuf::from(".config/hypr"));
    let current_lua = hyprland_dir.join("hyprland.lua");
    let legacy_config = hyprland_dir.join("hyprland.conf");
    SetupDiscoveryFacts {
        wayland_display: std::env::var("WAYLAND_DISPLAY").ok(),
        x11_display: std::env::var("DISPLAY").ok(),
        session_type: std::env::var("XDG_SESSION_TYPE").ok(),
        current_desktop: std::env::var("XDG_CURRENT_DESKTOP").ok(),
        session_desktop: std::env::var("XDG_SESSION_DESKTOP").ok(),
        distro_id: read_os_release_id(),
        hyprland_instance_signature: std::env::var("HYPRLAND_INSTANCE_SIGNATURE").ok(),
        current_lua_config: existing_file(current_lua),
        legacy_config: existing_file(legacy_config),
    }
}

fn existing_file(path: PathBuf) -> Option<PathBuf> {
    path.is_file().then_some(path)
}

fn read_os_release_id() -> Option<String> {
    let contents = std::fs::read_to_string("/etc/os-release").ok()?;
    contents.lines().find_map(|line| {
        let value = line
            .strip_prefix("ID=")?
            .trim_matches('"')
            .trim_matches('\'');
        (!value.is_empty()).then(|| value.to_owned())
    })
}

fn is_present(value: Option<&str>) -> bool {
    value.is_some_and(|value| !value.trim().is_empty())
}

fn is_fedora(value: Option<&str>) -> bool {
    value.is_some_and(|value| value.eq_ignore_ascii_case("fedora"))
}

fn has_supported_desktop(value: Option<&str>) -> bool {
    value.is_some_and(|value| {
        value.split([':', ';', ',']).map(str::trim).any(|label| {
            label.eq_ignore_ascii_case("kde")
                || label.eq_ignore_ascii_case("kde plasma")
                || label.eq_ignore_ascii_case("gnome")
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hyprland_facts() -> SetupDiscoveryFacts {
        SetupDiscoveryFacts {
            wayland_display: Some("wayland-1".to_owned()),
            session_type: Some("wayland".to_owned()),
            current_desktop: Some("Hyprland".to_owned()),
            hyprland_instance_signature: Some("live-instance".to_owned()),
            current_lua_config: Some(PathBuf::from("/tmp/hyprland.lua")),
            ..SetupDiscoveryFacts::default()
        }
    }

    #[test]
    fn hyprland_and_omarchy_share_the_hyprland_profile() {
        for desktop in ["Hyprland", "Omarchy"] {
            let mut facts = hyprland_facts();
            facts.current_desktop = Some(desktop.to_owned());
            let result = discover_setup_profile(&facts).expect("supported Hyprland session");
            assert_eq!(result.profile, SetupProfile::Hyprland);
            assert_eq!(
                result.hyprland_config,
                Some(HyprlandConfig::CurrentLua(PathBuf::from(
                    "/tmp/hyprland.lua"
                )))
            );
        }
    }

    #[test]
    fn desktop_label_without_live_hyprland_evidence_is_not_hyprland() {
        let mut facts = hyprland_facts();
        facts.hyprland_instance_signature = None;
        let error = discover_setup_profile(&facts).expect_err("label is not live evidence");
        assert!(matches!(
            error,
            SetupDiscoveryError::UnsupportedSession {
                session: SessionKind::Wayland,
                ..
            }
        ));
    }

    #[test]
    fn x11_is_not_mistaken_for_hyprland() {
        let mut facts = hyprland_facts();
        facts.wayland_display = None;
        facts.x11_display = Some(":0".to_owned());
        facts.session_type = Some("x11".to_owned());
        let error = discover_setup_profile(&facts).expect_err("X11 is unsupported here");
        assert!(matches!(
            error,
            SetupDiscoveryError::UnsupportedSession {
                session: SessionKind::X11,
                ..
            }
        ));
    }

    #[test]
    fn legacy_hyprland_config_is_actionable_and_non_mutating() {
        let mut facts = hyprland_facts();
        facts.current_lua_config = None;
        facts.legacy_config = Some(PathBuf::from("/tmp/hyprland.conf"));
        let error = discover_setup_profile(&facts).expect_err("legacy config must stop setup");
        assert_eq!(
            error.message(),
            "Hyprland is using legacy configuration at /tmp/hyprland.conf; update Hyprland to the current Lua configuration, then re-run `voisu setup`; no files were changed"
        );
    }

    #[test]
    fn fedora_kde_and_gnome_wayland_resolve_to_the_fedora_profile() {
        for desktop in ["KDE", "GNOME"] {
            let facts = SetupDiscoveryFacts {
                wayland_display: Some("wayland-0".to_owned()),
                session_type: Some("wayland".to_owned()),
                current_desktop: Some(desktop.to_owned()),
                distro_id: Some("fedora".to_owned()),
                ..SetupDiscoveryFacts::default()
            };
            assert_eq!(
                discover_setup_profile(&facts).unwrap().profile,
                SetupProfile::FedoraWayland
            );
        }
    }

    #[test]
    fn unknown_and_unsupported_sessions_are_distinct_and_unchanged() {
        let unknown = discover_setup_profile(&SetupDiscoveryFacts::default())
            .expect_err("missing facts must remain unknown");
        assert_eq!(unknown, SetupDiscoveryError::UnknownSession);

        let unsupported = SetupDiscoveryFacts {
            wayland_display: Some("wayland-0".to_owned()),
            session_type: Some("wayland".to_owned()),
            current_desktop: Some("Sway".to_owned()),
            ..SetupDiscoveryFacts::default()
        };
        assert!(matches!(
            discover_setup_profile(&unsupported),
            Err(SetupDiscoveryError::UnsupportedSession {
                session: SessionKind::Wayland,
                ..
            })
        ));
    }
}
