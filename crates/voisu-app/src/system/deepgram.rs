// Deepgram transcript provider: streaming WebSocket connection and transcript accumulation.
//
// Split out of system.rs as a pure move; module-global items come from `super`.

use super::*;

pub struct DeepgramProvider {
    reaper: ProviderReaper,
    /// nova-3 `keyterm` boosting terms, repeated as query params on the
    /// streaming URL. Ticket 04's shared dictionary is wired in here by the
    /// driver at merge; until then the list defaults to empty.
    keyterms: Vec<String>,
}

impl DeepgramProvider {
    /// Builds a Deepgram provider whose streams share the actor-owned `reaper`,
    /// so a stream dropped mid-abort hands its websocket I/O task to the
    /// supervisor the actor drains before Idle. No keyterm boosting.
    pub fn new(reaper: ProviderReaper) -> Self {
        Self::with_keyterms(reaper, Vec::new())
    }

    /// Same as [`DeepgramProvider::new`] but with nova-3 `keyterm` boosting
    /// terms appended to every streaming connection URL.
    pub fn with_keyterms(reaper: ProviderReaper, keyterms: Vec<String>) -> Self {
        Self { reaper, keyterms }
    }
}

impl TranscriptProvider for DeepgramProvider {
    fn start(&mut self, _recording_id: u64) -> Result<Box<dyn ProviderStream>, BoundaryError> {
        let credential = SecretStore::load(&mut SecretToolStore, Provider::Deepgram)?;
        let base = std::env::var("VOISU_DEEPGRAM_TRANSCRIPTION_URL")
            .unwrap_or_else(|_| "wss://api.deepgram.com/v1/listen".to_owned());
        let url = deepgram_streaming_url(&base, &self.keyterms)?;
        Ok(Box::new(DeepgramStream::connect(
            url,
            credential,
            DEEPGRAM_KEEPALIVE_INTERVAL,
            DEEPGRAM_CLOSE_GRACE,
            self.reaper.clone(),
        )))
    }
}

/// Fixed query params for the Deepgram nova-3 real-time streaming connection:
/// raw s16le/16kHz/mono PCM in, interim results on (finals are filtered by the
/// accumulator), smart formatting, and explicit endpointing/utterance-end
/// tuning for dictation pauses.
const DEEPGRAM_STREAMING_PARAMS: &[(&str, &str)] = &[
    ("model", "nova-3"),
    ("language", DEFAULT_TRANSCRIPTION_LANGUAGE),
    ("encoding", "linear16"),
    ("sample_rate", "16000"),
    ("channels", "1"),
    ("interim_results", "true"),
    ("smart_format", "true"),
    ("punctuate", "true"),
    ("endpointing", "300"),
    ("utterance_end_ms", "1000"),
];

