//! The worker protocol v1 server loop, matching `serve_stream` in
//! `tools/local-asr/voisu_asr/service.py`.
//!
//! Prepare loads the engine and runs one real health inference before Ready.
//! Transcribe reads its PCM, splits it into windows ([`crate::window`]), runs
//! each through the engine with the request's prompt, drops windows that only
//! echo the prompt ([`crate::echo`]) and joins the rest. Any failed window
//! fails the whole request: there is never a partial Transcript.
//!
//! A reader thread owns stdin, so a Shutdown frame or end of input that
//! arrives while a window is decoding sets the abort flag the engine polls
//! (whisper.cpp's `abort_callback`). The request then ends as
//! `Error{cancelled}` and the queued Shutdown is still answered. Clients
//! therefore wait for each reply before sending the next request, as the
//! daemon does: a Shutdown pipelined behind a Prepare cancels that Prepare.

use std::collections::HashSet;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, mpsc};
use std::time::Instant;

use regex::Regex;
use serde_json::{Map, Value, json};
use voisu_core::local_wire::{MAX_TRANSCRIPT_BYTES, PROTOCOL_VERSION, is_valid_prompt};

use crate::echo::is_prompt_echo;
use crate::engine::{Engine, EngineError};
use crate::window::{TooManyWindows, plan_windows};
use crate::wire::{self, WireError};

/// What the Ready frame reports and the health check expects.
#[derive(Clone, Debug)]
pub struct Config {
    pub model_id: String,
    pub observed_device: String,
    pub backend: String,
    pub health_samples: Vec<i16>,
    pub health_expected: String,
}

/// How a session ended.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Exit {
    /// A Shutdown frame was answered.
    Shutdown,
    /// Input ended between frames.
    EndOfInput,
    /// The stream broke (framing, PCM size, a failed write); already reported
    /// when the output still worked.
    Protocol(String),
}

enum Inbound {
    Request {
        frame: Value,
        pcm: Option<Vec<i16>>,
    },
    /// Fatal: the stream cannot be resynchronized. Answer, then stop.
    Broken {
        id: String,
        error: WireError,
    },
    EndOfInput,
}

/// Whether the session will answer this frame with Shutdown: the same checks
/// as [`Session::handle`] and the duplicate-id check in [`serve`], so a
/// refused Shutdown never cancels anything. `seen` mirrors `serve`'s ids.
fn is_honored_shutdown(frame: &Value, seen: &mut HashSet<String>) -> bool {
    let Some(id) = wire::request_id(frame) else {
        return false;
    };
    let fresh = seen.insert(id.to_owned());
    fresh
        && frame.get("v").and_then(Value::as_u64) == Some(u64::from(PROTOCOL_VERSION))
        && frame.get("op").and_then(Value::as_str) == Some("shutdown")
        && frame.as_object().map(Map::len) == Some(3)
}

/// Reads frames (and each Transcribe's PCM, before anything else is judged,
/// so a refused request never leaves audio in the stream) until input ends.
///
/// `sender` is a rendezvous channel: while the session works on one request
/// this thread holds at most the next one, so pipelined Transcribes cannot
/// pile up PCM in memory. Abort is set before the hand-off blocks.
fn read_requests(mut input: impl Read, sender: mpsc::SyncSender<Inbound>, abort: Arc<AtomicBool>) {
    let mut seen = HashSet::new();
    loop {
        let inbound = match wire::read_frame(&mut input) {
            Ok(None) => Inbound::EndOfInput,
            Err(error) => Inbound::Broken {
                id: String::new(),
                error,
            },
            Ok(Some(frame)) if frame.get("op").and_then(Value::as_str) == Some("transcribe") => {
                let id = wire::request_id(&frame).unwrap_or_default().to_owned();
                match wire::pcm_len(&frame).and_then(|size| wire::read_pcm(&mut input, size)) {
                    Ok(pcm) => {
                        if !id.is_empty() {
                            seen.insert(id);
                        }
                        Inbound::Request {
                            frame,
                            pcm: Some(pcm),
                        }
                    }
                    Err(error) => Inbound::Broken { id, error },
                }
            }
            Ok(Some(frame)) => {
                if is_honored_shutdown(&frame, &mut seen) {
                    abort.store(true, Ordering::Relaxed);
                }
                Inbound::Request { frame, pcm: None }
            }
        };
        let last = !matches!(inbound, Inbound::Request { .. });
        if last {
            // Nobody will ask for more: stop whatever is decoding.
            abort.store(true, Ordering::Relaxed);
        }
        if sender.send(inbound).is_err() || last {
            return;
        }
    }
}

