//! Doctor row collection, returning data instead of printing.
//!
//! `voisu doctor` prints these rows; the TUI shows them on Home. Collection can
//! reach the network through the live provider key checks, so callers that must
//! stay responsive run [`collect_rows`] off their render thread.

use voisu_core::{
    DaemonReadiness, KeyDiagnosis, KeyLocation, PasteActionState, PasteBackend, Provider,
    ProviderKeyStatus, ReadinessStatus, SecretStore, SessionKind, provider_free_tier_hint,
    resolve_session,
};

use crate::config::DeliveryMode;
use crate::system::{
    FedoraReadiness, ProviderHttpClient, RemoteDesktopAccess, SecretToolStore,
    remote_desktop_access,
};

/// One rendered doctor line: `label  value  STATUS`, with an optional runnable
/// action (shown indented only on FAIL) and reasoning (shown only under
/// --verbose). Capability findings, Delivery, and the provider-key checks are
/// all reduced to this single shape so the terseness contract holds
/// for every check, not just the first group.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DoctorRow {
    pub label: String,
    pub value: Option<String>,
    pub status: ReadinessStatus,
    pub action: Option<String>,
    pub detail: String,
    /// The fix the TUI can run for this row, if any. Never printed.
    pub fix: Option<DoctorFix>,
}

/// A fix the TUI can perform itself, named by the row that needs it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DoctorFix {
    RunSetup,
    StartVoisu,
    RestartVoisu,
}

impl DoctorRow {
    fn new(label: impl Into<String>, status: ReadinessStatus, detail: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            value: None,
            status,
            action: None,
            detail: detail.into(),
            fix: None,
        }
    }

    fn value(mut self, value: impl Into<String>) -> Self {
        self.value = Some(value.into());
        self
    }

    fn action(mut self, action: impl Into<String>) -> Self {
        self.action = Some(action.into());
        self
    }

    fn fix(mut self, fix: DoctorFix) -> Self {
        self.fix = Some(fix);
        self
    }
}

/// Runs every doctor check and returns its rows, in the order `voisu doctor`
/// prints them.
pub fn collect_rows() -> Vec<DoctorRow> {
    use crate::setup_profile::{TriggerIntegration, live_setup_facts, trigger_integration};

    let trigger_integration = trigger_integration(&live_setup_facts());
    let ubuntu_gnome = trigger_integration == TriggerIntegration::GnomeShellExtension;
    let mut inspector = FedoraReadiness::default();
    let mut rows: Vec<DoctorRow> = inspector
        .inspect_with_global_shortcuts(!ubuntu_gnome)
        .into_iter()
        .map(|finding| DoctorRow {
            label: finding.capability.cli_label().to_owned(),
            value: finding.value,
            status: finding.status,
            action: finding.action,
            detail: finding.detail,
            fix: None,
        })
        .collect();

    if ubuntu_gnome {
        use crate::ubuntu_gnome::{LiveState, TriggerState};
        let state = crate::ubuntu_gnome::doctor_state();
        rows.push(match state.overlay {
            LiveState::Active => DoctorRow::new(
                "Overlay",
                ReadinessStatus::Pass,
                "Ubuntu native GNOME Shell Overlay is active",
            )
            .value("active"),
            LiveState::RestartRequired => DoctorRow::new(
                "Overlay",
                ReadinessStatus::Warn,
                "GNOME persisted the native Overlay but needs a logout/login or reboot to load it",
            )
            .value("restart required"),
            LiveState::Unavailable => DoctorRow::new(
                "Overlay",
                ReadinessStatus::Fail,
                state
                    .detail
                    .as_deref()
                    .unwrap_or("Ubuntu native GNOME Shell Overlay is missing, disabled, or broken"),
            )
            .value("unavailable")
            .action("run `voisu setup`")
            .fix(DoctorFix::RunSetup),
        });
        rows.push(match state.trigger {
            TriggerState::Active => DoctorRow::new(
                "Trigger Key",
                ReadinessStatus::Pass,
                "GNOME accepted the extension-owned Trigger Key",
            )
            .value(state.trigger_key.as_deref().unwrap_or("configured")),
            TriggerState::RestartRequired => DoctorRow::new(
                "Trigger Key",
                ReadinessStatus::Warn,
                "Trigger Key will be attempted when GNOME loads the extension",
            )
            .value(format!(
                "{} / restart required",
                state.trigger_key.as_deref().unwrap_or("configured")
            )),
            TriggerState::NotInstalled => DoctorRow::new(
                "Trigger Key",
                ReadinessStatus::Warn,
                "GNOME could not install the configured Trigger Key",
            )
            .value(format!(
                "{} / not installed",
                state.trigger_key.as_deref().unwrap_or("configured")
            ))
            .action("use `voisu toggle`"),
        });
    }

    // The CLI's own probes remain useful, but they cannot prove that the
    // already-running service inherited the same display endpoint. Newer
    // daemons add an additive readiness payload on the handshake already used
    // for the Daemon row; old daemons simply retain the historic doctor table.
    if let Some(readiness) = inspector
        .daemon_status()
        .and_then(|response| response.readiness.as_ref())
    {
        rows.extend(daemon_readiness_rows(readiness));
    }

    if let Some(asr) = inspector
        .daemon_status()
        .and_then(|response| response.asr_mode.as_ref())
    {
        rows.extend(
            crate::local_doctor::rows_from_status(asr)
                .into_iter()
                .map(local_doctor_row),
        );
    }

    // The live key checks reach the network; a test that pins other doctor
    // output opts out with VOISU_TEST_SKIP_DOCTOR_KEYS and covers the key
    // classification through dedicated seams instead. Local doctor skips Cloud
    // probes with explicit SKIP rows rather than hiding them.
    if std::env::var_os("VOISU_TEST_SKIP_DOCTOR_KEYS").is_none() {
        if crate::local_doctor::skip_cloud_probes() {
            rows.extend(
                crate::local_doctor::skipped_cloud_key_rows()
                    .into_iter()
                    .map(local_doctor_row),
            );
        } else if let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            // The live key round trips get their own runtime, so the CLI never
            // needs an ambient async one.
            rows.extend(provider_key_rows(&runtime));
        }
    }

    rows
}

