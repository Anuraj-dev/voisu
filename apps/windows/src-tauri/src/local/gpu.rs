//! Which device Local mode runs on, asked of the worker without loading a
//! model: `voisu-local-asr --probe-gpu` prints the GPU whisper.cpp would use
//! (`vulkan:0 NVIDIA GeForce RTX 5050 Laptop GPU`: the shim's first discrete
//! GPU, else the first integrated one) and exits 0, or exits non-zero
//! without one. The answer picks the `auto` tier and is shown in Settings.

use std::fs;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Loading the Vulkan driver is the slow part; a stuck probe means CPU.
const PROBE_DEADLINE: Duration = Duration::from_secs(20);
/// The probe prints one line.
const MAX_OUTPUT: u64 = 16 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Detected {
    Cpu,
    Gpu {
        /// ggml's device as the worker reports it, such as `vulkan:0`.
        device: String,
        /// The driver's name for it, which Settings shows.
        name: String,
    },
}

impl Detected {
    pub fn on_gpu(&self) -> bool {
        matches!(self, Self::Gpu { .. })
    }

    /// The GPU's name, or `CPU`.
    pub fn label(&self) -> &str {
        match self {
            Self::Cpu => "CPU",
            Self::Gpu { name, .. } => name,
        }
    }
}

/// Reads the probe's exit `code` and stdout. Only a clean exit with a
/// `<device> <name>` line is a GPU.
pub fn parse_probe(code: Option<i32>, stdout: &str) -> Detected {
    if code != Some(0) {
        return Detected::Cpu;
    }
    let line = stdout.lines().next().unwrap_or_default().trim();
    let (device, name) = line.split_once(' ').unwrap_or((line, ""));
    let usable = device.contains(':')
        && device
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b':');
    if !usable {
        return Detected::Cpu;
    }
    let name = name.trim();
    Detected::Gpu {
        device: device.to_owned(),
        name: if name.is_empty() { device } else { name }.to_owned(),
    }
}

/// Runs the probe. Its stderr (ggml's device list) is appended to `log`.
/// Any failure to run it is the CPU. On Windows it runs in a kill-on-close
/// Job Object, as the worker does, so a probe stuck in a GPU driver ends
/// with Voisu.
pub fn probe(worker: &Path, log: &Path) -> Detected {
    let started = Instant::now();
    let detected = run(worker, log).unwrap_or_else(|reason| {
        tracing::warn!(%reason, "Local GPU probe failed; using the CPU");
        Detected::Cpu
    });
    tracing::info!(
        device = match &detected {
            Detected::Cpu => "cpu",
            Detected::Gpu { device, .. } => device,
        },
        name = detected.label(),
        ms = started.elapsed().as_millis() as u64,
        "Local GPU probe"
    );
    detected
}

fn run(worker: &Path, log: &Path) -> Result<Detected, String> {
    let stderr = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
        .map_or_else(|_| Stdio::null(), Stdio::from);
    let mut command = Command::new(worker);
    command
        .arg("--probe-gpu")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(stderr);
    super::worker::scrub_environment(&mut command);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(super::worker::CREATE_NO_WINDOW | super::worker::CREATE_SUSPENDED);
    }
    let mut child = command.spawn().map_err(|error| format!("spawn: {error}"))?;
    // Held until the probe has exited or been killed.
    #[cfg(windows)]
    let _job = match super::worker::job::contain(&child) {
        Ok(job) => job,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("not started in a Job Object: {error}"));
        }
    };
    let deadline = Instant::now() + PROBE_DEADLINE;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("no answer in {} s", PROBE_DEADLINE.as_secs()));
            }
            Err(error) => return Err(format!("wait: {error}")),
        }
    };
    let mut stdout = String::new();
    if let Some(pipe) = child.stdout.take() {
        let _ = pipe.take(MAX_OUTPUT).read_to_string(&mut stdout);
    }
    Ok(parse_probe(status.code(), &stdout))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_clean_exit_naming_a_device_is_a_gpu() {
        assert_eq!(
            parse_probe(Some(0), "vulkan:0 NVIDIA GeForce RTX 5050 Laptop GPU\n"),
            Detected::Gpu {
                device: "vulkan:0".into(),
                name: "NVIDIA GeForce RTX 5050 Laptop GPU".into(),
            }
        );
        assert_eq!(
            parse_probe(Some(0), "vulkan:1\n"),
            Detected::Gpu {
                device: "vulkan:1".into(),
                name: "vulkan:1".into(),
            }
        );
        // No GPU (3), refused (2), killed (None), or output that names none.
        assert_eq!(parse_probe(Some(3), ""), Detected::Cpu);
        assert_eq!(parse_probe(Some(2), "vulkan:0 X"), Detected::Cpu);
        assert_eq!(parse_probe(None, "vulkan:0 X"), Detected::Cpu);
        assert_eq!(parse_probe(Some(0), ""), Detected::Cpu);
        assert_eq!(parse_probe(Some(0), "no gpu here"), Detected::Cpu);
        assert!(!Detected::Cpu.on_gpu());
        assert_eq!(Detected::Cpu.label(), "CPU");
    }
}
