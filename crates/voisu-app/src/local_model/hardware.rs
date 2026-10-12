//! Bounded NVIDIA discovery and the existing host topology measurements.

use std::io::{self, Read};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

const CAPTURE_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NvidiaGpu {
    pub index: u32,
    pub name: String,
    pub total_vram_mib: u64,
    pub driver_version: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GpuProbe {
    Nvidia(Vec<NvidiaGpu>),
    NoneFound,
    Failed(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostHardware {
    pub gpu: GpuProbe,
    pub ram_bytes: Option<u64>,
    pub cpu_cores: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandOutput {
    pub status_success: bool,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RunError {
    NotFound,
    Timeout,
    Other(String),
}

pub trait CommandRunner {
    fn run(
        &self,
        program: &str,
        args: &[&str],
        timeout: Duration,
    ) -> Result<CommandOutput, RunError>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SystemCommandRunner;

impl CommandRunner for SystemCommandRunner {
    fn run(
        &self,
        program: &str,
        args: &[&str],
        timeout: Duration,
    ) -> Result<CommandOutput, RunError> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| RunError::Other("Invalid command timeout".into()))?;
        let mut child = Command::new(program)
            .args(args)
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| {
                if error.kind() == io::ErrorKind::NotFound {
                    RunError::NotFound
                } else {
                    io_error(error)
                }
            })?;
        let pgid = child.id() as libc::pid_t;
        let result = (|| {
            let stdout = child
                .stdout
                .take()
                .ok_or_else(|| RunError::Other("Missing command stdout pipe".into()))?;
            let stderr = child
                .stderr
                .take()
                .ok_or_else(|| RunError::Other("Missing command stderr pipe".into()))?;
            let stdout = capture_pipe(stdout).map_err(io_error)?;
            let stderr = capture_pipe(stderr).map_err(io_error)?;
            let status = loop {
                if Instant::now() >= deadline {
                    return Err(RunError::Timeout);
                }
                if let Some(status) = child.try_wait().map_err(io_error)? {
                    break status;
                }
                thread::sleep(
                    Duration::from_millis(10)
                        .min(deadline.saturating_duration_since(Instant::now())),
                );
            };
            // Pipe collection shares the deadline: inherited pipes must not hold up the caller.
            Ok(CommandOutput {
                status_success: status.success(),
                stdout: receive_pipe(stdout, deadline)?,
                stderr: receive_pipe(stderr, deadline)?,
            })
        })();
        if result.is_err() {
            let _ = child.kill();
            // SAFETY: pgid is our own child's group (spawned with process_group(0)),
            // so the signal cannot reach daemon siblings. Kill descendants even
            // when the direct child exited but inherited pipes missed the deadline.
            unsafe {
                libc::killpg(pgid, libc::SIGKILL);
            }
            let _ = child.wait();
        }
        result
    }
}

fn io_error(error: io::Error) -> RunError {
    RunError::Other(short_reason(&error.to_string()))
}

fn capture_pipe(mut pipe: impl Read + Send + 'static) -> io::Result<Receiver<io::Result<Vec<u8>>>> {
    let (sender, receiver) = mpsc::channel();
    thread::Builder::new().spawn(move || {
        let result = (|| {
            let mut captured = Vec::new();
            let mut buffer = [0; 8192];
            loop {
                let count = match pipe.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(count) => count,
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) => return Err(error),
                };
                // Keep draining after the cap so a verbose child cannot block on a full pipe.
                let retain = count.min(CAPTURE_BYTES - captured.len());
                captured.extend_from_slice(&buffer[..retain]);
            }
            Ok(captured)
        })();
        let _ = sender.send(result);
    })?;
    Ok(receiver)
}

fn receive_pipe(
    receiver: Receiver<io::Result<Vec<u8>>>,
    deadline: Instant,
) -> Result<String, RunError> {
    match receiver.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        Ok(Ok(bytes)) => Ok(String::from_utf8_lossy(&bytes).into_owned()),
        Ok(Err(error)) => Err(io_error(error)),
        Err(mpsc::RecvTimeoutError::Timeout) => Err(RunError::Timeout),
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            Err(RunError::Other("Command output reader stopped".into()))
        }
    }
}

#[must_use]
pub fn probe_hardware(runner: &dyn CommandRunner) -> HostHardware {
    probe_hardware_with(runner, &|| {
        std::path::Path::new("/proc/driver/nvidia/version").exists()
    })
}

