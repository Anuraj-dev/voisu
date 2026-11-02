// Cloud pipeline tests, moved with the code from voisu-app's system module.

use super::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;
use voisu_core::{ProviderCoordinator, ProviderStreams};

#[test]
fn read_capped_checked_completes_when_the_dump_fits() {
    let payload = vec![b'a'; 64];
    match read_capped_checked(&mut payload.as_slice(), 64).unwrap() {
        CappedRead::Complete(bytes) => assert_eq!(bytes, payload),
        CappedRead::Truncated { .. } => panic!("exact-cap dump must not look truncated"),
    }
}

#[test]
fn read_capped_checked_fail_closes_when_the_dump_exceeds_the_budget() {
    let payload = vec![b'b'; 65];
    match read_capped_checked(&mut payload.as_slice(), 64).unwrap() {
        CappedRead::Truncated { retained } => assert_eq!(retained, payload[..64]),
        CappedRead::Complete(_) => panic!("over-budget dump must not parse as complete"),
    }
}

#[test]
fn crypto_provider_installs_and_is_idempotent() {
    install_crypto_provider();
    assert!(rustls::crypto::CryptoProvider::get_default().is_some());
    install_crypto_provider();
    assert!(rustls::crypto::CryptoProvider::get_default().is_some());
}

/// Fixed Groq request tuning for stream constructor tests: deterministic and
/// independent of the host's environment and user dictionary.
fn test_groq_params() -> GroqRequestParams {
    GroqRequestParams {
        model: "whisper-large-v3".to_owned(),
        language: "en".to_owned(),
        prompt: "Groq, Tokio".to_owned(),
    }
}

#[test]
fn groq_stays_full_audio_at_or_below_the_limit_and_chunks_above() {
    // A Recording at exactly the full-audio limit still takes one full-audio
    // request; only once it grows past the limit does pre-streaming begin.
    assert!(!groq_prestream_active(GROQ_FULL_AUDIO_MAX_BYTES));
    assert!(!groq_prestream_active(GROQ_FULL_AUDIO_MAX_BYTES - 1));
    assert!(groq_prestream_active(GROQ_FULL_AUDIO_MAX_BYTES + 1));
}

#[test]
fn finalize_is_one_full_audio_request_at_or_below_the_limit() {
    assert_eq!(plan_finalize_chunks(1_000), vec![0..1_000]);
    assert_eq!(
        plan_finalize_chunks(GROQ_FULL_AUDIO_MAX_BYTES),
        vec![0..GROQ_FULL_AUDIO_MAX_BYTES]
    );
}

#[test]
fn finalize_chunks_a_backlog_inflated_recording_past_the_limit() {
    // 130 s finalized (e.g. a large capture backlog appended at Stop pushed a
    // Recording that streamed under 120 s past the limit): it must be chunked
    // into 60 s windows with a 4 s overlap, not one oversized request.
    let len = GROQ_FULL_AUDIO_MAX_BYTES + 16_000 * 2 * 10;
    let ranges = plan_finalize_chunks(len);
    assert!(
        ranges.len() >= 2,
        "past the limit finalize is chunked, not one request"
    );
    assert_eq!(ranges[0], 0..GROQ_CHUNK_BYTES);
    assert_eq!(ranges[1].start, GROQ_CHUNK_BYTES - GROQ_CHUNK_OVERLAP_BYTES);
    assert_eq!(
        ranges.last().unwrap().end,
        len,
        "the last window ends at the recording end"
    );
    for range in &ranges[..ranges.len() - 1] {
        assert_eq!(
            range.end - range.start,
            GROQ_CHUNK_BYTES,
            "non-final windows are full chunks"
        );
    }
}

#[test]
fn groq_chunk_geometry_is_sixty_second_windows_with_a_four_second_overlap() {
    assert_eq!(GROQ_CHUNK_BYTES, 16_000 * 2 * 60);
    assert_eq!(GROQ_CHUNK_OVERLAP_BYTES, 16_000 * 2 * 4);
    assert_eq!(GROQ_FULL_AUDIO_MAX_BYTES, 16_000 * 2 * 120);
}

/// send_audio advances its pre-stream buffer with `Vec::drain(..step)`.
/// This pins the invariant that change relies on: draining the front
/// `step` bytes leaves exactly the tail that the previous
/// `buffer[step..].to_vec()` re-allocation produced, at production
/// geometry and at arbitrary lengths.
#[test]
fn groq_overlap_rotation_drain_matches_slice_to_vec() {
    let step = GROQ_CHUNK_BYTES - GROQ_CHUNK_OVERLAP_BYTES;
    for &(len, cut) in &[
        (GROQ_CHUNK_BYTES, step),
        (4 * GROQ_CHUNK_BYTES, step),
        (5_000_000, step),
        (4096, 1000),
        (1000, 1000),
    ] {
        let original: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
        let mut via_drain = original.clone();
        via_drain.drain(..cut);
        assert_eq!(
            via_drain,
            original[cut..].to_vec(),
            "drain(..{cut}) must equal slice+to_vec for len={len}"
        );
    }
}

#[test]
fn groq_curl_config_carries_the_accuracy_gains() {
    let credential = Credential::new("secret-token".to_owned()).unwrap();
    let params = GroqRequestParams {
        model: "whisper-large-v3".to_owned(),
        language: "en".to_owned(),
        prompt: "Tokio, serde, SELinux".to_owned(),
    };
    let config = build_groq_curl_config(
        "https://api.groq.com/v1",
        &credential,
        "/tmp/rec.wav",
        &params,
    )
    .expect("valid config");
    assert!(config.contains("form = \"model=whisper-large-v3\""));
    assert!(config.contains("form = \"language=en\""));
    assert!(config.contains("form = \"temperature=0\""));
    assert!(config.contains("form = \"prompt=Tokio, serde, SELinux\""));
    // Slice B4: word-timestamp granularities ride on verbose_json, and
    // BOTH granularities are requested — `word` for the word list and
    // `segment` for the per-segment avg_logprob the word confidences are
    // derived from.
    assert!(config.contains("form = \"response_format=verbose_json\""));
    assert!(config.contains("form = \"timestamp_granularities[]=word\""));
    assert!(config.contains("form = \"timestamp_granularities[]=segment\""));
    assert!(config.contains("Authorization: Bearer secret-token"));
}

// Slice B6: the Groq language request field is wired to the same resolved
// value as Deepgram's `language` query param — any validated code is
// forwarded verbatim, not just the English default.
#[test]
fn groq_curl_config_carries_a_resolved_non_english_language() {
    let credential = Credential::new("secret-token".to_owned()).unwrap();
    let params = GroqRequestParams {
        model: "whisper-large-v3".to_owned(),
        language: "de-de".to_owned(),
        prompt: String::new(),
    };
    let config = build_groq_curl_config(
        "https://api.groq.com/v1",
        &credential,
        "/tmp/rec.wav",
        &params,
    )
    .expect("valid config");
    assert!(config.contains("form = \"language=de-de\""), "{config}");
}

#[test]
fn groq_recording_payload_is_valid_flac() {
    let pcm = vec![0_u8; 16_000 * 2];
    let flac = flac_from_pcm(&pcm).expect("one second Recording encodes");

    assert_eq!(&flac[..4], b"fLaC");
    assert!(flac.len() < pcm.len(), "silence compresses below raw PCM");
}

#[test]
fn groq_curl_config_uploads_a_flac_recording() {
    let credential = Credential::new("secret-token".to_owned()).unwrap();
    let config = build_groq_curl_config(
        "https://api.groq.com/v1",
        &credential,
        "/tmp/rec.flac",
        &test_groq_params(),
    )
    .expect("valid config");

    assert!(
        config.contains("form = \"file=@/tmp/rec.flac;filename=recording.flac;type=audio/flac\"")
    );
    assert!(!config.contains("recording.wav"));
    assert!(!config.contains("audio/wav"));
}

#[test]
fn groq_curl_config_omits_an_empty_prompt_and_language() {
    let credential = Credential::new("secret-token".to_owned()).unwrap();
    let params = GroqRequestParams {
        model: "whisper-large-v3".to_owned(),
        language: String::new(),
        prompt: String::new(),
    };
    let config = build_groq_curl_config(
        "https://api.groq.com/v1",
        &credential,
        "/tmp/rec.wav",
        &params,
    )
    .expect("valid config");
    assert!(!config.contains("prompt="));
    assert!(!config.contains("language="));
    // temperature is unconditional.
    assert!(config.contains("form = \"temperature=0\""));
}