/// Builds the streaming websocket URL from a base endpoint. `https`/`http`
/// bases are rewritten to `wss`/`ws` so the existing endpoint override env var
/// keeps working; plaintext `ws` is allowed only on loopback, mirroring the
/// HTTPS policy of the batch endpoints.
pub(super) fn deepgram_streaming_url(
    base: &str,
    keyterms: &[String],
) -> Result<String, BoundaryError> {
    if !endpoint_raw_string_is_allowed(base) {
        return Err(BoundaryError::new(
            BoundaryKind::Provider,
            "Deepgram streaming endpoint must use WSS except on loopback",
        ));
    }
    let normalized = if let Some(rest) = base.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = base.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        base.to_owned()
    };
    let url = url::Url::parse(&normalized).map_err(|_| {
        BoundaryError::new(
            BoundaryKind::Provider,
            "Deepgram streaming endpoint must use WSS except on loopback",
        )
    })?;
    // Reject userinfo outright: `ws://127.0.0.1:80@attacker.example/…` has a
    // loopback-LOOKING authority prefix but its HOST is attacker.example, and
    // loopback-checking the raw authority string would send the Token header
    // there over plaintext. Deepgram auth travels in the Authorization header,
    // so no legitimate endpoint carries userinfo.
    if !endpoint_authority_is_allowed(&url) {
        return Err(BoundaryError::new(
            BoundaryKind::Provider,
            "Deepgram streaming endpoint authority is invalid",
        ));
    }
    let plaintext = match url.scheme() {
        "ws" => true,
        "wss" => false,
        _ => {
            return Err(BoundaryError::new(
                BoundaryKind::Provider,
                "Deepgram streaming endpoint must use WSS except on loopback",
            ));
        }
    };
    if plaintext && !parsed_host_is_loopback(&url) {
        return Err(BoundaryError::new(
            BoundaryKind::Provider,
            "Deepgram streaming endpoint must use WSS except on loopback",
        ));
    }
    let mut url = normalized;
    let mut separator = if url.contains('?') { '&' } else { '?' };
    for (name, value) in DEEPGRAM_STREAMING_PARAMS {
        url.push(separator);
        url.push_str(name);
        url.push('=');
        url.push_str(value);
        separator = '&';
    }
    for keyterm in keyterms {
        let keyterm = keyterm.trim();
        if keyterm.is_empty() {
            continue;
        }
        url.push(separator);
        url.push_str("keyterm=");
        url.push_str(&percent_encode_query(keyterm));
        separator = '&';
    }
    Ok(url)
}

/// Percent-encodes a query component: RFC 3986 unreserved characters pass
/// through, everything else (including spaces) becomes `%XX`.
fn percent_encode_query(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(byte as char);
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

/// Assembles the Recording's Transcript from Deepgram streaming `Results`
/// messages. ONLY `is_final: true` segments are accumulated — interim results
/// for a time window are superseded by later messages and must never reach the
/// Transcript — so this can never blindly concatenate revisions of the same
/// audio the way the removed per-chunk batch path did.
///
/// Alongside each final segment's text, the segment's word-level confidences
/// (`channel/alternatives/0/words[].word`/`.confidence`) are accumulated in the
/// same order. They are slice-B2's minimal confidence evidence: the
/// user-vocabulary correction gate reads them; anything deeper is slice B4. A
/// word without a finite numeric confidence is carried as `0.0` — unproven,
/// never confidently transcribed — and every confidence is clamped to the
/// `[0, 1]` domain the gate assumes. A `Results` message without a words array
/// contributes nothing (the gate then falls back to applying, the documented
/// asymmetry).
#[derive(Default)]
pub(super) struct TranscriptAccumulator {
    segments: Vec<String>,
    words: Vec<(String, f64)>,
}

impl TranscriptAccumulator {
    pub(super) fn ingest(&mut self, message: &serde_json::Value) {
        if message.get("type").and_then(serde_json::Value::as_str) != Some("Results") {
            return;
        }
        if message.get("is_final").and_then(serde_json::Value::as_bool) != Some(true) {
            return;
        }
        let Some(text) = message
            .pointer("/channel/alternatives/0/transcript")
            .and_then(serde_json::Value::as_str)
        else {
            return;
        };
        let text = text.trim();
        if !text.is_empty() {
            self.segments.push(text.to_owned());
        }
        for word in message
            .pointer("/channel/alternatives/0/words")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(word_text) = word.get("word").and_then(serde_json::Value::as_str) else {
                continue;
            };
            if word_text.is_empty() {
                continue;
            }
            let confidence = word
                .get("confidence")
                .and_then(serde_json::Value::as_f64)
                // Clamp to the [0, 1] confidence domain the correction gate
                // assumes; a non-finite or missing number is unproven (0.0),
                // never confidently transcribed.
                .filter(|confidence| confidence.is_finite())
                .map(|confidence| confidence.clamp(0.0, 1.0))
                .unwrap_or(0.0);
            self.words.push((word_text.to_owned(), confidence));
        }
    }

    pub(super) fn text(&self) -> String {
        self.segments.join(" ")
    }

    /// The accumulated `(word, confidence)` evidence in transcript order.
    pub(super) fn words(&self) -> Vec<(String, f64)> {
        self.words.clone()
    }
}

