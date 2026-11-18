//! Protocol behavior of the server loop, driven with a scripted engine.

use std::io::{self, Read};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use voisu_core::local_wire::MAX_PCM_BYTES;
use voisu_local_asr::engine::{Engine, EngineError};
use voisu_local_asr::server::{Config, Exit, serve};
use voisu_local_asr::window::plan_windows;
use voisu_local_asr::wire;

const HEALTH: &str = "And so my fellow Americans";

/// stdin that the test feeds chunk by chunk, so a request can arrive while
/// another one is still decoding.
struct Input {
    chunks: Receiver<Vec<u8>>,
    pending: Vec<u8>,
}

impl Read for Input {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        while self.pending.is_empty() {
            match self.chunks.recv() {
                Ok(chunk) => self.pending = chunk,
                Err(_) => return Ok(0),
            }
        }
        let n = buf.len().min(self.pending.len());
        buf[..n].copy_from_slice(&self.pending[..n]);
        self.pending.drain(..n);
        Ok(n)
    }
}

#[derive(Clone, Debug, PartialEq)]
struct Call {
    samples: usize,
    prompt: Option<String>,
}

/// What the scripted engine does for one window.
enum Step {
    Text(&'static str),
    Fail,
    /// Block until the request is cancelled, then report the abort.
    WaitForAbort,
}

struct Scripted {
    steps: Arc<Mutex<Vec<Step>>>,
    calls: Arc<Mutex<Vec<Call>>>,
    started: Sender<()>,
}

impl Engine for Scripted {
    fn transcribe(
        &mut self,
        samples: &[f32],
        prompt: Option<&str>,
        abort: &AtomicBool,
    ) -> Result<String, EngineError> {
        self.calls.lock().unwrap().push(Call {
            samples: samples.len(),
            prompt: prompt.map(str::to_owned),
        });
        let _ = self.started.send(());
        let mut steps = self.steps.lock().unwrap();
        let step = if steps.is_empty() {
            Step::Text("")
        } else {
            steps.remove(0)
        };
        drop(steps);
        match step {
            Step::Text(text) => Ok(text.to_owned()),
            Step::Fail => Err(EngineError::new("inference_failed", "window failed")),
            Step::WaitForAbort => {
                let deadline = Instant::now() + Duration::from_secs(10);
                while !abort.load(Ordering::Relaxed) {
                    assert!(Instant::now() < deadline, "abort never arrived");
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(EngineError::new("inference_failed", "aborted"))
            }
        }
    }
}

struct Harness {
    input: Option<Sender<Vec<u8>>>,
    replies: Receiver<Value>,
    worker: Option<JoinHandle<Exit>>,
    calls: Arc<Mutex<Vec<Call>>>,
    started: Receiver<()>,
}

/// Collects reply frames as the server writes them.
struct Output {
    buffer: Vec<u8>,
    replies: Sender<Value>,
}

impl io::Write for Output {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.buffer.extend_from_slice(buf);
        loop {
            let mut reader = self.buffer.as_slice();
            let Ok(Some(frame)) = wire::read_frame(&mut reader) else {
                break;
            };
            let used = self.buffer.len() - reader.len();
            self.buffer.drain(..used);
            let _ = self.replies.send(frame);
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn health_samples() -> Vec<i16> {
    (0..16_000)
        .map(|i| if i % 80 < 40 { 1000 } else { -1000 })
        .collect()
}

impl Harness {
    fn start(steps: Vec<Step>) -> Self {
        let steps = Arc::new(Mutex::new(steps));
        let calls = Arc::new(Mutex::new(Vec::new()));
        let (started_tx, started) = mpsc::channel();
        let (input_tx, input_rx) = mpsc::channel();
        let (replies_tx, replies) = mpsc::channel();
        let config = Config {
            model_id: "whisper-cpp-test".into(),
            observed_device: "cpu".into(),
            backend: "whisper.cpp test".into(),
            health_samples: health_samples(),
            health_expected: HEALTH.into(),
        };
        let engine_calls = Arc::clone(&calls);
        let worker = std::thread::spawn(move || {
            let load = move || {
                Ok(Scripted {
                    steps: Arc::clone(&steps),
                    calls: Arc::clone(&engine_calls),
                    started: started_tx.clone(),
                })
            };
            let input = Input {
                chunks: input_rx,
                pending: Vec::new(),
            };
            let mut output = Output {
                buffer: Vec::new(),
                replies: replies_tx,
            };
            serve(config, load, input, &mut output)
        });
        Self {
            input: Some(input_tx),
            replies,
            worker: Some(worker),
            calls,
            started,
        }
    }

    fn send(&self, frame: &Value) {
        let mut bytes = Vec::new();
        wire::write_frame(&mut bytes, frame).unwrap();
        self.input.as_ref().unwrap().send(bytes).unwrap();
    }

    fn send_pcm(&self, samples: &[i16]) {
        let bytes = samples
            .iter()
            .flat_map(|sample| sample.to_le_bytes())
            .collect();
        self.input.as_ref().unwrap().send(bytes).unwrap();
    }

    fn transcribe(&self, id: &str, samples: &[i16], prompt: Option<&str>) {
        let mut frame =
            json!({ "v": 1, "op": "transcribe", "id": id, "pcm_bytes": samples.len() * 2 });
        if let Some(prompt) = prompt {
            frame["prompt"] = json!(prompt);
        }
        self.send(&frame);
        self.send_pcm(samples);
    }

    fn reply(&self) -> Value {
        self.replies
            .recv_timeout(Duration::from_secs(10))
            .expect("a reply frame")
    }

    fn prepare(&self) -> Value {
        self.send(&json!({ "v": 1, "op": "prepare", "id": "p" }));
        self.reply()
    }

    fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }

    /// Close stdin and wait for the server to return.
    fn finish(mut self) -> (Exit, Vec<Value>) {
        drop(self.input.take());
        let exit = self.worker.take().unwrap().join().unwrap();
        (exit, self.replies.try_iter().collect())
    }
}

fn speech(seconds: usize) -> Vec<i16> {
    (0..seconds * 16_000)
        .map(|i| if i % 80 < 40 { 3000 } else { -3000 })
        .collect()
}

#[test]
fn prepare_transcribe_and_shutdown_round_trip() {
    let harness = Harness::start(vec![Step::Text(HEALTH), Step::Text("  hello \n world ")]);
    let ready = harness.prepare();
    assert_eq!(ready["status"], "ready", "{ready}");
    assert_eq!(ready["id"], "p");
    assert_eq!(ready["model_id"], "whisper-cpp-test");
    assert_eq!(ready["observed_device"], "cpu");
    assert_eq!(ready["accepts_prompt"], true);
    // Load plus the health inference is not yet proof of residency.
    assert_eq!(ready["resident"], false);

    harness.transcribe("t1", &speech(2), None);
    let transcript = harness.reply();
    assert_eq!(transcript["status"], "transcript", "{transcript}");
    assert_eq!(transcript["id"], "t1");
    assert_eq!(transcript["text"], "hello world");
    assert_eq!(transcript["resident"], true);
    for field in [
        "inference_ms",
        "cpu_time_ms",
        "peak_host_ram_bytes",
        "peak_gpu_memory_bytes",
    ] {
        assert!(transcript[field].is_u64(), "{field} in {transcript}");
    }

    // Digital silence never reaches the engine.
    harness.transcribe("t2", &vec![0; 32_000], None);
    let empty = harness.reply();
    assert_eq!(empty["status"], "no_text", "{empty}");
    assert!(empty.get("text").is_none());
    assert_eq!(harness.calls().len(), 2);

    harness.send(&json!({ "v": 1, "op": "shutdown", "id": "s" }));
    assert_eq!(
        harness.reply(),
        json!({ "v": 1, "id": "s", "status": "shutdown" })
    );
    let (exit, rest) = harness.finish();
    assert_eq!(exit, Exit::Shutdown);
    assert!(rest.is_empty(), "{rest:?}");
}

#[test]
fn a_failed_health_check_refuses_ready() {
    let harness = Harness::start(vec![Step::Text("something else entirely")]);
    let reply = harness.prepare();
    assert_eq!(reply["status"], "error");
    assert_eq!(reply["code"], "health_mismatch");
    harness.transcribe("t", &speech(1), None);
    assert_eq!(harness.reply()["code"], "not_ready");
}

#[test]
fn requests_are_refused_in_protocol_order() {
    let harness = Harness::start(vec![Step::Text(HEALTH)]);
    harness.transcribe("early", &speech(1), None);
    assert_eq!(harness.reply()["code"], "not_ready");
    harness.prepare();
    harness.send(&json!({ "v": 1, "op": "prepare", "id": "p" }));
    assert_eq!(harness.reply()["code"], "duplicate_id");
    harness.send(&json!({ "v": 2, "op": "prepare", "id": "v2" }));
    assert_eq!(harness.reply()["code"], "unsupported_version");
    harness.send(&json!({ "v": 1, "op": "prepare", "id": "" }));
    assert_eq!(harness.reply()["code"], "invalid_id");
    harness.send(&json!({ "v": 1, "op": "dance", "id": "d" }));
    assert_eq!(harness.reply()["code"], "unknown_operation");
    harness.send(&json!({ "v": 1, "op": "prepare", "id": "x", "extra": 1 }));
    assert_eq!(harness.reply()["code"], "invalid_request");
    // The PCM of a refused Transcribe is still consumed: the next frame parses.
    let mut bad =
        json!({ "v": 1, "op": "transcribe", "id": "bad", "pcm_bytes": 4, "prompt": "a\u{7}b" });
    harness.send(&bad);
    harness.send_pcm(&[1, 2]);
    assert_eq!(harness.reply()["code"], "invalid_request");
    bad["id"] = json!("bad2");
    bad["mystery"] = json!(true);
    harness.send(&bad);
    harness.send_pcm(&[1, 2]);
    assert_eq!(harness.reply()["code"], "invalid_request");
    harness.send(&json!({ "v": 1, "op": "shutdown", "id": "s" }));
    assert_eq!(harness.reply()["status"], "shutdown");
}

#[test]
fn over_cap_pcm_is_refused_before_any_audio_is_read() {
    let harness = Harness::start(vec![Step::Text(HEALTH)]);
    harness.prepare();
    harness
        .send(&json!({ "v": 1, "op": "transcribe", "id": "big", "pcm_bytes": MAX_PCM_BYTES + 2 }));
    let reply = harness.reply();
    assert_eq!(reply["status"], "error");
    assert_eq!(reply["id"], "big");
    assert_eq!(reply["code"], "invalid_pcm_bytes");
    // The stream cannot resynchronize, so the worker stops without waiting for input.
    let worker = harness.worker.as_ref().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !worker.is_finished() {
        assert!(
            Instant::now() < deadline,
            "worker kept reading after a refused PCM size"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    let (exit, _) = harness.finish();
    assert_eq!(exit, Exit::Protocol("invalid_pcm_bytes".into()));
}

#[test]
fn the_prompt_reaches_every_window_and_echo_windows_are_dropped() {
    let prompt = "Glossary: Voisu, Tokio, wayfinder.";
    let samples = speech(75);
    let windows = plan_windows(&samples).unwrap();
    assert_eq!(windows.len(), 3);
    let harness = Harness::start(vec![
        Step::Text(HEALTH),
        Step::Text("first part"),
        Step::Text("Voisu, Tokio, wayfinder."),
        Step::Text("last part"),
    ]);
    harness.prepare();
    harness.transcribe("t", &samples, Some(prompt));
    let reply = harness.reply();
    assert_eq!(reply["text"], "first part last part", "{reply}");
    let calls = harness.calls();
    assert_eq!(
        calls[0].prompt, None,
        "the health check runs without a prompt"
    );
    let window_calls = &calls[1..];
    assert_eq!(
        window_calls
            .iter()
            .map(|call| call.samples)
            .collect::<Vec<_>>(),
        windows
            .iter()
            .map(|window| window.end - window.start)
            .collect::<Vec<_>>()
    );
    assert!(
        window_calls
            .iter()
            .all(|call| call.prompt.as_deref() == Some(prompt))
    );
}

#[test]
fn a_failed_window_fails_the_whole_request_without_a_partial_transcript() {
    let harness = Harness::start(vec![
        Step::Text(HEALTH),
        Step::Text("first part"),
        Step::Fail,
        Step::Text("never decoded"),
    ]);
    harness.prepare();
    harness.transcribe("t", &speech(75), None);
    let reply = harness.reply();
    assert_eq!(reply["status"], "error", "{reply}");
    assert_eq!(reply["code"], "inference_failed");
    assert!(!reply.to_string().contains("first part"));
    // Window 3 never ran.
    assert_eq!(harness.calls().len(), 3);
    let (exit, rest) = harness.finish();
    assert_eq!(exit, Exit::EndOfInput);
    assert!(rest.is_empty(), "no Transcript after the Error: {rest:?}");
}

#[test]
fn shutdown_cancels_a_window_mid_decode_and_is_still_answered() {
    let harness = Harness::start(vec![
        Step::Text(HEALTH),
        Step::Text("first"),
        Step::WaitForAbort,
    ]);
    harness.prepare();
    harness.started.recv().unwrap();
    harness.transcribe("t", &speech(75), None);
    // Wait until window 2 is decoding.
    harness
        .started
        .recv_timeout(Duration::from_secs(5))
        .unwrap();
    harness
        .started
        .recv_timeout(Duration::from_secs(5))
        .unwrap();
    harness.send(&json!({ "v": 1, "op": "shutdown", "id": "s" }));
    let cancelled = harness.reply();
    assert_eq!(cancelled["id"], "t");
    assert_eq!(cancelled["code"], "cancelled", "{cancelled}");
    assert_eq!(harness.reply()["status"], "shutdown");
    assert_eq!(harness.calls().len(), 3, "window 3 never started");
    let (exit, _) = harness.finish();
    assert_eq!(exit, Exit::Shutdown);
}

#[test]
fn end_of_input_cancels_a_window_mid_decode() {
    let harness = Harness::start(vec![Step::Text(HEALTH), Step::WaitForAbort]);
    harness.prepare();
    harness.started.recv().unwrap();
    harness.transcribe("t", &speech(5), None);
    harness
        .started
        .recv_timeout(Duration::from_secs(5))
        .unwrap();
    let (exit, replies) = harness.finish();
    assert_eq!(exit, Exit::EndOfInput);
    assert_eq!(replies.len(), 1, "{replies:?}");
    assert_eq!(replies[0]["code"], "cancelled");
}
