// Groq transcript provider: chunked upload/streaming and Groq-specific HTTP helpers.
//
// Split out of system.rs as a pure move; module-global items come from `super`.

use super::*;

pub struct GroqProvider {
    reaper: ProviderReaper,
    prompt: Option<String>,
    language: Option<String>,
}

impl GroqProvider {
    /// Builds a Groq provider whose streams share the actor-owned `reaper`, so a
    /// stream dropped mid-abort hands its curl reap to the supervisor the actor
    /// drains before Idle.
    pub fn new(reaper: ProviderReaper) -> Self {
        Self {
            reaper,
            prompt: None,
            language: None,
        }
    }

    /// Builds a provider with a Recording-start dictionary snapshot. Supplying
    /// the prompt keeps every Groq request for that Recording on the same
    /// glossary as its Deepgram stream.
    pub fn with_prompt(reaper: ProviderReaper, prompt: String) -> Self {
        Self {
            reaper,
            prompt: Some(prompt),
            language: None,
        }
    }

    /// Builds a provider from the exact Recording-start language snapshot that
    /// EnglishEligibility also records. This prevents later environment changes
    /// from making the declaration differ from what the request sends.
    pub fn with_prompt_and_language(
        reaper: ProviderReaper,
        prompt: String,
        language: String,
    ) -> Self {
        Self {
            reaper,
            prompt: Some(prompt),
            language: Some(language),
        }
    }
}

impl TranscriptProvider for GroqProvider {
    fn start(&mut self, _recording_id: u64) -> Result<Box<dyn ProviderStream>, BoundaryError> {
        let credential = SecretStore::load(&mut SecretToolStore, Provider::Groq)?;
        let endpoint = std::env::var("VOISU_GROQ_TRANSCRIPTION_URL")
            .unwrap_or_else(|_| "https://api.groq.com/openai/v1/audio/transcriptions".to_owned());
        // Hand curl the GATED serialization, never the raw string: the policy
        // verified the url crate's parse, and curl's own last-`@` userinfo
        // split accepts `\` in the authority, so a raw hand-over could be
        // reinterpreted into a target other than the one that was gated.
        let endpoint = provider_endpoint_url(&endpoint)
            .ok_or_else(|| {
                BoundaryError::new(
                    BoundaryKind::Provider,
                    "Groq transcription endpoint must use HTTPS except on loopback",
                )
            })?
            .as_str()
            .to_owned();
        Ok(Box::new(GroqStream {
            credential,
            endpoint,
            params: GroqRequestParams::from_config_with_language(
                self.prompt
                    .clone()
                    .unwrap_or_else(crate::dictionary::whisper_prompt),
                self.language
                    .clone()
                    .unwrap_or_else(groq_transcription_language),
            ),
            buffer: Vec::new(),
            streamed_bytes: 0,
            chunks: VecDeque::new(),
            cancel: CancelRegistry::new(),
            reaper: self.reaper.clone(),
        }))
    }
}

/// Parses `endpoint` under the provider transport policy: the URL must parse
/// cleanly (no control characters, backslash, or raw-authority `@` — see
/// `endpoint_raw_string_is_allowed`), carry no userinfo, and use HTTPS unless
/// the parsed host is loopback, so local test servers keep working. Returns the
/// parsed URL so the Groq curl paths hand over the GATED SERIALIZATION
/// (`url.as_str()`) instead of the raw string: curl splits userinfo at the
/// LAST `@` and accepts `\` inside the authority, while the url crate reads
/// both differently, so a raw hand-over could send the Bearer key somewhere
/// other than the host that was gated. Parsing instead of prefix matching is
/// what makes the policy hold: `http://localhost:8080@attacker.example/` has a
/// loopback-LOOKING authority prefix whose real host is attacker.example.
pub(super) fn provider_endpoint_url(endpoint: &str) -> Option<url::Url> {
    if !endpoint_raw_string_is_allowed(endpoint) {
        return None;
    }
    let url = url::Url::parse(endpoint).ok()?;
    if !endpoint_authority_is_allowed(&url) {
        return None;
    }
    match url.scheme() {
        "https" => Some(url),
        "http" if parsed_host_is_loopback(&url) => Some(url),
        _ => None,
    }
}