/// Frames the stream owner hands to the websocket I/O task: raw PCM goes out
/// as binary frames, `Finalize`/`CloseStream` as JSON text frames.
pub(super) enum DeepgramOutbound {
    Audio(Vec<u8>),
    Finalize,
    CloseStream,
}

type DeepgramSocket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

pub(super) struct DeepgramStream {
    /// `None` once `complete()` has taken the sender; dropping it lets the I/O
    /// task observe end-of-outbound and settle.
    pub(super) outbound: Option<tokio::sync::mpsc::UnboundedSender<DeepgramOutbound>>,
    pub(super) streamed_bytes: usize,
    /// The single long-lived websocket I/O task, kept in a deque so `Drop`
    /// hands it to the actor-owned `ProviderReaper` through the same adoption
    /// contract the curl chunk tasks used (await, never abort).
    pub(super) io_tasks: VecDeque<tokio::task::JoinHandle<Result<(), BoundaryError>>>,
    /// Filled by the I/O task as finalized `Results` arrive.
    pub(super) transcript: Arc<Mutex<TranscriptAccumulator>>,
    /// Per-Recording cancellation flag polled by the I/O task on a bounded
    /// tick, mirroring the poll-bound discipline of the subprocess waits.
    pub(super) cancel: Arc<CancelRegistry>,
    /// Awaitable companion to `cancel`: `abort()`/`Drop` notify it so the I/O
    /// task wakes immediately instead of waiting out a backoff sleep or poll
    /// tick — the abort path must not stretch the Processing window.
    pub(super) shutdown: Arc<tokio::sync::Notify>,
    /// Actor-owned supervisor that adopts the I/O task if the stream is
    /// dropped mid-abort, so the websocket teardown is retained and awaited
    /// rather than detached.
    pub(super) reaper: ProviderReaper,
}

impl DeepgramStream {
    /// Spawns the websocket I/O task for one Recording. Must be called on the
    /// runtime; connect failures surface later, through `send_audio` (closed
    /// channel) or `complete()`/`abort()` (the task's stored error).
    pub(super) fn connect(
        url: String,
        credential: Credential,
        keepalive: Duration,
        close_grace: Duration,
        reaper: ProviderReaper,
    ) -> Self {
        let (outbound_tx, outbound_rx) = tokio::sync::mpsc::unbounded_channel();
        let transcript = Arc::new(Mutex::new(TranscriptAccumulator::default()));
        let cancel = CancelRegistry::new();
        let shutdown = Arc::new(tokio::sync::Notify::new());
        let io_task = tokio::spawn(deepgram_ws_task(
            url,
            credential,
            outbound_rx,
            Arc::clone(&transcript),
            Arc::clone(&cancel),
            Arc::clone(&shutdown),
            keepalive,
            close_grace,
        ));
        Self {
            outbound: Some(outbound_tx),
            streamed_bytes: 0,
            io_tasks: VecDeque::from([io_task]),
            transcript,
            cancel,
            shutdown,
            reaper,
        }
    }
}

impl Drop for DeepgramStream {
    fn drop(&mut self) {
        // See `Drop for GroqStream`: cancel first, then adopt (await, never
        // abort) so the websocket I/O task finishes its teardown before the
        // reaper task completes and Idle becomes observable.
        self.cancel.cancel();
        self.shutdown.notify_waiters();
        self.reaper.adopt(std::mem::take(&mut self.io_tasks));
    }
}

impl ProviderStream for DeepgramStream {
    fn provider(&self) -> Provider {
        Provider::Deepgram
    }

    fn word_confidences(&self) -> Vec<(String, f64)> {
        self.transcript
            .lock()
            .expect("Deepgram transcript accumulator mutex poisoned")
            .words()
    }

