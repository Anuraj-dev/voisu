//! Runtime display-session detection and the tool choices that follow from it.
//!
//! Mint and Ubuntu users pick X11 or Wayland at the login screen, so the answer
//! can change between one daemon start and the next. Detection therefore runs
//! per invocation and is never persisted. The logic here is pure over injected
//! facts (which display variables are set, what `XDG_SESSION_TYPE` claims) so it
//! is testable with neither a compositor nor a display — the same adapter
//! discipline the overlay already follows.

/// The kind of display server the current login session is running.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionKind {
    Wayland,
    X11,
    Unknown,
}

/// The resolved session plus whether a Wayland session is presenting through
/// XWayland (a declared Wayland session with no Wayland display but a live X11
/// one). Callers that only care about which clipboard/tool to run read
/// `session`; the overlay additionally reports the XWayland fallback.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SessionResolution {
    pub session: SessionKind,
    pub xwayland_fallback: bool,
}

/// Resolve the session from the three facts that decide it: the value (if any)
/// of the Wayland display variable, the X11 display variable, and
/// `XDG_SESSION_TYPE`. This is the single source of truth lifted out of the
/// overlay so the clipboard backend, the feedback ladder, and `doctor` all
/// agree.
///
/// An **empty** variable is treated as absent — a set-but-empty `DISPLAY` names
/// no endpoint. A session that a variable **declares** but whose display
/// endpoint is missing resolves to `Unknown`, not to that kind: an X11 service
/// with `XDG_SESSION_TYPE=x11` but no `DISPLAY` has no X server to reach, so
/// committing to `xclip` there would fail with no fallback. `Unknown` correctly
/// makes callers try every backend.
pub fn resolve_session(
    wayland_display: Option<&str>,
    x11_display: Option<&str>,
    session_type: Option<&str>,
) -> SessionResolution {
    let wayland = is_present(wayland_display);
    let x11 = is_present(x11_display);
    let declared = session_type.filter(|value| !value.is_empty());
    let declared_wayland = matches!(declared, Some(value) if value.eq_ignore_ascii_case("wayland"));
    let declared_x11 = matches!(declared, Some(value) if value.eq_ignore_ascii_case("x11"));

    // A declared-Wayland session with no Wayland display but a live X11 one is
    // presenting through XWayland — an X11 endpoint that is real to reach.
    let xwayland_fallback = declared_wayland && !wayland && x11;

    let session = if declared_wayland {
        if wayland {
            SessionKind::Wayland
        } else if x11 {
            SessionKind::X11
        } else {
            SessionKind::Unknown
        }
    } else if declared_x11 {
        if x11 {
            SessionKind::X11
        } else {
            SessionKind::Unknown
        }
    } else if wayland {
        SessionKind::Wayland
    } else if x11 {
        SessionKind::X11
    } else {
        SessionKind::Unknown
    };
    SessionResolution { session, xwayland_fallback }
}

/// A variable counts as present only when it is set to a non-empty value.
fn is_present(value: Option<&str>) -> bool {
    value.is_some_and(|value| !value.is_empty())
}

/// A clipboard command-line tool. Kept as a subprocess boundary (never moved
/// in-process) so the whole adapter layer and its restricted-command discipline
/// carry over unchanged.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClipboardTool {
    /// `wl-copy` / `wl-paste` from `wl-clipboard` — the Wayland stack.
    WlClipboard,
    /// `xclip` — the X11 stack.
    Xclip,
}

impl ClipboardTool {
    /// The program and arguments that WRITE stdin to the clipboard, owning the
    /// selection until a later writer or process exit. `wl-copy` takes `--` so
    /// a Transcript that begins with `-` is never mistaken for an option.
    pub const fn write_command(self) -> (&'static str, &'static [&'static str]) {
        match self {
            Self::WlClipboard => ("wl-copy", &["--"]),
            Self::Xclip => ("xclip", &["-selection", "clipboard", "-in"]),
        }
    }

    /// The program and arguments that READ the clipboard to stdout, used by the
    /// doctor round-trip probe.
    pub const fn read_command(self) -> (&'static str, &'static [&'static str]) {
        match self {
            Self::WlClipboard => ("wl-paste", &["--no-newline"]),
            Self::Xclip => ("xclip", &["-selection", "clipboard", "-out"]),
        }
    }

    /// The distribution package that provides this tool, for doctor remediation.
    pub const fn install_package(self) -> &'static str {
        match self {
            Self::WlClipboard => "wl-clipboard",
            Self::Xclip => "xclip",
        }
    }
}

/// The clipboard tools to try, in order, for a session. Wayland and X11 each
/// have a single answer; an Unknown session tries Wayland first, then X11,
/// before failing with a named reason.
pub fn clipboard_candidates(session: SessionKind) -> &'static [ClipboardTool] {
    match session {
        SessionKind::Wayland => &[ClipboardTool::WlClipboard],
        SessionKind::X11 => &[ClipboardTool::Xclip],
        SessionKind::Unknown => &[ClipboardTool::WlClipboard, ClipboardTool::Xclip],
    }
}

/// A host package manager, detected only to print (never run) the exact install
/// command in doctor remediation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PackageManager {
    Apt,
    Dnf,
    Pacman,
    Zypper,
}

