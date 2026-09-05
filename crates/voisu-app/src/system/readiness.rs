// System readiness inspection (doctor): session, audio, clipboard, secrets, portals, daemon.
//
// Split out of system.rs as a pure move; module-global items come from `super`.

use super::*;

#[derive(Default)]
pub struct FedoraReadiness {
    /// Last Status handshake from `inspect`. Doctor reuses it so a hung
    /// socket is not held for a second PROCESS_DEADLINE.
    daemon_status: Option<Response>,
}

impl FedoraReadiness {
    pub fn daemon_status(&self) -> Option<&Response> {
        self.daemon_status.as_ref()
    }
}

impl ReadinessInspector for FedoraReadiness {
    fn inspect(&mut self) -> Vec<ReadinessFinding> {
        self.daemon_status = daemon_status_response();
        if let Some(value) = std::env::var_os("VOISU_TEST_READINESS") {
            return controlled_readiness(&value.to_string_lossy(), self.daemon_status.as_ref());
        }
        let mut findings = vec![
            session_finding(),
            pipewire_finding(),
            microphone_finding(),
            portals_finding(),
            clipboard_finding(),
            secret_service_finding(),
            daemon_finding(self.daemon_status.as_ref()),
        ];
        // Appended only when it can demonstrate a problem, so the common case
        // stays quiet and the golden table is unaffected.
        if let Some(finding) = service_display_env_finding() {
            findings.push(finding);
        }
        findings
    }
}

/// The first executable of `program` found on `PATH`, honoring `PATH` order so
/// the resolved path is the one a spawned helper would actually run.
fn first_on_path(program: &str) -> Option<PathBuf> {
    resolve_on_path(&std::env::var_os("PATH")?, program)
}

/// Pure `PATH` resolution over an injected `PATH` value, so it is testable
/// without mutating the process environment.
pub(super) fn resolve_on_path(path: &std::ffi::OsStr, program: &str) -> Option<PathBuf> {
    std::env::split_paths(path)
        .map(|dir| dir.join(program))
        .find(|candidate| {
            fs::metadata(candidate)
                .map(|meta| meta.is_file() && meta.mode() & 0o111 != 0)
                .unwrap_or(false)
        })
}

/// The host package manager, identified by which of the known binaries appears
/// on `PATH`. Detected only to print the correct install command — never run.
fn detect_package_manager() -> Option<PackageManager> {
    PACKAGE_MANAGERS
        .into_iter()
        .find(|manager| first_on_path(manager.probe_binary()).is_some())
}

/// A desktop label for the Session value column, e.g. `X11 (Cinnamon)`.
fn session_value(resolution: SessionResolution) -> String {
    let session = match resolution.session {
        SessionKind::Wayland => "Wayland",
        SessionKind::X11 if resolution.xwayland_fallback => "X11 (XWayland)",
        SessionKind::X11 => "X11",
        SessionKind::Unknown => "unknown",
    };
    match std::env::var("XDG_CURRENT_DESKTOP")
        .ok()
        .filter(|value| !value.is_empty())
    {
        Some(desktop) => format!("{session} ({desktop})"),
        None => session.to_owned(),
    }
}

/// Whether the systemd `--user` manager environment advertises `key` with a
/// non-empty value.
pub(super) fn manager_env_has(show_environment: &str, key: &str) -> bool {
    let prefix = format!("{key}=");
    show_environment
        .lines()
        .filter_map(|line| line.strip_prefix(&prefix))
        .any(|value| !value.is_empty())
}