    fn send_audio(&mut self, chunk: AudioChunk) -> BoundaryFuture<'_, ()> {
        Box::pin(async move {
            self.streamed_bytes = self.streamed_bytes.saturating_add(chunk.0.len());
            let outbound = self.outbound.as_ref().ok_or_else(|| {
                BoundaryError::new(BoundaryKind::Provider, "Deepgram stream already completed")
            })?;
            // A closed channel means the I/O task already failed. Do NOT fail
            // here: `ProviderCoordinator::stream_audio` propagates send errors
            // and would fail the whole Recording, while the bound decision is
            // that the parallel Groq stream carries it. The stored I/O-task
            // error surfaces visibly through `complete()` instead.
            let _ = outbound.send(DeepgramOutbound::Audio(chunk.0));
            Ok(())
        })
    }

    fn abort(mut self: Box<Self>) -> BoundaryFuture<'static, ()> {
        Box::pin(async move {
            // Signal cancellation first: the I/O task wakes on the shutdown
            // notification (the flag backstops a pre-poll race), closes the
            // websocket, and returns. Await it — never abort — through the
            // same front/pop discipline as the chunk tasks, so a drop
            // mid-await leaves the handle for the reaper. If the task had
            // ALREADY stored a provider failure (server Error, exhausted
            // dials) before this Recording was aborted for an unrelated
            // reason, surface it through abort's error channel rather than
            // discarding it — send_audio deliberately hides the closed
            // channel, so this is the failure's only remaining exit.
            // (Limitation: without voisu-core changes this reaches the
            // recovery-abort diagnostics, not the per-provider history.)
            self.cancel.cancel();
            self.shutdown.notify_waiters();
            let mut stored_failure = Ok(());
            while let Some(io_task) = self.io_tasks.front_mut() {
                let joined = io_task.await;
                self.io_tasks.pop_front();
                if let Ok(Err(error)) = joined {
                    stored_failure = Err(error);
                }
            }
            stored_failure
        })
    }

    fn complete(&mut self, audio: CapturedAudio) -> BoundaryFuture<'_, SourceTranscript> {
        Box::pin(async move {
            let pcm = audio.pcm_s16le_mono_16khz();
            if self.streamed_bytes > pcm.len() {
                return Err(BoundaryError::new(
                    BoundaryKind::Provider,
                    "Deepgram stream exceeded the finalized Recording",
                ));
            }
            if let Some(outbound) = self.outbound.take() {
                // Top up with any un-streamed tail, flush server-side buffers,
                // and end the stream gracefully. A closed channel here means
                // the I/O task already ended; its stored result carries the
                // error, so failed sends are deliberately ignored.
                let tail = &pcm[self.streamed_bytes..];
                if !tail.is_empty() {
                    let _ = outbound.send(DeepgramOutbound::Audio(tail.to_vec()));
                }
                let _ = outbound.send(DeepgramOutbound::Finalize);
                let _ = outbound.send(DeepgramOutbound::CloseStream);
            }
            // Await the I/O task WITHOUT removing it from `self.io_tasks`. If
            // this completion future is dropped mid-await (e.g. the Provider
            // Deadline elapses and the coordinator moves to `abort()`), the
            // handle must still be in the deque so the gated `abort()` awaits
            // the websocket teardown before Idle is observable.
            while let Some(io_task) = self.io_tasks.front_mut() {
                let joined = io_task.await;
                self.io_tasks.pop_front();
                joined.map_err(|_| {
                    BoundaryError::new(BoundaryKind::Provider, "Deepgram streaming task failed")
                })??;
            }
            let text = self
                .transcript
                .lock()
                .expect("Deepgram transcript accumulator mutex poisoned")
                .text();
            Ok(SourceTranscript {
                provider: Provider::Deepgram,
                text,
            })
        })
    }
}

