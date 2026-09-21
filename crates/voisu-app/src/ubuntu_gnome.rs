//! Ubuntu GNOME's native Overlay adapter lifecycle.
//!
//! The Shell extension is package-owned. This module only persists per-user
//! enablement and reports whether GNOME could activate it in the current login.

use std::path::Path;
use std::process::{Command, Output};
use std::thread;
use std::time::{Duration, Instant};

use crate::service::{UserServiceAction, manage_user_service};

pub const EXTENSION_UUID: &str = "overlay@voisu.app";
pub const EXTENSION_DIR: &str = "/usr/share/gnome-shell/extensions/overlay@voisu.app";
const SCHEMA: &str = "app.voisu.shell-overlay";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LiveState {
    Active,
    RestartRequired,
    Unavailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TriggerState {
    Active,
    NotInstalled,
    RestartRequired,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SetupResult {
    pub overlay: LiveState,
    pub trigger: TriggerState,
    pub trigger_key: Option<String>,
    pub daemon_restart_required: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DoctorState {
    pub overlay: LiveState,
    pub trigger: TriggerState,
    pub trigger_key: Option<String>,
    pub detail: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ExtensionInfoState {
    Active,
    Disabled,
    Error,
    OutOfDate,
    Pending(String),
    Other(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ExtensionRuntimeState {
    NotLoaded,
    Known(ExtensionInfoState),
}

pub fn setup() -> Result<SetupResult, String> {
    if !Path::new(EXTENSION_DIR).join("extension.js").is_file() {
        return Err(
            "Ubuntu native Overlay extension is not installed; reinstall the Voisu package"
                .to_owned(),
        );
    }

    // Ubuntu must never start the notification-only GTK observer alongside the
    // Shell capsule. Missing units and already-stopped units are harmless.
    crate::service::disable_ubuntu_gtk_overlay()?;

    let install = manage_user_service(UserServiceAction::Install)?;
    if install.exit_code != 0 {
        return Err(install.message);
    }
    let daemon_restart_required = match manage_user_service(UserServiceAction::Start) {
        Ok(report) if report.exit_code == 0 => false,
        _ if systemctl_enabled("voisu.service") => true,
        Err(error) => {
            return Err(format!(
                "daemon could not start and persistent enablement was not confirmed: {error}"
            ));
        }
        Ok(report) => {
            return Err(format!(
                "daemon could not start and persistent enablement was not confirmed: {}",
                report.message
            ));
        }
    };

    persist_extension_enablement()?;
    // This can activate an extension GNOME already discovered. A fresh
    // machine-wide install commonly needs the next login; persistence above is
    // the completion contract, not this best-effort live attempt.
    let _ = run("gnome-extensions", &["enable", EXTENSION_UUID]);

    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match query_extension_state()? {
            ExtensionRuntimeState::Known(ExtensionInfoState::Active) => {
                let trigger = if gsettings_bool("trigger-active") == Some(true) {
                    TriggerState::Active
                } else {
                    TriggerState::NotInstalled
                };
                return Ok(SetupResult {
                    overlay: LiveState::Active,
                    trigger,
                    trigger_key: configured_trigger_key(),
                    daemon_restart_required,
                });
            }
            ExtensionRuntimeState::NotLoaded if Instant::now() >= deadline => {
                return Ok(SetupResult {
                    overlay: LiveState::RestartRequired,
                    trigger: TriggerState::RestartRequired,
                    trigger_key: configured_trigger_key(),
                    daemon_restart_required,
                });
            }
            ExtensionRuntimeState::Known(ExtensionInfoState::Pending(state))
                if Instant::now() < deadline =>
            {
                let _ = state;
            }
            ExtensionRuntimeState::Known(state) => {
                return Err(extension_failure_detail(&state));
            }
            ExtensionRuntimeState::NotLoaded => {}
        }
        thread::sleep(Duration::from_millis(50));
    }
}

pub fn doctor_state() -> DoctorState {
    let binding = configured_trigger_key();
    if !Path::new(EXTENSION_DIR).join("extension.js").is_file() {
        return DoctorState {
            overlay: LiveState::Unavailable,
            trigger: TriggerState::NotInstalled,
            trigger_key: binding,
            detail: Some(
                "Ubuntu native Overlay extension is missing; reinstall the Voisu package"
                    .to_owned(),
            ),
        };
    }
    if !extension_is_persisted() {
        return DoctorState {
            overlay: LiveState::Unavailable,
            trigger: TriggerState::NotInstalled,
            trigger_key: binding,
            detail: Some(
                "GNOME did not confirm persistent Voisu Overlay enablement; run `voisu setup`"
                    .to_owned(),
            ),
        };
    }
    match query_extension_state() {
        Ok(runtime) => {
            let (overlay, trigger, detail) =
                classify_runtime_state(&runtime, gsettings_bool("trigger-active") == Some(true));
            DoctorState {
                overlay,
                trigger,
                trigger_key: binding,
                detail,
            }
        }
        Err(error) => DoctorState {
            overlay: LiveState::Unavailable,
            trigger: TriggerState::NotInstalled,
            trigger_key: binding,
            detail: Some(error),
        },
    }
}

fn classify_runtime_state(
    runtime: &ExtensionRuntimeState,
    trigger_active: bool,
) -> (LiveState, TriggerState, Option<String>) {
    match runtime {
        ExtensionRuntimeState::NotLoaded => (
            LiveState::RestartRequired,
            TriggerState::RestartRequired,
            None,
        ),
        ExtensionRuntimeState::Known(ExtensionInfoState::Active) => (
            LiveState::Active,
            if trigger_active {
                TriggerState::Active
            } else {
                TriggerState::NotInstalled
            },
            None,
        ),
        ExtensionRuntimeState::Known(state) => (
            LiveState::Unavailable,
            TriggerState::NotInstalled,
            Some(extension_failure_detail(state)),
        ),
    }
}

fn systemctl_enabled(unit: &str) -> bool {
    run("systemctl", &["--user", "is-enabled", unit]).is_ok_and(|output| {
        output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == "enabled"
    })
}

fn persist_extension_enablement() -> Result<(), String> {
    let output = run(
        "gsettings",
        &["get", "org.gnome.shell", "enabled-extensions"],
    )
    .map_err(|error| format!("could not read GNOME extension enablement: {error}"))?;
    if !output.status.success() {
        return Err("could not read GNOME extension enablement".to_owned());
    }
    let current = parse_string_array(&String::from_utf8_lossy(&output.stdout))
        .ok_or_else(|| "GNOME returned malformed extension enablement".to_owned())?;
    let merged = merge_extension_uuid(&current);
    let encoded = encode_string_array(&merged);
    let set = run(
        "gsettings",
        &["set", "org.gnome.shell", "enabled-extensions", &encoded],
    )
    .map_err(|error| format!("could not persist GNOME extension enablement: {error}"))?;
    if !set.status.success() || !extension_is_persisted() {
        return Err("GNOME did not confirm persistent Voisu Overlay enablement".to_owned());
    }
    Ok(())
}

fn extension_is_persisted() -> bool {
    let Ok(output) = run(
        "gsettings",
        &["get", "org.gnome.shell", "enabled-extensions"],
    ) else {
        return false;
    };
    output.status.success()
        && parse_string_array(&String::from_utf8_lossy(&output.stdout))
            .is_some_and(|items| items.iter().any(|item| item == EXTENSION_UUID))
}

fn query_extension_state() -> Result<ExtensionRuntimeState, String> {
    let list = run_c_locale("gnome-extensions", &["list"])
        .map_err(|error| format!("could not query GNOME extensions: {error}"))?;
    if !list.status.success() {
        return Err("could not query GNOME extensions; run `gnome-extensions list`".to_owned());
    }
    if !extension_list_contains(&String::from_utf8_lossy(&list.stdout), EXTENSION_UUID) {
        return Ok(ExtensionRuntimeState::NotLoaded);
    }

    let info = run_c_locale("gnome-extensions", &["info", EXTENSION_UUID])
        .map_err(|error| format!("could not query the Voisu Overlay state: {error}"))?;
    if !info.status.success() {
        return Err(format!(
            "GNOME knows {EXTENSION_UUID} but its state query failed; run `gnome-extensions info {EXTENSION_UUID}`"
        ));
    }
    let state = parse_extension_state(&String::from_utf8_lossy(&info.stdout)).ok_or_else(|| {
        format!(
            "GNOME returned no state for {EXTENSION_UUID}; run `gnome-extensions info {EXTENSION_UUID}`"
        )
    })?;
    Ok(ExtensionRuntimeState::Known(state))
}

fn extension_list_contains(output: &str, uuid: &str) -> bool {
    output.lines().any(|line| line.trim() == uuid)
}

fn parse_extension_state(output: &str) -> Option<ExtensionInfoState> {
    let value = output
        .lines()
        .find_map(|line| line.trim().strip_prefix("State:").map(str::trim))?;
    let normalized = value.replace([' ', '-'], "_").to_ascii_uppercase();
    Some(match normalized.as_str() {
        "ACTIVE" | "ENABLED" => ExtensionInfoState::Active,
        "DISABLED" | "INACTIVE" => ExtensionInfoState::Disabled,
        "ERROR" => ExtensionInfoState::Error,
        "OUT_OF_DATE" => ExtensionInfoState::OutOfDate,
        "ACTIVATING" | "DEACTIVATING" | "INITIALIZED" | "DOWNLOADING" => {
            ExtensionInfoState::Pending(normalized)
        }
        _ => ExtensionInfoState::Other(value.to_owned()),
    })
}

fn extension_failure_detail(state: &ExtensionInfoState) -> String {
    let state = match state {
        ExtensionInfoState::Active => "ACTIVE",
        ExtensionInfoState::Disabled => "DISABLED",
        ExtensionInfoState::Error => "ERROR",
        ExtensionInfoState::OutOfDate => "OUT_OF_DATE",
        ExtensionInfoState::Pending(state) | ExtensionInfoState::Other(state) => state,
    };
    format!(
        "GNOME reports the Voisu Overlay as {state}; inspect `gnome-extensions info {EXTENSION_UUID}` and the GNOME Shell journal, then run `voisu setup`"
    )
}

fn configured_trigger_key() -> Option<String> {
    let output = run("gsettings", &["get", SCHEMA, "voisu-trigger-key"]).ok()?;
    let values = parse_string_array(&String::from_utf8_lossy(&output.stdout))?;
    values.first().map(|value| format_binding(value))
}

fn format_binding(value: &str) -> String {
    value
        .replace("<Control>", "Ctrl+")
        .replace("<Shift>", "Shift+")
        .replace("<Alt>", "Alt+")
        .replace("<Super>", "Super+")
}

fn merge_extension_uuid(current: &[String]) -> Vec<String> {
    let mut merged = current.to_vec();
    if !merged.iter().any(|item| item == EXTENSION_UUID) {
        merged.push(EXTENSION_UUID.to_owned());
    }
    merged
}

fn parse_string_array(value: &str) -> Option<Vec<String>> {
    let value = value.trim().strip_prefix("@as ").unwrap_or(value.trim());
    let inner = value.strip_prefix('[')?.strip_suffix(']')?.trim();
    if inner.is_empty() {
        return Some(Vec::new());
    }
    let mut items = Vec::new();
    let mut chars = inner.chars().peekable();
    loop {
        while chars.peek().is_some_and(|c| c.is_whitespace() || *c == ',') {
            chars.next();
        }
        let quote = chars.next()?;
        if quote != '\'' && quote != '"' {
            return None;
        }
        let mut item = String::new();
        loop {
            match chars.next()? {
                '\\' => item.push(chars.next()?),
                c if c == quote => break,
                c => item.push(c),
            }
        }
        items.push(item);
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        match chars.peek() {
            None => break,
            Some(',') => {
                chars.next();
            }
            _ => return None,
        }
    }
    Some(items)
}

fn encode_string_array(items: &[String]) -> String {
    format!(
        "[{}]",
        items
            .iter()
            .map(|item| format!("'{}'", item.replace('\\', "\\\\").replace('\'', "\\'")))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

fn gsettings_bool(key: &str) -> Option<bool> {
    let output = run("gsettings", &["get", SCHEMA, key]).ok()?;
    if !output.status.success() {
        return None;
    }
    match String::from_utf8_lossy(&output.stdout).trim() {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

fn run(program: &str, args: &[&str]) -> std::io::Result<Output> {
    Command::new(program).args(args).output()
}

fn run_c_locale(program: &str, args: &[&str]) -> std::io::Result<Output> {
    Command::new(program)
        .args(args)
        .env("LC_ALL", "C")
        .env("LANG", "C")
        .output()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persistent_enablement_merge_preserves_existing_extensions_and_is_idempotent() {
        assert_eq!(
            merge_extension_uuid(&["other@example.org".to_owned()]),
            ["other@example.org", EXTENSION_UUID]
        );
        assert_eq!(
            merge_extension_uuid(&[EXTENSION_UUID.to_owned()]),
            [EXTENSION_UUID]
        );
    }

    #[test]
    fn gsettings_array_round_trips_preserved_values() {
        let original = vec!["one@example.org".to_owned(), "quote'and\\slash".to_owned()];
        assert_eq!(
            parse_string_array(&encode_string_array(&original)),
            Some(original)
        );
        assert_eq!(parse_string_array("@as []"), Some(Vec::new()));
    }

    #[test]
    fn c_locale_extension_info_parser_reads_only_the_state_field() {
        assert_eq!(
            parse_extension_state("Name: Voisu Overlay\nState: ACTIVE\n"),
            Some(ExtensionInfoState::Active)
        );
        assert_eq!(parse_extension_state("Estado: ACTIVE\n"), None);
        assert_eq!(
            parse_extension_state("State: OUT_OF_DATE\n"),
            Some(ExtensionInfoState::OutOfDate)
        );
        assert_eq!(
            parse_extension_state("State: DISABLED\n"),
            Some(ExtensionInfoState::Disabled)
        );
        assert_eq!(
            parse_extension_state("State: ERROR\n"),
            Some(ExtensionInfoState::Error)
        );
    }

    #[test]
    fn restart_is_reserved_for_a_persisted_extension_not_loaded_by_this_shell() {
        assert_eq!(
            classify_runtime_state(&ExtensionRuntimeState::NotLoaded, false),
            (
                LiveState::RestartRequired,
                TriggerState::RestartRequired,
                None
            )
        );
        for state in [
            ExtensionInfoState::Disabled,
            ExtensionInfoState::Error,
            ExtensionInfoState::OutOfDate,
        ] {
            let (overlay, trigger, detail) =
                classify_runtime_state(&ExtensionRuntimeState::Known(state), false);
            assert_eq!(overlay, LiveState::Unavailable);
            assert_eq!(trigger, TriggerState::NotInstalled);
            assert!(detail.is_some());
        }
    }
}