fn local_doctor_row(check: crate::local_doctor::DoctorCheck) -> DoctorRow {
    let mut row = DoctorRow::new(check.label, check.status, check.detail);
    if !check.value.is_empty() {
        row = row.value(check.value);
    }
    if let Some(action) = check.action {
        row = row.action(action);
    }
    // The one failing Local row is a missing model, which setup installs.
    if check.status == ReadinessStatus::Fail {
        row = row.fix(DoctorFix::RunSetup);
    }
    row
}

fn daemon_readiness_rows(readiness: &DaemonReadiness) -> Vec<DoctorRow> {
    let mut rows = Vec::with_capacity(6);
    let daemon_session_matches = daemon_session_matches_cli(readiness);
    rows.push(daemon_session_row(readiness));

    rows.push(delivery_row(
        readiness,
        crate::config::delivery_mode(),
        remote_desktop_access,
    ));

    let clipboard_value = readiness
        .clipboard_backend
        .as_deref()
        .unwrap_or("not installed");
    rows.push(if readiness.clipboard_usable && daemon_session_matches {
        DoctorRow::new(
            "Service clipboard",
            ReadinessStatus::Pass,
            "the Voisu service has a clipboard tool and a usable display endpoint",
        )
        .value(clipboard_value)
    } else if readiness.clipboard_backend.is_some() {
        DoctorRow::new(
            "Service clipboard",
            ReadinessStatus::Fail,
            "a clipboard tool is installed, but the Voisu service cannot reach its display endpoint",
        )
        .value(clipboard_value)
        .action("voisu service restart")
        .fix(DoctorFix::RestartVoisu)
    } else {
        DoctorRow::new(
            "Service clipboard",
            ReadinessStatus::Fail,
            "no clipboard tool is installed where the Voisu service runs",
        )
        .value(clipboard_value)
        .action("install wl-clipboard on Wayland or xclip on X11")
    });

    if is_hyprland_session() {
        rows.push(overlay_readiness_row());
    }
    rows
}