struct Failure {
    code: &'static str,
    message: String,
}

impl Failure {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl From<EngineError> for Failure {
    fn from(error: EngineError) -> Self {
        Self::new(error.code, error.message)
    }
}

struct Session<E, L> {
    config: Config,
    load: L,
    engine: Option<E>,
    prepared: bool,
    inferences: u64,
    abort: Arc<AtomicBool>,
}

impl<E: Engine, L: FnMut() -> Result<E, EngineError>> Session<E, L> {
    fn cancelled(&self) -> bool {
        self.abort.load(Ordering::Relaxed)
    }

    /// One Recording through every window; text, or the first failure.
    fn transcribe(&mut self, samples: &[i16], prompt: Option<&str>) -> Result<String, Failure> {
        let windows = plan_windows(samples).map_err(|TooManyWindows| {
            Failure::new(
                "too_many_windows",
                "Recording needs more than 20 windows of 30 seconds",
            )
        })?;
        let mut texts = Vec::new();
        for window in windows {
            let pcm = &samples[window.start..window.end];
            if pcm.iter().all(|&sample| sample == 0) {
                continue;
            }
            if self.cancelled() {
                return Err(Failure::new("cancelled", "request was cancelled"));
            }
            let audio: Vec<f32> = pcm
                .iter()
                .map(|&sample| f32::from(sample) / 32768.0)
                .collect();
            let engine = self
                .engine
                .as_mut()
                .ok_or_else(|| Failure::new("not_ready", "model is not loaded"))?;
            let result = engine.transcribe(&audio, prompt, &self.abort);
            if self.cancelled() {
                return Err(Failure::new("cancelled", "request was cancelled"));
            }
            let text = result?.split_whitespace().collect::<Vec<_>>().join(" ");
            self.inferences += 1;
            if prompt.is_some_and(|prompt| is_prompt_echo(&text, prompt)) {
                // Local diagnostic only, and it never names the terms.
                eprintln!("voisu-asr: dropped a window that only listed Dictionary terms");
                continue;
            }
            if !text.is_empty() {
                texts.push(text);
            }
        }
        let joined = texts.join(" ");
        if joined.len() > MAX_TRANSCRIPT_BYTES {
            return Err(Failure::new(
                "transcript_too_large",
                format!("joined Transcript exceeds {MAX_TRANSCRIPT_BYTES} bytes"),
            ));
        }
        Ok(joined)
    }

    fn prepare(&mut self) -> Result<(), Failure> {
        if self.prepared {
            return Ok(());
        }
        if self.engine.is_none() {
            self.engine = Some((self.load)()?);
        }
        let health = std::mem::take(&mut self.config.health_samples);
        let result = self.transcribe(&health, None);
        self.config.health_samples = health;
        let text = result?;
        if text.trim().is_empty() {
            return Err(Failure::new(
                "health_empty",
                "health inference returned no text",
            ));
        }
        if !health_phrase_matches(&text, &self.config.health_expected) {
            return Err(Failure::new(
                "health_mismatch",
                "health inference did not match the pinned phrase",
            ));
        }
        self.prepared = true;
        Ok(())
    }

    /// The engine's device once loaded (a GPU reports which one), else the
    /// manifest's.
    fn observed_device(&self) -> String {
        self.engine
            .as_ref()
            .and_then(Engine::observed_device)
            .unwrap_or_else(|| self.config.observed_device.clone())
    }

    fn resident(&self) -> bool {
        // Load plus one inference is still cold; a second without reload proves residency.
        self.inferences >= 2
    }