#[test]
fn groq_curl_config_rejects_a_control_character_model() {
    let credential = Credential::new("secret-token".to_owned()).unwrap();
    let params = GroqRequestParams {
        model: "bad\nmodel".to_owned(),
        language: "en".to_owned(),
        prompt: String::new(),
    };
    let error = build_groq_curl_config(
        "https://api.groq.com/v1",
        &credential,
        "/tmp/rec.wav",
        &params,
    )
    .unwrap_err();
    assert_eq!(error.diagnostic(), "invalid Groq model");
}

#[test]
fn merge_dedupes_an_overlap_wider_than_the_old_window() {
    // A ~30-word seam overlap — wider than the previous 24-word window —
    // must be collapsed, not duplicated, at the 4 s chunk boundary.
    let first: Vec<String> = (0..40).map(|i| format!("w{i}")).collect();
    let second: Vec<String> = (10..60).map(|i| format!("w{i}")).collect();
    let merged = merge_chunk_transcripts(vec![
        GroqChunkTranscript {
            text: first.join(" "),
            words: Vec::new(),
        },
        GroqChunkTranscript {
            text: second.join(" "),
            words: Vec::new(),
        },
    ]);
    let expected: Vec<String> = (0..60).map(|i| format!("w{i}")).collect();
    assert_eq!(
        merged.text,
        expected.join(" "),
        "the 30-word overlap is deduped"
    );
}

// ─── Slice B4: Groq verbose_json fixtures (documented API shape) ────────

/// A realistic single-segment verbose_json response, built from the
/// documented Groq/OpenAI-compatible Whisper shape: top-level `text`, a
/// `words[]` array of `{word, start, end}` entries, and a `segments[]`
/// array whose entries carry `avg_logprob` (nats/token, closer to zero is
/// more confident).
const VERBOSE_JSON_ONE_SEGMENT: &str = r#"{
    "task": "transcribe",
    "language": "en",
    "duration": 2.1,
    "text": "Deploy the cache migration today.",
    "words": [
        {"word": "Deploy", "start": 0.08, "end": 0.52},
        {"word": "the", "start": 0.56, "end": 0.71},
        {"word": "cache", "start": 0.75, "end": 1.09},
        {"word": "migration", "start": 1.14, "end": 1.72},
        {"word": "today.", "start": 1.78, "end": 2.05}
    ],
    "x_groq": {"id": "req_01j", "runtime": {"model": "whisper-large-v3", "processed_by": "whisper-asr"}},
    "segments": [
        {
            "id": 0,
            "seek": 0,
            "start": 0.0,
            "end": 2.1,
            "text": " Deploy the cache migration today.",
            "tokens": [1, 2],
            "temperature": 0.0,
            "avg_logprob": -0.15,
            "compression_ratio": 1.4,
            "no_speech_prob": 0.013,
            "transient": false
        }
    ]
}"#;

#[test]
fn a_verbose_json_fixture_parses_text_and_word_confidences() {
    let response: serde_json::Value = serde_json::from_str(VERBOSE_JSON_ONE_SEGMENT).unwrap();
    let parsed = parse_groq_transcription_response(&response).expect("parses");
    assert_eq!(parsed.text, "Deploy the cache migration today.");
    assert_eq!(parsed.words.len(), 5);
    // exp(-0.15) ≈ 0.861: a confident segment yields confident words.
    let expected = (-0.15_f64).exp().clamp(0.0, 1.0);
    assert!(
        parsed
            .words
            .iter()
            .all(|(_, confidence)| (confidence - expected).abs() < 1e-9),
        "every word inherits its covering segment's confidence: {parsed:?}"
    );
    assert_eq!(parsed.words[0].0, "Deploy");
    assert_eq!(parsed.words[4].0, "today.");
}

#[test]
fn a_verbose_json_word_inherits_its_own_covering_segment_logprob() {
    // Two segments: the first confident (-0.10), the second shaky (-1.20).
    // Each word must read the segment its start timestamp falls in — the
    // occurrence/position rule, never a single global confidence.
    let response: serde_json::Value = serde_json::from_str(
        r#"{
        "text": "Deploy the service now",
        "words": [
            {"word": "Deploy", "start": 0.10, "end": 0.50},
            {"word": "the", "start": 0.55, "end": 0.70},
            {"word": "service", "start": 1.20, "end": 1.80},
            {"word": "now", "start": 1.85, "end": 2.10}
        ],
        "segments": [
            {"id": 0, "start": 0.0, "end": 1.0, "text": "Deploy the", "avg_logprob": -0.10},
            {"id": 1, "start": 1.0, "end": 2.2, "text": "service now", "avg_logprob": -1.20}
        ]
    }"#,
    )
    .unwrap();
    let parsed = parse_groq_transcription_response(&response).expect("parses");
    let confident = (-0.10_f64).exp();
    let shaky = (-1.20_f64).exp();
    assert_eq!(parsed.words[0].1, confident);
    assert_eq!(parsed.words[1].1, confident);
    assert_eq!(parsed.words[2].1, shaky);
    assert_eq!(parsed.words[3].1, shaky);
}

#[test]
fn a_word_before_every_segment_falls_back_to_the_earliest_segment() {
    let response: serde_json::Value = serde_json::from_str(
        r#"{
        "text": "hello",
        "words": [{"word": "hello", "start": 0.0, "end": 0.4}],
        "segments": [
            {"id": 0, "start": 0.5, "end": 1.0, "text": "hello", "avg_logprob": -0.30}
        ]
    }"#,
    )
    .unwrap();
    let parsed = parse_groq_transcription_response(&response).expect("parses");
    assert_eq!(parsed.words[0].1, (-0.30_f64).exp().clamp(0.0, 1.0));
}

#[test]
fn non_finite_and_missing_logprob_evidence_is_unproven_not_confident() {
    // A word whose covering segment carries a NaN avg_logprob — and a
    // response whose segments carry none at all — degrades to 0.0,
    // mirroring the Deepgram ingest clamp.
    let response: serde_json::Value = serde_json::from_str(
        r#"{
        "text": "hello there",
        "words": [
            {"word": "hello", "start": 0.1, "end": 0.4},
            {"word": "there", "start": 0.5, "end": 0.9}
        ],
        "segments": [
            {"id": 0, "start": 0.0, "end": 1.0, "text": "hello there", "avg_logprob": "not-a-number"},
            {"id": 1, "start": 1.0, "end": 2.0, "text": "more"}
        ]
    }"#,
    )
    .unwrap();
    let parsed = parse_groq_transcription_response(&response).expect("parses");
    assert!(
        parsed
            .words
            .iter()
            .all(|(_, confidence)| *confidence == 0.0)
    );
}