/// The one Delivery row: what the running Voisu service will do with a
/// Transcript, and the fix when it cannot paste. `access` probes the
/// RemoteDesktop portal; it runs only for Paste outside Hyprland.
fn delivery_row(
    readiness: &DaemonReadiness,
    configured: DeliveryMode,
    access: impl FnOnce() -> RemoteDesktopAccess,
) -> DoctorRow {
    const LABEL: &str = "Delivery";
    if readiness.delivery_mode != configured.as_str() {
        return DoctorRow::new(
            LABEL,
            ReadinessStatus::Fail,
            format!(
                "the Voisu service still uses {} from before the last Delivery change",
                readiness.delivery_mode
            ),
        )
        .value("Restart Voisu to use your new Delivery setting")
        .action("voisu service restart")
        .fix(DoctorFix::RestartVoisu);
    }
    if configured == DeliveryMode::Copy {
        return DoctorRow::new(
            LABEL,
            ReadinessStatus::Pass,
            "Voisu puts the Transcript on the clipboard",
        )
        .value("copy · you paste with Ctrl+V");
    }
    match (&readiness.paste_backend, &readiness.paste_action) {
        (PasteBackend::Hyprland, _) => DoctorRow::new(
            LABEL,
            ReadinessStatus::Pass,
            "Voisu presses your Hyprland paste shortcut after copying",
        )
        .value(match readiness.paste_shortcut.as_deref() {
            Some(shortcut) => format!("paste · {shortcut}"),
            None => "paste".to_owned(),
        }),
        (PasteBackend::Unavailable, _) => DoctorRow::new(
            LABEL,
            ReadinessStatus::Warn,
            "add a paste shortcut to your Hyprland config, or run voisu delivery copy",
        )
        .value("No paste shortcut found in your Hyprland config. Your words stay on the clipboard.")
        .action("add a paste shortcut to your Hyprland config, or run voisu delivery copy"),
        (PasteBackend::NotRequired, PasteActionState::ClipboardOnly) => DoctorRow::new(
            LABEL,
            ReadinessStatus::Warn,
            "the Voisu service was started with VOISU_DISABLE_DIRECT_DELIVERY, so it copies only",
        )
        .value("pasting turned off by VOISU_DISABLE_DIRECT_DELIVERY"),
        (PasteBackend::NotRequired, _) => match access() {
            RemoteDesktopAccess::Allowed => DoctorRow::new(
                LABEL,
                ReadinessStatus::Pass,
                "you allowed Voisu keyboard control through the desktop portal",
            )
            .value("paste"),
            RemoteDesktopAccess::NotAllowed => DoctorRow::new(
                LABEL,
                ReadinessStatus::Warn,
                "your desktop will ask once to let Voisu control the keyboard; until then Voisu copies only",
            )
            .value("Allow keyboard control when asked"),
            RemoteDesktopAccess::Missing => DoctorRow::new(
                LABEL,
                ReadinessStatus::Fail,
                "the desktop portal has no RemoteDesktop interface, so Voisu can only copy",
            )
            .value("This desktop doesn't let apps paste for you")
            .action("voisu delivery copy"),
        },
    }
}

fn daemon_session_row(readiness: &DaemonReadiness) -> DoctorRow {
    let daemon_resolution = resolve_session(
        readiness.wayland_display.as_deref(),
        readiness.x11_display.as_deref(),
        readiness.session_type.as_deref(),
    );
    let daemon_session = match readiness.session_type.as_deref() {
        Some(value) if value.eq_ignore_ascii_case("wayland") => SessionKind::Wayland,
        Some(value) if value.eq_ignore_ascii_case("x11") => SessionKind::X11,
        _ => daemon_resolution.session,
    };
    let same_endpoint = daemon_session_matches_cli(readiness);
    let value = daemon_session_value(readiness, daemon_session);
    if same_endpoint {
        DoctorRow::new(
            "Service session",
            ReadinessStatus::Pass,
            "the Voisu service's session type and display endpoint match this terminal's",
        )
        .value(value)
    } else {
        DoctorRow::new(
            "Service session",
            ReadinessStatus::Fail,
            "the Voisu service's session is missing or older than this terminal's; restart it",
        )
        .value(value)
        .action("voisu service restart")
        .fix(DoctorFix::RestartVoisu)
    }
}

