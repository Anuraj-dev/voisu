// Narilabs transcript provider: streaming WebSocket connection and transcript accumulation.
//
// Modeled on deepgram.rs (same stream/reaper/accumulator shapes); the wire
// protocol is Narilabs' realtime STT: a `session.configure` handshake, base64
// PCM `input_audio_buffer.append` messages, a manual `input_audio_buffer.commit`
// at end of input, and per-utterance `transcript.completed` finals.

use super::*;

/// The Narilabs realtime transcription endpoint (public beta).
const NARILABS_REALTIME_URL: &str = "wss://api.narilabs.com/v1/realtime?intent=transcription";

/// The Narilabs model family used by this evaluation build.
const NARILABS_DEFAULT_MODEL: &str = "qwen3-asr:free";

/// Raw PCM bytes per `input_audio_buffer.append`. Every complete JSON message
/// must stay ≤128 KiB on the wire; base64 expands 3 bytes to 4, so 48_000 raw
/// bytes keep each append (envelope included) far under that budget.
const NARILABS_APPEND_MAX_RAW_BYTES: usize = 48_000;

/// The per-message wire budget Narilabs enforces on complete JSON messages.
const NARILABS_MESSAGE_BUDGET_BYTES: usize = 128 * 1024;

pub struct NarilabsProvider {
    reaper: ProviderReaper,
    /// The Recording's resolved transcription-language snapshot. `None` falls
    /// back to the shared config resolution at stream start; the daemon
    /// supplies the same resolved value it declared to EnglishEligibility so
    /// the request's language can never drift from the declaration.
    language: Option<String>,
    /// The Recording's resolved dictionary snapshot — the same glossary string
    /// Groq's Whisper prompt carries. `None` sends no `prompt` field.
    prompt: Option<String>,
}

impl NarilabsProvider {
    /// Builds a Narilabs provider whose streams share the actor-owned
    /// `reaper`, so a stream dropped mid-abort hands its websocket I/O task to
    /// the supervisor the actor drains before Idle. No language snapshot.
    pub fn new(reaper: ProviderReaper) -> Self {
        Self {
            reaper,
            language: None,
            prompt: None,
        }
    }

    /// Builds a Narilabs provider from the Recording's resolved language
    /// snapshot — the same value the daemon declares to EnglishEligibility and
    /// hands to Deepgram and Groq, so every provider transcribes one declared
    /// language.
    pub fn with_language(reaper: ProviderReaper, language: String) -> Self {
        Self {
            reaper,
            language: Some(language),
            prompt: None,
        }
    }

    /// Builds a Narilabs provider from the Recording's resolved dictionary and
    /// language snapshots — the prompt is the same glossary string Groq's
    /// Whisper requests carry, so every provider transcribes one Recording on
    /// one vocabulary.
    pub fn with_prompt_and_language(
        reaper: ProviderReaper,
        prompt: String,
        language: String,
    ) -> Self {
        Self {
            reaper,
            language: Some(language),
            prompt: Some(prompt),
        }
    }
}

impl TranscriptProvider for NarilabsProvider {
    fn start(&mut self, _recording_id: u64) -> Result<Box<dyn ProviderStream>, BoundaryError> {
        let credential = SecretStore::load(&mut SecretToolStore, Provider::Narilabs)?;
        let base = std::env::var("VOISU_NARILABS_TRANSCRIPTION_URL")
            .unwrap_or_else(|_| NARILABS_REALTIME_URL.to_owned());
        let model = std::env::var("VOISU_NARILABS_MODEL")
            .unwrap_or_else(|_| NARILABS_DEFAULT_MODEL.to_owned());
        let language = self
            .language
            .clone()
            .unwrap_or_else(crate::config::transcription_language);
        let prompt = self.prompt.clone().unwrap_or_default();
        let url = narilabs_streaming_url(&base)?;
        Ok(Box::new(NarilabsStream::connect(
            url,
            model,
            language,
            prompt,
            credential,
            self.reaper.clone(),
        )))
    }
}

/// Builds the streaming websocket URL from a base endpoint. `https`/`http`
/// bases are rewritten to `wss`/`ws` so an endpoint override env var keeps
/// working; plaintext `ws` is allowed only on loopback, mirroring the
/// HTTPS policy of the batch endpoints. Unlike Deepgram, no query params are
/// appended: the model and language ride the `session.configure` message.
pub(super) fn narilabs_streaming_url(base: &str) -> Result<String, BoundaryError> {
    if !endpoint_raw_string_is_allowed(base) {
        return Err(BoundaryError::new(
            BoundaryKind::Provider,
            "Narilabs streaming endpoint must use WSS except on loopback",
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
            "Narilabs streaming endpoint must use WSS except on loopback",
        )
    })?;
    // Reject userinfo outright, exactly as the Deepgram streaming policy does:
    // `ws://127.0.0.1:80@attacker.example/…` has a loopback-LOOKING authority
    // prefix but its HOST is attacker.example, and loopback-checking the raw
    // authority string would send the Bearer key there over plaintext.
    if !endpoint_authority_is_allowed(&url) {
        return Err(BoundaryError::new(
            BoundaryKind::Provider,
            "Narilabs streaming endpoint authority is invalid",
        ));
    }
    let plaintext = match url.scheme() {
        "ws" => true,
        "wss" => false,
        _ => {
            return Err(BoundaryError::new(
                BoundaryKind::Provider,
                "Narilabs streaming endpoint must use WSS except on loopback",
            ));
        }
    };
    if plaintext && !parsed_host_is_loopback(&url) {
        return Err(BoundaryError::new(
            BoundaryKind::Provider,
            "Narilabs streaming endpoint must use WSS except on loopback",
        ));
    }
    Ok(normalized)
}

/// The manual-commit session configuration sent before any audio. The model,
/// the Recording's resolved language, and — when the Recording resolved a
/// non-empty dictionary snapshot — the vocabulary prompt ride here (not the
/// URL); an empty prompt omits the field entirely, keeping the message
/// byte-identical to the prompt-less shape. `turn_detection: null` selects
/// manual commit mode: the stream owner owns utterance boundaries, and long
/// recordings come back as multiple completed utterances instead of
/// server-timed turns.
pub(super) fn narilabs_session_configure(model: &str, language: &str, prompt: &str) -> String {
    let mut session = serde_json::json!({
        "model": model,
        "language": language,
        "turn_detection": serde_json::Value::Null,
    });
    if !prompt.is_empty() {
        session["prompt"] = serde_json::Value::String(prompt.to_owned());
    }
    let message = serde_json::json!({
        "type": "session.configure",
        "session": session,
    })
    .to_string();
    debug_assert!(
        message.len() <= NARILABS_MESSAGE_BUDGET_BYTES,
        "session.configure must stay within the 128 KiB wire budget: {} bytes",
        message.len()
    );
    message
}

/// One `input_audio_buffer.append` text message carrying base64 PCM. The
/// caller keeps every raw slice ≤[`NARILABS_APPEND_MAX_RAW_BYTES`], so the
/// complete JSON message stays inside the 128 KiB wire budget.
pub(super) fn narilabs_append_message(pcm: &[u8]) -> String {
    use base64::Engine as _;
    let encoded = base64::engine::general_purpose::STANDARD.encode(pcm);
    format!("{{\"type\":\"input_audio_buffer.append\",\"audio\":\"{encoded}\"}}")
}