#[must_use]
pub fn probe_hardware_with(
    runner: &dyn CommandRunner,
    nvidia_driver_present: &dyn Fn() -> bool,
) -> HostHardware {
    let gpu = match runner.run(
        "nvidia-smi",
        &[
            "--query-gpu=index,name,memory.total,driver_version",
            "--format=csv,noheader,nounits",
        ],
        Duration::from_secs(3),
    ) {
        Err(RunError::NotFound) if nvidia_driver_present() => {
            GpuProbe::Failed("NVIDIA driver is loaded but nvidia-smi was not found".into())
        }
        Err(RunError::NotFound) => GpuProbe::NoneFound,
        Err(RunError::Timeout) => GpuProbe::Failed("NVIDIA GPU probe timed out after 3 s".into()),
        Err(RunError::Other(reason)) => {
            GpuProbe::Failed(short_reason(&format!("NVIDIA GPU probe failed: {reason}")))
        }
        Ok(output) if !output.status_success => GpuProbe::Failed(short_reason(&format!(
            "nvidia-smi exited unsuccessfully: {}",
            output.stderr
        ))),
        Ok(output) => match parse_nvidia_smi(&output.stdout) {
            Ok(gpus) if gpus.is_empty() => GpuProbe::NoneFound,
            Ok(gpus) => GpuProbe::Nvidia(gpus),
            Err(reason) => GpuProbe::Failed(reason),
        },
    };
    HostHardware {
        gpu,
        ram_bytes: crate::local_worker::host_physical_ram_bytes(),
        cpu_cores: crate::local_worker::host_cpu_cores(),
    }
}

fn short_reason(reason: &str) -> String {
    reason
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(200)
        .collect()
}