fn daemon_session_matches_cli(readiness: &DaemonReadiness) -> bool {
    let cli_wayland = std::env::var("WAYLAND_DISPLAY").ok();
    let cli_x11 = std::env::var("DISPLAY").ok();
    let cli_type = std::env::var("XDG_SESSION_TYPE").ok();
    let cli_resolution = resolve_session(
        cli_wayland.as_deref(),
        cli_x11.as_deref(),
        cli_type.as_deref(),
    );
    let daemon_resolution = resolve_session(
        readiness.wayland_display.as_deref(),
        readiness.x11_display.as_deref(),
        readiness.session_type.as_deref(),
    );
    let daemon_session = match readiness.session_type.as_deref() {
        Some(value) if value.eq_ignore_ascii_case("wayland") => SessionKind::Wayland,
        Some(value) if value.eq_ignore_ascii_case("x11") => SessionKind::X11,
        _ => daemon_resolution.session,
    };
    match cli_resolution.session {
        SessionKind::Wayland => {
            daemon_session == SessionKind::Wayland
                && cli_wayland.as_deref() == readiness.wayland_display.as_deref()
                && same_optional_identity(
                    std::env::var("HYPRLAND_INSTANCE_SIGNATURE").ok().as_deref(),
                    readiness.hyprland_instance_signature.as_deref(),
                )
        }
        SessionKind::X11 => {
            daemon_session == SessionKind::X11
                && cli_x11.as_deref() == readiness.x11_display.as_deref()
                && same_optional_identity(
                    std::env::var("XAUTHORITY").ok().as_deref(),
                    readiness.x_authority.as_deref(),
                )
        }
        SessionKind::Unknown => false,
    }
}

fn same_optional_identity(current: Option<&str>, daemon: Option<&str>) -> bool {
    current.map(str::trim).filter(|value| !value.is_empty())
        == daemon.map(str::trim).filter(|value| !value.is_empty())
}

fn daemon_session_value(readiness: &DaemonReadiness, session: SessionKind) -> String {
    let label = match session {
        SessionKind::Wayland => "Wayland",
        SessionKind::X11 => "X11",
        SessionKind::Unknown => "unknown",
    };
    let display = match session {
        SessionKind::Wayland => readiness.wayland_display.as_deref(),
        SessionKind::X11 => readiness.x11_display.as_deref(),
        SessionKind::Unknown => None,
    }
    .filter(|value| !value.trim().is_empty())
    .unwrap_or("missing");
    format!("{label} / {display}")
}

fn is_hyprland_session() -> bool {
    std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some_and(|value| !value.is_empty())
        && resolve_session(
            std::env::var("WAYLAND_DISPLAY").ok().as_deref(),
            std::env::var("DISPLAY").ok().as_deref(),
            std::env::var("XDG_SESSION_TYPE").ok().as_deref(),
        )
        .session
            == SessionKind::Wayland
}

fn overlay_readiness_row() -> DoctorRow {
    if let Ok(value) = std::env::var("VOISU_TEST_OVERLAY_READINESS") {
        return match value.as_str() {
            "ready" => DoctorRow::new(
                "Overlay",
                ReadinessStatus::Pass,
                "Hyprland Overlay is enabled and active",
            )
            .value("active"),
            "enabled" => DoctorRow::new(
                "Overlay",
                ReadinessStatus::Fail,
                "Hyprland Overlay is enabled but not active",
            )
            .value("stopped")
            .action("voisu service start")
            .fix(DoctorFix::StartVoisu),
            _ => DoctorRow::new(
                "Overlay",
                ReadinessStatus::Fail,
                "Hyprland Overlay is not enabled",
            )
            .value("disabled")
            .action("voisu service install"),
        };
    }

    match crate::service::hyprland_overlay_readiness() {
        Ok(readiness) if readiness.enabled && readiness.active => DoctorRow::new(
            "Overlay",
            ReadinessStatus::Pass,
            "Hyprland Overlay is enabled and active",
        )
        .value("active"),
        Ok(readiness) if readiness.enabled => DoctorRow::new(
            "Overlay",
            ReadinessStatus::Fail,
            "Hyprland Overlay is enabled but not active",
        )
        .value("stopped")
        .action("voisu service start")
        .fix(DoctorFix::StartVoisu),
        Ok(_) => DoctorRow::new(
            "Overlay",
            ReadinessStatus::Fail,
            "Hyprland Overlay is not enabled",
        )
        .value("disabled")
        .action("voisu service install"),
        Err(detail) => DoctorRow::new("Overlay", ReadinessStatus::Fail, detail)
            .value("unknown")
            .action(crate::service::hyprland_overlay_install_command()),
    }
}