/// The per-Recording Groq/Whisper request tuning built once at stream start:
/// the model, the transcription language, and the vocabulary prompt. Cloned
/// into every chunk request so all requests for a Recording share one glossary.
#[derive(Clone)]
pub(super) struct GroqRequestParams {
    pub(super) model: String,
    pub(super) language: String,
    pub(super) prompt: String,
}

impl GroqRequestParams {
    /// Resolves model configuration while retaining the exact language snapshot
    /// captured at the Recording boundary.
    fn from_config_with_language(prompt: String, language: String) -> Self {
        let model =
            std::env::var("VOISU_GROQ_MODEL").unwrap_or_else(|_| "whisper-large-v3".to_owned());
        Self {
            model,
            language,
            prompt,
        }
    }
}

/// Resolve the exact Groq transcription language once at a Recording boundary.
#[must_use]
pub fn groq_transcription_language() -> String {
    std::env::var("VOISU_GROQ_LANGUAGE")
        .unwrap_or_else(|_| DEFAULT_TRANSCRIPTION_LANGUAGE.to_owned())
}

/// Whether the Groq stream should pre-stream chunks yet. Recordings at or below
/// the full-audio limit never pre-stream — they take one full-audio request at
/// finalize; only once a Recording grows past the limit does chunking begin.
pub(super) fn groq_prestream_active(total_received_bytes: usize) -> bool {
    total_received_bytes > GROQ_FULL_AUDIO_MAX_BYTES
}

/// Plans the finalize Groq request(s) over a `len`-byte finalized buffer. A
/// buffer at or below the full-audio limit is one full-audio request; a buffer
/// past the limit (for example when a capture backlog appended at Stop pushes it
/// over) is split into 60 s windows with a 4 s overlap so no single request is
/// oversized and the word-overlap dedup can stitch the seams.
// A one-element Vec<Range> IS the intent: the whole capture as a single chunk.
#[allow(clippy::single_range_in_vec_init)]
pub(super) fn plan_finalize_chunks(len: usize) -> Vec<std::ops::Range<usize>> {
    if len <= GROQ_FULL_AUDIO_MAX_BYTES {
        return vec![0..len];
    }
    let step = GROQ_CHUNK_BYTES - GROQ_CHUNK_OVERLAP_BYTES;
    let mut ranges = Vec::new();
    let mut start = 0;
    while start < len {
        let end = (start + GROQ_CHUNK_BYTES).min(len);
        ranges.push(start..end);
        if end == len {
            break;
        }
        start += step;
    }
    ranges
}

pub(super) struct GroqStream {
    pub(super) credential: Credential,
    pub(super) endpoint: String,
    pub(super) params: GroqRequestParams,
    pub(super) buffer: Vec<u8>,
    pub(super) streamed_bytes: usize,
    pub(super) chunks: VecDeque<tokio::task::JoinHandle<Result<String, BoundaryError>>>,
    /// Per-Recording cancellation flag observed by each in-flight curl
    /// request's owning bounded wait. Because each Recording gets its own
    /// stream and flag, cancelling one Recording can never touch the next
    /// one's requests, and stale results die with their aborted stream.
    pub(super) cancel: Arc<CancelRegistry>,
    /// Actor-owned supervisor that adopts this stream's chunk tasks if the
    /// stream is dropped mid-abort, so their curl reap is retained and awaited
    /// rather than detached.
    pub(super) reaper: ProviderReaper,
}

/// A retained provider-stream cleanup: a future that awaits an adopted chunk
/// task deque until every chunk task — and therefore every nested
/// `spawn_blocking` curl reap those tasks own — has completed.
pub(super) type ReapTask = std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>;

impl Drop for GroqStream {
    fn drop(&mut self) {
        // Signal cancellation FIRST so each in-flight curl request's owning
        // bounded wait kills and reaps its child, then hand the still-live chunk
        // tasks to the actor-owned reaper. Never abort them here: aborting the
        // outer task drops its nested `spawn_blocking` handle and detaches the
        // curl kill/reap, which is exactly the window that let Idle be published
        // over live blocking work.
        self.cancel.cancel();
        self.reaper.adopt(std::mem::take(&mut self.chunks));
    }
}

impl ProviderStream for GroqStream {
    fn provider(&self) -> Provider {
        Provider::Groq
    }