#[test]
fn a_plain_json_response_degrades_to_text_without_evidence() {
    // A server that ignored response_format and returned the historic
    // plain shape must still deliver its text — the Groq flow degrades to
    // the pre-B4 no-evidence behavior instead of failing the Recording.
    let response: serde_json::Value =
        serde_json::from_str(r#"{"text": "deploy the cache migration today"}"#).unwrap();
    let parsed = parse_groq_transcription_response(&response).expect("parses");
    assert_eq!(parsed.text, "deploy the cache migration today");
    assert!(parsed.words.is_empty());
}

#[test]
fn a_verbose_json_response_without_text_or_segments_is_rejected() {
    let response: serde_json::Value = serde_json::from_str(r#"{"task": "transcribe"}"#).unwrap();
    let error = parse_groq_transcription_response(&response).unwrap_err();
    assert_eq!(error.diagnostic(), "Groq response omitted text");
}

#[test]
fn chunk_confidence_evidence_merges_with_the_same_overlap_skip() {
    // Two chunks whose texts share a 3-word seam: the merged confidence
    // list must skip the second chunk's first three entries exactly as
    // the text skip drops its first three words, so no seam word carries
    // doubled evidence.
    let first = GroqChunkTranscript {
        text: "deploy the cache migration".to_owned(),
        words: vec![
            ("deploy".to_owned(), 0.9),
            ("the".to_owned(), 0.9),
            ("cache".to_owned(), 0.2),
            ("migration".to_owned(), 0.9),
        ],
    };
    let second = GroqChunkTranscript {
        text: "cache migration finished today".to_owned(),
        words: vec![
            ("cache".to_owned(), 0.8),
            ("migration".to_owned(), 0.8),
            ("finished".to_owned(), 0.8),
            ("today".to_owned(), 0.8),
        ],
    };
    let merged = merge_chunk_transcripts(vec![first, second]);
    assert_eq!(merged.text, "deploy the cache migration finished today");
    assert_eq!(
        merged
            .words
            .iter()
            .map(|(word, _)| word.as_str())
            .collect::<Vec<_>>(),
        vec!["deploy", "the", "cache", "migration", "finished", "today"],
        "the seam's duplicated words do not double their evidence"
    );
    assert_eq!(
        merged.words[2].1, 0.2,
        "the FIRST chunk's seam confidence wins"
    );
}

#[tokio::test]
async fn a_dropped_finalize_request_is_owned_by_the_reaper_not_detached() {
    // The single full-audio request of a short Recording is issued at
    // finalize. If the Provider Deadline drops complete() while that request
    // is in flight, its curl child must be OWNED — handed to the
    // ProviderReaper by Drop — never detached. A local server that accepts
    // and never answers keeps the finalize request in flight.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        let mut held = Vec::new();
        for stream in listener.incoming() {
            match stream {
                Ok(connection) => held.push(connection),
                Err(_) => break,
            }
        }
    });

    let reaper = CleanupSupervisor::new();
    let mut stream = GroqStream {
        credential: Credential::new("controlled-credential".to_owned()).unwrap(),
        endpoint: format!("http://{address}/audio/transcriptions"),
        params: test_groq_params(),
        buffer: Vec::new(),
        streamed_bytes: 0,
        chunks: VecDeque::new(),
        cancel: CancelRegistry::new(),
        reaper: reaper.clone(),
        word_confidence_evidence: Vec::new(),
    };

    // Drive complete() far enough to issue the finalize request, then drop it
    // exactly as the Provider Deadline would.
    {
        let completion = stream.complete(CapturedAudio::empty());
        assert!(
            tokio::time::timeout(Duration::from_millis(750), completion)
                .await
                .is_err(),
            "the hanging finalize request must not complete on its own"
        );
    }
    // Dropping the stream must adopt the still-live finalize task into the
    // reaper (cancel-first, then adopt), not detach its curl child. With the
    // finalize request awaited inline this count is zero.
    drop(stream);
    assert_eq!(
        reaper.pending(),
        1,
        "the finalize request handle must be owned by the reaper, not detached"
    );
    // Draining cancels the request and reaps its curl child.
    reaper.drain_to_completion(Duration::from_secs(5)).await;
}

/// A probe chunk task shaped like a real provider chunk: the outer async
/// task awaits an inner `spawn_blocking` request. The blocking closure — the
/// one holding a live curl child in production — waits for cancellation, then
/// performs a kill-and-reap that the test releases explicitly, so the test
/// can observe cleanup ownership at the exact instant the coordinator's error
/// surfaces. `entered` proves the blocking task actually started; `reap_done`
/// is the reap-completion latch that is set only after the child is reaped.
struct BlockingChunkProbe {
    entered: Arc<AtomicBool>,
    reap_done: Arc<AtomicBool>,
    release: std::sync::mpsc::Sender<()>,
}

fn spawn_blocking_backed_chunk(
    cancel: Arc<CancelRegistry>,
) -> (
    tokio::task::JoinHandle<Result<GroqChunkTranscript, BoundaryError>>,
    BlockingChunkProbe,
) {
    let entered = Arc::new(AtomicBool::new(false));
    let reap_done = Arc::new(AtomicBool::new(false));
    let (release, release_rx) = std::sync::mpsc::channel::<()>();
    let entered_task = Arc::clone(&entered);
    let reap_done_task = Arc::clone(&reap_done);
    let handle = tokio::spawn(async move {
        tokio::task::spawn_blocking(move || {
            entered_task.store(true, Ordering::SeqCst);
            // Mirror an in-flight curl request owned by this blocking task:
            // run until the owning bounded wait observes cancellation.
            while !cancel.is_cancelled() {
                std::thread::sleep(Duration::from_millis(1));
            }
            // Cancellation observed. The kill-and-reap of the child is gated
            // by the test so the reap deliberately outlasts the abort
            // deadline, forcing the coordinator down its timeout path while
            // the blocking work is still live.
            let _ = release_rx.recv();
            reap_done_task.store(true, Ordering::SeqCst);
            Err(BoundaryError::new(
                BoundaryKind::Provider,
                "request cancelled",
            ))
        })
        .await
        .map_err(|_| BoundaryError::new(BoundaryKind::Provider, "request task failed"))?
    });
    (
        handle,
        BlockingChunkProbe {
            entered,
            reap_done,
            release,
        },
    )
}

#[tokio::test]
async fn provider_abort_deadline_retains_and_reaps_blocking_work_before_idle() {
    let reaper = CleanupSupervisor::new();
    let credential = Credential::new("controlled-credential".to_owned()).unwrap();
    let deepgram_cancel = CancelRegistry::new();
    let groq_cancel = CancelRegistry::new();
    let (deepgram_chunk, deepgram_probe) =
        spawn_blocking_backed_chunk(Arc::clone(&deepgram_cancel));
    let (groq_chunk, groq_probe) = spawn_blocking_backed_chunk(Arc::clone(&groq_cancel));
    // Shape the Deepgram websocket I/O task like a real one whose teardown
    // still owns nested blocking work when cancellation fires.
    let deepgram_io_task = tokio::spawn(async move {
        deepgram_chunk
            .await
            .map_err(|_| {
                BoundaryError::new(BoundaryKind::Provider, "Deepgram streaming task failed")
            })?
            .map(|_| ())
    });
    let (deepgram_outbound, _deepgram_outbound_rx) = tokio::sync::mpsc::unbounded_channel();
    let streams = ProviderStreams {
        deepgram: Box::new(DeepgramStream {
            outbound: Some(deepgram_outbound),
            streamed_bytes: 0,
            io_tasks: VecDeque::from([deepgram_io_task]),
            transcript: Arc::new(Mutex::new(TranscriptAccumulator::default())),
            cancel: deepgram_cancel,
            shutdown: Arc::new(tokio::sync::Notify::new()),
            reaper: reaper.clone(),
        }),
        groq: Box::new(GroqStream {
            credential,
            endpoint: "http://localhost/groq".to_owned(),
            params: test_groq_params(),
            buffer: Vec::new(),
            streamed_bytes: 0,
            chunks: VecDeque::from([groq_chunk]),
            cancel: groq_cancel,
            reaper: reaper.clone(),
            word_confidence_evidence: Vec::new(),
        }),
    };

    // Both blocking requests must actually be executing inside spawn_blocking
    // before the deadline fires, so cleanup has real nested ownership to lose.
    wait_for(&deepgram_probe.entered).await;
    wait_for(&groq_probe.entered).await;

    let error = ProviderCoordinator::start(
        Duration::from_millis(10),
        Duration::from_millis(10),
        streams,
    )
    .complete(CapturedAudio::empty())
    .await
    .unwrap_err();
    assert_eq!(error.diagnostic(), "provider deadline cleanup timed out");

    // The moment the coordinator's cleanup-timeout error surfaces, the
    // blocking curl work is still live (its reap has not been released).
    // Publishing Idle here without draining would strand that live work.
    assert!(
        !deepgram_probe.reap_done.load(Ordering::SeqCst),
        "Deepgram curl reap must still be in flight when cleanup times out"
    );
    assert!(
        !groq_probe.reap_done.load(Ordering::SeqCst),
        "Groq curl reap must still be in flight when cleanup times out"
    );
    // Cleanup ownership was RETAINED by the actor-owned supervisor rather
    // than aborted and detached: with the detach defect this count is zero.
    assert_eq!(
        reaper.pending(),
        2,
        "both dropped streams must hand their curl reap to the supervisor"
    );

    // Release the reaps and drain the supervisor, exactly as the actor does
    // before it publishes Idle. Draining must await the retained reaper tasks
    // until the nested blocking work has actually completed its reap.
    let _ = deepgram_probe.release.send(());
    let _ = groq_probe.release.send(());
    assert!(
        reaper.drain(Duration::from_secs(2)).await,
        "the supervisor must fully drain within the bound"
    );
    assert!(
        deepgram_probe.reap_done.load(Ordering::SeqCst),
        "draining must not return until the Deepgram blocking reap completed"
    );
    assert!(
        groq_probe.reap_done.load(Ordering::SeqCst),
        "draining must not return until the Groq blocking reap completed"
    );
    assert_eq!(
        reaper.pending(),
        0,
        "a full drain must leave nothing retained"
    );
}