/// A doctor diagnosis for the systemd-user-service delivery gap: the daemon runs
/// under the `--user` manager, and if that manager never imported the graphical
/// session's display variables, Delivery cannot reach the X/Wayland server even
/// though the interactive CLI can. Returns a WARN only when the manager clearly
/// lacks this session's display variable; on an undetermined session, an
/// unreachable manager, or a manager that already has it, no row is produced.
fn service_display_env_finding() -> Option<ReadinessFinding> {
    // The variables the daemon's clipboard helper needs for THIS session. The
    // display endpoint depends on the session; XAUTHORITY is additionally
    // required whenever this CLI has a non-default one, since without it an X11
    // helper (xclip, or an XWayland fallback) cannot authenticate to the server.
    let mut needed: Vec<&str> = match current_session().session {
        SessionKind::Wayland => vec!["WAYLAND_DISPLAY"],
        SessionKind::X11 => vec!["DISPLAY"],
        SessionKind::Unknown => return None,
    };
    if std::env::var("XAUTHORITY").is_ok_and(|value| !value.is_empty()) {
        needed.push("XAUTHORITY");
    }
    let outcome = run_restricted("systemctl", &["--user", "show-environment"], None, true).ok()?;
    if !outcome.success {
        return None;
    }
    let show_environment = String::from_utf8_lossy(&outcome.stdout);
    let missing: Vec<&str> = needed
        .into_iter()
        .filter(|key| !manager_env_has(&show_environment, key))
        .collect();
    if missing.is_empty() {
        return None;
    }
    let names = missing.join(", ");
    Some(
        readiness(
            ReadinessCapability::ServiceEnvironment,
            ReadinessStatus::Warn,
            &format!(
                "the systemd --user manager is missing {names}, so Delivery from the daemon \
                 cannot reach or authenticate to the display; run `voisu service restart` from \
                 your graphical session (or `systemctl --user import-environment {}`)",
                missing.join(" ")
            ),
        )
        .with_value("missing display env"),
    )
}

/// The Session check: which display server this login is running. Both Wayland
/// and X11 are fully supported, so a cleanly detected session passes; only a
/// session that cannot be determined warns.
fn session_finding() -> ReadinessFinding {
    let resolution = current_session();
    let value = session_value(resolution);
    match resolution.session {
        SessionKind::Wayland | SessionKind::X11 => ReadinessFinding::new(
            ReadinessCapability::Session,
            ReadinessStatus::Pass,
            "display session detected; the matching clipboard backend is selected at runtime",
        )
        .with_value(value),
        SessionKind::Unknown => readiness(
            ReadinessCapability::Session,
            ReadinessStatus::Warn,
            "could not determine the display session; clipboard Delivery will try Wayland then X11. Log in to a graphical session (Wayland or X11)",
        )
        .with_value(value),
    }
}

/// Parse the PipeWire version `pw-record --help` reports in its
/// `Compiled with libpipewire X.Y.Z` banner. Best-effort: absent on some builds.
fn pw_record_version() -> Option<String> {
    let outcome = run_restricted("pw-record", &["--help"], None, true).ok()?;
    let text = String::from_utf8_lossy(&outcome.stdout).into_owned()
        + &String::from_utf8_lossy(&outcome.stderr);
    let marker = "libpipewire ";
    let start = text.find(marker)? + marker.len();
    let version: String = text[start..]
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    (!version.is_empty()).then_some(version)
}

/// The install command for `pw-record` (the recorder), whose package name
/// differs per distribution. Printed, never run.
fn pw_record_install_command() -> String {
    match detect_package_manager() {
        Some(PackageManager::Apt) => "sudo apt install pipewire-bin".to_owned(),
        Some(PackageManager::Dnf) => "sudo dnf install pipewire-utils".to_owned(),
        Some(PackageManager::Pacman) => "sudo pacman -S pipewire".to_owned(),
        Some(PackageManager::Zypper) => "sudo zypper install pipewire-tools".to_owned(),
        None => "install pipewire (pw-record) with your package manager".to_owned(),
    }
}

