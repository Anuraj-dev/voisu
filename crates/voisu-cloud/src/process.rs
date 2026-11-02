//! Bounded, environment-restricted subprocess runner shared by the cloud
//! providers (curl) and voisu-app's local helpers, plus the Linux
//! parent-death guard every external child gets.

use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use voisu_core::CancelRegistry;

#[cfg(target_os = "linux")]
use std::os::unix::process::CommandExt;

pub fn guard_external_child(command: &mut Command) {
    #[cfg(target_os = "linux")]
    guard_external_child_for_parent(command, unsafe { libc::getpid() });
    // Other platforms have no parent-death signal; the bounded waits and
    // owned reaps remain the guarantee there.
    #[cfg(not(target_os = "linux"))]
    let _ = command;
}

#[cfg(target_os = "linux")]
fn guard_external_child_for_parent(command: &mut Command, expected_parent: libc::pid_t) {
    // SAFETY: both syscalls are async-signal-safe and this hook performs no
    // allocation between fork and exec. Checking PPID closes the documented
    // race where the parent dies after fork but before PR_SET_PDEATHSIG.
    unsafe {
        command.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::getppid() != expected_parent {
                return Err(std::io::Error::from_raw_os_error(libc::ECHILD));
            }
            Ok(())
        });
    }
}

pub const PROCESS_DEADLINE: Duration = Duration::from_secs(2);

pub const PROCESS_POLL: Duration = Duration::from_millis(10);

pub const MAX_RETAINED_STDERR_BYTES: usize = 4 * 1024;

pub const MAX_RETAINED_STDOUT_BYTES: usize = 64 * 1024;