/// Splits raw PCM into the append messages the wire budget allows, in order.
pub(super) fn narilabs_audio_appends(pcm: &[u8]) -> Vec<String> {
    pcm.chunks(NARILABS_APPEND_MAX_RAW_BYTES)
        .map(narilabs_append_message)
        .collect()
}

/// The end-of-input commit. The server answers with
/// `input_audio_buffer.committed` — or `input_audio_buffer.commit_empty` when
/// no audio is pending; both count as the end acknowledgement.
fn narilabs_commit_message() -> String {
    r#"{"type":"input_audio_buffer.commit","event_id":"end_of_input"}"#.to_owned()
}

/// Assembles the Recording's Transcript from Narilabs realtime events. A long
/// recording produces multiple completed utterances (an utterance also
/// auto-finalizes at 36 s with `commit_reason: "max_duration"`); they are
/// joined with a single space in first-seen item order. `transcript.partial`
/// revisions are superseded by later messages and by the item's
/// `transcript.completed`, so they never mix into completed text — they only
/// serve as the fallback when the connection ends before completion.
#[derive(Default)]
pub(super) struct NarilabsTranscriptAccumulator {
    /// Every item_id the server referenced, in first-seen order.
    seen_items: Vec<String>,
    /// Items that received `transcript.completed`.
    completed_items: std::collections::HashSet<String>,
    /// Finalized utterance texts by item_id, in arrival order.
    completed: Vec<(String, String)>,
    /// Last partial text per item_id, in arrival order.
    partials: Vec<(String, String)>,
    /// Whether the end-of-input commit was acknowledged.
    end_acknowledged: bool,
}

impl NarilabsTranscriptAccumulator {
    fn note_item(&mut self, item_id: &str) {
        if !self.seen_items.iter().any(|seen| seen == item_id) {
            self.seen_items.push(item_id.to_owned());
        }
    }

    pub(super) fn ingest_partial(&mut self, item_id: &str, text: &str) {
        self.note_item(item_id);
        let text = text.trim();
        match self.partials.iter_mut().find(|(seen, _)| seen == item_id) {
            Some((_, stored)) => *stored = text.to_owned(),
            None => self.partials.push((item_id.to_owned(), text.to_owned())),
        }
    }

    pub(super) fn ingest_completed(&mut self, item_id: &str, text: &str) {
        self.note_item(item_id);
        self.completed_items.insert(item_id.to_owned());
        // The partial revision is superseded by the item's completion.
        self.partials.retain(|(seen, _)| seen != item_id);
        let text = text.trim();
        if !text.is_empty() {
            self.completed.push((item_id.to_owned(), text.to_owned()));
        }
    }

    pub(super) fn ingest_end_acknowledged(&mut self, item_id: Option<&str>) {
        self.end_acknowledged = true;
        // The committed ack names the item whose completion is still owed; a
        // commit_empty names none. Without this, the final utterance — which
        // may never have been partially seen — could not block the close.
        if let Some(item_id) = item_id {
            self.note_item(item_id);
        }
    }

    /// The complete termination rule: the end commit was acknowledged AND
    /// every seen item finalized.
    pub(super) fn termination_satisfied(&self) -> bool {
        self.end_acknowledged
            && self
                .seen_items
                .iter()
                .all(|item| self.completed_items.contains(item))
    }