/// The PipeWire check. `pw-record` absent is a hard FAIL naming the package —
/// Voisu cannot capture without it, and a responding pw-cli must not mask that.
/// Otherwise the status comes from whether the PipeWire core answers, and the
/// value column carries the detected version and the capture path (`--raw`
/// headerless PCM, or the WAV-container fallback for PipeWire < 1.1).
fn pipewire_finding() -> ReadinessFinding {
    let mode = pw_record_capture_mode();
    let version = pw_record_version();
    if mode == PwRecordProbe::Unavailable {
        return ReadinessFinding::new(
            ReadinessCapability::PipeWire,
            ReadinessStatus::Fail,
            "pw-record is not available, so Voisu cannot capture audio",
        )
        .with_value("pw-record missing")
        .with_action(pw_record_install_command());
    }
    let responds =
        run_restricted("pw-cli", &["info", "0"], None, false).is_ok_and(|outcome| outcome.success);
    let path = if mode == PwRecordProbe::Raw {
        "raw"
    } else {
        "WAV fallback"
    };
    let value = match version {
        Some(version) => format!("{version} ({path})"),
        None => format!("({path})"),
    };
    let detail = if mode == PwRecordProbe::Raw {
        "PipeWire core responds; pw-record --raw yields headerless PCM"
    } else {
        "PipeWire core responds; pw-record lacks --raw, so the WAV container is unwrapped to PCM"
    };
    if responds {
        ReadinessFinding::new(ReadinessCapability::PipeWire, ReadinessStatus::Pass, detail)
            .with_value(value)
    } else {
        ReadinessFinding::new(
            ReadinessCapability::PipeWire,
            ReadinessStatus::Fail,
            "PipeWire core does not respond",
        )
        .with_value(value)
        .with_action("start PipeWire and WirePlumber")
    }
}

fn controlled_readiness(value: &str, daemon_status: Option<&Response>) -> Vec<ReadinessFinding> {
    // Host-independent findings so the doctor-output golden test is stable
    // everywhere: no real probes, no package-manager detection.
    let mut findings = vec![
        readiness(
            ReadinessCapability::Session,
            ReadinessStatus::Pass,
            "display session detected",
        )
        .with_value("Wayland (KDE)"),
        readiness(
            ReadinessCapability::PipeWire,
            ReadinessStatus::Pass,
            "PipeWire core responds",
        )
        .with_value("1.4.11 (raw)"),
        readiness(
            ReadinessCapability::Microphone,
            ReadinessStatus::Pass,
            "default source available",
        ),
        readiness(
            ReadinessCapability::Portals,
            ReadinessStatus::Pass,
            "desktop portal responds",
        ),
        readiness(
            ReadinessCapability::Clipboard,
            ReadinessStatus::Pass,
            "clipboard roundtrip succeeds",
        ),
        readiness(
            ReadinessCapability::SecretStorage,
            ReadinessStatus::Pass,
            "Secret Service responds",
        ),
        daemon_finding(daemon_status),
    ];
    if value == "pass" {
        return findings;
    }
    for override_value in value.split(',') {
        let Some((capability, status)) = override_value.split_once('=') else {
            continue;
        };
        let (status, detail, action) = match status {
            "warn" => (
                ReadinessStatus::Warn,
                "needs attention; see remediation",
                None,
            ),
            "fail" => (
                ReadinessStatus::Fail,
                "not available; see remediation",
                Some("run the printed remediation command"),
            ),
            _ => continue,
        };
        if let Some(finding) = findings.iter_mut().find(|finding| {
            matches!(
                (capability, finding.capability),
                ("session", ReadinessCapability::Session)
                    | ("pipewire", ReadinessCapability::PipeWire)
                    | ("microphone", ReadinessCapability::Microphone)
                    | ("portals", ReadinessCapability::Portals)
                    | ("clipboard", ReadinessCapability::Clipboard)
                    | ("secret-storage", ReadinessCapability::SecretStorage)
                    | ("daemon", ReadinessCapability::Daemon)
            )
        }) {
            finding.status = status;
            finding.detail = detail.to_owned();
            finding.action = action.map(str::to_owned);
        }
    }
    // The Service-env row is appended by the real inspector only when a problem
    // is detected, so it is not in the base list. A `service-env=warn` override
    // synthesizes it here to exercise its formatting and diagnosis hermetically.
    for override_value in value.split(',') {
        if let Some(("service-env", "warn")) = override_value.split_once('=') {
            findings.push(
                readiness(
                    ReadinessCapability::ServiceEnvironment,
                    ReadinessStatus::Warn,
                    "the systemd --user manager is missing WAYLAND_DISPLAY, XAUTHORITY, so Delivery from the daemon cannot reach or authenticate to the display; run `voisu service restart`",
                )
                .with_value("missing display env"),
            );
        }
    }
    findings
}

