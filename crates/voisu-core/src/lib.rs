//! Shared domain, lifecycle, and IPC types for Voisu.

use std::env;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 1;

pub fn socket_path() -> Result<PathBuf, String> {
    let runtime_dir = env::var_os("XDG_RUNTIME_DIR")
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "XDG_RUNTIME_DIR is not set".to_owned())?;
    Ok(PathBuf::from(runtime_dir)
        .join("voisu")
        .join(format!("v{PROTOCOL_VERSION}"))
        .join("daemon.sock"))
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Command {
    Start,
    Stop,
    Toggle,
    Status,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DaemonState {
    Idle,
    Recording,
}

impl DaemonState {
    pub fn cli_label(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Recording => "Recording",
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Request {
    pub version: u32,
    pub command: Command,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Response {
    pub version: u32,
    pub ok: bool,
    pub state: Option<DaemonState>,
    pub message: String,
}

impl Response {
    pub fn success(state: DaemonState, message: impl Into<String>) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            ok: true,
            state: Some(state),
            message: message.into(),
        }
    }

    pub fn rejected(state: Option<DaemonState>, message: impl Into<String>) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            ok: false,
            state,
            message: message.into(),
        }
    }
}

#[derive(Debug)]
pub struct CapturedAudio;

#[derive(Debug)]
pub struct SourceTranscript(pub String);

#[derive(Debug)]
pub struct Transcript(pub String);

pub trait AudioCapture: Send {
    fn begin(&mut self) -> Result<(), String>;
    fn finish(&mut self) -> Result<CapturedAudio, String>;
}

pub trait TranscriptProvider: Send {
    fn transcribe(&mut self, audio: CapturedAudio) -> Result<SourceTranscript, String>;
}

pub trait TranscriptValidator: Send {
    fn validate(&mut self, source: SourceTranscript) -> Result<Transcript, String>;
}

pub trait DeliveryAdapter: Send {
    fn deliver(&mut self, transcript: Transcript) -> Result<(), String>;
}

pub trait Clock: Send {
    fn now_millis(&mut self) -> u64;
}

pub struct RecordingLifecycle {
    state: DaemonState,
    started_at_millis: Option<u64>,
    capture: Box<dyn AudioCapture>,
    provider: Box<dyn TranscriptProvider>,
    validator: Box<dyn TranscriptValidator>,
    delivery: Box<dyn DeliveryAdapter>,
    clock: Box<dyn Clock>,
}

impl RecordingLifecycle {
    pub fn new(
        capture: Box<dyn AudioCapture>,
        provider: Box<dyn TranscriptProvider>,
        validator: Box<dyn TranscriptValidator>,
        delivery: Box<dyn DeliveryAdapter>,
        clock: Box<dyn Clock>,
    ) -> Self {
        Self {
            state: DaemonState::Idle,
            started_at_millis: None,
            capture,
            provider,
            validator,
            delivery,
            clock,
        }
    }

    pub fn execute(&mut self, command: Command) -> Response {
        match command {
            Command::Start => self.start(),
            Command::Stop => self.stop(),
            Command::Toggle if self.state == DaemonState::Idle => self.start(),
            Command::Toggle => self.stop(),
            Command::Status => Response::success(self.state, self.state.cli_label()),
        }
    }

    fn start(&mut self) -> Response {
        if self.state == DaemonState::Recording {
            return Response::rejected(Some(self.state), "Recording already active");
        }
        if let Err(message) = self.capture.begin() {
            return Response::rejected(Some(self.state), message);
        }
        self.started_at_millis = Some(self.clock.now_millis());
        self.state = DaemonState::Recording;
        Response::success(self.state, "Recording started")
    }

    fn stop(&mut self) -> Response {
        if self.state == DaemonState::Idle {
            return Response::rejected(Some(self.state), "No Recording active");
        }

        let audio = match self.capture.finish() {
            Ok(audio) => audio,
            Err(message) => return Response::rejected(Some(self.state), message),
        };
        self.state = DaemonState::Idle;
        self.started_at_millis = None;

        let completed = self
            .provider
            .transcribe(audio)
            .and_then(|source| self.validator.validate(source))
            .and_then(|transcript| self.delivery.deliver(transcript));

        match completed {
            Ok(()) => Response::success(self.state, "Recording completed; Transcript delivered"),
            Err(message) => Response::rejected(Some(self.state), message),
        }
    }
}