/// How one websocket connection ended, as seen by the per-connection driver.
enum DeepgramConnectionEnd {
    /// The stream ended on purpose: `CloseStream` acknowledged, outbound side
    /// dropped, or cancellation observed. The I/O task is done.
    Finished,
    /// The connection dropped mid-Recording; the I/O task may redial within
    /// the bounded reconnect budget.
    Lost,
}

/// The long-lived websocket I/O task: one per Recording, owning the Deepgram
/// connection end to end. Slots into the existing `ProviderReaper` adoption
/// contract as a single `JoinHandle`. A connection lost mid-Recording is
/// redialed at most `DEEPGRAM_RECONNECT_ATTEMPTS` times (audio already in
/// flight during the drop is lost — the parallel Groq stream covers the gap);
/// past the budget the error is stored here and surfaces through `complete()`.
#[allow(clippy::too_many_arguments)] // WS plumbing carries the full session context; fate tied to the Deepgram keep/delete decision
async fn deepgram_ws_task(
    url: String,
    credential: Credential,
    outbound: tokio::sync::mpsc::UnboundedReceiver<DeepgramOutbound>,
    transcript: Arc<Mutex<TranscriptAccumulator>>,
    cancel: Arc<CancelRegistry>,
    shutdown: Arc<tokio::sync::Notify>,
    keepalive: Duration,
    close_grace: Duration,
) -> Result<(), BoundaryError> {
    // Arm the shutdown wakeup before any other await so an abort lands
    // immediately at whichever await point the session loop is parked on —
    // a backoff sleep or in-flight dial must not stretch the abort. The
    // cancellation flag backstops a notify that fires before this task's
    // first poll. Dropping the session future mid-await only drops an
    // in-process socket — nothing external is left to reap.
    let shutdown_notified = shutdown.notified();
    tokio::pin!(shutdown_notified);
    let sessions = deepgram_ws_sessions(
        url,
        credential,
        outbound,
        transcript,
        Arc::clone(&cancel),
        keepalive,
        close_grace,
    );
    tokio::pin!(sessions);
    if cancel.is_cancelled() {
        return Ok(());
    }
    tokio::select! {
        result = &mut sessions => result,
        _ = &mut shutdown_notified => Ok(()),
    }
}

/// The reconnect-bounded connection loop driven by [`deepgram_ws_task`].
async fn deepgram_ws_sessions(
    url: String,
    credential: Credential,
    mut outbound: tokio::sync::mpsc::UnboundedReceiver<DeepgramOutbound>,
    transcript: Arc<Mutex<TranscriptAccumulator>>,
    cancel: Arc<CancelRegistry>,
    keepalive: Duration,
    close_grace: Duration,
) -> Result<(), BoundaryError> {
    let mut reconnects_left = DEEPGRAM_RECONNECT_ATTEMPTS;
    let mut pending: Option<DeepgramOutbound> = None;
    // Set once any audio frame has been accepted by any socket: from then on
    // a lost connection is unrecoverable (see DEEPGRAM_RECONNECT_ATTEMPTS).
    let mut audio_delivered = false;
    loop {
        if cancel.is_cancelled() {
            return Ok(());
        }
        let socket = match deepgram_ws_connect(&url, &credential, &cancel).await {
            Ok(socket) => socket,
            Err(error) => {
                if cancel.is_cancelled() {
                    // Aborted while dialing: nothing was connected, nothing to
                    // reap — finish instead of burning the reconnect budget.
                    return Ok(());
                }
                if reconnects_left == 0 {
                    return Err(error);
                }
                reconnects_left -= 1;
                tokio::time::sleep(DEEPGRAM_RECONNECT_BACKOFF).await;
                continue;
            }
        };
        match drive_deepgram_connection(
            socket,
            &mut outbound,
            &mut pending,
            &mut audio_delivered,
            &transcript,
            &cancel,
            keepalive,
            close_grace,
        )
        .await?
        {
            DeepgramConnectionEnd::Finished => return Ok(()),
            DeepgramConnectionEnd::Lost => {
                if audio_delivered {
                    // Audio accepted by the dropped socket but not yet
                    // finalized cannot be replayed: redialing and continuing
                    // would return a plausible Transcript with a silent gap.
                    // Fail visibly; the parallel Groq stream carries the
                    // Recording (PRD §3.3).
                    return Err(BoundaryError::new(
                        BoundaryKind::Provider,
                        "Deepgram streaming connection lost",
                    ));
                }
                if reconnects_left == 0 {
                    return Err(BoundaryError::new(
                        BoundaryKind::Provider,
                        "Deepgram streaming connection lost",
                    ));
                }
                reconnects_left -= 1;
                tokio::time::sleep(DEEPGRAM_RECONNECT_BACKOFF).await;
            }
        }
    }
}

