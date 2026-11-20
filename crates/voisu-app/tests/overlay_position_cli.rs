//! `voisu overlay-position` shows, sets, resets and drags the Overlay's spot
//! through the real CLI. Move mode talks to a stand-in Overlay listening on
//! the move socket in a private temporary runtime directory.

use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::thread;
use std::time::{Duration, Instant};

use voisu_app::config::OverlayPosition;
use voisu_app::overlay::move_mode::{CancelReason, MoveListener, MoveReply};

struct Sandbox {
    config: tempfile::TempDir,
    runtime: tempfile::TempDir,
}

impl Sandbox {
    fn new() -> Self {
        let runtime = tempfile::tempdir().expect("runtime tempdir");
        // `runtime_dir` insists on a private directory.
        std::fs::set_permissions(
            runtime.path(),
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        )
        .expect("private runtime dir");
        Self {
            config: tempfile::tempdir().expect("config tempdir"),
            runtime,
        }
    }

    /// Runs the CLI in a KDE Plasma Wayland session.
    fn run(&self, arguments: &[&str]) -> Output {
        self.run_in(arguments, Some("wayland-0"), "wayland", "KDE")
    }

    /// Runs the CLI in the given session: `WAYLAND_DISPLAY` (unset when
    /// `None`) and `XDG_CURRENT_DESKTOP`.
    fn run_in(
        &self,
        arguments: &[&str],
        wayland: Option<&str>,
        session_type: &str,
        desktop: &str,
    ) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_voisu"));
        command
            .args(arguments)
            .env("XDG_CONFIG_HOME", self.config.path())
            .env("XDG_RUNTIME_DIR", self.runtime.path())
            .env("XDG_CURRENT_DESKTOP", desktop)
            .env("XDG_SESSION_TYPE", session_type)
            .env("DISPLAY", ":0");
        match wayland {
            Some(display) => command.env("WAYLAND_DISPLAY", display),
            None => command.env_remove("WAYLAND_DISPLAY"),
        };
        command.output().expect("run voisu CLI")
    }

    fn config_file(&self) -> PathBuf {
        self.config.path().join("voisu").join("config.toml")
    }

    /// The move socket a running Overlay would listen on.
    fn overlay_socket(&self) -> PathBuf {
        let dir = self
            .runtime
            .path()
            .join("voisu")
            .join(format!("v{}", voisu_core::PROTOCOL_VERSION));
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)
            .expect("private socket dir");
        dir.join("overlay.sock")
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// A stand-in Overlay that accepts one move and answers `replies` in order.
fn fake_overlay(
    socket: &Path,
    refusal: Option<&str>,
    replies: Vec<MoveReply>,
) -> thread::JoinHandle<()> {
    let listener = MoveListener::bind_at(socket, refusal.map(str::to_owned)).expect("listen");
    thread::spawn(move || {
        if replies.is_empty() {
            // A refusing listener answers on its own thread; keep it alive.
            thread::sleep(Duration::from_secs(3));
            return;
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut requester = loop {
            if let Some(requester) = listener.take() {
                break requester;
            }
            assert!(Instant::now() < deadline, "no move request arrived");
            thread::sleep(Duration::from_millis(5));
        };
        for reply in replies {
            assert!(requester.reply(&reply));
        }
    })
}

#[test]
fn overlay_position_shows_sets_and_resets_fractions_and_defaults_to_bottom_centre() {
    let sandbox = Sandbox::new();
    let fresh = sandbox.run(&["overlay-position"]);
    assert!(fresh.status.success(), "{fresh:?}");
    assert_eq!(stdout(&fresh), "Overlay position: 0.500,0.974\n");

    let set = sandbox.run(&["overlay-position", "0.25,0.1"]);
    assert!(set.status.success(), "{set:?}");
    assert_eq!(stdout(&set), "Overlay position set to 0.250,0.100.\n");
    let config = std::fs::read_to_string(sandbox.config_file()).expect("config written");
    assert!(
        config.contains("overlay_position = \"0.250,0.100\""),
        "{config}"
    );
    assert_eq!(
        stdout(&sandbox.run(&["overlay-position"])),
        "Overlay position: 0.250,0.100\n"
    );

    // The old preset words still work and land on the same spots.
    let top = sandbox.run(&["overlay-position", "top-right"]);
    assert!(top.status.success(), "{top:?}");
    assert_eq!(stdout(&top), "Overlay position set to 0.956,0.026.\n");

    let reset = sandbox.run(&["overlay-position", "reset"]);
    assert!(reset.status.success(), "{reset:?}");
    assert_eq!(
        stdout(&reset),
        "Overlay position reset to the bottom centre (0.500,0.974).\n"
    );
    assert_eq!(
        stdout(&sandbox.run(&["overlay-position"])),
        "Overlay position: 0.500,0.974\n"
    );

    for invalid in ["middle", "1.5,0.5", "0.5", "0.5, 0.5"] {
        let output = sandbox.run(&["overlay-position", invalid]);
        assert_eq!(output.status.code(), Some(2), "{invalid}: {output:?}");
        assert!(
            stderr(&output).contains("overlay-position takes move, reset, or a spot"),
            "{invalid}: {output:?}"
        );
    }
    assert_eq!(
        stdout(&sandbox.run(&["overlay-position"])),
        "Overlay position: 0.500,0.974\n",
        "a rejected value changes nothing"
    );
}

#[test]
fn a_hand_edited_bad_overlay_position_reads_as_the_default() {
    let sandbox = Sandbox::new();
    let dir = sandbox.config.path().join("voisu");
    std::fs::create_dir_all(&dir).expect("config dir");
    for bad in ["\"centre\"", "0.5", "\"0.5,2\"", "\"0.5,0.5"] {
        std::fs::write(
            dir.join("config.toml"),
            format!("overlay_position = {bad}\n"),
        )
        .expect("write config");
        let output = sandbox.run(&["overlay-position"]);
        assert!(output.status.success(), "{output:?}");
        assert_eq!(stdout(&output), "Overlay position: 0.500,0.974\n", "{bad}");
    }
}

#[test]
fn move_saves_where_the_overlay_says_the_pill_was_dropped() {
    let sandbox = Sandbox::new();
    let overlay = fake_overlay(
        &sandbox.overlay_socket(),
        None,
        vec![
            MoveReply::Moving,
            MoveReply::Save(OverlayPosition::from_fractions(0.25, 0.1).unwrap()),
        ],
    );
    let output = sandbox.run(&["overlay-position", "move"]);
    overlay.join().unwrap();
    assert!(output.status.success(), "{output:?}");
    let shown = stdout(&output);
    assert!(shown.contains("Drag the pill on screen"), "{shown}");
    assert!(
        shown.ends_with("Overlay position saved at 0.250,0.100.\n"),
        "{shown}"
    );
    assert_eq!(
        stdout(&sandbox.run(&["overlay-position"])),
        "Overlay position: 0.250,0.100\n"
    );
}

#[test]
fn a_cancelled_move_keeps_the_saved_spot() {
    let sandbox = Sandbox::new();
    let socket = sandbox.overlay_socket();
    for (reason, message) in [
        (
            CancelReason::User,
            "Move cancelled; the Overlay stays where it was.\n",
        ),
        (
            CancelReason::Timeout,
            "Move timed out after a minute; the Overlay stays where it was.\n",
        ),
        (
            CancelReason::Recording,
            "Move cancelled: a Recording started.\n",
        ),
        (
            CancelReason::Unconfirmed,
            "Move not saved: Voisu didn't answer, so a Recording may have started. \
             The Overlay stays where it was.\n",
        ),
    ] {
        let overlay = fake_overlay(
            &socket,
            None,
            vec![MoveReply::Moving, MoveReply::Cancel(reason)],
        );
        let output = sandbox.run(&["overlay-position", "move"]);
        overlay.join().unwrap();
        assert!(output.status.success(), "{output:?}");
        assert!(stdout(&output).ends_with(message), "{output:?}");
        assert!(!sandbox.config_file().exists(), "a cancel writes nothing");
    }
}

#[test]
fn move_without_a_usable_overlay_fails_clearly() {
    let sandbox = Sandbox::new();
    let missing = sandbox.run(&["overlay-position", "move"]);
    assert_eq!(missing.status.code(), Some(1), "{missing:?}");
    assert!(
        stderr(&missing).contains("The Overlay isn't running"),
        "{missing:?}"
    );
    assert!(
        stderr(&missing).contains("systemctl --user start voisu-overlay"),
        "{missing:?}"
    );
    // GNOME (the Shell extension) and X11 (no Overlay service) are told move
    // mode isn't available there, not to start a service that won't help.
    // An X11 session is one even with a stray WAYLAND_DISPLAY, as the Overlay
    // itself decides.
    for (wayland, session, desktop, says) in [
        (
            Some("wayland-0"),
            "wayland",
            "ubuntu:GNOME",
            "GNOME Shell extension, which can't be moved",
        ),
        (None, "wayland", "KDE", "this session isn't Wayland"),
        (None, "x11", "KDE", "this session isn't Wayland"),
        (
            Some("wayland-0"),
            "x11",
            "KDE",
            "this session isn't Wayland",
        ),
    ] {
        let elsewhere = sandbox.run_in(&["overlay-position", "move"], wayland, session, desktop);
        assert_eq!(elsewhere.status.code(), Some(1), "{elsewhere:?}");
        let error = stderr(&elsewhere);
        assert!(error.contains(says), "{desktop}: {error}");
        assert!(!error.contains("systemctl"), "{desktop}: {error}");
    }

    let overlay = fake_overlay(
        &sandbox.overlay_socket(),
        Some("This desktop has no layer-shell support, so the pill can't be moved."),
        Vec::new(),
    );
    let refused = sandbox.run(&["overlay-position", "move"]);
    assert_eq!(refused.status.code(), Some(1), "{refused:?}");
    assert_eq!(
        stderr(&refused),
        "This desktop has no layer-shell support, so the pill can't be moved.\n"
    );
    overlay.join().unwrap();
}