#[tokio::test]
async fn stream_dropped_without_a_runtime_still_retains_its_blocking_cleanup() {
    // Runtime teardown (and any non-runtime thread) can drop a provider
    // stream where Handle::try_current() fails. Adoption must be synchronous
    // and runtime-free: the cleanup is retained for a later drain, never
    // aborted — aborting would detach the nested spawn_blocking curl reap.
    let reaper = CleanupSupervisor::new();
    let cancel = CancelRegistry::new();
    let (chunk, probe) = spawn_blocking_backed_chunk(Arc::clone(&cancel));
    wait_for(&probe.entered).await;
    let stream = GroqStream {
        credential: Credential::new("controlled-credential".to_owned()).unwrap(),
        endpoint: "http://localhost/groq".to_owned(),
        params: test_groq_params(),
        buffer: Vec::new(),
        streamed_bytes: 0,
        chunks: VecDeque::from([chunk]),
        cancel,
        reaper: reaper.clone(),
        word_confidence_evidence: Vec::new(),
    };
    std::thread::spawn(move || drop(stream))
        .join()
        .expect("dropping a stream off the runtime must not panic");
    assert_eq!(
        reaper.pending(),
        1,
        "a stream dropped without a runtime must still retain its cleanup"
    );
    let _ = probe.release.send(());
    assert!(
        reaper.drain(Duration::from_secs(2)).await,
        "the retained cleanup must drain once released"
    );
    assert!(
        probe.reap_done.load(Ordering::SeqCst),
        "draining must await the blocking reap to completion"
    );
}

#[tokio::test]
async fn concurrent_drains_never_report_completion_over_live_cleanup() {
    // While one drain temporarily holds cleanup futures out of the
    // supervisor, a concurrent drain must serialize behind it — never
    // observe an empty supervisor and report a completed drain while the
    // blocking reap is still running.
    let reaper = CleanupSupervisor::new();
    let cancel = CancelRegistry::new();
    cancel.cancel();
    let (chunk, probe) = spawn_blocking_backed_chunk(Arc::clone(&cancel));
    wait_for(&probe.entered).await;
    let mut chunks = VecDeque::new();
    chunks.push_back(chunk);
    reaper.adopt(chunks);

    let first = tokio::spawn({
        let reaper = reaper.clone();
        async move { reaper.drain(Duration::from_secs(2)).await }
    });
    // Give the first drain time to take the cleanup batch out of the
    // supervisor before the concurrent drain starts.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let second = tokio::spawn({
        let reaper = reaper.clone();
        let reap_done = Arc::clone(&probe.reap_done);
        async move {
            let drained = reaper.drain(Duration::from_secs(2)).await;
            (drained, reap_done.load(Ordering::SeqCst))
        }
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    let _ = probe.release.send(());

    assert!(
        first.await.expect("first drain must not panic"),
        "the first drain must complete once the reap is released"
    );
    let (second_drained, reap_done_when_second_returned) =
        second.await.expect("second drain must not panic");
    assert!(second_drained, "the concurrent drain must also complete");
    assert!(
        reap_done_when_second_returned,
        "a concurrent drain must not report completion while the blocking reap runs"
    );
}

#[tokio::test]
async fn drain_to_completion_survives_pass_timeouts_without_detaching_cleanup() {
    // A teardown path whose single bounded drain times out would retain the
    // unfinished cleanup only to drop it with the runtime immediately
    // after. drain_to_completion must keep draining across pass timeouts
    // and return only once the blocking reap has actually completed.
    let reaper = CleanupSupervisor::new();
    let cancel = CancelRegistry::new();
    cancel.cancel();
    let (chunk, probe) = spawn_blocking_backed_chunk(Arc::clone(&cancel));
    wait_for(&probe.entered).await;
    reaper.adopt(VecDeque::from([chunk]));

    // Release the reap well after several 50ms passes have timed out.
    let release = probe.release.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        let _ = release.send(());
    });
    reaper.drain_to_completion(Duration::from_millis(50)).await;
    assert!(
        probe.reap_done.load(Ordering::SeqCst),
        "drain_to_completion must not return before the blocking reap completed"
    );
    assert_eq!(
        reaper.pending(),
        0,
        "a completed teardown drain must leave nothing retained"
    );
}

/// Spins until an `entered` latch is set, bounded so a genuine failure to
/// enter spawn_blocking surfaces as a timeout rather than a hang.
async fn wait_for(flag: &Arc<AtomicBool>) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while !flag.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("probe blocking task must enter spawn_blocking");
}

#[test]
fn transcript_accumulator_assembles_only_finalized_results_in_order() {
    let mut accumulator = TranscriptAccumulator::default();
    // Interim revision of a window that a later final supersedes.
    accumulator.ingest(&serde_json::json!({
        "type": "Results", "is_final": false,
        "channel": {"alternatives": [{"transcript": "the quick brown"}]}
    }));
    accumulator.ingest(&serde_json::json!({
        "type": "Results", "is_final": true,
        "channel": {"alternatives": [{"transcript": "the quick brown fox"}]}
    }));
    // Non-Results, whitespace-only finals, and shape drift are ignored.
    accumulator.ingest(&serde_json::json!({"type": "Metadata"}));
    accumulator.ingest(&serde_json::json!({
        "type": "Results", "is_final": true,
        "channel": {"alternatives": [{"transcript": "   "}]}
    }));
    accumulator.ingest(&serde_json::json!({"type": "Results", "is_final": true}));
    accumulator.ingest(&serde_json::json!({
        "type": "Results", "is_final": true, "speech_final": true,
        "channel": {"alternatives": [{"transcript": "jumps over."}]}
    }));
    assert_eq!(accumulator.text(), "the quick brown fox jumps over.");
}

#[test]
fn transcript_accumulator_keeps_word_confidences_from_finals_only() {
    let mut accumulator = TranscriptAccumulator::default();
    // An interim revision's words are superseded and must never be kept.
    accumulator.ingest(&serde_json::json!({
        "type": "Results", "is_final": false,
        "channel": {"alternatives": [{"transcript": "the quick brown",
            "words": [{"word": "the", "confidence": 0.1}]}]}
    }));
    accumulator.ingest(&serde_json::json!({
        "type": "Results", "is_final": true,
        "channel": {"alternatives": [{"transcript": "the quick brown fox",
            "words": [
                {"word": "the", "confidence": 0.98},
                {"word": "quick", "confidence": 0.31},
                {"word": "brown", "confidence": 0.87},
                {"word": "fox", "confidence": 0.99}
            ]}]}
    }));
    // A final segment without a words array contributes no evidence.
    accumulator.ingest(&serde_json::json!({
        "type": "Results", "is_final": true,
        "channel": {"alternatives": [{"transcript": "jumps over."}]}
    }));
    // A word without a numeric confidence is carried as 0.0 (unproven,
    // never confidently transcribed), and a wordless entry is skipped.
    // Confidences are clamped to the [0, 1] domain at ingest.
    accumulator.ingest(&serde_json::json!({
        "type": "Results", "is_final": true,
        "channel": {"alternatives": [{"transcript": "now",
            "words": [{"word": "now"}, {"confidence": 0.9}, {"word": "", "confidence": 1.0},
                      {"word": "big", "confidence": 1.7}, {"word": "tiny", "confidence": -0.5}]}]}
    }));

    assert_eq!(
        accumulator.words(),
        vec![
            ("the".to_owned(), 0.98),
            ("quick".to_owned(), 0.31),
            ("brown".to_owned(), 0.87),
            ("fox".to_owned(), 0.99),
            ("now".to_owned(), 0.0),
            ("big".to_owned(), 1.0),
            ("tiny".to_owned(), 0.0),
        ]
    );
}

#[test]
fn deepgram_streaming_url_carries_nova3_params_and_repeated_encoded_keyterms() {
    let url = deepgram_streaming_url(
        "wss://api.deepgram.com/v1/listen",
        &[
            "Voisu".to_owned(),
            "smart format".to_owned(),
            "  ".to_owned(),
        ],
        "en",
    )
    .unwrap();
    assert!(
        url.starts_with("wss://api.deepgram.com/v1/listen?"),
        "{url}"
    );
    for expected in [
        "model=nova-3",
        "language=en",
        "encoding=linear16",
        "sample_rate=16000",
        "channels=1",
        "interim_results=true",
        "smart_format=true",
        "punctuate=true",
        "endpointing=300",
        "utterance_end_ms=1000",
    ] {
        assert!(url.contains(expected), "{url} is missing {expected}");
    }
    assert!(url.contains("keyterm=Voisu"), "{url}");
    assert!(url.contains("keyterm=smart%20format"), "{url}");
    assert_eq!(
        url.matches("keyterm=").count(),
        2,
        "blank keyterms must be dropped: {url}"
    );
}