pub struct ProcessOutcome {
    pub success: bool,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

pub enum ProcessError {
    Unavailable,
    Input,
    TimedOut,
    Wait,
    Output,
}

pub fn restricted_command(program: &str) -> Command {
    let mut command = Command::new(program);
    guard_external_child(&mut command);
    command.env_clear();
    if let Some(path) = std::env::var_os("PATH") {
        command.env("PATH", path);
    }
    // The Linux helpers (pw-record, wl-copy, xclip, hyprctl) need the session
    // variables below; a Windows child (curl.exe) has no use for them.
    #[cfg(unix)]
    for name in [
        "XDG_RUNTIME_DIR",
        "DBUS_SESSION_BUS_ADDRESS",
        "WAYLAND_DISPLAY",
        "XDG_SESSION_TYPE",
        "HYPRLAND_INSTANCE_SIGNATURE",
        // X11 helpers (xclip, and any tool that talks to the X server) need
        // DISPLAY to find the server and XAUTHORITY to authenticate to it.
        // Without these, a spawned X11 helper can never reach the display —
        // which is why the field clipboard wrappers had to restore them by
        // hand. Forwarding XAUTHORITY widens what a helper can read (it names a
        // file holding X credentials), so this line is reviewed as
        // security-relevant and gated on a real host.
        "DISPLAY",
        "XAUTHORITY",
    ] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    // Windows: a child with no SystemRoot cannot initialize Winsock, so curl
    // could never open a socket. It names the OS directory, not a credential.
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        if let Some(value) = std::env::var_os("SystemRoot") {
            command.env("SystemRoot", value);
        }
        // A GUI-subsystem host (the Tauri shell) spawning a console program
        // would otherwise flash a console window for every curl request.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}

pub fn run_restricted(
    program: &str,
    arguments: &[&str],
    input: Option<&[u8]>,
    capture_stdout: bool,
) -> Result<ProcessOutcome, ProcessError> {
    run_restricted_with_deadline(
        program,
        arguments,
        input,
        capture_stdout,
        PROCESS_DEADLINE,
        None,
    )
}

/// Runs a helper whose SUCCESS mode is to fork a descendant that keeps
/// serving after the parent exits — real `wl-copy` serves the clipboard this
/// way. The descendant inherits the parent's pipes, so capturing output would
/// read the healthy case as a pipe held past the deadline; both streams are
/// discarded and only the parent's own exit status is observed.
pub fn run_restricted_serving(
    program: &str,
    arguments: &[&str],
    input: Option<&[u8]>,
) -> Result<ProcessOutcome, ProcessError> {
    run_restricted_serving_within(program, arguments, input, PROCESS_DEADLINE)
}

/// As [`run_restricted_serving`], but bounded by an explicit deadline so a
/// shared budget can span several candidate backends (see `clipboard_write`).
pub fn run_restricted_serving_within(
    program: &str,
    arguments: &[&str],
    input: Option<&[u8]>,
    deadline: Duration,
) -> Result<ProcessOutcome, ProcessError> {
    let started = Instant::now();
    let mut command = restricted_command(program);
    command
        .args(arguments)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = command.spawn().map_err(|_| ProcessError::Unavailable)?;
    let writer = match input {
        Some(input) => {
            let input = input.to_vec();
            let mut stdin = child.stdin.take().ok_or(ProcessError::Input)?;
            Some(thread::spawn(move || {
                let result = stdin.write_all(&input);
                drop(stdin);
                result
            }))
        }
        None => None,
    };
    let status = wait_for_child(&mut child, started, deadline, None);
    let writer = writer.map(|handle| bounded_join(handle, started, &mut child, deadline));
    let status = status?;
    if let Some(writer) = writer {
        match writer {
            Ok(Ok(())) => {}
            Err(ProcessError::TimedOut) => return Err(ProcessError::TimedOut),
            _ => return Err(ProcessError::Input),
        }
    }
    Ok(ProcessOutcome {
        success: status.success(),
        stdout: Vec::new(),
        stderr: Vec::new(),
    })
}

pub fn run_restricted_with_deadline(
    program: &str,
    arguments: &[&str],
    input: Option<&[u8]>,
    capture_stdout: bool,
    deadline: Duration,
    cancel: Option<&CancelRegistry>,
) -> Result<ProcessOutcome, ProcessError> {
    // Fail fast without spawning when the operation is already cancelled.
    if cancel.is_some_and(CancelRegistry::is_cancelled) {
        return Err(ProcessError::TimedOut);
    }
    let started = Instant::now();
    let mut command = restricted_command(program);
    command
        .args(arguments)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(if capture_stdout {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|_| ProcessError::Unavailable)?;
    // The whole-operation deadline starts before spawn and covers startup, the
    // stdin write, pipe drains, and wait. The write runs on its own thread so
    // the polling loop can kill an overdue child, which breaks the pipe and
    // unblocks the writer.
    let writer = match input {
        Some(input) => {
            let input = input.to_vec();
            let mut stdin = child.stdin.take().ok_or(ProcessError::Input)?;
            Some(thread::spawn(move || {
                let result = stdin.write_all(&input);
                drop(stdin);
                result
            }))
        }
        None => None,
    };
    let stdout_reader = child.stdout.take().map(|mut stdout| {
        thread::spawn(move || read_capped(&mut stdout, MAX_RETAINED_STDOUT_BYTES))
    });
    let stderr_reader = child.stderr.take().map(|mut stderr| {
        thread::spawn(move || read_capped(&mut stderr, MAX_RETAINED_STDERR_BYTES))
    });
    // Every helper thread join is bounded by the same Instant budget on every
    // path: a descendant of the child can inherit and hold the pipes open past
    // the child's own exit, which would otherwise block a bare join() forever
    // (or, on the error path, silently leave detached threads blocked).
    // Collect every helper-thread result FIRST, then decide the outcome: an
    // early return between joins would silently detach a later thread while it
    // may still be blocked on a descendant-held pipe.
    let status = wait_for_child(&mut child, started, deadline, cancel);
    let writer = writer.map(|handle| bounded_join(handle, started, &mut child, deadline));
    let stdout_joined =
        stdout_reader.map(|handle| bounded_join(handle, started, &mut child, deadline));
    let stderr_joined =
        stderr_reader.map(|handle| bounded_join(handle, started, &mut child, deadline));
    let stdout = pipe_bytes(stdout_joined)?;
    let stderr = pipe_bytes(stderr_joined)?;
    let status = status?;
    if let Some(writer) = writer {
        match writer {
            Ok(Ok(())) => {}
            Err(ProcessError::TimedOut) => return Err(ProcessError::TimedOut),
            _ => return Err(ProcessError::Input),
        }
    }
    Ok(ProcessOutcome {
        success: status.success(),
        stdout,
        stderr,
    })
}

/// Joins a helper thread under the remaining process budget. On budget
/// exhaustion the overdue child is killed and the thread is deliberately
/// detached — it can never be forced to finish while a descendant holds the
/// pipe — and the caller receives the timeout error.
pub fn bounded_join<T: Send + 'static>(
    handle: thread::JoinHandle<T>,
    started: Instant,
    child: &mut Child,
    deadline: Duration,
) -> Result<T, ProcessError> {
    while !handle.is_finished() {
        if started.elapsed() >= deadline {
            let _ = child.kill();
            reap_briefly(child);
            drop(handle);
            return Err(ProcessError::TimedOut);
        }
        thread::sleep(PROCESS_POLL);
    }
    handle.join().map_err(|_| ProcessError::Output)
}

/// Best-effort reap of a killed child under a small extra budget so no zombie
/// is left behind; if it still has not been collected, give up rather than
/// block the caller further.
pub fn reap_briefly(child: &mut Child) {
    let reap_started = Instant::now();
    while reap_started.elapsed() < Duration::from_millis(250) {
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => return,
            Ok(None) => thread::sleep(PROCESS_POLL),
        }
    }
}

pub fn pipe_bytes(
    joined: Option<Result<std::io::Result<Vec<u8>>, ProcessError>>,
) -> Result<Vec<u8>, ProcessError> {
    match joined {
        Some(result) => result?.map_err(|_| ProcessError::Output),
        None => Ok(Vec::new()),
    }
}

/// Drains a pipe to EOF so the child never blocks on a full buffer, but
/// retains only the first `cap` bytes: a noisy child cannot force unbounded
/// memory growth inside the deadline window.
pub fn read_capped(source: &mut impl Read, cap: usize) -> std::io::Result<Vec<u8>> {
    match read_capped_checked(source, cap)? {
        CappedRead::Complete(bytes) | CappedRead::Truncated { retained: bytes } => Ok(bytes),
    }
}

pub enum CappedRead {
    Complete(Vec<u8>),
    Truncated { retained: Vec<u8> },
}

/// Like [`read_capped`], but reports whether any bytes past `cap` were drained.
pub fn read_capped_checked(source: &mut impl Read, cap: usize) -> std::io::Result<CappedRead> {
    let mut retained = Vec::new();
    let mut truncated = false;
    let mut buffer = [0_u8; 1024];
    loop {
        match source.read(&mut buffer) {
            Ok(0) => {
                return Ok(if truncated {
                    CappedRead::Truncated { retained }
                } else {
                    CappedRead::Complete(retained)
                });
            }
            Ok(read) => {
                if truncated {
                    continue;
                }
                let room = cap.saturating_sub(retained.len());
                let keep = read.min(room);
                retained.extend_from_slice(&buffer[..keep]);
                if keep < read {
                    truncated = true;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
}

pub fn wait_for_child(
    child: &mut Child,
    started: Instant,
    deadline: Duration,
    cancel: Option<&CancelRegistry>,
) -> Result<std::process::ExitStatus, ProcessError> {
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) => {}
            Err(_) => {
                // The child may still be live even though its status cannot be
                // read; kill and best-effort reap before surfacing the error.
                let _ = child.kill();
                reap_briefly(child);
                return Err(ProcessError::Wait);
            }
        }
        // Cancellation is observed by the loop that owns the Child handle:
        // killing through the handle is pid-reuse-safe because this loop is
        // also the only reaper. Latency is at most one poll tick.
        if cancel.is_some_and(CancelRegistry::is_cancelled) || started.elapsed() >= deadline {
            let _ = child.kill();
            reap_briefly(child);
            return Err(ProcessError::TimedOut);
        }
        thread::sleep(PROCESS_POLL);
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::{guard_external_child, guard_external_child_for_parent};
    use std::process::Command;

    #[test]
    fn guarded_child_reports_sigkill_parent_death_contract() {
        let mut child = Command::new("python3");
        child.args([
            "-c",
            "import ctypes, signal, sys; value = ctypes.c_int(); result = ctypes.CDLL(None).prctl(2, ctypes.byref(value)); sys.exit(result != 0 or value.value != signal.SIGKILL)",
        ]);
        guard_external_child(&mut child);

        assert!(child.status().unwrap().success());
    }

    #[test]
    fn child_refuses_exec_when_expected_parent_is_already_gone() {
        let mut child = Command::new("true");
        guard_external_child_for_parent(&mut child, unsafe { libc::getpid() } + 1);

        assert!(child.status().is_err());
    }
}