    /// Completed utterances joined with a single space in first-seen item
    /// order; with none, the last partials joined the same way.
    pub(super) fn text(&self) -> String {
        let completed: Vec<&str> = self
            .seen_items
            .iter()
            .filter_map(|item| {
                self.completed
                    .iter()
                    .find(|(seen, _)| seen == item)
                    .map(|(_, text)| text.as_str())
            })
            .filter(|text| !text.is_empty())
            .collect();
        if !completed.is_empty() {
            return completed.join(" ");
        }
        self.seen_items
            .iter()
            .filter_map(|item| {
                self.partials
                    .iter()
                    .find(|(seen, _)| seen == item)
                    .map(|(_, text)| text.as_str())
            })
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// Frames the stream owner hands to the websocket I/O task: raw PCM goes out
/// as append messages (split to the wire budget by the I/O task), the
/// end-of-input commit as its JSON text frame.
pub(super) enum NarilabsOutbound {
    Audio(Vec<u8>),
    Commit,
}

type NarilabsSocket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

pub(super) struct NarilabsStream {
    /// `None` once `complete()` has taken the sender; dropping it lets the I/O
    /// task observe end-of-outbound and settle.
    pub(super) outbound: Option<tokio::sync::mpsc::UnboundedSender<NarilabsOutbound>>,
    pub(super) streamed_bytes: usize,
    /// The single long-lived websocket I/O task, kept in a deque so `Drop`
    /// hands it to the actor-owned `ProviderReaper` through the same adoption
    /// contract the Deepgram stream uses (await, never abort).
    pub(super) io_tasks: VecDeque<tokio::task::JoinHandle<Result<(), BoundaryError>>>,
    /// Filled by the I/O task as partials and completed utterances arrive.
    pub(super) transcript: Arc<Mutex<NarilabsTranscriptAccumulator>>,
    /// Per-Recording cancellation flag polled by the I/O task on a bounded
    /// tick, mirroring the poll-bound discipline of the Deepgram stream.
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

impl NarilabsStream {
    /// Spawns the websocket I/O task for one Recording. Must be called on the
    /// runtime; connect failures surface later, through `send_audio` (closed
    /// channel) or `complete()`/`abort()` (the task's stored error).
    pub(super) fn connect(
        url: String,
        model: String,
        language: String,
        prompt: String,
        credential: Credential,
        reaper: ProviderReaper,
    ) -> Self {
        let (outbound_tx, outbound_rx) = tokio::sync::mpsc::unbounded_channel();
        let transcript = Arc::new(Mutex::new(NarilabsTranscriptAccumulator::default()));
        let cancel = CancelRegistry::new();
        let shutdown = Arc::new(tokio::sync::Notify::new());
        let io_task = tokio::spawn(narilabs_ws_task(
            url,
            credential,
            model,
            language,
            prompt,
            outbound_rx,
            Arc::clone(&transcript),
            Arc::clone(&cancel),
            Arc::clone(&shutdown),
            NARILABS_CLOSE_GRACE,
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

impl Drop for NarilabsStream {
    fn drop(&mut self) {
        // See `Drop for DeepgramStream`: cancel first, then adopt (await,
        // never abort) so the websocket I/O task finishes its teardown before
        // the reaper task completes and Idle becomes observable.
        self.cancel.cancel();
        self.shutdown.notify_waiters();
        self.reaper.adopt(std::mem::take(&mut self.io_tasks));
    }
}

impl ProviderStream for NarilabsStream {
    fn provider(&self) -> Provider {
        Provider::Narilabs
    }

    fn send_audio(&mut self, chunk: AudioChunk) -> BoundaryFuture<'_, ()> {
        Box::pin(async move {
            self.streamed_bytes = self.streamed_bytes.saturating_add(chunk.0.len());
            let outbound = self.outbound.as_ref().ok_or_else(|| {
                BoundaryError::new(BoundaryKind::Provider, "Narilabs stream already completed")
            })?;
            // A closed channel means the I/O task already failed. Do NOT fail
            // here: `ProviderCoordinator::stream_audio` propagates send errors
            // and would fail the whole Recording, while the parallel streams
            // carry it. The stored I/O-task error surfaces visibly through
            // `complete()` instead.
            let _ = outbound.send(NarilabsOutbound::Audio(chunk.0));
            Ok(())
        })
    }

    fn abort(mut self: Box<Self>) -> BoundaryFuture<'static, ()> {
        Box::pin(async move {
            // Signal cancellation first: the I/O task wakes on the shutdown
            // notification (the flag backstops a pre-poll race), closes the
            // websocket, and returns. Await it — never abort — through the
            // same front/pop discipline as the Deepgram stream, so a drop
            // mid-await leaves the handle for the reaper. If the task had
            // ALREADY stored a provider failure (server Error, exhausted
            // dials) before this Recording was aborted for an unrelated
            // reason, surface it through abort's error channel rather than
            // discarding it — send_audio deliberately hides the closed
            // channel, so this is the failure's only remaining exit.
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
                    "Narilabs stream exceeded the finalized Recording",
                ));
            }
            if let Some(outbound) = self.outbound.take() {
                // Top up with any un-streamed tail, then end the input: the
                // commit makes the server finalize every pending utterance. A
                // closed channel here means the I/O task already ended; its
                // stored result carries the error, so failed sends are
                // deliberately ignored.
                let tail = &pcm[self.streamed_bytes..];
                if !tail.is_empty() {
                    let _ = outbound.send(NarilabsOutbound::Audio(tail.to_vec()));
                }
                let _ = outbound.send(NarilabsOutbound::Commit);
            }
            // Await the I/O task WITHOUT removing it from `self.io_tasks`. If
            // this completion future is dropped mid-await (e.g. the Provider
            // Deadline elapses and the coordinator moves to `abort()`), the
            // handle must still be in the deque so the gated `abort()` awaits
            // the websocket teardown before Idle is observable.
            let mut stored_failure = Ok(());
            while let Some(io_task) = self.io_tasks.front_mut() {
                let joined = io_task.await;
                self.io_tasks.pop_front();
                match joined {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => stored_failure = Err(error),
                    Err(_) => {
                        return Err(BoundaryError::new(
                            BoundaryKind::Provider,
                            "Narilabs streaming task failed",
                        ));
                    }
                }
            }
            let text = self
                .transcript
                .lock()
                .expect("Narilabs transcript accumulator mutex poisoned")
                .text();
            if text.is_empty() {
                // Nothing usable accumulated (no completed utterance, no
                // partial): surface the stored provider failure — a server
                // error event or a lost connection — instead of an empty
                // Transcript. With utterances completed this is the documented
                // partial success: keep them.
                stored_failure?;
            }
            Ok(SourceTranscript {
                provider: Provider::Narilabs,
                text,
            })
        })
    }
}

/// How one websocket connection ended, as seen by the per-connection driver.
enum NarilabsConnectionEnd {
    /// The stream ended on purpose: the commit round trip completed (or the
    /// server closed first and the accumulated text is the documented
    /// fallback), or cancellation was observed. The I/O task is done.
    Finished,
    /// The connection dropped mid-Recording; the I/O task may redial within
    /// the bounded reconnect budget.
    Lost,
}

/// The long-lived websocket I/O task: one per Recording, owning the Narilabs
/// connection end to end. Slots into the existing `ProviderReaper` adoption
/// contract as a single `JoinHandle`. A connection lost mid-Recording is
/// redialed at most `NARILABS_RECONNECT_ATTEMPTS` times while no audio has
/// been accepted yet (audio buffered before `session.configured` is lost with
/// the socket); past the budget — or after any audio was delivered — the
/// error is stored here and surfaces through `complete()` with whatever
/// utterances already completed.
#[allow(clippy::too_many_arguments)] // WS plumbing carries the full session context; test-only Provider
async fn narilabs_ws_task(
    url: String,
    credential: Credential,
    model: String,
    language: String,
    prompt: String,
    outbound: tokio::sync::mpsc::UnboundedReceiver<NarilabsOutbound>,
    transcript: Arc<Mutex<NarilabsTranscriptAccumulator>>,
    cancel: Arc<CancelRegistry>,
    shutdown: Arc<tokio::sync::Notify>,
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
    let sessions = narilabs_ws_sessions(
        url,
        credential,
        model,
        language,
        prompt,
        outbound,
        transcript,
        Arc::clone(&cancel),
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

/// The reconnect-bounded connection loop driven by [`narilabs_ws_task`].
#[allow(clippy::too_many_arguments)] // WS plumbing carries the full session context; test-only Provider
async fn narilabs_ws_sessions(
    url: String,
    credential: Credential,
    model: String,
    language: String,
    prompt: String,
    mut outbound: tokio::sync::mpsc::UnboundedReceiver<NarilabsOutbound>,
    transcript: Arc<Mutex<NarilabsTranscriptAccumulator>>,
    cancel: Arc<CancelRegistry>,
    close_grace: Duration,
) -> Result<(), BoundaryError> {
    let mut reconnects_left = NARILABS_RECONNECT_ATTEMPTS;
    let mut pending: Option<NarilabsOutbound> = None;
    // Set once any audio frame has been accepted by any socket: from then on
    // a lost connection is unrecoverable (unfinalized audio cannot be
    // replayed, and redialing would return a Transcript with a silent gap).
    let mut audio_delivered = false;
    loop {
        if cancel.is_cancelled() {
            return Ok(());
        }
        let socket = match narilabs_ws_connect(&url, &credential, &cancel).await {
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
                tokio::time::sleep(NARILABS_RECONNECT_BACKOFF).await;
                continue;
            }
        };
        match drive_narilabs_connection(
            socket,
            &mut outbound,
            &mut pending,
            &mut audio_delivered,
            &model,
            &language,
            &prompt,
            &transcript,
            &cancel,
            close_grace,
        )
        .await
        {
            Ok(NarilabsConnectionEnd::Finished) => return Ok(()),
            Ok(NarilabsConnectionEnd::Lost) => {
                if audio_delivered {
                    // Audio accepted by the dropped socket but not yet
                    // finalized cannot be replayed: redialing and continuing
                    // would return a plausible Transcript with a silent gap.
                    // Fail visibly; the parallel streams carry the Recording
                    // and the accumulator keeps any completed utterances.
                    return Err(BoundaryError::new(
                        BoundaryKind::Provider,
                        "Narilabs streaming connection lost",
                    ));
                }
                if reconnects_left == 0 {
                    return Err(BoundaryError::new(
                        BoundaryKind::Provider,
                        "Narilabs streaming connection lost",
                    ));
                }
                reconnects_left -= 1;
                tokio::time::sleep(NARILABS_RECONNECT_BACKOFF).await;
            }
            // A server error event or a malformed frame is fatal for the
            // provider; whatever utterances already completed stay in the
            // accumulator for `complete()`'s partial-success path.
            Err(error) => return Err(error),
        }
    }
}

/// Dials the streaming endpoint with the `Authorization: Bearer` header
/// scheme the Narilabs realtime protocol requires. The whole handshake is
/// bounded by `NARILABS_CONNECT_DEADLINE` and observes cancellation on the
/// poll tick, so an abort never waits on a slow DNS/TLS dial: dropping the
/// in-process connect future cancels it without leaving anything to reap.
async fn narilabs_ws_connect(
    url: &str,
    credential: &Credential,
    cancel: &CancelRegistry,
) -> Result<NarilabsSocket, BoundaryError> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    let mut request = url.into_client_request().map_err(|_| {
        BoundaryError::new(BoundaryKind::Provider, "Narilabs streaming URL is invalid")
    })?;
    let token = format!("Bearer {}", credential.expose_to_boundary());
    request.headers_mut().insert(
        tokio_tungstenite::tungstenite::http::header::AUTHORIZATION,
        token.parse().map_err(|_| {
            BoundaryError::new(
                BoundaryKind::Provider,
                "Narilabs credential is not header-safe",
            )
        })?,
    );
    let connect = tokio_tungstenite::connect_async(request);
    tokio::pin!(connect);
    let deadline = tokio::time::sleep(NARILABS_CONNECT_DEADLINE);
    tokio::pin!(deadline);
    let mut ticks = tokio::time::interval(NARILABS_CANCEL_POLL);
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            result = &mut connect => {
                let (socket, _response) = result.map_err(|_| {
                    BoundaryError::new(
                        BoundaryKind::Provider,
                        "Narilabs websocket connect failed",
                    )
                })?;
                return Ok(socket);
            }
            _ = &mut deadline => {
                return Err(BoundaryError::new(
                    BoundaryKind::Provider,
                    "Narilabs websocket connect deadline elapsed",
                ));
            }
            _ = ticks.tick() => {
                if cancel.is_cancelled() {
                    return Err(BoundaryError::new(
                        BoundaryKind::Provider,
                        "Narilabs websocket connect cancelled",
                    ));
                }
            }
        }
    }
}