// Slice B6: the unset-environment default ("en") must produce a
// BYTE-IDENTICAL streaming URL to the pre-B6 fixed param list — same
// values, same order.
#[test]
fn deepgram_streaming_url_default_language_is_byte_identical_to_the_pre_b6_request() {
    let url = deepgram_streaming_url("wss://api.deepgram.com/v1/listen", &[], "en").unwrap();
    assert_eq!(
        url,
        "wss://api.deepgram.com/v1/listen?model=nova-3&language=en&encoding=linear16\
         &sample_rate=16000&channels=1&interim_results=true&smart_format=true\
         &punctuate=true&endpointing=300&utterance_end_ms=1000"
    );
}

// A resolved non-English language rides the same query slot, and provider
// codes pass through verbatim — BCP-47 region tags and Deepgram's
// multi-language code alike.
#[test]
fn deepgram_streaming_url_carries_the_resolved_transcription_language() {
    for language in ["de-de", "es-419", "multi"] {
        let url =
            deepgram_streaming_url("wss://api.deepgram.com/v1/listen", &[], language).unwrap();
        assert!(
            url.contains(&format!("&language={language}&")),
            "{url} is missing language={language}"
        );
    }
}

// An EMPTY dictionary must produce a byte-identical request: no keyterm
// parameter at all, so the provider sees exactly the pre-B2 request.
#[test]
fn deepgram_streaming_url_with_no_keyterms_omits_the_parameter_entirely() {
    let plain = deepgram_streaming_url("wss://api.deepgram.com/v1/listen", &[], "en").unwrap();
    assert!(!plain.contains("keyterm="), "{plain}");
    // Blank terms are dropped by the URL builder itself (mirroring the
    // persisted-dictionary parser, which can never yield an empty term):
    // even a caller handing it blanks keeps the request byte-identical.
    let blanks = deepgram_streaming_url(
        "wss://api.deepgram.com/v1/listen",
        &vec![String::new(); 3],
        "en",
    )
    .unwrap();
    assert_eq!(plain, blanks, "blank keyterms must change nothing");
}

#[test]
fn deepgram_streaming_url_rewrites_http_schemes_and_requires_wss_off_loopback() {
    assert!(
        deepgram_streaming_url("https://api.deepgram.com/v1/listen", &[], "en")
            .unwrap()
            .starts_with("wss://api.deepgram.com/v1/listen?")
    );
    assert!(
        deepgram_streaming_url("http://127.0.0.1:9999/v1/listen", &[], "en")
            .unwrap()
            .starts_with("ws://127.0.0.1:9999/v1/listen?")
    );
    assert!(deepgram_streaming_url("ws://deepgram.test/v1/listen", &[], "en").is_err());
    assert!(deepgram_streaming_url("http://deepgram.test/v1/listen", &[], "en").is_err());
    // The loopback decision is the PARSED host: the whole 127.0.0.0/8 range
    // is loopback, and a lookalike suffix is a different host entirely.
    assert!(deepgram_streaming_url("ws://127.7.7.7:9/v1/listen", &[], "en").is_ok());
    assert!(
        deepgram_streaming_url("ws://localhost.attacker.example/v1/listen", &[], "en").is_err()
    );
    // A base that already carries a query keeps it and appends with '&'.
    let url = deepgram_streaming_url("wss://host/listen?tier=custom", &[], "en").unwrap();
    assert!(url.contains("?tier=custom&model=nova-3"), "{url}");
}

async fn mock_deepgram_listener() -> (tokio::net::TcpListener, String) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("ws://{}/v1/listen", listener.local_addr().unwrap());
    (listener, base)
}

fn test_deepgram_stream(
    base: &str,
    keepalive: Duration,
    reaper: &CleanupSupervisor,
) -> DeepgramStream {
    test_deepgram_stream_with_grace(base, keepalive, DEEPGRAM_CLOSE_GRACE, reaper)
}

fn test_deepgram_stream_with_grace(
    base: &str,
    keepalive: Duration,
    close_grace: Duration,
    reaper: &CleanupSupervisor,
) -> DeepgramStream {
    DeepgramStream::connect(
        deepgram_streaming_url(base, &[], "en").unwrap(),
        Credential::new("controlled-credential".to_owned()).unwrap(),
        keepalive,
        close_grace,
        reaper.clone(),
    )
}

/// The terminal summary message Deepgram sends after `CloseStream`, before
/// closing the connection.
fn deepgram_metadata_frame() -> String {
    serde_json::json!({"type": "Metadata", "request_id": "mock-request"}).to_string()
}

fn deepgram_results_frame(transcript: &str, is_final: bool) -> String {
    serde_json::json!({
        "type": "Results",
        "is_final": is_final,
        "speech_final": is_final,
        "channel": {"alternatives": [{"transcript": transcript}]}
    })
    .to_string()
}

#[tokio::test]
// The tungstenite accept_hdr callback's Err type is the crate's ~136-byte
// http::Response — fixed by the third-party signature, not shrinkable here.
#[allow(clippy::result_large_err)]
async fn deepgram_streams_binary_audio_and_returns_only_finalized_segments() {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;
    use tokio_tungstenite::tungstenite::handshake::server::{
        Request as WsRequest, Response as WsResponse,
    };

    let (listener, base) = mock_deepgram_listener().await;
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
        let mut audio: Vec<u8> = Vec::new();
        let mut control: Vec<String> = Vec::new();
        let mut interim_sent = false;
        while let Some(message) = socket.next().await {
            match message.unwrap() {
                Message::Binary(bytes) => {
                    audio.extend_from_slice(&bytes);
                    if !interim_sent {
                        interim_sent = true;
                        socket
                            .send(Message::Text(deepgram_results_frame(
                                "this interim revision must never reach the Transcript",
                                false,
                            )))
                            .await
                            .unwrap();
                    }
                }
                Message::Text(text) => {
                    let closing = text.contains("CloseStream");
                    control.push(text);
                    if closing {
                        socket
                            .send(Message::Text(deepgram_results_frame("Hello world.", true)))
                            .await
                            .unwrap();
                        socket
                            .send(Message::Text(deepgram_results_frame(
                                "Second segment.",
                                true,
                            )))
                            .await
                            .unwrap();
                        socket
                            .send(Message::Text(deepgram_metadata_frame()))
                            .await
                            .unwrap();
                        let _ = socket.send(Message::Close(None)).await;
                        break;
                    }
                }
                Message::Close(_) => break,
                _ => {}
            }
        }
        let captured = handshake.lock().unwrap().clone();
        (captured, audio, control)
    });

    let reaper = CleanupSupervisor::new();
    let mut stream = test_deepgram_stream(&base, Duration::from_secs(5), &reaper);
    let mut pcm = Vec::new();
    for chunk in [vec![1u8; 64], vec![2u8; 64]] {
        pcm.extend_from_slice(&chunk);
        stream.send_audio(AudioChunk(chunk)).await.unwrap();
    }
    // An un-streamed tail that complete() must top up before Finalize.
    pcm.extend_from_slice(&[3u8; 32]);
    let transcript = stream
        .complete(CapturedAudio::new(pcm.clone()))
        .await
        .unwrap();
    assert_eq!(transcript.provider, Provider::Deepgram);
    assert_eq!(transcript.text, "Hello world. Second segment.");

    let (captured, audio, control) = server.await.unwrap();
    let (uri, authorization) = captured.expect("handshake must be captured");
    assert_eq!(authorization, "Token controlled-credential");
    assert!(uri.contains("model=nova-3"), "{uri}");
    assert!(uri.contains("interim_results=true"), "{uri}");
    assert_eq!(audio, pcm, "every PCM byte must arrive as binary frames");
    assert!(
        control.iter().any(|text| text.contains("\"Finalize\"")),
        "{control:?}"
    );
    assert!(
        control.iter().any(|text| text.contains("\"CloseStream\"")),
        "{control:?}"
    );
    assert_eq!(reaper.pending(), 0);
}