    fn ready(&self, id: &str) -> Value {
        json!({
            "v": PROTOCOL_VERSION,
            "id": id,
            "status": "ready",
            "model_id": self.config.model_id,
            "observed_device": self.observed_device(),
            "resident": self.resident(),
            "backend": self.config.backend,
            "peak_host_ram_bytes": peak_host_ram_bytes(),
            "peak_gpu_memory_bytes": 0,
            "accepts_prompt": true,
        })
    }

    fn handle(&mut self, frame: &Value, pcm: Option<Vec<i16>>) -> Result<Value, (String, Failure)> {
        let Some(id) = wire::request_id(frame) else {
            return Err((
                String::new(),
                Failure::new(
                    "invalid_id",
                    "request id must be a non-empty bounded string",
                ),
            ));
        };
        let id = id.to_owned();
        let fail = |failure: Failure| (id.clone(), failure);
        let keys = frame.as_object().map(Map::len).unwrap_or_default();
        if frame.get("v").and_then(Value::as_u64) != Some(u64::from(PROTOCOL_VERSION)) {
            return Err(fail(Failure::new(
                "unsupported_version",
                "request protocol version must be 1",
            )));
        }
        match frame.get("op").and_then(Value::as_str) {
            Some("prepare") => {
                if keys != 3 {
                    return Err(fail(Failure::new(
                        "invalid_request",
                        "prepare has unknown fields",
                    )));
                }
                self.prepare().map_err(fail)?;
                Ok(self.ready(&id))
            }
            Some("transcribe") => {
                let prompt = frame.get("prompt");
                if keys != 4 + usize::from(prompt.is_some()) {
                    return Err(fail(Failure::new(
                        "invalid_request",
                        "transcribe has unknown fields",
                    )));
                }
                let prompt = match prompt {
                    None => None,
                    Some(value) => match value.as_str().filter(|prompt| is_valid_prompt(prompt)) {
                        Some(prompt) => Some(prompt.to_owned()),
                        None => {
                            return Err(fail(Failure::new(
                                "invalid_request",
                                "prompt must be a non-empty string without control characters within the size limit",
                            )));
                        }
                    },
                };
                if !self.prepared {
                    return Err(fail(Failure::new(
                        "not_ready",
                        "send prepare before transcribe",
                    )));
                }
                let samples = pcm.unwrap_or_default();
                let started = Instant::now();
                let cpu_started = cpu_time_ms();
                let text = self.transcribe(&samples, prompt.as_deref()).map_err(fail)?;
                let mut reply = json!({
                    "v": PROTOCOL_VERSION,
                    "id": id,
                    "status": if text.is_empty() { "no_text" } else { "transcript" },
                    "observed_device": self.observed_device(),
                    "resident": self.resident(),
                    "inference_ms": u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                    "cpu_time_ms": cpu_time_ms().saturating_sub(cpu_started),
                    "peak_host_ram_bytes": peak_host_ram_bytes(),
                    "peak_gpu_memory_bytes": 0,
                });
                if !text.is_empty() {
                    reply["text"] = Value::String(text);
                }
                Ok(reply)
            }
            Some("shutdown") => {
                if keys != 3 {
                    return Err(fail(Failure::new(
                        "invalid_request",
                        "shutdown has unknown fields",
                    )));
                }
                Ok(json!({ "v": PROTOCOL_VERSION, "id": id, "status": "shutdown" }))
            }
            _ => Err(fail(Failure::new(
                "unknown_operation",
                "operation must be prepare, transcribe, or shutdown",
            ))),
        }
    }
}

fn error_frame(id: &str, code: &str, message: &str) -> Value {
    json!({
        "v": PROTOCOL_VERSION,
        "id": id,
        "status": "error",
        "code": code,
        "message": wire::sanitize_message(message),
    })
}

/// Serve one daemon session over `input`/`output` until Shutdown or end of
/// input. `load` runs on the first Prepare; only frames reach `output`.
pub fn serve<E, L, R, W>(config: Config, load: L, input: R, output: &mut W) -> Exit
where
    E: Engine,
    L: FnMut() -> Result<E, EngineError>,
    R: Read + Send + 'static,
    W: Write,
{
    let abort = Arc::new(AtomicBool::new(false));
    let (sender, receiver) = mpsc::sync_channel(0);
    let reader_abort = Arc::clone(&abort);
    std::thread::Builder::new()
        .name("voisu-local-asr-stdin".into())
        .spawn(move || read_requests(input, sender, reader_abort))
        .expect("spawn the stdin reader");

    let mut session = Session {
        config,
        load,
        engine: None,
        prepared: false,
        inferences: 0,
        abort,
    };
    let mut seen_ids = HashSet::new();
    loop {
        let (frame, pcm) = match receiver.recv() {
            Ok(Inbound::Request { frame, pcm }) => (frame, pcm),
            Ok(Inbound::EndOfInput) | Err(_) => return Exit::EndOfInput,
            Ok(Inbound::Broken { id, error }) => {
                let _ = wire::write_frame(output, &error_frame(&id, error.code, &error.message));
                return Exit::Protocol(error.code.to_owned());
            }
        };
        let reply = match wire::request_id(&frame) {
            Some(id) if !seen_ids.insert(id.to_owned()) => Err((
                id.to_owned(),
                Failure::new("duplicate_id", "request id must be unique for this worker"),
            )),
            _ => session.handle(&frame, pcm),
        };
        let (frame_out, shutdown) = match reply {
            Ok(reply) => {
                let shutdown = reply["status"] == "shutdown";
                (reply, shutdown)
            }
            Err((id, failure)) => (error_frame(&id, failure.code, &failure.message), false),
        };
        if let Err(error) = wire::write_frame(output, &frame_out) {
            return Exit::Protocol(format!("write failed: {error}"));
        }
        if shutdown {
            return Exit::Shutdown;
        }
    }
}

/// The health gate: same tokens in the same order, ignoring case and
/// punctuation (`normalize_tokens` in `voisu_asr/audio.py`).
#[must_use]
pub fn health_phrase_matches(actual: &str, expected: &str) -> bool {
    static TOKEN: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"[^\W_]+(?:['’][^\W_]+)?").expect("static pattern"));
    let tokens = |text: &str| -> Vec<String> {
        TOKEN
            .find_iter(text)
            .map(|found| found.as_str().to_lowercase())
            .collect()
    };
    let expected = tokens(expected);
    !expected.is_empty() && tokens(actual) == expected
}

