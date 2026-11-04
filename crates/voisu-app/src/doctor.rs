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

use crate::system::{FedoraReadiness, ProviderHttpClient, SecretToolStore};

/// One rendered doctor line: `label  value  STATUS`, with an optional runnable
/// action (shown indented only on FAIL) and reasoning (shown only under
/// --verbose). Capability findings, the focus guard, and the provider-key
/// checks are all reduced to this single shape so the terseness contract holds
/// for every check, not just the first group.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DoctorRow {
    pub label: String,
    pub value: Option<String>,
    pub status: ReadinessStatus,
    pub action: Option<String>,
    pub detail: String,
}

impl DoctorRow {
    fn new(label: impl Into<String>, status: ReadinessStatus, detail: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            value: None,
            status,
            action: None,
            detail: detail.into(),
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
        })
        .collect();

    // One runtime serves both the focus probe and the live per-provider key
    // round trips, so the CLI never needs an ambient async runtime.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .ok();
    let focus_backend = runtime
        .as_ref()
        .map(|runtime| runtime.block_on(crate::focus::detect_focus_backend()))
        .unwrap_or(crate::focus::FocusBackendKind::None);
    rows.push(focus_guard_row(focus_backend));

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
            .action("run `voisu setup`"),
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
        } else if let Some(runtime) = runtime.as_ref() {
            rows.extend(provider_key_rows(runtime));
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
    row
}

fn focus_guard_row(backend: crate::focus::FocusBackendKind) -> DoctorRow {
    if backend == crate::focus::FocusBackendKind::None {
        DoctorRow::new(
            "Focus guard",
            ReadinessStatus::Warn,
            "no focus backend; guarded Delivery fails closed to the clipboard",
        )
        .value("none")
    } else {
        DoctorRow::new(
            "Focus guard",
            ReadinessStatus::Pass,
            "focus backend available for guarded Delivery",
        )
        .value(backend.as_str())
    }
}

fn daemon_readiness_rows(readiness: &DaemonReadiness) -> Vec<DoctorRow> {
    let mut rows = Vec::with_capacity(6);
    let daemon_session_matches = daemon_session_matches_cli(readiness);
    rows.push(daemon_session_row(readiness));

    let cli_delivery = crate::config::delivery_mode().as_str();
    let delivery_matches = readiness.delivery_mode == cli_delivery;
    rows.push(
        DoctorRow::new(
            "Daemon delivery",
            if delivery_matches {
                ReadinessStatus::Pass
            } else {
                ReadinessStatus::Fail
            },
            if delivery_matches {
                "daemon Delivery mode matches the current configuration"
            } else {
                "daemon selected Delivery before the current configuration; restart the service"
            },
        )
        .value(readiness.delivery_mode.clone())
        .action("voisu service restart"),
    );

    rows.push(match readiness.paste_action {
        PasteActionState::Verified => DoctorRow::new(
            "Paste action",
            ReadinessStatus::Pass,
            "Hyprland Paste Action binding was verified by the daemon",
        )
        .value("verified"),
        PasteActionState::ClipboardOnly => DoctorRow::new(
            "Paste action",
            ReadinessStatus::Warn,
            "no verified Hyprland Paste Action; Delivery remains clipboard-only",
        )
        .value("clipboard-only"),
        PasteActionState::NotRequired => DoctorRow::new(
            "Paste action",
            ReadinessStatus::Skip,
            "the selected Delivery mode does not use a Hyprland Paste Action",
        )
        .value("not selected"),
    });

    rows.push(match readiness.paste_backend {
        PasteBackend::Hyprland => DoctorRow::new(
            "Paste backend",
            ReadinessStatus::Pass,
            "the daemon emits the verified Paste Action through Hyprland send_key_state",
        )
        .value("hyprland"),
        PasteBackend::Unavailable => DoctorRow::new(
            "Paste backend",
            ReadinessStatus::Warn,
            "a verified Paste Action binding exists, but the Hyprland press path cannot run",
        )
        .value("unavailable"),
        PasteBackend::NotRequired => DoctorRow::new(
            "Paste backend",
            ReadinessStatus::Skip,
            "the selected Delivery mode does not use the Hyprland press path",
        )
        .value("not required"),
    });

    let clipboard_value = readiness
        .clipboard_backend
        .as_deref()
        .unwrap_or("not installed");
    rows.push(if readiness.clipboard_usable && daemon_session_matches {
        DoctorRow::new(
            "Daemon clipboard",
            ReadinessStatus::Pass,
            "the daemon has a clipboard tool and a usable display endpoint",
        )
        .value(clipboard_value)
    } else if readiness.clipboard_backend.is_some() {
        DoctorRow::new(
            "Daemon clipboard",
            ReadinessStatus::Fail,
            "a clipboard tool is installed, but the daemon cannot reach its display endpoint",
        )
        .value(clipboard_value)
        .action("voisu service restart")
    } else {
        DoctorRow::new(
            "Daemon clipboard",
            ReadinessStatus::Fail,
            "no clipboard backend is installed in the daemon environment",
        )
        .value(clipboard_value)
        .action("install wl-clipboard on Wayland or xclip on X11")
    });

    if is_hyprland_session() {
        rows.push(overlay_readiness_row());
    }
    rows
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
            "Daemon session",
            ReadinessStatus::Pass,
            "daemon session type and display endpoint match the current CLI session",
        )
        .value(value)
    } else {
        DoctorRow::new(
            "Daemon session",
            ReadinessStatus::Fail,
            "daemon session state is missing or stale compared with this CLI; restart the service",
        )
        .value(value)
        .action("voisu service restart")
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
            .action("voisu service start"),
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
        .action("voisu service start"),
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
                    ProviderKeyStatus::RateLimited => "rate-limited",
                    ProviderKeyStatus::QuotaExhausted => "quota exhausted",
                    ProviderKeyStatus::Unreachable => "unreachable",
                };
                let mut detail = format!("{}{}", status.headline(), location_note);
                let mut row = DoctorRow::new(label, status.readiness(), String::new()).value(value);
                match status {
                    ProviderKeyStatus::InvalidKey => {
                        detail = format!("{detail}; {}", provider_free_tier_hint(provider));
                        row = row.action("run `voisu setup`");
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
                .value("env override invalid")
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