#[tokio::test]
async fn deepgram_connection_lost_mid_recording_fails_the_provider_visibly() {
    use futures_util::StreamExt;

    let (listener, base) = mock_deepgram_listener().await;
    let server = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(tcp).await.unwrap();
        // Take the first frame, then drop both the connection and the
        // listener so every redial within the bounded budget is refused.
        let _ = socket.next().await;
        drop(socket);
        drop(listener);
    });

    let reaper = CleanupSupervisor::new();
    let mut stream = test_deepgram_stream(&base, Duration::from_secs(5), &reaper);
    stream.send_audio(AudioChunk(vec![0u8; 32])).await.unwrap();
    server.await.unwrap();
    let error = stream
        .complete(CapturedAudio::new(vec![0u8; 32]))
        .await
        .unwrap_err();
    assert!(
        error.diagnostic().contains("Deepgram"),
        "a mid-Recording drop must surface a visible provider error, got {:?}",
        error.diagnostic()
    );
}

#[tokio::test]
async fn deepgram_server_error_message_fails_the_provider_visibly() {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    let (listener, base) = mock_deepgram_listener().await;
    let server = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(tcp).await.unwrap();
        let _ = socket.next().await;
        socket
            .send(Message::Text(
                r#"{"type":"Error","description":"rejected"}"#.to_owned(),
            ))
            .await
            .unwrap();
        // Hold the connection open; the client must fail on the Error
        // message itself, not on a transport drop.
        while let Some(Ok(message)) = socket.next().await {
            if matches!(message, Message::Close(_)) {
                break;
            }
        }
    });

    let reaper = CleanupSupervisor::new();
    let mut stream = test_deepgram_stream(&base, Duration::from_secs(5), &reaper);
    stream.send_audio(AudioChunk(vec![0u8; 32])).await.unwrap();
    let error = stream
        .complete(CapturedAudio::new(vec![0u8; 32]))
        .await
        .unwrap_err();
    assert_eq!(error.diagnostic(), "Deepgram reported a streaming error");
    server.abort();
}

#[tokio::test]
async fn deepgram_drop_after_delivered_audio_fails_visibly_without_redialing() {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    let (listener, base) = mock_deepgram_listener().await;
    let redialed = Arc::new(AtomicBool::new(false));
    let redialed_flag = Arc::clone(&redialed);
    let server = tokio::spawn(async move {
        // Finalize one segment for delivered audio, then drop abruptly.
        let (tcp, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(tcp).await.unwrap();
        let _ = socket.next().await;
        socket
            .send(Message::Text(deepgram_results_frame("schedule the", true)))
            .await
            .unwrap();
        drop(socket);
        // Keep listening: a redial WOULD succeed here, so a pass proves the
        // client refused to redial rather than that it couldn't.
        if tokio::time::timeout(Duration::from_secs(1), listener.accept())
            .await
            .is_ok()
        {
            redialed_flag.store(true, Ordering::SeqCst);
        }
    });

    let reaper = CleanupSupervisor::new();
    let mut stream = test_deepgram_stream(&base, Duration::from_secs(5), &reaper);
    stream.send_audio(AudioChunk(vec![7u8; 32])).await.unwrap();
    // Wait until the finalized segment was ingested, then give a
    // redial-and-continue implementation time to observe the drop and
    // redial BEFORE the Recording completes — the exact window where a
    // silent audio gap would hide.
    tokio::time::timeout(Duration::from_secs(2), async {
        while stream.transcript.lock().unwrap().text() != "schedule the" {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the finalized segment must be ingested");
    tokio::time::sleep(Duration::from_millis(600)).await;
    // Audio already accepted by the dropped socket cannot be replayed, so a
    // redial-and-continue would return a plausible Transcript with a silent
    // gap. The provider must fail visibly instead; Groq carries.
    let error = stream
        .complete(CapturedAudio::new(vec![7u8; 32]))
        .await
        .unwrap_err();
    assert!(
        error.diagnostic().contains("Deepgram"),
        "got {:?}",
        error.diagnostic()
    );
    let _ = server.await;
    assert!(
        !redialed.load(Ordering::SeqCst),
        "a drop after delivered audio must not be redialed"
    );
}

#[tokio::test]
async fn deepgram_redials_a_failed_dial_before_any_audio_within_the_budget() {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    let (listener, base) = mock_deepgram_listener().await;
    let (second_up_tx, second_up_rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        // First connection dies before any audio was delivered on it.
        let (tcp, _) = listener.accept().await.unwrap();
        let socket = tokio_tungstenite::accept_async(tcp).await.unwrap();
        drop(socket);
        // Second connection: the bounded redial carries the Recording.
        let (tcp, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(tcp).await.unwrap();
        let _ = second_up_tx.send(());
        while let Some(Ok(message)) = socket.next().await {
            if let Message::Text(text) = message
                && text.contains("CloseStream")
            {
                socket
                    .send(Message::Text(deepgram_results_frame("after redial", true)))
                    .await
                    .unwrap();
                socket
                    .send(Message::Text(deepgram_metadata_frame()))
                    .await
                    .unwrap();
                let _ = socket.send(Message::Close(None)).await;
                break;
            }
        }
    });

    let reaper = CleanupSupervisor::new();
    let mut stream = test_deepgram_stream(&base, Duration::from_secs(5), &reaper);
    // No audio has been handed to the first connection, so nothing can be
    // lost: the dial phase stays covered by the bounded reconnect budget.
    second_up_rx.await.unwrap();
    stream.send_audio(AudioChunk(vec![7u8; 32])).await.unwrap();
    let transcript = stream
        .complete(CapturedAudio::new(vec![7u8; 32]))
        .await
        .unwrap();
    assert_eq!(transcript.text, "after redial");
    server.await.unwrap();
}

#[tokio::test]
async fn deepgram_close_without_terminal_metadata_fails_visibly() {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    let (listener, base) = mock_deepgram_listener().await;
    let server = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(tcp).await.unwrap();
        while let Some(Ok(message)) = socket.next().await {
            if let Message::Text(text) = message
                && text.contains("CloseStream")
            {
                // A final Results but NO terminal Metadata before the
                // close: the server-side flush is unconfirmed, so the
                // Transcript may be truncated.
                socket
                    .send(Message::Text(deepgram_results_frame("truncated", true)))
                    .await
                    .unwrap();
                let _ = socket.send(Message::Close(None)).await;
                break;
            }
        }
    });

    let reaper = CleanupSupervisor::new();
    let mut stream = test_deepgram_stream(&base, Duration::from_secs(5), &reaper);
    stream.send_audio(AudioChunk(vec![5u8; 32])).await.unwrap();
    let error = stream
        .complete(CapturedAudio::new(vec![5u8; 32]))
        .await
        .unwrap_err();
    assert!(
        error.diagnostic().contains("Deepgram"),
        "a close without the terminal Metadata must fail visibly, got {:?}",
        error.diagnostic()
    );
    server.await.unwrap();
}

#[tokio::test]
async fn deepgram_unanswered_closestream_fails_visibly_at_the_close_grace() {
    use futures_util::StreamExt;
    use tokio_tungstenite::tungstenite::Message;

    let (listener, base) = mock_deepgram_listener().await;
    let server = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(tcp).await.unwrap();
        // Read everything, never answer CloseStream.
        while let Some(Ok(message)) = socket.next().await {
            if matches!(message, Message::Close(_)) {
                break;
            }
        }
    });

    let reaper = CleanupSupervisor::new();
    let mut stream = test_deepgram_stream_with_grace(
        &base,
        Duration::from_secs(5),
        Duration::from_millis(200),
        &reaper,
    );
    stream.send_audio(AudioChunk(vec![4u8; 32])).await.unwrap();
    // Returning the partial accumulator here would deliver a plausible but
    // truncated Source Transcript inside the 14s Provider Deadline.
    let error = stream
        .complete(CapturedAudio::new(vec![4u8; 32]))
        .await
        .unwrap_err();
    assert!(
        error.diagnostic().contains("Deepgram"),
        "an unanswered CloseStream must fail visibly at the close grace, got {:?}",
        error.diagnostic()
    );
    drop(stream);
    let _ = reaper.drain(Duration::from_secs(2)).await;
    let _ = server.await;
}

#[test]
fn deepgram_streaming_url_rejects_userinfo_in_the_authority() {
    // ws://127.0.0.1:80@attacker.example/… — the raw authority starts with
    // a loopback-looking userinfo but the HOST is attacker.example; the
    // Token header must never travel over that connection (let alone in
    // plaintext).
    assert!(
        deepgram_streaming_url("ws://127.0.0.1:80@attacker.example/listen", &[], "en").is_err()
    );
    assert!(deepgram_streaming_url("http://localhost@attacker.example/listen", &[], "en").is_err());
    assert!(deepgram_streaming_url("wss://user@api.deepgram.com/v1/listen", &[], "en").is_err());
    assert!(deepgram_streaming_url("ws:///listen", &[], "en").is_err());
    // The EMPTY userinfo form parses with no username and no password (the
    // url crate drops the `@` entirely), so the raw authority is what sees it.
    assert!(deepgram_streaming_url("ws://@localhost/listen", &[], "en").is_err());
    // A `\` ends the url crate's userinfo scan but is accepted inside
    // curl's last-`@` split: fail closed on the raw string.
    assert!(
        deepgram_streaming_url("ws://localhost:8080\\@attacker.example/listen", &[], "en").is_err()
    );
}

