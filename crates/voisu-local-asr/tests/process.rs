//! The real `voisu-local-asr` binary over stdin/stdout.
//!
//! `whisper_cpp_transcribes_the_health_clip` needs a GGML model and a 16 kHz
//! mono WAV of the JFK clip, so it is `#[ignore]`d; run it with both env vars
//! naming files (it fails, not skips, when they do not). It runs on the CPU
//! unless `VOISU_WHISPER_TEST_DEVICE=gpu`, which needs a build with
//! `--features vulkan` and a Vulkan GPU:
//!
//! ```text
//! VOISU_WHISPER_TEST_MODEL=/path/ggml-tiny.en.bin \
//! VOISU_WHISPER_TEST_WAV=/path/jfk.wav \
//!   cargo test -p voisu-local-asr --test process -- --ignored
//! ```

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use voisu_local_asr::wire;

const JFK: &str = "And so my fellow Americans ask not what your country can do for you. Ask what you can do for your country.";

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("voisu-local-asr-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn worker() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_voisu-local-asr"))
}

fn pin(path: &Path) -> Value {
    let bytes = std::fs::read(path).unwrap();
    let sha256: String = Sha256::digest(&bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    json!({ "path": path, "sha256": sha256, "bytes": bytes.len() })
}

fn wav(samples: &[i16]) -> Vec<u8> {
    let data = u32::try_from(samples.len() * 2).unwrap();
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36 + data).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16u32.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&16_000u32.to_le_bytes());
    bytes.extend_from_slice(&32_000u32.to_le_bytes());
    bytes.extend_from_slice(&2u16.to_le_bytes());
    bytes.extend_from_slice(&16u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&data.to_le_bytes());
    bytes.extend(samples.iter().flat_map(|sample| sample.to_le_bytes()));
    bytes
}

fn manifest(dir: &Path, model: &Path, health: &Path) -> PathBuf {
    manifest_on(dir, model, health, "cpu", 0)
}

fn manifest_on(dir: &Path, model: &Path, health: &Path, device: &str, vram_bytes: u64) -> PathBuf {
    let document = json!({
        "version": 1,
        "model_id": "whisper-cpp-test",
        "device": device,
        "language": "en",
        "runtime": { "worker": worker(), "revision": "test" },
        "model_path": model,
        "files": [pin(&worker()), pin(model), pin(health)],
        "options": { "health_audio": health, "health_expected": JFK, "n_threads": 4 },
        "limits": { "cpu_cores": 4, "vram_bytes": vram_bytes }
    });
    let path = dir.join("manifest.json");
    std::fs::write(&path, serde_json::to_vec(&document).unwrap()).unwrap();
    path
}

fn frame(value: &Value) -> Vec<u8> {
    let mut bytes = Vec::new();
    wire::write_frame(&mut bytes, value).unwrap();
    bytes
}

struct Session {
    status: std::process::ExitStatus,
    frames: Vec<Value>,
    stderr: String,
}

/// Run the worker, sending each request only after the previous reply (as the
/// daemon does), then close stdin. Every byte on stdout must be a frame.
fn run(manifest: &Path, requests: &[Vec<u8>]) -> Session {
    let mut child = Command::new(worker())
        .arg("--manifest")
        .arg(manifest)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let stderr = std::thread::spawn(move || {
        let mut text = String::new();
        stderr.read_to_string(&mut text).unwrap();
        text
    });
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let mut frames = Vec::new();
    for request in requests {
        if stdin.write_all(request).is_err() {
            break;
        }
        match wire::read_frame(&mut stdout) {
            Ok(Some(reply)) => frames.push(reply),
            Ok(None) => break,
            Err(error) => panic!("stdout is not frames only: {error:?}"),
        }
    }
    drop(stdin);
    let mut rest = Vec::new();
    stdout.read_to_end(&mut rest).unwrap();
    let mut rest = rest.as_slice();
    while let Some(reply) = wire::read_frame(&mut rest)
        .unwrap_or_else(|error| panic!("stdout is not frames only: {error:?}"))
    {
        frames.push(reply);
    }
    let status = child.wait().unwrap();
    Session {
        status,
        frames,
        stderr: stderr.join().unwrap(),
    }
}