/// Sends one raw PCM frame as budget-sized append messages. `Delivered` means
/// every append reached the socket; `NothingSent` means none did (the frame
/// can be retried whole on a redial); `PartiallySent` means some audio reached
/// the socket and cannot be replayed without a silent gap.
async fn send_audio_appends(
    sink: &mut (
             impl futures_util::Sink<
        tokio_tungstenite::tungstenite::Message,
        Error = tokio_tungstenite::tungstenite::Error,
    > + Unpin
         ),
    bytes: &[u8],
) -> AudioSendOutcome {
    use futures_util::SinkExt;
    let mut delivered_any = false;
    for message in narilabs_audio_appends(bytes) {
        if sink
            .send(tokio_tungstenite::tungstenite::Message::Text(message))
            .await
            .is_err()
        {
            return if delivered_any {
                AudioSendOutcome::PartiallySent
            } else {
                AudioSendOutcome::NothingSent
            };
        }
        delivered_any = true;
    }
    AudioSendOutcome::Delivered
}

enum AudioSendOutcome {
    Delivered,
    NothingSent,
    PartiallySent,
}

/// Handles one audio send outcome: `Delivered` reports success; `NothingSent`
/// parks the frame for the next connection's configured flush and reports
/// failure; `PartiallySent` marks the connection audio-delivered — the bytes
/// already on the socket cannot be replayed — and reports failure.
fn handle_audio_send(
    outcome: AudioSendOutcome,
    bytes: Vec<u8>,
    pending: &mut Option<NarilabsOutbound>,
    audio_delivered: &mut bool,
) -> bool {
    match outcome {
        AudioSendOutcome::Delivered => {
            *audio_delivered = true;
            true
        }
        AudioSendOutcome::NothingSent => {
            *pending = Some(NarilabsOutbound::Audio(bytes));
            false
        }
        AudioSendOutcome::PartiallySent => {
            *audio_delivered = true;
            false
        }
    }
}

