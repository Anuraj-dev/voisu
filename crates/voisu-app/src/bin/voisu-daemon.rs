use std::fs;
use std::path::Path;
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex;
use voisu_core::{
    AudioCapture, CapturedAudio, Clock, DeliveryAdapter, PROTOCOL_VERSION, RecordingLifecycle,
    Request, Response, SourceTranscript, Transcript, TranscriptProvider, TranscriptValidator,
    socket_path,
};

#[tokio::main]
async fn main() {
    if let Err(message) = run().await {
        eprintln!("{message}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), String> {
    let path = socket_path()?;
    let parent = path
        .parent()
        .ok_or_else(|| "daemon socket has no parent directory".to_owned())?;
    fs::create_dir_all(parent).map_err(|error| format!("cannot create runtime directory: {error}"))?;
    let listener = UnixListener::bind(&path)
        .map_err(|error| format!("cannot bind daemon socket {}: {error}", path.display()))?;
    let _socket = SocketCleanup(&path);
    let lifecycle = Arc::new(Mutex::new(RecordingLifecycle::new(
        Box::new(ControlledCapture),
        Box::new(ControlledProvider),
        Box::new(ControlledValidator),
        Box::new(ControlledDelivery),
        Box::new(ControlledClock),
    )));

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted.map_err(|error| format!("cannot accept CLI connection: {error}"))?;
                let lifecycle = Arc::clone(&lifecycle);
                tokio::spawn(async move {
                    let _ = serve(stream, lifecycle).await;
                });
            }
            signal = tokio::signal::ctrl_c() => {
                signal.map_err(|error| format!("cannot listen for shutdown: {error}"))?;
                return Ok(());
            }
        }
    }
}

async fn serve(
    stream: UnixStream,
    lifecycle: Arc<Mutex<RecordingLifecycle>>,
) -> Result<(), String> {
    let (reader, mut writer) = stream.into_split();
    let mut request = String::new();
    BufReader::new(reader)
        .read_line(&mut request)
        .await
        .map_err(|error| format!("cannot read CLI command: {error}"))?;
    let request: Request = serde_json::from_str(&request)
        .map_err(|error| format!("cannot decode CLI command: {error}"))?;
    let response = if request.version == PROTOCOL_VERSION {
        lifecycle.lock().await.execute(request.command)
    } else {
        Response::rejected(
            None,
            format!(
                "unsupported protocol version {}; expected {PROTOCOL_VERSION}",
                request.version
            ),
        )
    };
    let mut encoded = serde_json::to_vec(&response)
        .map_err(|error| format!("cannot encode daemon response: {error}"))?;
    encoded.push(b'\n');
    writer
        .write_all(&encoded)
        .await
        .map_err(|error| format!("cannot write daemon response: {error}"))
}

struct SocketCleanup<'a>(&'a Path);

impl Drop for SocketCleanup<'_> {
    fn drop(&mut self) {
        let _ = fs::remove_file(self.0);
    }
}

struct ControlledCapture;

impl AudioCapture for ControlledCapture {
    fn begin(&mut self) -> Result<(), String> {
        Ok(())
    }

    fn finish(&mut self) -> Result<CapturedAudio, String> {
        Ok(CapturedAudio)
    }
}

struct ControlledProvider;

impl TranscriptProvider for ControlledProvider {
    fn transcribe(&mut self, _audio: CapturedAudio) -> Result<SourceTranscript, String> {
        Ok(SourceTranscript("controlled Source Transcript".to_owned()))
    }
}

struct ControlledValidator;

impl TranscriptValidator for ControlledValidator {
    fn validate(&mut self, source: SourceTranscript) -> Result<Transcript, String> {
        Ok(Transcript(source.0))
    }
}

struct ControlledDelivery;

impl DeliveryAdapter for ControlledDelivery {
    fn deliver(&mut self, _transcript: Transcript) -> Result<(), String> {
        Ok(())
    }
}

struct ControlledClock;

impl Clock for ControlledClock {
    fn now_millis(&mut self) -> u64 {
        0
    }
}