#[test]
fn stdout_carries_only_frames_while_whisper_logs_go_to_stderr() {
    let dir = TempDir::new("frames-only");
    let model = dir.0.join("model.bin");
    std::fs::write(&model, b"not a ggml model").unwrap();
    let health = dir.0.join("health.wav");
    std::fs::write(&health, wav(&[1000; 16_000])).unwrap();
    let manifest = manifest(&dir.0, &model, &health);

    let requests = [
        frame(&json!({ "v": 1, "op": "prepare", "id": "p" })),
        frame(&json!({ "v": 1, "op": "shutdown", "id": "s" })),
    ];
    let Session {
        status,
        frames,
        stderr,
    } = run(&manifest, &requests);

    assert_eq!(status.code(), Some(0));
    assert_eq!(frames.len(), 2, "{frames:?}");
    assert_eq!(frames[0]["id"], "p");
    assert_eq!(frames[0]["code"], "model_load_failed");
    assert_eq!(frames[1]["status"], "shutdown");
    assert!(
        stderr.contains("whisper"),
        "whisper.cpp logs belong on stderr: {stderr}"
    );
}

#[test]
fn a_refused_manifest_is_one_startup_error_frame_and_exit_2() {
    let dir = TempDir::new("refused");
    let model = dir.0.join("model.bin");
    std::fs::write(&model, b"model").unwrap();
    let health = dir.0.join("health.wav");
    std::fs::write(&health, wav(&[1000; 160])).unwrap();
    let manifest = manifest(&dir.0, &model, &health);
    std::fs::write(&model, b"swapped after the daemon looked").unwrap();

    let Session { status, frames, .. } = run(&manifest, &[]);
    assert_eq!(status.code(), Some(2));
    assert_eq!(frames.len(), 1, "{frames:?}");
    assert_eq!(frames[0]["id"], "");
    assert_eq!(frames[0]["code"], "manifest_error");
    assert!(
        frames[0]["message"]
            .as_str()
            .unwrap()
            .contains("pinned artifact changed")
    );
}

fn fixture(name: &str) -> Option<PathBuf> {
    let path = PathBuf::from(std::env::var_os(name)?);
    path.is_file().then_some(path)
}

#[test]
#[ignore = "needs VOISU_WHISPER_TEST_MODEL and VOISU_WHISPER_TEST_WAV; run with --ignored"]
fn whisper_cpp_transcribes_the_health_clip() {
    let (Some(model), Some(clip)) = (
        fixture("VOISU_WHISPER_TEST_MODEL"),
        fixture("VOISU_WHISPER_TEST_WAV"),
    ) else {
        panic!("set VOISU_WHISPER_TEST_MODEL and VOISU_WHISPER_TEST_WAV to existing files");
    };
    let gpu = std::env::var("VOISU_WHISPER_TEST_DEVICE").as_deref() == Ok("gpu");
    let dir = TempDir::new("whisper");
    let manifest = if gpu {
        manifest_on(&dir.0, &model, &clip, "gpu", 1536 << 20)
    } else {
        manifest(&dir.0, &model, &clip)
    };
    let samples = voisu_local_asr::manifest::read_health_wav(&clip).unwrap();
    let pcm: Vec<u8> = samples
        .iter()
        .flat_map(|sample| sample.to_le_bytes())
        .collect();

    let mut transcribe =
        frame(&json!({ "v": 1, "op": "transcribe", "id": "t", "pcm_bytes": pcm.len() }));
    transcribe.extend(&pcm);
    let requests = [
        frame(&json!({ "v": 1, "op": "prepare", "id": "p" })),
        transcribe,
        frame(&json!({ "v": 1, "op": "shutdown", "id": "s" })),
    ];
    let Session {
        status,
        frames,
        stderr,
    } = run(&manifest, &requests);

    assert_eq!(status.code(), Some(0), "{stderr}");
    assert_eq!(frames[0]["status"], "ready", "{frames:?}");
    assert_eq!(frames[1]["status"], "transcript", "{frames:?}");
    let observed = frames[1]["observed_device"].as_str().unwrap();
    if gpu {
        assert!(observed.starts_with("vulkan:"), "{observed}");
    } else {
        assert_eq!(observed, "cpu");
    }
    assert!(
        voisu_local_asr::server::health_phrase_matches(frames[1]["text"].as_str().unwrap(), JFK),
        "{}",
        frames[1]["text"]
    );
    assert_eq!(frames[1]["resident"], true);
    assert_eq!(frames[2]["status"], "shutdown");
}