#[cfg(unix)]
fn rusage() -> libc::rusage {
    // SAFETY: getrusage fills the zeroed struct for this process.
    unsafe {
        let mut usage: libc::rusage = std::mem::zeroed();
        libc::getrusage(libc::RUSAGE_SELF, &mut usage);
        usage
    }
}

/// User plus system CPU time of every worker thread so far.
#[cfg(unix)]
fn cpu_time_ms() -> u64 {
    let usage = rusage();
    let ms = |time: libc::timeval| {
        u64::try_from(time.tv_sec).unwrap_or(0) * 1000
            + u64::try_from(time.tv_usec).unwrap_or(0) / 1000
    };
    ms(usage.ru_utime) + ms(usage.ru_stime)
}

/// Process high-water RSS.
#[cfg(unix)]
fn peak_host_ram_bytes() -> u64 {
    let max_rss = u64::try_from(rusage().ru_maxrss).unwrap_or(0);
    // Linux reports kilobytes; macOS reports bytes.
    if cfg!(target_os = "macos") {
        max_rss
    } else {
        max_rss * 1024
    }
}

#[cfg(not(unix))]
fn cpu_time_ms() -> u64 {
    0
}

#[cfg(not(unix))]
fn peak_host_ram_bytes() -> u64 {
    0
}

#[cfg(test)]
mod tests {
    use super::health_phrase_matches;

    #[test]
    fn health_gate_ignores_case_and_punctuation_but_not_words() {
        let expected = "And so my fellow Americans, ask not what your country can do for you";
        assert!(health_phrase_matches(
            "and so, my fellow Americans: ask not what your country can do for you.",
            expected
        ));
        assert!(!health_phrase_matches(
            "and so my fellow Americans ask not",
            expected
        ));
        assert!(!health_phrase_matches("anything", ""));
        assert!(health_phrase_matches("Don’t stop", "don’t stop"));
    }
}
