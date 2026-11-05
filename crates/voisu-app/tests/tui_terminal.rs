//! How the interface ends on a real pseudo-terminal, which `TestBackend` cannot
//! show. A closed terminal window must not leave a process behind.

use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Read};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Child, Command, ExitStatus};
use std::time::{Duration, Instant};

use tempfile::TempDir;

const PATIENCE: Duration = Duration::from_secs(5);
const ENTER_ALTERNATE_SCREEN: &[u8] = b"\x1b[?1049h";
const LEAVE_ALTERNATE_SCREEN: &[u8] = b"\x1b[?1049l";
/// The local flags raw mode clears and leaving must restore.
const COOKED: libc::tcflag_t = libc::ICANON | libc::ECHO;

/// `voisu` with no arguments on a terminal of its own, under temporary
/// directories and an empty `PATH`, so nothing real is reachable.
struct Interface {
    master: Option<File>,
    child: Child,
    output: Vec<u8>,
    _dirs: TempDir,
}

impl Interface {
    fn open() -> Self {
        let mut interface = Self::spawn(&[]);
        // The first frame comes after the alternate screen, once the loop runs.
        interface.wait_for_output(b"Settings");
        interface
    }

    fn spawn(args: &[&str]) -> Self {
        let dirs = TempDir::new().unwrap();
        let runtime = dirs.path().join("runtime");
        std::fs::create_dir(&runtime).unwrap();
        std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700)).unwrap();

        let (master, slave) = pseudo_terminal();
        let mut command = Command::new(env!("CARGO_BIN_EXE_voisu"));
        command.args(args);
        command
            .env_clear()
            .env("TERM", "xterm-256color")
            .env("PATH", "")
            .env("HOME", dirs.path())
            .env("XDG_CONFIG_HOME", dirs.path().join("config"))
            .env("XDG_STATE_HOME", dirs.path().join("state"))
            .env("XDG_RUNTIME_DIR", &runtime)
            .env("DBUS_SESSION_BUS_ADDRESS", "unix:path=/nonexistent")
            .env("VOISU_TEST_SECRET_STORE", "available")
            .stdin(slave.try_clone().unwrap())
            .stdout(slave.try_clone().unwrap())
            .stderr(slave);
        // SAFETY: both calls are async-signal-safe. Stdin is the terminal by now,
        // and owning it is what makes the kernel send SIGHUP when it closes.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn().unwrap();
        let mut interface = Self {
            master: Some(master),
            child,
            output: Vec::new(),
            _dirs: dirs,
        };
        interface.wait_for_output(ENTER_ALTERNATE_SCREEN);
        interface
    }

    /// Drains the terminal, so the interface can never block writing to it.
    fn pump(&mut self) {
        let Some(master) = self.master.as_mut() else {
            return;
        };
        let mut buffer = [0; 4096];
        loop {
            match master.read(&mut buffer) {
                Ok(0) => return,
                Ok(count) => self.output.extend_from_slice(&buffer[..count]),
                Err(error) if error.kind() == ErrorKind::WouldBlock => return,
                Err(_) => return,
            }
        }
    }

    fn printed(&self, wanted: &[u8]) -> bool {
        self.output
            .windows(wanted.len())
            .any(|window| window == wanted)
    }

    fn wait_for_output(&mut self, wanted: &[u8]) {
        let deadline = Instant::now() + PATIENCE;
        while !self.printed(wanted) {
            assert!(
                Instant::now() < deadline,
                "never printed {:?}",
                String::from_utf8_lossy(wanted)
            );
            self.pump();
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Waits for `wanted` to appear at or after byte `start` of the output.
    fn wait_for_output_after(&mut self, start: usize, wanted: &[u8]) {
        let deadline = Instant::now() + PATIENCE;
        while !self.output[start..]
            .windows(wanted.len())
            .any(|window| window == wanted)
        {
            assert!(
                Instant::now() < deadline,
                "never printed {:?} again",
                String::from_utf8_lossy(wanted)
            );
            self.pump();
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn type_keys(&mut self, keys: &[u8]) {
        let master = self.master.as_ref().unwrap();
        std::io::Write::write_all(&mut &*master, keys).unwrap();
        // One read per key, so an escape sequence is never split or merged.
        std::thread::sleep(Duration::from_millis(30));
        self.pump();
    }

    fn wait_for_exit(&mut self) -> ExitStatus {
        let deadline = Instant::now() + PATIENCE;
        loop {
            self.pump();
            if let Some(status) = self.child.try_wait().unwrap() {
                self.pump();
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "still running {PATIENCE:?} later"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn signal(&self, signal: libc::c_int) {
        // SAFETY: the child is unreaped, so its pid still names this process.
        assert_eq!(unsafe { libc::kill(self.child.id() as i32, signal) }, 0);
    }

    /// The terminal's local flags, which raw mode clears and leaving must restore.
    fn local_flags(&self) -> libc::tcflag_t {
        let master = self.master.as_ref().unwrap();
        // SAFETY: `termios` is plain data that `tcgetattr` fills in.
        unsafe {
            let mut termios = std::mem::zeroed();
            assert_eq!(libc::tcgetattr(master.as_raw_fd(), &mut termios), 0);
            termios.c_lflag
        }
    }
}

impl Drop for Interface {
    fn drop(&mut self) {
        // SIGKILL, because a stuck interface would ignore anything gentler.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A non-blocking master and the slave end to hand to the child.
fn pseudo_terminal() -> (File, File) {
    // SAFETY: plain descriptor calls, each checked, and the name buffer outlives
    // the call that fills it.
    unsafe {
        let master =
            libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY | libc::O_NONBLOCK | libc::O_CLOEXEC);
        assert!(master >= 0, "no pseudo-terminal");
        let master = File::from_raw_fd(master);
        assert_eq!(libc::grantpt(master.as_raw_fd()), 0);
        assert_eq!(libc::unlockpt(master.as_raw_fd()), 0);
        let mut name = [0 as libc::c_char; 128];
        assert_eq!(
            libc::ptsname_r(master.as_raw_fd(), name.as_mut_ptr(), name.len()),
            0
        );
        let path = std::ffi::CStr::from_ptr(name.as_ptr()).to_str().unwrap();
        let size = libc::winsize {
            ws_row: 24,
            ws_col: 80,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        assert_eq!(
            libc::ioctl(master.as_raw_fd(), libc::TIOCSWINSZ as _, &size),
            0
        );
        let slave = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOCTTY)
            .open(path)
            .unwrap();
        (master, slave)
    }
}

#[test]
fn closing_the_terminal_ends_the_interface() {
    let mut interface = Interface::open();
    interface.master = None;
    assert_eq!(interface.wait_for_exit().code(), Some(128 + libc::SIGHUP));
}

#[test]
fn sigterm_restores_the_terminal_and_reports_the_signal() {
    let mut interface = Interface::open();
    let raw = interface.local_flags();
    assert_eq!(raw & COOKED, 0, "should start raw");

    interface.signal(libc::SIGTERM);
    let status = interface.wait_for_exit();

    assert_eq!(status.code(), Some(128 + libc::SIGTERM), "{status:?}");
    assert_eq!(status.signal(), None);
    assert!(interface.printed(LEAVE_ALTERNATE_SCREEN));
    let restored = interface.local_flags();
    assert_eq!(restored & COOKED, COOKED);
}

#[test]
fn quitting_with_q_restores_the_terminal_and_exits_zero() {
    let mut interface = Interface::open();
    assert_eq!(interface.local_flags() & COOKED, 0, "should start raw");
    interface.type_keys(b"q");

    let status = interface.wait_for_exit();

    assert_eq!(status.code(), Some(0), "{status:?}");
    assert!(interface.printed(LEAVE_ALTERNATE_SCREEN));
    assert_eq!(interface.local_flags() & COOKED, COOKED);
}

/// Setup runs on the same terminal and comes back to a raw, working interface.
/// With no desktop session in the environment, setup fails at once, which is
/// enough to cover it running inside the interface without a real desktop.
#[test]
fn setup_runs_inside_the_interface_and_comes_back_to_it() {
    let mut interface = Interface::open();
    // A fresh home is not set up, so bare voisu opens straight on setup.
    interface.wait_for_output(b"Setup didn't finish");
    let failed = interface.output.len();
    assert!(
        !interface.printed(LEAVE_ALTERNATE_SCREEN),
        "setup must not leave the screen"
    );
    assert_eq!(interface.local_flags() & COOKED, 0, "raw during setup");

    interface.type_keys(b"\r");
    interface.wait_for_output_after(failed, b"Settings");
    assert_eq!(interface.local_flags() & COOKED, 0, "raw after setup");
    // Back on the tabs it stays there, offering setup again instead of
    // reopening it.
    interface.wait_for_output_after(failed, b"Not set up");

    interface.type_keys(b"q");
    assert_eq!(
        interface.wait_for_exit().code(),
        Some(0),
        "bare voisu was opened by no script"
    );
    assert_eq!(interface.local_flags() & COOKED, COOKED);
}

/// `voisu setup` on a terminal opens the screens and still reports how it went.
#[test]
fn voisu_setup_on_a_terminal_reports_how_setup_went() {
    let mut interface = Interface::spawn(&["setup"]);
    interface.wait_for_output(b"Setup didn't finish");
    interface.type_keys(b"q");
    assert_eq!(interface.wait_for_exit().code(), Some(4));
    assert_eq!(interface.local_flags() & COOKED, COOKED);
}