#[test]
fn provider_endpoint_gate_parses_the_url_instead_of_prefix_matching() {
    // HTTPS or loopback HTTP for live providers — not a model-catalog download gate.
    let secure = |endpoint: &str| provider_endpoint_url(endpoint).is_some();
    assert!(secure(
        "https://api.groq.com/openai/v1/audio/transcriptions"
    ));
    assert!(secure("http://localhost:8080/transcribe"));
    assert!(secure("http://127.0.0.1:9999/transcribe"));
    assert!(secure("http://127.7.7.7/transcribe"));
    assert!(!secure("http://attacker.example/transcribe"));
    // Userinfo smuggling: both of these passed the old prefix checks — any
    // `https://…` passed, and `localhost:8080@…` starts with `localhost:` —
    // while the real parsed host is attacker.example.
    assert!(!secure(
        "https://user:pass@api.groq.com@attacker.example/transcribe"
    ));
    assert!(!secure("http://localhost:8080@attacker.example/transcribe"));
    // Exact host comparison: a lookalike suffix is a different host.
    assert!(!secure("http://localhost.attacker.example/transcribe"));
    assert!(!secure("not a url"));
}

#[ignore = "L0-DEFECT: R4 catalog URL parser is not implemented"]
#[test]
fn catalog_download_url_rejects_uncataloged_https_and_http_loopback() {
    unimplemented!("catalog_download_url must not reuse provider_endpoint_url");
}

#[test]
fn provider_endpoint_gate_rejects_backslash_and_stripped_control_characters() {
    let secure = |endpoint: &str| provider_endpoint_url(endpoint).is_some();
    // The url crate stops its userinfo/host scan at `\` for special schemes
    // while curl splits userinfo at the LAST `@` and accepts `\` inside it:
    // this shape gates as loopback-without-userinfo yet connects to
    // attacker.example, so the raw string fails closed before parsing.
    assert!(!secure(
        "http://localhost:8080\\@attacker.example/transcribe"
    ));
    assert!(!secure(
        "https://api.groq.com\\@attacker.example/transcribe"
    ));
    // The URL parser silently strips tab (and newline) before validating:
    // `localho\tst` must not gate as `localhost`, and `\0` has no place in
    // an endpoint.
    assert!(!secure("http://localho\tst/transcribe"));
    assert!(!secure("http://localhost/transcribe\r"));
    assert!(!secure("http://localhost\0/transcribe"));
}

#[test]
fn provider_endpoint_gate_rejects_empty_userinfo() {
    let secure = |endpoint: &str| provider_endpoint_url(endpoint).is_some();
    // `http://@localhost/` parses with username()=="" and no password —
    // indistinguishable from no userinfo at the parsed level, and the url
    // crate drops the `@` from its serialization. The raw-authority gate
    // sees it: any `@` in the authority is userinfo.
    assert!(!secure("http://@localhost/transcribe"));
    assert!(!secure("http://:pass@localhost/transcribe"));
    assert!(!secure("http://user@localhost/transcribe"));
    // An `@` in the path or query is not userinfo.
    assert!(secure(
        "https://api.groq.com/openai/v1/audio/transcriptions?at=@here"
    ));
}

#[test]
fn groq_curl_paths_hand_over_the_gated_serialization() {
    // A parseable endpoint is handed over exactly as the url crate
    // serialized it — the string the policy verified is the string curl
    // receives, so curl's looser last-`@`/`\` authority reading can never
    // reinterpret it.
    let handed = provider_endpoint_url("https://api.groq.com/openai/v1/audio/transcriptions")
        .unwrap()
        .as_str()
        .to_owned();
    assert_eq!(
        handed,
        "https://api.groq.com/openai/v1/audio/transcriptions"
    );
    let loopback = provider_endpoint_url("http://localhost:8080/transcribe")
        .unwrap()
        .as_str()
        .to_owned();
    assert_eq!(loopback, "http://localhost:8080/transcribe");
    assert!(!loopback.contains('\\'));
    // Even a backslash that survived into a parse (had the raw gate ever
    // been bypassed) is serialized out of the authority: a `\`-terminated
    // authority becomes a `/@…` PATH, which curl treats as a path.
    let path_borne = url::Url::parse("https://api.groq.com/openai\\@attacker.example/x")
        .unwrap()
        .as_str()
        .to_owned();
    assert!(!path_borne.contains('\\'), "{path_borne}");
    assert!(
        path_borne.starts_with("https://api.groq.com/")
            && path_borne.contains("/@attacker.example/x"),
        "{path_borne}"
    );
}