impl PackageManager {
    /// The binary whose presence on `PATH` identifies this manager.
    pub const fn probe_binary(self) -> &'static str {
        match self {
            Self::Apt => "apt",
            Self::Dnf => "dnf",
            Self::Pacman => "pacman",
            Self::Zypper => "zypper",
        }
    }

    /// The install command a user would run, printed verbatim in doctor output.
    pub fn install_command(self, package: &str) -> String {
        match self {
            Self::Apt => format!("sudo apt install {package}"),
            Self::Dnf => format!("sudo dnf install {package}"),
            Self::Pacman => format!("sudo pacman -S {package}"),
            Self::Zypper => format!("sudo zypper install {package}"),
        }
    }
}

/// The managers doctor probes for, in the order it prefers them.
pub const PACKAGE_MANAGERS: [PackageManager; 4] = [
    PackageManager::Apt,
    PackageManager::Dnf,
    PackageManager::Pacman,
    PackageManager::Zypper,
];

/// A generic install instruction when no package manager could be identified,
/// so remediation is never empty.
pub fn install_instruction(manager: Option<PackageManager>, package: &str) -> String {
    match manager {
        Some(manager) => manager.install_command(package),
        None => format!("install {package} with your package manager"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_session_type_wins_over_display_variables() {
        assert_eq!(
            resolve_session(Some(":1"), Some(":0"), Some("x11")).session,
            SessionKind::X11
        );
        assert_eq!(
            resolve_session(Some("wayland-0"), Some(":0"), Some("wayland")).session,
            SessionKind::Wayland
        );
    }

    #[test]
    fn display_variables_decide_when_session_type_is_unset() {
        assert_eq!(
            resolve_session(Some("wayland-0"), None, None).session,
            SessionKind::Wayland
        );
        assert_eq!(
            resolve_session(None, Some(":0"), None).session,
            SessionKind::X11
        );
        assert_eq!(
            resolve_session(None, None, None).session,
            SessionKind::Unknown
        );
    }

    #[test]
    fn declared_wayland_without_a_wayland_display_falls_back_to_x11() {
        let resolution = resolve_session(None, Some(":0"), Some("wayland"));
        assert_eq!(resolution.session, SessionKind::X11);
        assert!(resolution.xwayland_fallback);
    }

    #[test]
    fn empty_values_are_treated_as_absent() {
        // A set-but-empty DISPLAY names no X server; with an empty Wayland
        // display too, nothing is reachable.
        assert_eq!(
            resolve_session(Some(""), Some(""), None).session,
            SessionKind::Unknown
        );
        // Empty Wayland display but a real X11 one is an X11 session.
        assert_eq!(
            resolve_session(Some(""), Some(":0"), None).session,
            SessionKind::X11
        );
    }

    #[test]
    fn a_declared_session_missing_its_display_endpoint_is_unknown() {
        // XDG_SESSION_TYPE=x11 but no DISPLAY: no X server to reach, so
        // committing to xclip would fail with no fallback. Unknown makes the
        // caller try every backend instead.
        assert_eq!(
            resolve_session(None, None, Some("x11")).session,
            SessionKind::Unknown
        );
        // XDG_SESSION_TYPE=wayland but neither display advertised: likewise
        // Unknown, and never an XWayland fallback (no X11 endpoint to use).
        let resolution = resolve_session(None, None, Some("wayland"));
        assert_eq!(resolution.session, SessionKind::Unknown);
        assert!(!resolution.xwayland_fallback);
    }

    #[test]
    fn clipboard_candidates_follow_the_session() {
        assert_eq!(
            clipboard_candidates(SessionKind::Wayland),
            &[ClipboardTool::WlClipboard]
        );
        assert_eq!(
            clipboard_candidates(SessionKind::X11),
            &[ClipboardTool::Xclip]
        );
        assert_eq!(
            clipboard_candidates(SessionKind::Unknown),
            &[ClipboardTool::WlClipboard, ClipboardTool::Xclip]
        );
    }

    #[test]
    fn clipboard_argv_is_stable_per_tool() {
        assert_eq!(ClipboardTool::WlClipboard.write_command(), ("wl-copy", &["--"][..]));
        assert_eq!(
            ClipboardTool::WlClipboard.read_command(),
            ("wl-paste", &["--no-newline"][..])
        );
        assert_eq!(
            ClipboardTool::Xclip.write_command(),
            ("xclip", &["-selection", "clipboard", "-in"][..])
        );
        assert_eq!(
            ClipboardTool::Xclip.read_command(),
            ("xclip", &["-selection", "clipboard", "-out"][..])
        );
    }

    #[test]
    fn install_commands_are_manager_specific() {
        assert_eq!(
            PackageManager::Apt.install_command("xclip"),
            "sudo apt install xclip"
        );
        assert_eq!(
            PackageManager::Dnf.install_command("xclip"),
            "sudo dnf install xclip"
        );
        assert_eq!(
            PackageManager::Pacman.install_command("xclip"),
            "sudo pacman -S xclip"
        );
        assert_eq!(
            PackageManager::Zypper.install_command("xclip"),
            "sudo zypper install xclip"
        );
    }

    #[test]
    fn install_instruction_falls_back_when_no_manager_is_known() {
        assert_eq!(
            install_instruction(None, "xclip"),
            "install xclip with your package manager"
        );
        assert_eq!(
            install_instruction(Some(PackageManager::Apt), "xclip"),
            "sudo apt install xclip"
        );
    }
}