fn microphone_finding() -> ReadinessFinding {
    match run_restricted("wpctl", &["inspect", "@DEFAULT_AUDIO_SOURCE@"], None, true) {
        Ok(outcome) if outcome.success => readiness(
            ReadinessCapability::Microphone,
            ReadinessStatus::Pass,
            "default source available",
        ),
        // WARN carries no action line (that is reserved for FAIL); the
        // remediation lives in the reasoning, shown under --verbose.
        Ok(_) => readiness(
            ReadinessCapability::Microphone,
            ReadinessStatus::Warn,
            "no default microphone is set; connect one and set it as the default source",
        ),
        Err(_) => ReadinessFinding::new(
            ReadinessCapability::Microphone,
            ReadinessStatus::Fail,
            "WirePlumber is unavailable",
        )
        .with_action("start PipeWire and WirePlumber"),
    }
}

/// Hand-written clipboard wrappers (from a workaround guide) that precede the
/// packaged tools on `PATH`. On a Wayland login they silently reroute the
/// Wayland clipboard through the wrong backend and break it. Detected as a
/// `wl-copy`/`wl-paste` that resolves under `$HOME` rather than a system bin;
/// each is reported by its exact path so remediation removes only what shadows.
fn shadowing_clipboard_wrappers() -> Vec<PathBuf> {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return Vec::new();
    };
    ["wl-copy", "wl-paste"]
        .into_iter()
        .filter_map(first_on_path)
        .filter(|winner| winner.starts_with(&home))
        .collect()
}

/// POSIX single-quote a path for a copy-pasteable shell command, quoting only
/// when the path contains characters a shell would interpret.
fn shell_quote(path: &Path) -> String {
    let text = path.to_string_lossy();
    let safe = !text.is_empty()
        && text
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "._-/".contains(character));
    if safe {
        text.into_owned()
    } else {
        format!("'{}'", text.replace('\'', r"'\''"))
    }
}

/// The outcome of round-tripping the clipboard through one backend tool.
pub(super) enum ClipboardProbe {
    /// The round trip worked and the prior clipboard was restored (or there was
    /// nothing to restore).
    WorkedRestored,
    /// The round trip worked but the prior clipboard could not be restored.
    WorkedNotRestored,
    /// The tool binary is not installed (its write could not even be spawned).
    ToolMissing,
    /// The tool ran but the round trip failed — no reachable display or the
    /// selection never took.
    Failed,
}

/// Round-trip the clipboard through one tool. The probe is WRITTEN first, which
/// also establishes selection ownership, so a read-back succeeds even on a
/// previously empty or ownerless clipboard (`xclip -out` errors with no owner).
/// The prior value is preserved only when the initial read genuinely returned
/// one; a failed initial read is treated as "empty", never as "unavailable".
fn probe_clipboard_roundtrip(tool: ClipboardTool) -> ClipboardProbe {
    let (read_program, read_arguments) = tool.read_command();
    let (write_program, write_arguments) = tool.write_command();

    let original = match run_restricted(read_program, read_arguments, None, true) {
        Ok(outcome) if outcome.success => Some(outcome.stdout),
        _ => None,
    };

    let probe = format!("voisu-readiness-{}", std::process::id());
    probe_clipboard_roundtrip_with(
        original,
        probe.as_bytes(),
        |value| {
            run_restricted_serving(write_program, write_arguments, Some(value))
                .map(|outcome| outcome.success)
        },
        || {
            run_restricted(read_program, read_arguments, None, true)
                .ok()
                .filter(|outcome| outcome.success)
                .map(|outcome| outcome.stdout == probe.as_bytes())
                .unwrap_or(false)
        },
    )
}