#[tokio::test]
async fn deepgram_abort_surfaces_a_stored_streaming_failure() {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    let (listener, base) = mock_deepgram_listener().await;
    let server = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(tcp).await.unwrap();
        let _ = socket.next().await;
        socket
            .send(Message::Text(
                r#"{"type":"Error","description":"rejected"}"#.to_owned(),
            ))
            .await
            .unwrap();
        while let Some(Ok(message)) = socket.next().await {
            if matches!(message, Message::Close(_)) {
                break;
            }
        }
    });

    let reaper = CleanupSupervisor::new();
    let mut stream = test_deepgram_stream(&base, Duration::from_secs(5), &reaper);
    stream.send_audio(AudioChunk(vec![3u8; 32])).await.unwrap();
    // Wait until the I/O task stored the failure, mirroring a Recording
    // whose capture fails (or whose Recording Deadline fires) after
    // Deepgram already failed: abort(), not complete(), is what runs.
    tokio::time::timeout(Duration::from_secs(2), async {
        while !stream.io_tasks.front().unwrap().is_finished() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the streaming task must settle its failure");
    let error = Box::new(stream).abort().await.unwrap_err();
    assert_eq!(
        error.diagnostic(),
        "Deepgram reported a streaming error",
        "abort must surface the stored provider failure, not discard it"
    );
    server.abort();
}

#[test]
fn malformed_deepgram_text_frames_fail_visibly() {
    let transcript = Arc::new(Mutex::new(TranscriptAccumulator::default()));
    // Not JSON at all.
    assert!(ingest_deepgram_message(&transcript, "not-json").is_err());
    // A Results frame with no is_final marker.
    assert!(ingest_deepgram_message(&transcript, r#"{"type":"Results"}"#).is_err());
    // A finalized Results frame whose transcript text is missing: skipping
    // it would silently truncate the Transcript.
    assert!(
        ingest_deepgram_message(
            &transcript,
            r#"{"type":"Results","is_final":true,"speech_final":true}"#
        )
        .is_err()
    );
    // Unknown message types stay tolerated (server-side schema additions),
    // and interim shape drift is UI-only.
    assert!(ingest_deepgram_message(&transcript, r#"{"type":"SpeechStarted"}"#).is_ok());
    assert!(ingest_deepgram_message(&transcript, r#"{"type":"Results","is_final":false}"#).is_ok());
    assert_eq!(
        transcript.lock().unwrap().text(),
        "",
        "no rejected frame may leak text into the Transcript"
    );
}

#[tokio::test]
async fn deepgram_malformed_final_results_fail_the_provider_visibly() {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    let (listener, base) = mock_deepgram_listener().await;
    let server = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(tcp).await.unwrap();
        let _ = socket.next().await;
        // A malformed finalized Results frame, followed by a perfectly
        // clean drain: only frame-level strictness can catch this.
        socket
            .send(Message::Text(
                r#"{"type":"Results","is_final":true}"#.to_owned(),
            ))
            .await
            .unwrap();
        while let Some(Ok(message)) = socket.next().await {
            match message {
                Message::Text(text) if text.contains("CloseStream") => {
                    let _ = socket.send(Message::Text(deepgram_metadata_frame())).await;
                    let _ = socket.send(Message::Close(None)).await;
                    break;
                }
                Message::Close(_) => break,
                _ => {}
            }
        }
    });

    let reaper = CleanupSupervisor::new();
    let mut stream = test_deepgram_stream(&base, Duration::from_secs(5), &reaper);
    stream.send_audio(AudioChunk(vec![2u8; 32])).await.unwrap();
    let error = stream
        .complete(CapturedAudio::new(vec![2u8; 32]))
        .await
        .unwrap_err();
    assert!(
        error.diagnostic().contains("Deepgram"),
        "a malformed finalized Results frame must fail visibly, got {:?}",
        error.diagnostic()
    );
    let _ = server.await;
}

#[tokio::test]
async fn deepgram_sends_keepalive_text_frames_during_outbound_gaps() {
    use futures_util::StreamExt;
    use tokio_tungstenite::tungstenite::Message;

    let (listener, base) = mock_deepgram_listener().await;
    let server = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(tcp).await.unwrap();
        loop {
            match socket.next().await {
                Some(Ok(Message::Text(text))) if text.contains("KeepAlive") => return true,
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => return false,
                _ => {}
            }
        }
    });

    let reaper = CleanupSupervisor::new();
    let stream = test_deepgram_stream(&base, Duration::from_millis(50), &reaper);
    let keepalive_seen = tokio::time::timeout(Duration::from_secs(2), server)
        .await
        .expect("the mock server must observe a frame within the bound")
        .unwrap();
    assert!(keepalive_seen, "an idle outbound side must emit KeepAlive");
    Box::new(stream).abort().await.unwrap();
}

#[tokio::test]
async fn deepgram_stream_dropped_mid_abort_hands_the_streaming_task_to_the_reaper() {
    use futures_util::StreamExt;
    use tokio_tungstenite::tungstenite::Message;

    let (listener, base) = mock_deepgram_listener().await;
    let (connected_tx, connected_rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(tcp).await.unwrap();
        let _ = connected_tx.send(());
        while let Some(Ok(message)) = socket.next().await {
            if matches!(message, Message::Close(_)) {
                break;
            }
        }
    });

    let reaper = CleanupSupervisor::new();
    let stream = test_deepgram_stream(&base, Duration::from_secs(5), &reaper);
    // Cancellation before the connection exists would end the I/O task
    // pre-connect; the drop under test must land on a live websocket.
    connected_rx.await.unwrap();
    drop(stream);
    assert_eq!(
        reaper.pending(),
        1,
        "a dropped stream must hand its websocket I/O task to the supervisor"
    );
    assert!(
        reaper.drain(Duration::from_secs(2)).await,
        "the retained I/O task must observe cancellation and drain"
    );
    server.await.unwrap();
}

#[tokio::test]
async fn deepgram_abort_awaits_the_streaming_task_and_leaves_nothing_retained() {
    use futures_util::StreamExt;
    use tokio_tungstenite::tungstenite::Message;

    let (listener, base) = mock_deepgram_listener().await;
    let (connected_tx, connected_rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(tcp).await.unwrap();
        let _ = connected_tx.send(());
        while let Some(Ok(message)) = socket.next().await {
            if matches!(message, Message::Close(_)) {
                break;
            }
        }
    });

    let reaper = CleanupSupervisor::new();
    let mut stream = test_deepgram_stream(&base, Duration::from_secs(5), &reaper);
    stream.send_audio(AudioChunk(vec![9u8; 32])).await.unwrap();
    // The abort under test must land on a live websocket, not on an I/O
    // task that observed cancellation before it ever connected.
    connected_rx.await.unwrap();
    Box::new(stream).abort().await.unwrap();
    assert_eq!(
        reaper.pending(),
        0,
        "abort must await the I/O task itself, leaving nothing for the supervisor"
    );
    server.await.unwrap();
}

// PR_SET_PDEATHSIG is Linux-only.
#[cfg(target_os = "linux")]
#[test]
fn every_restricted_external_child_receives_the_parent_death_contract() {
    let mut child = restricted_command("python3");
    child.args([
        "-c",
        "import ctypes, signal, sys; value = ctypes.c_int(); result = ctypes.CDLL(None).prctl(2, ctypes.byref(value)); sys.exit(result != 0 or value.value != signal.SIGKILL)",
    ]);

    assert!(child.status().unwrap().success());
}

#[cfg(unix)]
#[test]
fn cancel_set_mid_wait_kills_the_owned_child_within_the_poll_bound() {
    let cancel = CancelRegistry::new();
    let registry = Arc::clone(&cancel);
    let canceller = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(50));
        registry.cancel();
    });
    let started = Instant::now();
    let result = run_restricted_with_deadline(
        "sleep",
        &["5"],
        None,
        false,
        Duration::from_secs(4),
        Some(&cancel),
    );
    canceller.join().unwrap();
    assert!(matches!(result, Err(ProcessError::TimedOut)));
    assert!(
        started.elapsed() < Duration::from_millis(600),
        "a mid-wait cancel must kill within the poll bound, elapsed {:?}",
        started.elapsed()
    );
}

#[test]
fn already_cancelled_operations_fail_fast_without_spawning() {
    let cancel = CancelRegistry::new();
    cancel.cancel();
    let started = Instant::now();
    let result = run_restricted_with_deadline(
        "sleep",
        &["5"],
        None,
        false,
        Duration::from_secs(4),
        Some(&cancel),
    );
    assert!(matches!(result, Err(ProcessError::TimedOut)));
    assert!(
        started.elapsed() < Duration::from_millis(100),
        "an already-cancelled operation must not spawn, elapsed {:?}",
        started.elapsed()
    );
}

/// #98: the default reconciliation model is the exact selected Qwen id.
#[test]
fn default_groq_reconciliation_model_is_exact_qwen_id() {
    assert_eq!(DEFAULT_GROQ_RECONCILIATION_MODEL, "qwen/qwen3.8-27b");
}

/// #98: only the exact selected model gets `reasoning_effort: "none"`.
#[test]
fn selected_qwen_reconciliation_body_sets_reasoning_effort_none() {
    let body = groq_reconciliation_request_body(
        DEFAULT_GROQ_RECONCILIATION_MODEL,
        "Reconcile these Source Transcripts.",
    );
    assert_eq!(
        body.get("model").and_then(|value| value.as_str()),
        Some("qwen/qwen3.8-27b")
    );
    assert_eq!(
        body.get("reasoning_effort")
            .and_then(|value| value.as_str()),
        Some("none")
    );
    assert_eq!(
        body.get("temperature").and_then(|value| value.as_f64()),
        Some(0.0)
    );
}

#[test]
fn intent_reconstruction_body_uses_json_object_mode_with_both_sources_and_dictionary() {
    let body = groq_intent_reconstruction_request_body(&IntentReconstructionRequest {
        sources: vec![
            SourceTranscript {
                provider: Provider::Deepgram,
                text: "sanitized one".to_owned(),
            },
            SourceTranscript {
                provider: Provider::Groq,
                text: "sanitized two".to_owned(),
            },
        ],
        dictionary_terms: vec!["Voisu".to_owned()],
    });
    assert_eq!(body["model"], "qwen/qwen3.8-27b");
    assert_eq!(body["reasoning_effort"], "none");
    assert_eq!(
        body["response_format"],
        serde_json::json!({"type": "json_object"})
    );
    let system = body["messages"][0]["content"].as_str().unwrap();
    assert!(system.contains("exactly this shape: {\"wording\":\"...\"}"));
    assert!(system.contains(
        "When the user spells a word letter by letter (for example a n t i g r a v i t y or ANTIGRAVITYCLI), join those letters into the intended word (Antigravity / Antigravity CLI) instead of leaving spaces or inventing a different name."
    ));
    let user: serde_json::Value =
        serde_json::from_str(body["messages"][1]["content"].as_str().unwrap()).unwrap();
    assert_eq!(user["sources"].as_array().unwrap().len(), 2);
    assert_eq!(user["dictionary"], serde_json::json!(["Voisu"]));
}

/// #98: every other override omits `reasoning_effort` (no family-wide Qwen
/// rule; GPT-OSS rejects `none` with HTTP 400).
#[test]
fn non_selected_reconciliation_models_omit_reasoning_effort() {
    let overrides = [
        "openai/gpt-oss-120b",
        "llama-3.3-70b-versatile",
        "qwen/qwen3.6-27b",
        "configured-model",
    ];
    for model in overrides {
        let body = groq_reconciliation_request_body(model, "Reconcile these Source Transcripts.");
        assert_eq!(
            body.get("model").and_then(|value| value.as_str()),
            Some(model),
            "model id must pass through unchanged: {model}"
        );
        assert!(
            body.get("reasoning_effort").is_none(),
            "reasoning_effort must be omitted for override {model}: {body}"
        );
    }
}