/// Live per-provider key round trips as structured rows. A wrong key (401/403)
/// is a hard failure naming the fix; a missing key, quota, throttle, or
/// unreachable provider is a warning, since none of those is a definitively bad
/// key; a disabled provider is skipped.
fn provider_key_rows(runtime: &tokio::runtime::Runtime) -> Vec<DoctorRow> {
    let mut rows = Vec::new();
    let deepgram_enabled = crate::config::deepgram_enabled();
    for provider in [Provider::Deepgram, Provider::Groq] {
        let label = format!("{} key", provider.cli_label());
        if provider == Provider::Deepgram && !deepgram_enabled {
            rows.push(
                DoctorRow::new(
                    label,
                    ReadinessStatus::Skip,
                    "Deepgram is off; run `voisu deepgram on` to enable",
                )
                .value("off"),
            );
            continue;
        }
        let row = match SecretToolStore.diagnose(provider) {
            KeyDiagnosis::Found {
                location,
                credential,
            } => {
                let status = runtime.block_on(ProviderHttpClient.check(provider, credential));
                let location_note = match location {
                    KeyLocation::PlaintextFile => {
                        " — stored in the plaintext fallback file; run `voisu setup` to migrate it into your keyring"
                    }
                    KeyLocation::EnvOverride => {
                        " — provided by the environment override, which wins over any stored key"
                    }
                    KeyLocation::Keyring => "",
                };
                let value = match status {
                    ProviderKeyStatus::Valid => "valid",
                    ProviderKeyStatus::InvalidKey => "invalid",
                    ProviderKeyStatus::RateLimited => "too many requests, try later",
                    ProviderKeyStatus::QuotaExhausted => "out of credit",
                    ProviderKeyStatus::Unreachable => "unreachable",
                };
                let mut detail = format!("{}{}", status.headline(), location_note);
                let mut row = DoctorRow::new(label, status.readiness(), String::new()).value(value);
                match status {
                    ProviderKeyStatus::InvalidKey => {
                        detail = format!("{detail}; {}", provider_free_tier_hint(provider));
                        row = row.action("run `voisu setup`").fix(DoctorFix::RunSetup);
                    }
                    ProviderKeyStatus::QuotaExhausted => {
                        detail = format!("{detail}; {}", provider_free_tier_hint(provider));
                    }
                    _ => {}
                }
                row.detail = detail;
                row
            }
            // A present env override always wins at runtime, so a malformed one
            // shadows every stored key and breaks dictation: a hard failure
            // naming the variable, never a PASS on the shadowed key.
            KeyDiagnosis::EnvOverrideInvalid => {
                let variable = provider.environment_variable();
                DoctorRow::new(
                    label,
                    ReadinessStatus::Fail,
                    format!(
                        "{variable} is set but is not a usable key (empty or contains a line \
                         break) and it overrides any stored key"
                    ),
                )
                .value(format!("{variable} is invalid"))
                .action(format!("unset or fix {variable}"))
            }
            KeyDiagnosis::Absent => DoctorRow::new(
                label,
                ReadinessStatus::Warn,
                format!(
                    "not configured — run `voisu setup`; {}",
                    provider_free_tier_hint(provider)
                ),
            )
            .value("not configured"),
            // The keyring could not be consulted: steer the user at the real fix
            // (unlock / start / install) rather than telling them to write a
            // plaintext key they may not need to.
            KeyDiagnosis::Locked => DoctorRow::new(
                label,
                ReadinessStatus::Warn,
                "keyring locked — unlock it, or run `voisu setup` to store a key",
            )
            .value("keyring locked"),
            KeyDiagnosis::Unavailable => DoctorRow::new(
                label,
                ReadinessStatus::Warn,
                "no desktop keyring available — run `voisu setup` to store a key",
            )
            .value("no keyring"),
            KeyDiagnosis::ToolMissing => DoctorRow::new(
                label,
                ReadinessStatus::Warn,
                "secret-tool is not installed — install libsecret-tools, or run `voisu setup`",
            )
            .value("secret-tool missing"),
        };
        rows.push(row);
    }
    rows
}

#[cfg(test)]
mod delivery_row_tests {
    use super::*;