pub(super) fn probe_clipboard_roundtrip_with<F, R>(
    original: Option<Vec<u8>>,
    probe: &[u8],
    write: F,
    readback_succeeds: R,
) -> ClipboardProbe
where
    F: Fn(&[u8]) -> Result<bool, ProcessError>,
    R: Fn() -> bool,
{
    // Restore the prior value only if there genuinely was one; writing an empty
    // string back would install an empty clipboard owner where none existed.
    let restore = || {
        original
            .as_deref()
            .is_none_or(|original| write(original).is_ok_and(|success| success))
    };

    match write(probe) {
        Ok(true) => {}
        // A spawn failure is the only definitive "tool is not installed" signal.
        Err(ProcessError::Unavailable) => return ClipboardProbe::ToolMissing,
        Ok(false) | Err(_) => {
            let _ = restore();
            return ClipboardProbe::Failed;
        }
    }

    if !readback_succeeds() {
        let _ = restore();
        return ClipboardProbe::Failed;
    }

    match original.as_deref() {
        Some(original) => match write(original) {
            Ok(true) => ClipboardProbe::WorkedRestored,
            Ok(false) | Err(_) => ClipboardProbe::WorkedNotRestored,
        },
        None => ClipboardProbe::WorkedRestored,
    }
}

/// The Clipboard check. It round-trips through the backend that matches the
/// detected session (`wl-copy`/`wl-paste` on Wayland, `xclip` on X11; an Unknown
/// session tries each in turn), and on failure prescribes the exact install
/// command for the host package manager.
fn clipboard_finding() -> ReadinessFinding {
    let resolution = current_session();
    if resolution.session == SessionKind::Wayland
        && let Some(finding) = shadowed_wl_clipboard_finding()
    {
        return finding;
    }

    clipboard_finding_for_candidates(clipboard_candidates(resolution.session))
}

fn clipboard_finding_for_backend(tool: ClipboardTool) -> ReadinessFinding {
    if tool == ClipboardTool::WlClipboard
        && let Some(finding) = shadowed_wl_clipboard_finding()
    {
        return finding;
    }
    clipboard_finding_for_candidates(std::slice::from_ref(&tool))
}

fn shadowed_wl_clipboard_finding() -> Option<ReadinessFinding> {
    // A shadowing wrapper is a Wayland-only hazard (harmless on X11). Per the
    // terseness contract the remediation lives in the reasoning (--verbose),
    // naming only the exact shadowing paths, shell-quoted.
    let shadows = shadowing_clipboard_wrappers();
    if shadows.is_empty() {
        return None;
    }
    let names = shadows
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(" and ");
    let removal = shadows
        .iter()
        .map(|path| shell_quote(path))
        .collect::<Vec<_>>()
        .join(" ");
    Some(
        ReadinessFinding::new(
            ReadinessCapability::Clipboard,
            ReadinessStatus::Warn,
            format!(
                "{names} shadow the packaged wl-clipboard on PATH and reroute the Wayland \
                 clipboard through the wrong backend; remove with: rm {removal}"
            ),
        )
        .with_value("shadowed wrapper"),
    )
}

