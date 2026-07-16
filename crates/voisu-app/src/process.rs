use std::process::Command;

#[cfg(target_os = "linux")]
use std::os::unix::process::CommandExt;

pub(crate) fn guard_external_child(command: &mut Command) {
    #[cfg(target_os = "linux")]
    guard_external_child_for_parent(command, unsafe { libc::getpid() });
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