/// Installs the process-level rustls CryptoProvider (ring). The
/// `rustls-tls-webpki-roots` feature of tokio-tungstenite does not select a
/// crypto backend, and rustls panics on the first TLS handshake when none is
/// installed. Idempotent: a second call leaves the installed provider in place.
pub fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// Dials the streaming endpoint with the `Authorization: Token` header scheme
/// the batch path already used. The whole handshake is bounded by
/// `DEEPGRAM_CONNECT_DEADLINE` and observes cancellation on the poll tick, so
/// an abort never waits on a slow DNS/TLS dial: dropping the in-process
/// connect future cancels it without leaving anything to reap.
async fn deepgram_ws_connect(
    url: &str,
    credential: &Credential,
    cancel: &CancelRegistry,
) -> Result<DeepgramSocket, BoundaryError> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    let mut request = url.into_client_request().map_err(|_| {
        BoundaryError::new(BoundaryKind::Provider, "Deepgram streaming URL is invalid")
    })?;
    let token = format!("Token {}", credential.expose_to_boundary());
    request.headers_mut().insert(
        tokio_tungstenite::tungstenite::http::header::AUTHORIZATION,
        token.parse().map_err(|_| {
            BoundaryError::new(
                BoundaryKind::Provider,
                "Deepgram credential is not header-safe",
            )
        })?,
    );
    let connect = tokio_tungstenite::connect_async(request);
    tokio::pin!(connect);
    let deadline = tokio::time::sleep(DEEPGRAM_CONNECT_DEADLINE);
    tokio::pin!(deadline);
    let mut ticks = tokio::time::interval(DEEPGRAM_CANCEL_POLL);
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            result = &mut connect => {
                let (socket, _response) = result.map_err(|_| {
                    BoundaryError::new(
                        BoundaryKind::Provider,
                        "Deepgram websocket connect failed",
                    )
                })?;
                return Ok(socket);
            }
            _ = &mut deadline => {
                return Err(BoundaryError::new(
                    BoundaryKind::Provider,
                    "Deepgram websocket connect deadline elapsed",
                ));
            }
            _ = ticks.tick() => {
                if cancel.is_cancelled() {
                    return Err(BoundaryError::new(
                        BoundaryKind::Provider,
                        "Deepgram websocket connect cancelled",
                    ));
                }
            }
        }
    }
}