fn clipboard_finding_for_candidates(candidates: &[ClipboardTool]) -> ReadinessFinding {
    let mut a_tool_was_present = false;
    for tool in candidates {
        match probe_clipboard_roundtrip(*tool) {
            ClipboardProbe::WorkedRestored => {
                return readiness(
                    ReadinessCapability::Clipboard,
                    ReadinessStatus::Pass,
                    "clipboard roundtrip succeeds and the prior clipboard was restored",
                );
            }
            ClipboardProbe::WorkedNotRestored => {
                return readiness(
                    ReadinessCapability::Clipboard,
                    ReadinessStatus::Warn,
                    "clipboard roundtrip succeeds but the prior clipboard could not be restored",
                );
            }
            // Present-but-broken and missing both continue to the next
            // candidate; only the final message distinguishes them.
            ClipboardProbe::Failed => {
                a_tool_was_present = true;
            }
            ClipboardProbe::ToolMissing => {}
        }
    }

    let primary = candidates
        .first()
        .copied()
        .unwrap_or(ClipboardTool::WlClipboard);
    let detail = if a_tool_was_present {
        "the clipboard tool ran but the roundtrip failed — no reachable display or selection owner"
    } else {
        "no clipboard backend is installed for this session"
    };
    ReadinessFinding::new(
        ReadinessCapability::Clipboard,
        ReadinessStatus::Fail,
        detail,
    )
    .with_action(install_instruction(
        detect_package_manager(),
        primary.install_package(),
    ))
}

/// Read-only check that `tool` can reach the current display.
///
/// Used by the daemon at start so `clipboard_usable` is not inferred from a
/// display env var plus an executable on `PATH`. The probe never writes, so it
/// cannot clobber the user clipboard. An empty selection still counts as
/// usable; a connect-failure does not.
pub fn clipboard_backend_display_reachable(tool: ClipboardTool) -> bool {
    let (program, arguments) = tool.read_command();
    match run_restricted(program, arguments, None, false) {
        Ok(outcome) => clipboard_read_proves_display(outcome.success, &outcome.stderr),
        Err(_) => false,
    }
}

pub(super) fn clipboard_read_proves_display(success: bool, stderr: &[u8]) -> bool {
    let stderr = String::from_utf8_lossy(stderr).to_ascii_lowercase();
    if clipboard_connect_failed(&stderr) {
        return false;
    }
    success || clipboard_empty_selection(&stderr)
}

fn clipboard_connect_failed(stderr: &str) -> bool {
    stderr.contains("failed to connect")
        || stderr.contains("can't open display")
        || stderr.contains("cannot open display")
        || stderr.contains("unable to open display")
        || stderr.contains("unable to connect")
        || stderr.contains("could not connect")
        || stderr.contains("connection refused")
}

fn clipboard_empty_selection(stderr: &str) -> bool {
    stderr.contains("nothing is copied")
        || stderr.contains("clipboard is empty")
        || stderr.contains("no mime type")
        || stderr.contains("target string not available")
        || stderr.contains("no owner")
}

/// Verifies the clipboard backend that the daemon's Delivery adapters use for
/// Transcript preservation. On Wayland this probes wl-copy/wl-paste
/// specifically; an installed xclip cannot make a missing or broken
/// wl-clipboard backend usable.
pub fn verify_clipboard_delivery() -> Result<(), String> {
    // Hyprland's Clipboard, Type, and Guarded adapters all preserve through
    // WlClipboard. Do not use the doctor's Unknown-session fallback here:
    // xclip being installed cannot satisfy a missing wl-copy/wl-paste pair.
    let finding = clipboard_finding_for_backend(ClipboardTool::WlClipboard);
    if clipboard_finding_is_usable(&finding) {
        return Ok(());
    }

    let action = finding
        .action
        .map(|action| format!(" (install with {action})"))
        .unwrap_or_default();
    Err(format!(
        "selected Clipboard Delivery backend is not usable: {}{action}",
        finding.detail
    ))
}

pub(super) fn clipboard_finding_is_usable(finding: &ReadinessFinding) -> bool {
    matches!(finding.status, ReadinessStatus::Pass)
}

