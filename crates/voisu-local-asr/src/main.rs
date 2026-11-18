//! `voisu-local-asr --manifest <path>`: serve worker protocol v1 on
//! stdin/stdout. Exit 0 after Shutdown or end of input, 1 when the stream
//! broke, 2 when the manifest is refused.
//!
//! `voisu-local-asr --probe-gpu`: print the GPU the worker would use as
//! `<device> <description>` and exit 0, or exit 3 when there is none.

use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use voisu_core::local_wire::PROTOCOL_VERSION;
use voisu_local_asr::engine::{WhisperCpp, device_label, probe_gpu};
use voisu_local_asr::manifest::{ManifestError, load_manifest, read_health_wav};
use voisu_local_asr::server::{self, Config, Exit};
use voisu_local_asr::wire;

/// Take fd 1 for frames and point fd 1 at stderr, so nothing a library prints
/// to stdout can corrupt the protocol stream.
#[cfg(unix)]
fn claim_stdout() -> std::io::Result<File> {
    use std::os::fd::FromRawFd;
    // SAFETY: plain descriptor duplication; the new fd is owned by the File.
    unsafe {
        let frames = libc::dup(libc::STDOUT_FILENO);
        if frames < 0 || libc::dup2(libc::STDERR_FILENO, libc::STDOUT_FILENO) < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(File::from_raw_fd(frames))
    }
}

#[cfg(unix)]
type Frames = File;

#[cfg(not(unix))]
type Frames = std::io::Stdout;

#[cfg(not(unix))]
fn claim_stdout() -> std::io::Result<Frames> {
    Ok(std::io::stdout())
}

fn manifest_path() -> Result<PathBuf, ManifestError> {
    let mut args = std::env::args_os().skip(1);
    match (args.next(), args.next(), args.next()) {
        (Some(flag), Some(path), None) if flag == "--manifest" => Ok(PathBuf::from(path)),
        _ => Err(ManifestError(
            "usage: voisu-local-asr --manifest <path>".into(),
        )),
    }
}

fn config() -> Result<(Config, voisu_local_asr::manifest::WorkerManifest), ManifestError> {
    let path = manifest_path()?;
    let running = std::env::current_exe().ok();
    let manifest = load_manifest(&path, running.as_deref())?;
    let config = Config {
        model_id: manifest.model_id.clone(),
        observed_device: manifest.device.clone(),
        backend: format!("whisper.cpp {}", voisu_whisper_sys::WHISPER_CPP_VERSION),
        health_samples: read_health_wav(&manifest.health_audio)?,
        health_expected: manifest.health_expected.clone(),
    };
    Ok((config, manifest))
}

fn probe() -> ExitCode {
    match probe_gpu() {
        Ok(gpu) => {
            println!("{} {}", device_label(&gpu.name), gpu.description);
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("voisu-asr: {}: {}", error.code, error.message);
            ExitCode::from(3)
        }
    }
}

fn main() -> ExitCode {
    // ggml loads every module GGML_BACKEND_PATH names, whatever directory the
    // backends are loaded from; only the modules beside this worker may load.
    // SAFETY: no other thread exists yet, so nothing reads the environment
    // concurrently.
    unsafe { std::env::remove_var("GGML_BACKEND_PATH") };
    if std::env::args_os().skip(1).eq(["--probe-gpu"]) {
        return probe();
    }
    let mut frames: Frames = match claim_stdout() {
        Ok(frames) => frames,
        Err(error) => {
            eprintln!("voisu-asr: cannot claim stdout: {error}");
            return ExitCode::from(1);
        }
    };
    let (config, manifest) = match config() {
        Ok(loaded) => loaded,
        Err(ManifestError(message)) => {
            eprintln!("voisu-asr: manifest refused: {message}");
            let refusal = serde_json::json!({
                "v": PROTOCOL_VERSION,
                "id": "",
                "status": "error",
                "code": "manifest_error",
                "message": wire::sanitize_message(&message),
            });
            let _ = wire::write_frame(&mut frames, &refusal);
            return ExitCode::from(2);
        }
    };
    let load = || {
        WhisperCpp::load(
            &manifest.model_path,
            manifest.engine_device,
            manifest.decode,
        )
    };
    let exit = server::serve(config, load, std::io::stdin(), &mut frames);
    let _ = frames.flush();
    match exit {
        Exit::Shutdown | Exit::EndOfInput => ExitCode::SUCCESS,
        Exit::Protocol(reason) => {
            eprintln!("voisu-asr: protocol stream ended: {reason}");
            ExitCode::from(1)
        }
    }
}