/// Drives one websocket connection: sends `session.configure` on connect,
/// starts consuming the outbound channel only once `session.configured`
/// confirms the session (frames wait in the channel, in order, meanwhile),
/// forwards audio as budget-sized append messages, sends the end-of-input
/// commit when the owner hands it over, and after that commit drains inbound
/// events until the end is acknowledged and every seen item finalized —
/// bounded by the close grace. Server closes, transport errors, and grace
/// expiries after the commit all FINISH with the accumulated text (completed
/// utterances, else last partials), per the Narilabs termination contract.
/// Transport drops before the commit return `Lost` for the caller's
/// reconnect decision; a server error event is fatal for the provider.
#[allow(clippy::too_many_arguments)] // WS plumbing carries the full session context; test-only Provider
async fn drive_narilabs_connection(
    socket: NarilabsSocket,
    outbound: &mut tokio::sync::mpsc::UnboundedReceiver<NarilabsOutbound>,
    pending: &mut Option<NarilabsOutbound>,
    audio_delivered: &mut bool,
    model: &str,
    language: &str,
    prompt: &str,
    transcript: &Arc<Mutex<NarilabsTranscriptAccumulator>>,
    cancel: &CancelRegistry,
    close_grace: Duration,
) -> Result<NarilabsConnectionEnd, BoundaryError> {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    fn termination_satisfied(transcript: &Arc<Mutex<NarilabsTranscriptAccumulator>>) -> bool {
        transcript
            .lock()
            .expect("Narilabs transcript accumulator mutex poisoned")
            .termination_satisfied()
    }

    let (mut sink, mut stream) = socket.split();
    let mut ticks = tokio::time::interval(NARILABS_CANCEL_POLL);
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // The session configuration goes out before anything else; the protocol
    // forbids audio before the server confirms with `session.configured`.
    let configure = narilabs_session_configure(model, language, prompt);
    if sink.send(Message::Text(configure)).await.is_err() {
        return Ok(NarilabsConnectionEnd::Lost);
    }
    let mut configured = false;
    // Once the commit is out, stop consuming outbound frames and only drain
    // inbound until the end is acknowledged and every seen item finalized,
    // bounded by the close grace.
    let mut commit_pending = false;
    let mut draining_deadline: Option<tokio::time::Instant> = None;
    loop {
        tokio::select! {
            // The outbound channel is consumed only once the session is
            // configured (the protocol forbids audio before that) and until
            // the commit is out. Frames sent before the confirmation wait in
            // the channel, in order.
            frame = outbound.recv(), if configured && !commit_pending => {
                let Some(frame) = frame else {
                    // Stream owner dropped without `complete()` (abort/Drop
                    // path): close this connection out and finish.
                    let _ = sink.send(Message::Close(None)).await;
                    return Ok(NarilabsConnectionEnd::Finished);
                };
                match frame {
                    NarilabsOutbound::Audio(bytes) => {
                        let outcome = send_audio_appends(&mut sink, &bytes).await;
                        let delivered =
                            handle_audio_send(outcome, bytes, pending, audio_delivered);
                        if !delivered {
                            // NothingSent parks the frame for the redial's
                            // configured flush; PartiallySent means some audio
                            // reached the socket and cannot be replayed. Both
                            // lose the connection.
                            return Ok(NarilabsConnectionEnd::Lost);
                        }
                    }
                    NarilabsOutbound::Commit => {
                        if sink
                            .send(Message::Text(narilabs_commit_message()))
                            .await
                            .is_err()
                        {
                            return Ok(NarilabsConnectionEnd::Lost);
                        }
                        commit_pending = true;
                        draining_deadline =
                            Some(tokio::time::Instant::now() + close_grace);
                    }
                }
            }
            incoming = stream.next() => match incoming {
                Some(Ok(Message::Text(text))) => {
                    match ingest_narilabs_message(transcript, &text)? {
                        NarilabsMessageKind::SessionConfigured if !configured => {
                            configured = true;
                            // A frame parked by a failed send on the previous
                            // connection goes out before any new channel
                            // frame: the outbound channel has not been
                            // consumed yet, so order is preserved.
                            while let Some(frame) = pending.take() {
                                match frame {
                                    NarilabsOutbound::Audio(bytes) => {
                                        let outcome =
                                            send_audio_appends(&mut sink, &bytes).await;
                                        if !handle_audio_send(
                                            outcome,
                                            bytes,
                                            pending,
                                            audio_delivered,
                                        ) {
                                            return Ok(NarilabsConnectionEnd::Lost);
                                        }
                                    }
                                    // A commit never reaches `pending`: the
                                    // drain it starts never ends in a redial.
                                    NarilabsOutbound::Commit => {
                                        return Ok(NarilabsConnectionEnd::Lost);
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                    if commit_pending && termination_satisfied(transcript) {
                        let _ = sink.send(Message::Close(None)).await;
                        return Ok(NarilabsConnectionEnd::Finished);
                    }
                }
                Some(Ok(Message::Close(_))) | None => {
                    // The server closed. After the commit this is the normal
                    // end — or an early close, where the accumulated text
                    // (completed utterances, else partials) is the documented
                    // fallback. Either way the connection is done.
                    return Ok(NarilabsConnectionEnd::Finished);
                }
                Some(Err(_)) => {
                    if commit_pending {
                        // The transport died between the commit and the final
                        // utterances: finishing with what accumulated is the
                        // documented fallback, not a truncated-Transcript lie.
                        return Ok(NarilabsConnectionEnd::Finished);
                    }
                    return Ok(NarilabsConnectionEnd::Lost);
                }
                Some(Ok(_)) => {}
            },
            _ = ticks.tick() => {
                if cancel.is_cancelled() {
                    let _ = sink.send(Message::Close(None)).await;
                    return Ok(NarilabsConnectionEnd::Finished);
                }
                if let Some(deadline) = draining_deadline
                    && (termination_satisfied(transcript)
                        || tokio::time::Instant::now() >= deadline)
                {
                    // The round trip completed, or the grace elapsed
                    // without it: finish with whatever accumulated.
                    let _ = sink.send(Message::Close(None)).await;
                    return Ok(NarilabsConnectionEnd::Finished);
                }
            }
        }
    }
}

/// What one inbound text frame turned out to be, for the caller's
/// session-configuration tracking.
#[derive(Debug)]
pub(super) enum NarilabsMessageKind {
    /// `session.configured` — audio may flow from now on.
    SessionConfigured,
    Other,
}

/// Parses one inbound text frame into the accumulator. `transcript.partial`
/// and `transcript.completed` feed it; the commit acknowledgements mark the
/// end; a server `error` event (e.g. `INSUFFICIENT_CREDITS`), a frame that is
/// not JSON, and an event frame missing its required fields are all fatal —
/// silently skipping them would truncate the Transcript without a trace.
/// Unknown-but-well-formed message types stay tolerated so server-side schema
/// ADDITIONS never break the Recording.
pub(super) fn ingest_narilabs_message(
    transcript: &Arc<Mutex<NarilabsTranscriptAccumulator>>,
    text: &str,
) -> Result<NarilabsMessageKind, BoundaryError> {
    const MALFORMED: &str = "Narilabs sent a malformed streaming message";
    let Ok(message) = serde_json::from_str::<serde_json::Value>(text) else {
        return Err(BoundaryError::new(BoundaryKind::Provider, MALFORMED));
    };
    match message.get("type").and_then(serde_json::Value::as_str) {
        Some("session.configured") => Ok(NarilabsMessageKind::SessionConfigured),
        Some("transcript.partial") => {
            let Some(item_id) = message.get("item_id").and_then(serde_json::Value::as_str) else {
                return Err(BoundaryError::new(BoundaryKind::Provider, MALFORMED));
            };
            let Some(text) = message
                .get("transcript")
                .and_then(serde_json::Value::as_str)
            else {
                return Err(BoundaryError::new(BoundaryKind::Provider, MALFORMED));
            };
            transcript
                .lock()
                .expect("Narilabs transcript accumulator mutex poisoned")
                .ingest_partial(item_id, text);
            Ok(NarilabsMessageKind::Other)
        }
        Some("transcript.completed") => {
            let Some(item_id) = message.get("item_id").and_then(serde_json::Value::as_str) else {
                return Err(BoundaryError::new(BoundaryKind::Provider, MALFORMED));
            };
            let Some(text) = message
                .get("transcript")
                .and_then(serde_json::Value::as_str)
            else {
                return Err(BoundaryError::new(BoundaryKind::Provider, MALFORMED));
            };
            transcript
                .lock()
                .expect("Narilabs transcript accumulator mutex poisoned")
                .ingest_completed(item_id, text);
            Ok(NarilabsMessageKind::Other)
        }
        Some("input_audio_buffer.committed") | Some("input_audio_buffer.commit_empty") => {
            let item_id = message.get("item_id").and_then(serde_json::Value::as_str);
            transcript
                .lock()
                .expect("Narilabs transcript accumulator mutex poisoned")
                .ingest_end_acknowledged(item_id);
            Ok(NarilabsMessageKind::Other)
        }
        Some("error") => {
            let code = message
                .pointer("/error/code")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown");
            let detail = message
                .pointer("/error/message")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("no detail");
            Err(BoundaryError::new(
                BoundaryKind::Provider,
                format!("Narilabs reported a streaming error ({code}): {detail}"),
            ))
        }
        _ => Ok(NarilabsMessageKind::Other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Accepts any websocket upgrade request. A plain fn item, not a closure:
    /// the handshake callback's lifetime contract is not general enough for a
    /// zero-capture closure at this crate's rustc.
    // The callback's Err type is the crate's ~136-byte http::Response — fixed
    // by the third-party signature, not shrinkable here.
    #[allow(clippy::result_large_err)]
    fn accept_any_request(
        _: &tokio_tungstenite::tungstenite::handshake::server::Request,
        response: tokio_tungstenite::tungstenite::http::Response<()>,
    ) -> Result<
        tokio_tungstenite::tungstenite::http::Response<()>,
        tokio_tungstenite::tungstenite::http::Response<Option<std::string::String>>,
    > {
        Ok(response)
    }

    fn accumulator_with(events: &[serde_json::Value]) -> NarilabsTranscriptAccumulator {
        let transcript = Arc::new(Mutex::new(NarilabsTranscriptAccumulator::default()));
        for event in events {
            ingest_narilabs_message(&transcript, &event.to_string()).unwrap();
        }
        Arc::try_unwrap(transcript)
            .ok()
            .expect("test owns the accumulator")
            .into_inner()
            .unwrap()
    }

    #[test]
    fn the_streaming_url_defaults_to_wss_and_keeps_its_query() {
        assert_eq!(
            narilabs_streaming_url(NARILABS_REALTIME_URL).unwrap(),
            "wss://api.narilabs.com/v1/realtime?intent=transcription"
        );
    }

    #[test]
    fn an_https_endpoint_is_rewritten_to_wss() {
        assert_eq!(
            narilabs_streaming_url("https://api.narilabs.com/v1/realtime?intent=transcription")
                .unwrap(),
            "wss://api.narilabs.com/v1/realtime?intent=transcription"
        );
    }

    #[test]
    fn a_loopback_plaintext_endpoint_is_allowed_for_mock_servers() {
        assert_eq!(
            narilabs_streaming_url("http://127.0.0.1:9999/v1/realtime").unwrap(),
            "ws://127.0.0.1:9999/v1/realtime"
        );
        assert_eq!(
            narilabs_streaming_url("ws://localhost:9999/v1/realtime").unwrap(),
            "ws://localhost:9999/v1/realtime"
        );
        assert!(narilabs_streaming_url("ws://127.7.7.7:9/v1/realtime").is_ok());
    }

    #[test]
    fn a_non_loopback_plaintext_endpoint_is_refused() {
        assert!(narilabs_streaming_url("ws://api.narilabs.com/v1/realtime").is_err());
        assert!(narilabs_streaming_url("http://api.narilabs.com/v1/realtime").is_err());
        // The loopback decision is the PARSED host: a lookalike suffix is a
        // different host entirely.
        assert!(narilabs_streaming_url("ws://localhost.attacker.example/v1/realtime").is_err());
        // Userinfo is rejected outright: the Bearer key must never ride to a
        // host other than the one that was gated.
        assert!(narilabs_streaming_url("ws://127.0.0.1:80@attacker.example/v1/realtime").is_err());
    }

    #[test]
    fn the_session_configure_message_carries_the_exact_protocol_shape() {
        let message: serde_json::Value =
            serde_json::from_str(&narilabs_session_configure("qwen3-asr:free", "en", "")).unwrap();
        assert_eq!(message["type"], "session.configure");
        assert_eq!(message["session"]["model"], "qwen3-asr:free");
        assert_eq!(message["session"]["language"], "en");
        assert!(
            message["session"]["turn_detection"].is_null(),
            "manual commit mode is turn_detection: null: {}",
            message["session"]["turn_detection"]
        );
    }

    #[test]
    fn an_empty_prompt_keeps_the_configure_message_byte_identical() {
        let expected = serde_json::json!({
            "type": "session.configure",
            "session": {
                "model": "qwen3-asr:free",
                "language": "en",
                "turn_detection": serde_json::Value::Null,
            },
        })
        .to_string();
        assert_eq!(
            narilabs_session_configure("qwen3-asr:free", "en", ""),
            expected,
            "an empty prompt must omit the field, not send an empty string"
        );
    }

    #[test]
    fn a_prompt_rides_the_session_configuration_without_changing_the_rest() {
        let message: serde_json::Value = serde_json::from_str(&narilabs_session_configure(
            "qwen3-asr:free",
            "en",
            "Voisu, cargo",
        ))
        .unwrap();
        assert_eq!(message["session"]["prompt"], "Voisu, cargo");
        assert_eq!(message["session"]["model"], "qwen3-asr:free");
        assert_eq!(message["session"]["language"], "en");
        assert!(message["session"]["turn_detection"].is_null());
    }

    #[test]
    fn append_messages_stay_inside_the_wire_budget() {
        let pcm = vec![7_u8; NARILABS_APPEND_MAX_RAW_BYTES];
        let message = narilabs_append_message(&pcm);
        assert!(
            message.len() <= NARILABS_MESSAGE_BUDGET_BYTES,
            "a max-sized append must stay within 128 KiB: {} bytes",
            message.len()
        );
        // One byte more must split into two appends.
        let pcm = vec![7_u8; NARILABS_APPEND_MAX_RAW_BYTES + 1];
        let messages = narilabs_audio_appends(&pcm);
        assert_eq!(messages.len(), 2);
        for message in &messages {
            assert!(
                message.len() <= NARILABS_MESSAGE_BUDGET_BYTES,
                "{} bytes exceeds the wire budget",
                message.len()
            );
        }
    }

    #[test]
    fn append_messages_round_trip_the_exact_pcm_bytes() {
        use base64::Engine as _;
        let pcm: Vec<u8> = (0..NARILABS_APPEND_MAX_RAW_BYTES * 2 + 11)
            .map(|byte| (byte % 251) as u8)
            .collect();
        let messages = narilabs_audio_appends(&pcm);
        let mut decoded = Vec::new();
        for message in &messages {
            let value: serde_json::Value = serde_json::from_str(message).unwrap();
            assert_eq!(value["type"], "input_audio_buffer.append");
            decoded.extend(
                base64::engine::general_purpose::STANDARD
                    .decode(value["audio"].as_str().unwrap())
                    .unwrap(),
            );
        }
        assert_eq!(decoded, pcm);
    }

    #[test]
    fn completed_utterances_join_in_first_seen_item_order() {
        // item b was referenced before item a (the server may finalize out of
        // arrival order), so its text leads the join despite arriving first.
        let accumulator = accumulator_with(&[
            serde_json::json!({
                "type": "transcript.partial", "item_id": "a",
                "transcript": "First utterance.", "revision": 1
            }),
            serde_json::json!({
                "type": "transcript.partial", "item_id": "b",
                "transcript": "second utterance", "revision": 1
            }),
            serde_json::json!({
                "type": "transcript.completed", "item_id": "a",
                "transcript": "First utterance.", "commit_reason": "manual"
            }),
            serde_json::json!({
                "type": "transcript.completed", "item_id": "b",
                "transcript": "second utterance", "commit_reason": "max_duration"
            }),
        ]);
        assert_eq!(accumulator.text(), "First utterance. second utterance");
    }

    #[test]
    fn a_partial_revision_is_superseded_by_its_completion() {
        let accumulator = accumulator_with(&[
            serde_json::json!({
                "type": "transcript.partial", "item_id": "a",
                "transcript": "hello wor", "revision": 1
            }),
            serde_json::json!({
                "type": "transcript.partial", "item_id": "a",
                "transcript": "hello world", "revision": 2
            }),
            serde_json::json!({
                "type": "transcript.completed", "item_id": "a",
                "transcript": "Hello world.", "commit_reason": "manual"
            }),
        ]);
        assert_eq!(accumulator.text(), "Hello world.");
    }

    #[test]
    fn without_completions_the_last_partials_are_the_fallback() {
        let accumulator = accumulator_with(&[
            serde_json::json!({
                "type": "transcript.partial", "item_id": "a",
                "transcript": "Only a partial", "revision": 3
            }),
            serde_json::json!({
                "type": "transcript.partial", "item_id": "b",
                "transcript": "survives", "revision": 1
            }),
        ]);
        assert_eq!(accumulator.text(), "Only a partial survives");
    }

    #[test]
    fn termination_needs_the_end_ack_and_every_seen_item_completed() {
        let transcript = Arc::new(Mutex::new(NarilabsTranscriptAccumulator::default()));
        let satisfied = |transcript: &Arc<Mutex<NarilabsTranscriptAccumulator>>| {
            transcript
                .lock()
                .expect("test accumulator mutex")
                .termination_satisfied()
        };
        ingest_narilabs_message(
            &transcript,
            &serde_json::json!({
                "type": "transcript.completed", "item_id": "a",
                "transcript": "done", "commit_reason": "manual"
            })
            .to_string(),
        )
        .unwrap();
        assert!(!satisfied(&transcript), "no end acknowledgement yet");
        ingest_narilabs_message(
            &transcript,
            &serde_json::json!({"type": "input_audio_buffer.committed", "item_id": "a"})
                .to_string(),
        )
        .unwrap();
        assert!(satisfied(&transcript));
        // An item seen AFTER the acknowledgement reopens the termination rule.
        ingest_narilabs_message(
            &transcript,
            &serde_json::json!({
                "type": "transcript.partial", "item_id": "b",
                "transcript": "late", "revision": 1
            })
            .to_string(),
        )
        .unwrap();
        assert!(!satisfied(&transcript));
        ingest_narilabs_message(
            &transcript,
            &serde_json::json!({"type": "input_audio_buffer.commit_empty"}).to_string(),
        )
        .unwrap();
        assert!(!satisfied(&transcript), "the late item never completed");
    }

    #[test]
    fn a_server_error_event_names_the_code_and_is_fatal() {
        let transcript = Arc::new(Mutex::new(NarilabsTranscriptAccumulator::default()));
        let error = ingest_narilabs_message(
            &transcript,
            &serde_json::json!({
                "type": "error",
                "error": {"code": "INSUFFICIENT_CREDITS", "message": "beta exhausted"}
            })
            .to_string(),
        )
        .unwrap_err();
        assert_eq!(
            error.diagnostic(),
            "Narilabs reported a streaming error (INSUFFICIENT_CREDITS): beta exhausted"
        );
    }

    #[tokio::test]
    // The tungstenite accept_hdr callback's Err type is the crate's ~136-byte
    // http::Response — fixed by the third-party signature, not shrinkable here.
    #[allow(clippy::result_large_err)]
    async fn narilabs_streams_appends_and_joins_completed_utterances_after_the_commit() {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;
        use tokio_tungstenite::tungstenite::handshake::server::{
            Request as WsRequest, Response as WsResponse,
        };

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!(
            "ws://{}/v1/realtime?intent=transcription",
            listener.local_addr().unwrap()
        );
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let handshake: Arc<Mutex<Option<(String, String)>>> = Arc::default();
            let capture = Arc::clone(&handshake);
            let mut socket = tokio_tungstenite::accept_hdr_async(
                tcp,
                move |request: &WsRequest, response: WsResponse| {
                    let authorization = request
                        .headers()
                        .get("authorization")
                        .and_then(|value| value.to_str().ok())
                        .unwrap_or_default()
                        .to_owned();
                    *capture.lock().unwrap() = Some((request.uri().to_string(), authorization));
                    Ok(response)
                },
            )
            .await
            .unwrap();
            use base64::Engine as _;
            let mut audio: Vec<u8> = Vec::new();
            let mut appends: Vec<usize> = Vec::new();
            let mut interim_sent = false;
            let mut first_finalized = false;
            while let Some(message) = socket.next().await {
                match message.unwrap() {
                    Message::Text(text) => {
                        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
                        match value["type"].as_str().unwrap_or_default() {
                            "session.configure" => {
                                assert!(
                                    value["session"].get("prompt").is_none(),
                                    "an empty prompt must omit the field: {value}"
                                );
                                socket
                                    .send(Message::Text(
                                        serde_json::json!({"type": "session.configured"})
                                            .to_string(),
                                    ))
                                    .await
                                    .unwrap();
                            }
                            "input_audio_buffer.append" => {
                                appends.push(text.len());
                                audio.extend(
                                    base64::engine::general_purpose::STANDARD
                                        .decode(value["audio"].as_str().unwrap())
                                        .unwrap(),
                                );
                                if !interim_sent {
                                    interim_sent = true;
                                    socket
                                        .send(Message::Text(
                                            serde_json::json!({
                                                "type": "transcript.partial",
                                                "item_id": "item-1",
                                                "transcript": "this interim revision must never reach the Transcript",
                                                "revision": 1
                                            })
                                            .to_string(),
                                        ))
                                        .await
                                        .unwrap();
                                } else if !first_finalized {
                                    first_finalized = true;
                                    // The first utterance auto-finalized at the
                                    // 36 s max-duration mark; the connection
                                    // stays open for the next one.
                                    socket
                                        .send(Message::Text(
                                            serde_json::json!({
                                                "type": "transcript.completed",
                                                "item_id": "item-1",
                                                "transcript": "Hello world.",
                                                "commit_reason": "max_duration"
                                            })
                                            .to_string(),
                                        ))
                                        .await
                                        .unwrap();
                                    socket
                                        .send(Message::Text(
                                            serde_json::json!({
                                                "type": "transcript.partial",
                                                "item_id": "item-2",
                                                "transcript": "Second utterance",
                                                "revision": 1
                                            })
                                            .to_string(),
                                        ))
                                        .await
                                        .unwrap();
                                }
                            }
                            "input_audio_buffer.commit" => {
                                assert_eq!(value["event_id"], "end_of_input");
                                socket
                                    .send(Message::Text(
                                        serde_json::json!({
                                            "type": "input_audio_buffer.committed",
                                            "item_id": "item-2",
                                            "client_event_id": "end_of_input"
                                        })
                                        .to_string(),
                                    ))
                                    .await
                                    .unwrap();
                                socket
                                    .send(Message::Text(
                                        serde_json::json!({
                                            "type": "transcript.completed",
                                            "item_id": "item-2",
                                            "transcript": "Second utterance.",
                                            "commit_reason": "manual"
                                        })
                                        .to_string(),
                                    ))
                                    .await
                                    .unwrap();
                                let _ = socket.send(Message::Close(None)).await;
                                break;
                            }
                            _ => {}
                        }
                    }
                    Message::Close(_) => break,
                    _ => {}
                }
            }
            let captured = handshake.lock().unwrap().clone();
            (captured, audio, appends)
        });

        let reaper = ProviderReaper::new();
        let mut stream = NarilabsStream::connect(
            narilabs_streaming_url(&base).unwrap(),
            "qwen3-asr:free".to_owned(),
            "en".to_owned(),
            String::new(),
            Credential::new("controlled-credential".to_owned()).unwrap(),
            reaper.clone(),
        );
        let mut pcm = Vec::new();
        for chunk in [vec![1_u8; 64], vec![2_u8; 64]] {
            pcm.extend_from_slice(&chunk);
            stream.send_audio(AudioChunk(chunk)).await.unwrap();
        }
        // An un-streamed tail that complete() must top up before the commit.
        pcm.extend_from_slice(&[3_u8; 32]);
        let transcript = stream
            .complete(CapturedAudio::new(pcm.clone()))
            .await
            .unwrap();
        assert_eq!(transcript.provider, Provider::Narilabs);
        assert_eq!(transcript.text, "Hello world. Second utterance.");

        let (captured, audio, appends) = server.await.unwrap();
        let (uri, authorization) = captured.expect("handshake must be captured");
        assert_eq!(authorization, "Bearer controlled-credential");
        assert!(uri.contains("intent=transcription"), "{uri}");
        assert_eq!(audio, pcm, "every PCM byte must arrive base64-encoded");
        assert!(
            appends
                .iter()
                .all(|len| *len <= NARILABS_MESSAGE_BUDGET_BYTES),
            "every append must stay inside the wire budget: {appends:?}"
        );
        assert_eq!(reaper.pending(), 0);
    }

    #[tokio::test]
    #[allow(clippy::result_large_err)]
    async fn the_recording_prompt_rides_the_session_configure_frame() {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("ws://{}/v1/realtime", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_hdr_async(tcp, accept_any_request)
                .await
                .unwrap();
            while let Some(message) = socket.next().await {
                match message.unwrap() {
                    Message::Text(text) => {
                        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
                        match value["type"].as_str().unwrap_or_default() {
                            "session.configure" => {
                                assert_eq!(
                                    value["session"]["prompt"], "Voisu, cargo",
                                    "the Recording's dictionary prompt must ride the handshake: {value}"
                                );
                                socket
                                    .send(Message::Text(
                                        serde_json::json!({"type": "session.configured"})
                                            .to_string(),
                                    ))
                                    .await
                                    .unwrap();
                            }
                            "input_audio_buffer.commit" => {
                                socket
                                    .send(Message::Text(
                                        serde_json::json!({"type": "input_audio_buffer.commit_empty"})
                                            .to_string(),
                                    ))
                                    .await
                                    .unwrap();
                                let _ = socket.send(Message::Close(None)).await;
                                break;
                            }
                            _ => {}
                        }
                    }
                    Message::Close(_) => break,
                    _ => {}
                }
            }
        });

        let reaper = ProviderReaper::new();
        let mut stream = NarilabsStream::connect(
            narilabs_streaming_url(&base).unwrap(),
            "qwen3-asr:free".to_owned(),
            "en".to_owned(),
            "Voisu, cargo".to_owned(),
            Credential::new("controlled-credential".to_owned()).unwrap(),
            reaper.clone(),
        );
        let transcript = stream
            .complete(CapturedAudio::new(Vec::new()))
            .await
            .unwrap();
        assert_eq!(transcript.text, "");
        server.await.unwrap();
        assert_eq!(reaper.pending(), 0);
    }

    #[tokio::test]
    #[allow(clippy::result_large_err)]
    async fn an_early_close_after_the_commit_falls_back_to_the_last_partials() {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("ws://{}/v1/realtime", listener.local_addr().unwrap());
        let _server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_hdr_async(tcp, accept_any_request)
                .await
                .unwrap();
            while let Some(message) = socket.next().await {
                match message.unwrap() {
                    Message::Text(text) => {
                        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
                        match value["type"].as_str().unwrap_or_default() {
                            "session.configure" => {
                                assert!(
                                    value["session"].get("prompt").is_none(),
                                    "an empty prompt must omit the field: {value}"
                                );
                                socket
                                    .send(Message::Text(
                                        serde_json::json!({"type": "session.configured"})
                                            .to_string(),
                                    ))
                                    .await
                                    .unwrap();
                            }
                            "input_audio_buffer.commit" => {
                                // The end is acknowledged, but the server
                                // closes before any utterance completes.
                                socket
                                    .send(Message::Text(
                                        serde_json::json!({
                                            "type": "input_audio_buffer.committed",
                                            "item_id": "item-1",
                                            "client_event_id": "end_of_input"
                                        })
                                        .to_string(),
                                    ))
                                    .await
                                    .unwrap();
                                socket
                                    .send(Message::Text(
                                        serde_json::json!({
                                            "type": "transcript.partial",
                                            "item_id": "item-1",
                                            "transcript": "Only a partial survives",
                                            "revision": 4
                                        })
                                        .to_string(),
                                    ))
                                    .await
                                    .unwrap();
                                let _ = socket.send(Message::Close(None)).await;
                                break;
                            }
                            _ => {}
                        }
                    }
                    Message::Close(_) => break,
                    _ => {}
                }
            }
        });

        let reaper = ProviderReaper::new();
        let mut stream = NarilabsStream::connect(
            narilabs_streaming_url(&base).unwrap(),
            "qwen3-asr:free".to_owned(),
            "en".to_owned(),
            String::new(),
            Credential::new("controlled-credential".to_owned()).unwrap(),
            reaper.clone(),
        );
        stream.send_audio(AudioChunk(vec![1_u8; 64])).await.unwrap();
        let transcript = stream
            .complete(CapturedAudio::new(vec![1_u8; 64]))
            .await
            .unwrap();
        assert_eq!(transcript.text, "Only a partial survives");
        assert_eq!(reaper.pending(), 0);
    }

    #[tokio::test]
    #[allow(clippy::result_large_err)]
    async fn a_server_error_without_utterances_fails_the_provider_with_the_code() {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("ws://{}/v1/realtime", listener.local_addr().unwrap());
        let _server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_hdr_async(tcp, accept_any_request)
                .await
                .unwrap();
            // The beta refusal lands before any utterance exists.
            socket
                .send(Message::Text(
                    serde_json::json!({
                        "type": "error",
                        "error": {"code": "INSUFFICIENT_CREDITS", "message": "beta exhausted"}
                    })
                    .to_string(),
                ))
                .await
                .unwrap();
            while let Some(message) = socket.next().await {
                if matches!(message.unwrap(), Message::Close(_)) {
                    break;
                }
            }
        });

        let reaper = ProviderReaper::new();
        let mut stream = NarilabsStream::connect(
            narilabs_streaming_url(&base).unwrap(),
            "qwen3-asr:free".to_owned(),
            "en".to_owned(),
            String::new(),
            Credential::new("controlled-credential".to_owned()).unwrap(),
            reaper.clone(),
        );
        stream.send_audio(AudioChunk(vec![1_u8; 64])).await.unwrap();
        let error = stream
            .complete(CapturedAudio::new(vec![1_u8; 64]))
            .await
            .unwrap_err();
        assert_eq!(
            error.diagnostic(),
            "Narilabs reported a streaming error (INSUFFICIENT_CREDITS): beta exhausted"
        );
        assert_eq!(reaper.pending(), 0);
    }
}