fn secret_service_finding() -> ReadinessFinding {
    // Probe a nonexistent attribute. On a healthy, unlocked keyring this exits
    // without a match and without diagnostics: reaching the service cleanly is
    // the readiness signal, not whether a credential was found. Real secret-tool
    // reports a no-match with a nonzero exit and empty stdout/stderr, while a
    // D-Bus/service failure or a locked keyring prints an error to stderr.
    let probe = std::process::id().to_string();
    match run_restricted(
        "secret-tool",
        &["lookup", "voisu-doctor-probe", &probe],
        None,
        false,
    ) {
        Ok(outcome) if outcome.success || outcome.stderr.is_empty() => readiness(
            ReadinessCapability::SecretStorage,
            ReadinessStatus::Pass,
            "Secret Service is reachable",
        ),
        Ok(_) => readiness(
            ReadinessCapability::SecretStorage,
            ReadinessStatus::Warn,
            "Secret Service reported an error; unlock the keyring or log in to the desktop session",
        ),
        Err(_) => ReadinessFinding::new(
            ReadinessCapability::SecretStorage,
            ReadinessStatus::Fail,
            "Secret Service is unavailable",
        )
        .with_action("start or unlock the desktop keyring"),
    }
}

/// The Portals readiness check. A portal that does not answer at all fails
/// closed; a portal that answers but exposes no `GlobalShortcuts` interface
/// warns with the Hyprland remediation, because plain wlroots portals implement
/// no GlobalShortcuts and there is no desktop dialog to bind the Trigger Key —
/// the daemon can never receive an activation there until the user installs
/// xdg-desktop-portal-hyprland and declares the bind in hyprland.conf. Only a
/// portal that answers AND exposes GlobalShortcuts passes.
fn portals_finding() -> ReadinessFinding {
    let portal_up = run_restricted(
        "busctl",
        &["--user", "--no-pager", "status", PORTAL_BUS_NAME],
        None,
        false,
    )
    .is_ok_and(|outcome| outcome.success);
    if !portal_up {
        return ReadinessFinding::new(
            ReadinessCapability::Portals,
            ReadinessStatus::Fail,
            "the desktop portal does not respond, so the Trigger Key cannot bind",
        )
        .with_action("start xdg-desktop-portal in this desktop session");
    }
    if global_shortcuts_available() {
        readiness(
            ReadinessCapability::Portals,
            ReadinessStatus::Pass,
            "desktop portal responds",
        )
    } else {
        // Detection is kept; only the presentation is made terse. The full
        // reasoning moves to --verbose. On a desktop without portal
        // GlobalShortcuts (Cinnamon/X11, plain wlroots) the Trigger Key is bound
        // through a desktop Custom Shortcut running `voisu toggle`.
        // WARN carries no action line; the remediation (install the Hyprland
        // portal, or bind a desktop Custom Shortcut to `voisu toggle`) is in the
        // reasoning, shown under --verbose.
        readiness(
            ReadinessCapability::Portals,
            ReadinessStatus::Warn,
            "the desktop portal exposes no GlobalShortcuts interface, so Voisu cannot bind the Trigger Key itself; on Hyprland install xdg-desktop-portal-hyprland, and on Cinnamon/X11 bind a desktop Custom Shortcut to run: voisu toggle",
        )
    }
}

/// Whether `org.freedesktop.portal.GlobalShortcuts` is exposed on the desktop
/// portal. Reads its `version` property: the portal answers with the interface
/// version when it is implemented and fails when it is absent. This mirrors how
/// the codebase already talks to the portal over the session bus (via busctl),
/// staying in one subprocess convention rather than opening a second zbus edge
/// just for a probe.
fn global_shortcuts_available() -> bool {
    run_restricted(
        "busctl",
        &[
            "--user",
            "get-property",
            PORTAL_BUS_NAME,
            PORTAL_OBJECT_PATH,
            GLOBAL_SHORTCUTS_INTERFACE,
            "version",
        ],
        None,
        false,
    )
    .is_ok_and(|outcome| outcome.success)
}