    fn readiness(mode: &str, action: PasteActionState, backend: PasteBackend) -> DaemonReadiness {
        DaemonReadiness {
            session_type: Some("wayland".to_owned()),
            wayland_display: Some("wayland-1".to_owned()),
            x11_display: None,
            x_authority: None,
            hyprland_instance_signature: None,
            delivery_mode: mode.to_owned(),
            paste_action: action,
            paste_backend: backend,
            paste_shortcut: None,
            clipboard_usable: true,
            clipboard_backend: Some("wl-copy".to_owned()),
        }
    }

    fn no_probe() -> RemoteDesktopAccess {
        panic!("only Paste outside Hyprland probes the RemoteDesktop portal")
    }

    fn shown(row: &DoctorRow) -> (&str, ReadinessStatus, Option<&str>) {
        (
            row.value.as_deref().unwrap_or(""),
            row.status,
            row.action.as_deref(),
        )
    }

    #[test]
    fn a_service_started_before_the_last_change_asks_for_a_restart() {
        let stale = readiness(
            "clipboard",
            PasteActionState::ClipboardOnly,
            PasteBackend::NotRequired,
        );
        let row = delivery_row(&stale, DeliveryMode::Paste, no_probe);
        assert_eq!(
            shown(&row),
            (
                "Restart Voisu to use your new Delivery setting",
                ReadinessStatus::Fail,
                Some("voisu service restart")
            )
        );
        assert_eq!(row.fix, Some(DoctorFix::RestartVoisu));
    }

    #[test]
    fn copy_says_the_user_pastes() {
        let copy = readiness(
            "copy",
            PasteActionState::ClipboardOnly,
            PasteBackend::NotRequired,
        );
        let row = delivery_row(&copy, DeliveryMode::Copy, no_probe);
        assert_eq!(
            shown(&row),
            ("copy · you paste with Ctrl+V", ReadinessStatus::Pass, None)
        );
    }

    #[test]
    fn hyprland_paste_names_its_shortcut_or_says_how_to_add_one() {
        let mut verified = readiness("paste", PasteActionState::Verified, PasteBackend::Hyprland);
        verified.paste_shortcut = Some("SUPER + V".to_owned());
        let row = delivery_row(&verified, DeliveryMode::Paste, no_probe);
        assert_eq!(
            shown(&row),
            ("paste · SUPER + V", ReadinessStatus::Pass, None)
        );

        let missing = readiness(
            "paste",
            PasteActionState::ClipboardOnly,
            PasteBackend::Unavailable,
        );
        let row = delivery_row(&missing, DeliveryMode::Paste, no_probe);
        assert_eq!(
            shown(&row),
            (
                "No paste shortcut found in your Hyprland config. Your words stay on the clipboard.",
                ReadinessStatus::Warn,
                Some("add a paste shortcut to your Hyprland config, or run voisu delivery copy")
            )
        );
        assert_eq!(row.fix, None);
    }

    #[test]
    fn the_opt_out_says_pasting_is_turned_off() {
        let disabled = readiness(
            "paste",
            PasteActionState::ClipboardOnly,
            PasteBackend::NotRequired,
        );
        let row = delivery_row(&disabled, DeliveryMode::Paste, no_probe);
        assert_eq!(
            shown(&row),
            (
                "pasting turned off by VOISU_DISABLE_DIRECT_DELIVERY",
                ReadinessStatus::Warn,
                None
            )
        );
    }

    #[test]
    fn portal_paste_reports_whether_the_desktop_lets_voisu_paste() {
        let portal = readiness(
            "paste",
            PasteActionState::NotRequired,
            PasteBackend::NotRequired,
        );
        let cases = [
            (
                RemoteDesktopAccess::Allowed,
                ("paste", ReadinessStatus::Pass, None),
            ),
            (
                RemoteDesktopAccess::NotAllowed,
                (
                    "Allow keyboard control when asked",
                    ReadinessStatus::Warn,
                    None,
                ),
            ),
            (
                RemoteDesktopAccess::Missing,
                (
                    "This desktop doesn't let apps paste for you",
                    ReadinessStatus::Fail,
                    Some("voisu delivery copy"),
                ),
            ),
        ];
        for (access, expected) in cases {
            let row = delivery_row(&portal, DeliveryMode::Paste, || access);
            assert_eq!(shown(&row), expected, "{access:?}");
        }
    }
}