    fn send_audio(&mut self, chunk: AudioChunk) -> BoundaryFuture<'_, ()> {
        Box::pin(async move {
            self.streamed_bytes = self.streamed_bytes.saturating_add(chunk.0.len());
            self.buffer.extend_from_slice(&chunk.0);
            // A Recording at or below the full-audio limit never pre-streams: it
            // is transcribed as one full-audio request at finalize. Only once it
            // grows past the limit do we cut 60 s chunks with a 4 s overlap.
            if groq_prestream_active(self.streamed_bytes) {
                while self.buffer.len() >= GROQ_CHUNK_BYTES {
                    let pcm = self.buffer[..GROQ_CHUNK_BYTES].to_vec();
                    // Drain (not re-slice into a fresh Vec): the shift happens
                    // in place, so each 60 s chunk of a long pre-streamed
                    // Recording does one memmove instead of allocating and
                    // copying a fresh ~1.8 MB tail Vec.
                    self.buffer
                        .drain(..(GROQ_CHUNK_BYTES - GROQ_CHUNK_OVERLAP_BYTES));
                    let credential = self.credential.clone();
                    let endpoint = self.endpoint.clone();
                    let params = self.params.clone();
                    let cancel = Arc::clone(&self.cancel);
                    self.chunks.push_back(tokio::spawn(async move {
                        ProviderHttpClient
                            .transcribe_groq_chunk(credential, endpoint, params, pcm, cancel)
                            .await
                    }));
                }
            }
            Ok(())
        })
    }

    fn abort(mut self: Box<Self>) -> BoundaryFuture<'static, ()> {
        Box::pin(async move {
            // Cancel the in-flight curl children first: each owning bounded
            // wait observes the flag within one poll tick and kills through
            // its own Child handle. Aborting the tasks alone would detach
            // already-running blocking requests, letting work from the failed
            // Recording overlap the next one.
            self.cancel.cancel();
            while let Some(chunk) = self.chunks.front_mut() {
                let _ = chunk.await;
                self.chunks.pop_front();
            }
            Ok(())
        })
    }

    fn complete(&mut self, audio: CapturedAudio) -> BoundaryFuture<'_, SourceTranscript> {
        Box::pin(async move {
            let pcm = audio.pcm_s16le_mono_16khz();
            if self.streamed_bytes > pcm.len() {
                return Err(BoundaryError::new(
                    BoundaryKind::Provider,
                    "Groq stream exceeded the finalized Recording",
                ));
            }
            self.buffer.extend_from_slice(&pcm[self.streamed_bytes..]);
            // A finalize request needs issuing when nothing was pre-streamed, or
            // when the retained overlap tail carries fresh audio past the last
            // pre-streamed chunk. Its handle MUST live in `self.chunks` so a
            // Provider Deadline that drops this future still leaves `abort` /
            // `Drop` / the `ProviderReaper` owning — and killing — its curl
            // child, exactly as pre-streamed chunks are owned. Awaiting the
            // request inline here would detach that curl on cancellation.
            let needs_finalize =
                self.chunks.is_empty() || self.buffer.len() > GROQ_CHUNK_OVERLAP_BYTES;
            if needs_finalize {
                let buffer = std::mem::take(&mut self.buffer);
                // Re-evaluate the full-audio gate against the FINALIZED length:
                // a capture backlog appended at Stop can push a Recording past
                // the 120 s limit even when nothing crossed it during streaming,
                // in which case it must be chunked, not sent as one request.
                for range in plan_finalize_chunks(buffer.len()) {
                    let pcm = buffer[range].to_vec();
                    let credential = self.credential.clone();
                    let endpoint = self.endpoint.clone();
                    let params = self.params.clone();
                    let cancel = Arc::clone(&self.cancel);
                    self.chunks.push_back(tokio::spawn(async move {
                        ProviderHttpClient
                            .transcribe_groq_chunk(credential, endpoint, params, pcm, cancel)
                            .await
                    }));
                }
            }
            let mut transcripts = Vec::new();
            while let Some(chunk) = self.chunks.front_mut() {
                // Keep the handle in `self.chunks` for the await so a Provider
                // Deadline that drops this future still leaves `Drop` an owned
                // task to adopt. Once the await resolves, pop it BEFORE the `?`
                // error propagation: a completed handle left behind would be
                // polled a second time by the reaper's adopt closure and panic
                // ("JoinHandle polled after completion").
                let joined = chunk.await;
                self.chunks.pop_front();
                let transcript = joined.map_err(|_| {
                    BoundaryError::new(BoundaryKind::Provider, "Groq chunk task failed")
                })??;
                transcripts.push(transcript);
            }
            let text = merge_chunk_transcripts(transcripts);
            Ok(SourceTranscript {
                provider: Provider::Groq,
                text,
            })
        })
    }
}