fn daemon_finding(response: Option<&Response>) -> ReadinessFinding {
    if response.is_some() {
        return readiness(
            ReadinessCapability::Daemon,
            ReadinessStatus::Pass,
            "status handshake succeeds",
        );
    }
    // A daemon that was simply never started reads differently from a unit
    // systemd tried to run and could not: when the unit is in the failed state
    // (e.g. a namespace/exec failure that never reaches our handshake), point
    // the user at the journal rather than telling them to "start" a unit that
    // is already failing to start.
    if service_reports_failed() {
        ReadinessFinding::new(
            ReadinessCapability::Daemon,
            ReadinessStatus::Fail,
            "the daemon did not answer the status handshake and systemctl --user reports voisu.service failed",
        )
        .with_action("journalctl --user -u voisu.service")
    } else {
        ReadinessFinding::new(
            ReadinessCapability::Daemon,
            ReadinessStatus::Fail,
            "the daemon did not answer the status handshake",
        )
        .with_action("start voisu-daemon and run voisu doctor again")
    }
}

/// Whether systemd reports `voisu.service` in the failed state. A dedicated test
/// seam (`VOISU_TEST_SERVICE_FAILED`) keeps the doctor daemon check hermetic —
/// tests never depend on the host's real unit state.
fn service_reports_failed() -> bool {
    if let Some(value) = std::env::var_os("VOISU_TEST_SERVICE_FAILED") {
        return matches!(value.to_string_lossy().trim(), "1" | "failed");
    }
    crate::service::service_is_failed()
}

/// Query the daemon's status response with the same bounded framing used by
/// the basic daemon readiness check. The CLI uses the additive readiness
/// payload to compare the daemon's inherited session with its own.
pub fn daemon_status_response() -> Option<Response> {
    let path = socket_path().ok()?;
    let mut stream = UnixStream::connect(path).ok()?;
    // A single Instant budget bounds the whole handshake. A per-read timeout is
    // reset by every byte, so a peer trickling one byte per interval would hold
    // doctor forever; the accumulated response is also capped during reading so
    // an oversized frame can never be fully buffered before the cap is checked.
    let started = Instant::now();
    stream.set_write_timeout(Some(PROCESS_DEADLINE)).ok()?;
    serde_json::to_writer(&mut stream, &Request::new(DaemonCommand::Status)).ok()?;
    stream.write_all(b"\n").ok()?;
    let response = read_bounded_frame(&mut stream, started).ok()?;
    let envelope: VersionEnvelope = serde_json::from_str(&response).ok()?;
    let response: Response = serde_json::from_str(&response).ok()?;
    (envelope.version == PROTOCOL_VERSION && response.ok && response.state.is_some())
        .then_some(response)
}

fn read_bounded_frame(stream: &mut UnixStream, started: Instant) -> Result<String, ()> {
    let mut response = Vec::new();
    let mut buffer = [0_u8; 1024];
    loop {
        let remaining = PROCESS_DEADLINE
            .checked_sub(started.elapsed())
            .filter(|remaining| !remaining.is_zero())
            .ok_or(())?;
        stream.set_read_timeout(Some(remaining)).map_err(|_| ())?;
        match stream.read(&mut buffer) {
            Ok(0) => return Err(()),
            Ok(read) => {
                // Reject before appending: a flooding peer must never force an
                // allocation beyond the response cap.
                if response.len() + read > MAX_DAEMON_RESPONSE_BYTES {
                    return Err(());
                }
                response.extend_from_slice(&buffer[..read]);
                if response.ends_with(b"\n") {
                    return String::from_utf8(response).map_err(|_| ());
                }
                if response.contains(&b'\n') {
                    return Err(());
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                return Err(());
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return Err(()),
        }
    }
}

fn readiness(
    capability: ReadinessCapability,
    status: ReadinessStatus,
    detail: &str,
) -> ReadinessFinding {
    ReadinessFinding::new(capability, status, detail)
}