/// Drives one websocket connection: forwards outbound audio/control frames,
/// ingests inbound `Results` into the accumulator, sends `KeepAlive` during
/// outbound gaps, and observes cancellation on a bounded tick. Marks
/// `audio_delivered` once any audio frame is accepted by the socket. Returns
/// `Err` for fatal failures (server-reported errors, malformed frames, an
/// unconfirmed CloseStream); transport drops return
/// `DeepgramConnectionEnd::Lost` and the caller decides whether a redial is
/// safe. A drain only Finishes when the server confirmed CloseStream with its
/// terminal summary `Metadata` before closing — Deepgram's contract is to
/// process remaining audio, return final results plus summary metadata, then
/// terminate; anything less may be a truncated Transcript.
#[allow(clippy::too_many_arguments)] // WS plumbing carries the full session context; fate tied to the Deepgram keep/delete decision
async fn drive_deepgram_connection(
    socket: DeepgramSocket,
    outbound: &mut tokio::sync::mpsc::UnboundedReceiver<DeepgramOutbound>,
    pending: &mut Option<DeepgramOutbound>,
    audio_delivered: &mut bool,
    transcript: &Arc<Mutex<TranscriptAccumulator>>,
    cancel: &CancelRegistry,
    keepalive: Duration,
    close_grace: Duration,
) -> Result<DeepgramConnectionEnd, BoundaryError> {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    let (mut sink, mut stream) = socket.split();
    let mut ticks = tokio::time::interval(DEEPGRAM_CANCEL_POLL);
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_sent = tokio::time::Instant::now();
    // Once `CloseStream` is out, stop consuming outbound frames and only
    // drain inbound until the server flushes final `Results`, confirms with
    // its terminal `Metadata`, and closes — bounded by the close grace.
    let mut draining_deadline: Option<tokio::time::Instant> = None;
    let mut terminal_metadata_seen = false;
    // A frame that failed to send on the previous connection is retried first
    // (only reachable before any audio was delivered — see the caller).
    if let Some(frame) = pending.take() {
        let is_audio = matches!(frame, DeepgramOutbound::Audio(_));
        let draining = matches!(frame, DeepgramOutbound::CloseStream);
        if sink.send(deepgram_ws_frame(&frame)).await.is_err() {
            *pending = Some(frame);
            return Ok(DeepgramConnectionEnd::Lost);
        }
        if is_audio {
            *audio_delivered = true;
        }
        last_sent = tokio::time::Instant::now();
        if draining {
            draining_deadline = Some(last_sent + close_grace);
        }
    }
    loop {
        tokio::select! {
            frame = outbound.recv(), if draining_deadline.is_none() => {
                let Some(frame) = frame else {
                    // Stream owner dropped without `complete()` (abort/Drop
                    // path): close this connection out and finish.
                    let _ = sink.send(Message::Close(None)).await;
                    return Ok(DeepgramConnectionEnd::Finished);
                };
                let is_audio = matches!(frame, DeepgramOutbound::Audio(_));
                let draining = matches!(frame, DeepgramOutbound::CloseStream);
                if sink.send(deepgram_ws_frame(&frame)).await.is_err() {
                    *pending = Some(frame);
                    return Ok(DeepgramConnectionEnd::Lost);
                }
                if is_audio {
                    *audio_delivered = true;
                }
                last_sent = tokio::time::Instant::now();
                if draining {
                    draining_deadline = Some(last_sent + close_grace);
                }
            }
            incoming = stream.next() => match incoming {
                Some(Ok(Message::Text(text))) => {
                    let kind = ingest_deepgram_message(transcript, &text)?;
                    if draining_deadline.is_some()
                        && matches!(kind, DeepgramMessageKind::Metadata)
                    {
                        terminal_metadata_seen = true;
                    }
                }
                Some(Ok(Message::Close(_))) | None => {
                    if draining_deadline.is_none() {
                        return Ok(DeepgramConnectionEnd::Lost);
                    }
                    if terminal_metadata_seen {
                        return Ok(DeepgramConnectionEnd::Finished);
                    }
                    // Closed after CloseStream but WITHOUT the terminal
                    // summary Metadata: the server-side flush is unconfirmed
                    // and the accumulated Transcript may be truncated.
                    return Err(BoundaryError::new(
                        BoundaryKind::Provider,
                        "Deepgram closed without confirming CloseStream",
                    ));
                }
                Some(Err(_)) => {
                    if draining_deadline.is_some() {
                        // The transport died between CloseStream and the
                        // server's flush: the final Results may be missing.
                        // Returning the partial accumulator here would
                        // silently truncate the Transcript — fail visibly and
                        // let the parallel Groq stream carry the Recording.
                        return Err(BoundaryError::new(
                            BoundaryKind::Provider,
                            "Deepgram streaming connection lost",
                        ));
                    }
                    return Ok(DeepgramConnectionEnd::Lost);
                }
                Some(Ok(_)) => {}
            },
            _ = ticks.tick() => {
                if cancel.is_cancelled() {
                    let _ = sink.send(Message::Close(None)).await;
                    return Ok(DeepgramConnectionEnd::Finished);
                }
                if let Some(deadline) = draining_deadline {
                    if tokio::time::Instant::now() >= deadline {
                        // Deepgram never confirmed CloseStream within the
                        // grace: the accumulated prefix would be a plausible
                        // but truncated Transcript, well inside the Provider
                        // Deadline — fail visibly instead.
                        let _ = sink.send(Message::Close(None)).await;
                        return Err(BoundaryError::new(
                            BoundaryKind::Provider,
                            "Deepgram did not confirm CloseStream within the close grace",
                        ));
                    }
                } else if last_sent.elapsed() >= keepalive {
                    if sink
                        .send(Message::Text(r#"{"type":"KeepAlive"}"#.to_owned()))
                        .await
                        .is_err()
                    {
                        return Ok(DeepgramConnectionEnd::Lost);
                    }
                    last_sent = tokio::time::Instant::now();
                }
            }
        }
    }
}

fn deepgram_ws_frame(frame: &DeepgramOutbound) -> tokio_tungstenite::tungstenite::Message {
    use tokio_tungstenite::tungstenite::Message;

    match frame {
        DeepgramOutbound::Audio(bytes) => Message::Binary(bytes.clone()),
        DeepgramOutbound::Finalize => Message::Text(r#"{"type":"Finalize"}"#.to_owned()),
        DeepgramOutbound::CloseStream => Message::Text(r#"{"type":"CloseStream"}"#.to_owned()),
    }
}

/// What one inbound text frame turned out to be, for the caller's
/// drain-confirmation tracking.
pub(super) enum DeepgramMessageKind {
    /// The summary `Metadata` message — terminal when it follows CloseStream.
    Metadata,
    Other,
}

/// Parses one inbound text frame. `Results` feed the accumulator; a server
/// `Error` message, a frame that is not JSON, and a `Results` frame missing
/// its `is_final` marker or (when finalized) its transcript text are all
/// fatal — silently skipping them would truncate the Transcript without a
/// trace. Unknown-but-well-formed message types stay tolerated so server-side
/// schema ADDITIONS never break the Recording; interim shape drift is UI-only
/// and equally tolerated.
pub(super) fn ingest_deepgram_message(
    transcript: &Arc<Mutex<TranscriptAccumulator>>,
    text: &str,
) -> Result<DeepgramMessageKind, BoundaryError> {
    let Ok(message) = serde_json::from_str::<serde_json::Value>(text) else {
        return Err(BoundaryError::new(
            BoundaryKind::Provider,
            "Deepgram sent a malformed streaming message",
        ));
    };
    match message.get("type").and_then(serde_json::Value::as_str) {
        Some("Error") => Err(BoundaryError::new(
            BoundaryKind::Provider,
            "Deepgram reported a streaming error",
        )),
        Some("Results") => {
            let Some(is_final) = message.get("is_final").and_then(serde_json::Value::as_bool)
            else {
                return Err(BoundaryError::new(
                    BoundaryKind::Provider,
                    "Deepgram sent a malformed streaming message",
                ));
            };
            if is_final
                && message
                    .pointer("/channel/alternatives/0/transcript")
                    .and_then(serde_json::Value::as_str)
                    .is_none()
            {
                return Err(BoundaryError::new(
                    BoundaryKind::Provider,
                    "Deepgram sent a malformed streaming message",
                ));
            }
            transcript
                .lock()
                .expect("Deepgram transcript accumulator mutex poisoned")
                .ingest(&message);
            Ok(DeepgramMessageKind::Other)
        }
        Some("Metadata") => Ok(DeepgramMessageKind::Metadata),
        _ => Ok(DeepgramMessageKind::Other),
    }
}