pub(super) fn request_groq_chunk(
    credential: Credential,
    endpoint: String,
    params: &GroqRequestParams,
    pcm: Vec<u8>,
    cancel: &CancelRegistry,
) -> Result<String, BoundaryError> {
    let mut file = tempfile::Builder::new()
        .prefix("voisu-recording-")
        .suffix(".flac")
        .tempfile()
        .map_err(|_| {
            BoundaryError::new(BoundaryKind::Provider, "temporary audio file unavailable")
        })?;
    let flac = flac_from_pcm(&pcm)?;
    file.write_all(&flac)
        .and_then(|()| file.flush())
        .map_err(|_| BoundaryError::new(BoundaryKind::Provider, "temporary audio write failed"))?;
    let config = build_groq_curl_config(
        &endpoint,
        &credential,
        &file.path().to_string_lossy(),
        params,
    )?;
    let outcome = run_restricted_with_deadline(
        "curl",
        &[
            "-q",
            "--config",
            "-",
            "--fail",
            "--silent",
            "--show-error",
            "--max-time",
            "15",
        ],
        Some(config.as_bytes()),
        true,
        PROVIDER_PROCESS_DEADLINE,
        Some(cancel),
    )
    .map_err(|error| match error {
        ProcessError::TimedOut => {
            BoundaryError::new(BoundaryKind::Provider, "Groq Provider Deadline elapsed")
        }
        _ => BoundaryError::new(BoundaryKind::Provider, "Groq request unavailable or failed"),
    })?;
    if !outcome.success {
        return Err(BoundaryError::new(
            BoundaryKind::Provider,
            "Groq rejected the audio request",
        ));
    }
    let response: serde_json::Value = serde_json::from_slice(&outcome.stdout)
        .map_err(|_| BoundaryError::new(BoundaryKind::Provider, "Groq returned malformed JSON"))?;
    response
        .get("text")
        .and_then(|text| text.as_str())
        .map(str::to_owned)
        .ok_or_else(|| BoundaryError::new(BoundaryKind::Provider, "Groq response omitted text"))
}

pub(super) fn merge_chunk_transcripts(transcripts: Vec<String>) -> String {
    let mut merged: Vec<String> = Vec::new();
    for transcript in transcripts {
        let words: Vec<String> = transcript.split_whitespace().map(str::to_owned).collect();
        let overlap = (1..=merged.len().min(words.len()).min(GROQ_MERGE_OVERLAP_WORDS))
            .rev()
            .find(|count| merged[merged.len() - count..] == words[..*count])
            .unwrap_or(0);
        merged.extend(words.into_iter().skip(overlap));
    }
    merged.join(" ")
}

pub(super) fn flac_from_pcm(pcm: &[u8]) -> Result<Vec<u8>, BoundaryError> {
    use flacenc::bitsink::ByteSink;
    use flacenc::component::BitRepr;
    use flacenc::config::Encoder;
    use flacenc::error::Verify;
    use flacenc::source::MemSource;

    let (chunks, remainder) = pcm.as_chunks::<2>();
    let samples: Vec<i32> = chunks
        .iter()
        .map(|bytes| i16::from_le_bytes(*bytes) as i32)
        .collect();
    if !remainder.is_empty() {
        return Err(BoundaryError::new(
            BoundaryKind::Provider,
            "Recording PCM length is invalid",
        ));
    }
    let config = Encoder::default().into_verified().map_err(|_| {
        BoundaryError::new(
            BoundaryKind::Provider,
            "FLAC encoder configuration is invalid",
        )
    })?;
    let source = MemSource::from_samples(&samples, 1, 16, 16_000);
    let stream = flacenc::encode_with_fixed_block_size(&config, source, config.block_size)
        .map_err(|_| BoundaryError::new(BoundaryKind::Provider, "Recording FLAC encode failed"))?;
    let mut sink = ByteSink::new();
    stream
        .write(&mut sink)
        .map_err(|_| BoundaryError::new(BoundaryKind::Provider, "Recording FLAC output failed"))?;
    Ok(sink.into_inner())
}