pub fn parse_nvidia_smi(stdout: &str) -> Result<Vec<NvidiaGpu>, String> {
    let mut gpus = Vec::new();
    for (line_number, line) in stdout.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let fields: Vec<_> = line.split(',').map(str::trim).collect();
        let invalid = || format!("Invalid NVIDIA GPU row {}", line_number + 1);
        if fields.len() != 4 || fields[1].is_empty() || fields[3].is_empty() {
            return Err(invalid());
        }
        let index = fields[0].parse::<u32>().map_err(|_| invalid())?;
        let total_vram_mib = fields[2].parse::<u64>().map_err(|_| invalid())?;
        if gpus.iter().any(|gpu: &NvidiaGpu| gpu.index == index) {
            return Err(format!("Duplicate NVIDIA GPU index {index}"));
        }
        gpus.push(NvidiaGpu {
            index,
            name: fields[1].into(),
            total_vram_mib,
            driver_version: fields[3].into(),
        });
    }
    Ok(gpus)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeRunner(Result<CommandOutput, RunError>);

    impl CommandRunner for FakeRunner {
        fn run(
            &self,
            program: &str,
            args: &[&str],
            timeout: Duration,
        ) -> Result<CommandOutput, RunError> {
            assert_eq!(program, "nvidia-smi");
            assert_eq!(
                args,
                [
                    "--query-gpu=index,name,memory.total,driver_version",
                    "--format=csv,noheader,nounits"
                ]
            );
            assert_eq!(timeout, Duration::from_secs(3));
            self.0.clone()
        }
    }

    fn output(success: bool, stdout: &str, stderr: &str) -> Result<CommandOutput, RunError> {
        Ok(CommandOutput {
            status_success: success,
            stdout: stdout.into(),
            stderr: stderr.into(),
        })
    }

    #[test]
    fn probe_distinguishes_absent_failed_and_detected_nvidia_gpus() {
        for result in [Err(RunError::NotFound), output(true, " \n", "")] {
            assert_eq!(
                probe_hardware_with(&FakeRunner(result), &|| false).gpu,
                GpuProbe::NoneFound
            );
        }
        for result in [
            Err(RunError::Timeout),
            Err(RunError::Other("permission denied".into())),
            output(false, "", &"driver error\n".repeat(100)),
            output(true, "0, RTX 5050, [N/A], 580.95", ""),
        ] {
            let GpuProbe::Failed(reason) = probe_hardware(&FakeRunner(result)).gpu else {
                panic!("Probe failure must remain visible");
            };
            assert!(!reason.is_empty());
            assert!(reason.chars().count() <= 200);
        }
        let hw = probe_hardware(&FakeRunner(output(true, "0, RTX 5050, 8151, 580.95", "")));
        assert_eq!(
            hw.gpu,
            GpuProbe::Nvidia(vec![NvidiaGpu {
                index: 0,
                name: "RTX 5050".into(),
                total_vram_mib: 8151,
                driver_version: "580.95".into(),
            }])
        );
        assert!(hw.cpu_cores > 0);
    }

    #[test]
    fn missing_nvidia_smi_with_loaded_driver_is_a_probe_failure() {
        assert_eq!(
            probe_hardware_with(&FakeRunner(Err(RunError::NotFound)), &|| true).gpu,
            GpuProbe::Failed("NVIDIA driver is loaded but nvidia-smi was not found".into())
        );
    }

    #[test]
    fn missing_nvidia_smi_without_loaded_driver_means_no_gpu_found() {
        assert_eq!(
            probe_hardware_with(&FakeRunner(Err(RunError::NotFound)), &|| false).gpu,
            GpuProbe::NoneFound
        );
    }

    #[test]
    fn parses_gpu_rows_with_spaces_and_surrounding_whitespace() {
        assert_eq!(
            parse_nvidia_smi(
                " \n 0, NVIDIA GeForce RTX 5050 Laptop GPU, 8151, 580.95.05 \n\n 1, NVIDIA RTX 4090, 24564, 580.95.05\n"
            ),
            Ok(vec![
                NvidiaGpu {
                    index: 0,
                    name: "NVIDIA GeForce RTX 5050 Laptop GPU".into(),
                    total_vram_mib: 8151,
                    driver_version: "580.95.05".into(),
                },
                NvidiaGpu {
                    index: 1,
                    name: "NVIDIA RTX 4090".into(),
                    total_vram_mib: 24564,
                    driver_version: "580.95.05".into(),
                },
            ])
        );
    }

    #[test]
    fn rejects_unusable_gpu_rows_without_partial_results() {
        for row in [
            "0, RTX 5050, [N/A], 580.95",
            "0, RTX 5050, -1, 580.95",
            "invalid, RTX 5050, 8151, 580.95",
            "0, RTX 5050, 8151",
            "0, , 8151, 580.95",
            "0, RTX 5050, 8151, ",
            "0, RTX 5050, 18446744073709551616, 580.95",
            "0, RTX 5050, 8151, 580.95\n0, RTX 4090, 24564, 580.95",
            "0, RTX 5050, 8151, 580.95\ngarbage",
        ] {
            assert!(parse_nvidia_smi(row).is_err(), "{row}");
        }
        assert_eq!(parse_nvidia_smi(" \n\t"), Ok(vec![]));
    }

    #[test]
    fn system_runner_times_out_and_reports_missing_programs() {
        let runner = SystemCommandRunner;
        let started = std::time::Instant::now();
        assert_eq!(
            runner.run("sleep", &["5"], Duration::from_millis(200)),
            Err(RunError::Timeout)
        );
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(
            runner.run(
                "/voisu-test-nonexistent-program",
                &[],
                Duration::from_secs(1)
            ),
            Err(RunError::NotFound)
        );
    }

    #[test]
    fn system_runner_caps_both_streams_while_draining() {
        let output = SystemCommandRunner
            .run(
                "sh",
                &[
                    "-c",
                    "head -c 131072 /dev/zero; head -c 131072 /dev/zero >&2",
                ],
                Duration::from_secs(3),
            )
            .expect("bounded output command");
        assert!(output.status_success);
        assert_eq!(output.stdout.len(), 65536);
        assert_eq!(output.stderr.len(), 65536);
    }

    #[test]
    fn system_runner_kills_descendants_holding_pipes_after_child_exits() {
        let pid_file = tempfile::NamedTempFile::new().expect("background PID file");
        let started = Instant::now();
        let result = SystemCommandRunner.run(
            "sh",
            &[
                "-c",
                "sleep 30 & echo $! > \"$1\"; exit 0",
                "voisu-test",
                pid_file.path().to_str().expect("PID file path"),
            ],
            Duration::from_millis(300),
        );
        assert_eq!(result, Err(RunError::Timeout));
        assert!(started.elapsed() < Duration::from_secs(5));

        let pid: u32 = std::fs::read_to_string(pid_file.path())
            .expect("recorded background PID")
            .trim()
            .parse()
            .expect("background PID");
        let process_stat = format!("/proc/{pid}/stat");
        let cleanup_deadline = Instant::now() + Duration::from_secs(1);
        loop {
            // An orphan can remain a zombie until init reaps it; it no longer holds pipes.
            match std::fs::read_to_string(&process_stat) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => break,
                Ok(stat)
                    if stat
                        .rsplit_once(") ")
                        .is_some_and(|(_, rest)| rest.starts_with('Z')) =>
                {
                    break;
                }
                _ if Instant::now() < cleanup_deadline => thread::sleep(Duration::from_millis(10)),
                _ => {
                    // Also clean up when this regression test runs against the unfixed runner.
                    let _ = Command::new("kill")
                        .args(["-KILL", &pid.to_string()])
                        .status();
                    panic!("background sleep {pid} survived command timeout");
                }
            }
        }
    }
}
