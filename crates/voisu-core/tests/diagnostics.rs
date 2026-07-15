use std::time::Duration;

use tempfile::TempDir;
use voisu_core::{
    correlation_id, export_record, replay_capture, unix_millis_now, AudioChunk, BoundaryFuture,
    CapturedAudio, DebugAudioRecord, DiagnosticRecord, DiagnosticStore, LifecycleStage, Provider,
    ProviderCoordinator, ProviderStream, ProviderStreams, RetentionPolicy, SourceTranscript,
    Transcript, TranscriptDecision, TranscriptSelection, TranscriptValidator, REDACTED,
};

fn record_at(id: u64, recorded_at_unix_ms: u64) -> DiagnosticRecord {
    let mut record = DiagnosticRecord::new(format!("rec-{id}"), id);
    record.recorded_at_unix_ms = recorded_at_unix_ms;
    record
}

#[test]
fn correlation_id_is_unique_and_carries_the_recording_id() {
    let first = correlation_id(7);
    let second = correlation_id(7);
    assert_ne!(first, second, "correlation IDs must not collide");
    assert!(first.contains("-7-"), "correlation ID must carry recording id: {first}");
}

#[test]
fn retention_drops_records_beyond_the_count_bound_newest_first() {
    let policy = RetentionPolicy {
        max_records: 2,
        max_age: Duration::from_secs(3600),
        debug_audio_ttl: Duration::from_secs(3600),
    };
    let now = 10_000;
    let records = vec![record_at(1, now - 300), record_at(2, now - 200), record_at(3, now - 100)];
    let outcome = policy.prune(records, now);
    let kept: Vec<u64> = outcome.kept.iter().map(|record| record.recording_id).collect();
    assert_eq!(kept, vec![2, 3], "the two newest records are retained, in chronological order");
}

#[test]
fn retention_expires_records_past_the_age_bound() {
    let policy = RetentionPolicy {
        max_records: 100,
        max_age: Duration::from_millis(500),
        debug_audio_ttl: Duration::from_secs(3600),
    };
    let now = 10_000;
    let records = vec![record_at(1, now - 5_000), record_at(2, now - 100)];
    let outcome = policy.prune(records, now);
    let kept: Vec<u64> = outcome.kept.iter().map(|record| record.recording_id).collect();
    assert_eq!(kept, vec![2], "only the fresh record survives the age bound");
}

#[test]
fn retention_detaches_expired_debug_audio_but_keeps_the_record() {
    let policy = RetentionPolicy {
        max_records: 100,
        max_age: Duration::from_secs(3600),
        debug_audio_ttl: Duration::from_secs(1),
    };
    let now = 10_000;
    let mut record = record_at(1, now - 10);
    record.debug_audio = Some(DebugAudioRecord {
        file_name: "does-not-matter-exp9000.pcm".to_owned(),
        captured_at_unix_ms: now - 5_000,
        expires_at_unix_ms: now - 1_000,
    });
    let outcome = policy.prune(vec![record], now);
    assert_eq!(outcome.kept.len(), 1, "the record survives");
    assert!(outcome.kept[0].debug_audio.is_none(), "expired audio is detached");
    assert_eq!(outcome.expired_audio.len(), 1, "the expired audio path is returned for deletion");
}

#[test]
fn export_environment_is_an_explicit_allowlist_with_no_secret_keys() {
    let record = record_at(1, unix_millis_now());
    let environment = vec![
        ("VOISU_GROQ_API_KEY".to_owned(), "super-secret".to_owned()),
        ("VOISU_GROQ_TRANSCRIPTION_URL".to_owned(), "https://groq.test/v1".to_owned()),
        ("VOISU_CUSTOM_NOTE".to_owned(), "maybe-a-secret".to_owned()),
        ("HOME".to_owned(), "/home/person".to_owned()),
        ("AWS_SECRET_ACCESS_KEY".to_owned(), "leak".to_owned()),
    ];
    let export = export_record(record, environment);
    assert!(
        !export.environment.contains_key("VOISU_GROQ_API_KEY"),
        "secret keys never appear in an export, even masked"
    );
    assert_eq!(
        export.environment.get("VOISU_GROQ_TRANSCRIPTION_URL").map(String::as_str),
        Some("https://groq.test/v1"),
    );
    assert!(
        !export.environment.contains_key("VOISU_CUSTOM_NOTE"),
        "unknown VOISU_* values are omitted, not trusted"
    );
    assert!(!export.environment.contains_key("HOME"), "unrelated env is dropped");
    assert!(!export.environment.contains_key("AWS_SECRET_ACCESS_KEY"));
    let encoded = serde_json::to_string(&export).unwrap();
    assert!(!encoded.contains("super-secret"), "no credential value survives export: {encoded}");
    assert!(!encoded.contains("maybe-a-secret"), "no unlisted value survives export: {encoded}");
}

#[test]
fn export_scrubs_secret_values_from_transcripts_and_reasons() {
    // Adversarial: the user dictated (or a provider echoed) the literal API key
    // and a reason embedded it — the exported free-form strings must be scrubbed.
    let mut record = record_at(1, unix_millis_now());
    record.source_transcripts = vec![voisu_core::SourceTranscriptRecord {
        provider: Provider::Groq,
        text: "my key is sk-live-hostile-123 please".to_owned(),
    }];
    record.set_final_transcript("use sk-live-hostile-123 for auth".to_owned());
    record.validation_reason = Some("candidate contained sk-live-hostile-123".to_owned());
    record.fallback_reason = Some("sk-live-hostile-123 rejected".to_owned());
    let environment = vec![
        ("VOISU_GROQ_API_KEY".to_owned(), "sk-live-hostile-123".to_owned()),
    ];
    let export = export_record(record, environment);
    let encoded = serde_json::to_string(&export).unwrap();
    assert!(
        !encoded.contains("sk-live-hostile-123"),
        "a known secret value must not survive anywhere in an export: {encoded}"
    );
    assert!(encoded.contains(REDACTED), "the secret is masked, not silently dropped");
}

#[test]
fn exported_endpoint_urls_lose_userinfo_credentials_and_query_parameters() {
    assert_eq!(
        voisu_core::sanitize_url("https://user:hunter2@groq.test/v1/audio?api_key=leak#frag"),
        "https://groq.test/v1/audio"
    );
    assert_eq!(
        voisu_core::sanitize_url("https://groq.test/v1"),
        "https://groq.test/v1"
    );
    let environment = vec![(
        "VOISU_GROQ_TRANSCRIPTION_URL".to_owned(),
        "https://user:hunter2@groq.test/v1?token=leak".to_owned(),
    )];
    let export = export_record(record_at(1, unix_millis_now()), environment);
    let encoded = serde_json::to_string(&export).unwrap();
    assert!(!encoded.contains("hunter2"), "URL userinfo must be stripped: {encoded}");
    assert!(!encoded.contains("token=leak"), "URL query must be stripped: {encoded}");
}

#[test]
fn store_appends_prunes_and_finds_by_correlation_id() {
    let dir = TempDir::new().unwrap();
    let policy = RetentionPolicy {
        max_records: 2,
        max_age: Duration::from_secs(3600),
        debug_audio_ttl: Duration::from_secs(3600),
    };
    let store = DiagnosticStore::open(dir.path().join("diag"), policy).unwrap();

    for id in 1..=3 {
        let mut record = DiagnosticRecord::new(format!("corr-{id}"), id);
        record.stages = vec![LifecycleStage::CaptureStarted, LifecycleStage::DeliveryCompleted];
        record.set_final_transcript(format!("transcript {id}"));
        store.record(record).unwrap();
    }

    let history = store.history().unwrap();
    assert_eq!(history.len(), 2, "retention bounds the stored history");
    assert_eq!(history[0].correlation_id, "corr-3", "newest first");
    assert!(store.find("corr-3").unwrap().is_some());
    assert!(store.find("corr-1").unwrap().is_none(), "pruned record is gone");
}

#[test]
fn store_debug_audio_is_written_privately_and_cleaned_up_on_expiry() {
    let dir = TempDir::new().unwrap();
    let policy = RetentionPolicy {
        max_records: 100,
        max_age: Duration::from_secs(3600),
        debug_audio_ttl: Duration::from_secs(0),
    };
    let store = DiagnosticStore::open(dir.path().join("diag"), policy).unwrap();
    let audio = store.store_debug_audio("corr-audio", &[1, 2, 3, 4]).unwrap();
    assert!(
        audio.file_name.contains(&format!("exp{}", audio.expires_at_unix_ms)),
        "the expiry is encoded in the file name: {}",
        audio.file_name
    );
    let path = store.audio_dir().join(&audio.file_name);
    assert!(path.exists(), "debug audio is written");

    let mut record = DiagnosticRecord::new("corr-audio".to_owned(), 1);
    record.debug_audio = Some(audio);
    store.record(record).unwrap();

    // With a zero TTL the next history read must expire and remove the capture.
    let history = store.history().unwrap();
    assert!(history[0].debug_audio.is_none(), "expired audio is detached from the record");
    assert!(!path.exists(), "expired debug audio file is removed safely");
}

#[test]
fn startup_cleanup_purges_orphaned_and_expired_debug_audio() {
    let dir = TempDir::new().unwrap();
    let store = DiagnosticStore::open(dir.path().join("diag"), RetentionPolicy::default()).unwrap();

    // An orphan left by a crash before its record persisted: expired by name.
    let expired_orphan = store.audio_dir().join("crashed-exp1000.pcm");
    std::fs::write(&expired_orphan, b"pcm").unwrap();
    // An orphan with an unparsable name: also removed.
    let junk = store.audio_dir().join("garbage.pcm");
    std::fs::write(&junk, b"pcm").unwrap();
    // A live, referenced capture: retained.
    let live = store.store_debug_audio("corr-live", &[1, 2]).unwrap();
    let live_path = store.audio_dir().join(&live.file_name);
    let mut record = DiagnosticRecord::new("corr-live".to_owned(), 1);
    record.debug_audio = Some(live);
    store.record(record).unwrap();

    store.cleanup_expired().unwrap();

    assert!(!expired_orphan.exists(), "expired orphan is purged");
    assert!(!junk.exists(), "unparsable orphan is purged");
    assert!(live_path.exists(), "a referenced, unexpired capture survives");
}

#[test]
fn tampered_history_audio_paths_cannot_steer_deletion_outside_the_store() {
    let dir = TempDir::new().unwrap();
    let victim = dir.path().join("victim.txt");
    std::fs::write(&victim, "precious").unwrap();
    let policy = RetentionPolicy {
        max_records: 100,
        max_age: Duration::from_secs(3600),
        debug_audio_ttl: Duration::from_secs(0),
    };
    let store = DiagnosticStore::open(dir.path().join("diag"), policy).unwrap();

    // Adversarial: a corrupt/tampered history record carries a traversal path.
    let mut record = DiagnosticRecord::new("corr-evil".to_owned(), 1);
    record.debug_audio = Some(DebugAudioRecord {
        file_name: "../../victim.txt".to_owned(),
        captured_at_unix_ms: 0,
        expires_at_unix_ms: 0,
    });
    store.record(record).unwrap();
    let _ = store.history().unwrap();

    assert!(victim.exists(), "cleanup must never delete outside the audio directory");
}

#[test]
fn concurrent_writers_never_lose_records_or_corrupt_history() {
    let dir = TempDir::new().unwrap();
    let policy = RetentionPolicy {
        max_records: 1000,
        max_age: Duration::from_secs(3600),
        debug_audio_ttl: Duration::from_secs(3600),
    };
    let store =
        std::sync::Arc::new(DiagnosticStore::open(dir.path().join("diag"), policy).unwrap());
    let mut handles = Vec::new();
    for writer in 0..4_u64 {
        let store = std::sync::Arc::clone(&store);
        handles.push(std::thread::spawn(move || {
            for sequence in 0..25_u64 {
                let id = writer * 100 + sequence;
                store
                    .record(DiagnosticRecord::new(format!("corr-{id}"), id))
                    .unwrap();
            }
        }));
    }
    for handle in handles {
        handle.join().unwrap();
    }
    let history = store.history().unwrap();
    assert_eq!(history.len(), 100, "no record is lost to a concurrent stale rewrite");
}

struct FixtureStream {
    provider: Provider,
    text: String,
}

impl ProviderStream for FixtureStream {
    fn provider(&self) -> Provider {
        self.provider
    }

    fn send_audio(&mut self, _chunk: AudioChunk) -> BoundaryFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }

    fn abort(self: Box<Self>) -> BoundaryFuture<'static, ()> {
        Box::pin(async { Ok(()) })
    }

    fn complete(&mut self, _audio: CapturedAudio) -> BoundaryFuture<'_, SourceTranscript> {
        let provider = self.provider;
        let text = self.text.clone();
        Box::pin(async move { Ok(SourceTranscript { provider, text }) })
    }
}

struct EchoValidator;

impl TranscriptValidator for EchoValidator {
    fn validate(&mut self, sources: Vec<SourceTranscript>) -> BoundaryFuture<'_, TranscriptDecision> {
        Box::pin(async move {
            let text = sources.first().map(|source| source.text.clone()).unwrap_or_default();
            Ok(TranscriptDecision {
                transcript: Transcript(text),
                selection: TranscriptSelection::NearIdenticalGroq,
                validation_reason: "fixture replay".to_owned(),
                fallback_reason: None,
                reconciliation_requested: false,
                recovery_attempted: false,
            })
        })
    }
}

#[tokio::test]
async fn replay_runs_a_fixed_fixture_through_provider_and_validation_boundaries() {
    let streams = ProviderStreams {
        deepgram: Box::new(FixtureStream {
            provider: Provider::Deepgram,
            text: "replayed dictation".to_owned(),
        }),
        groq: Box::new(FixtureStream {
            provider: Provider::Groq,
            text: "replayed dictation".to_owned(),
        }),
    };
    let coordinator =
        ProviderCoordinator::start(Duration::from_secs(5), Duration::from_secs(1), streams);
    let mut validator = EchoValidator;
    let outcome = replay_capture(CapturedAudio::new(vec![0_u8; 3_200]), coordinator, &mut validator)
        .await
        .expect("replay succeeds");
    assert_eq!(outcome.source_transcripts.len(), 2, "both providers replayed the fixture");
    assert_eq!(outcome.decision.transcript.0, "replayed dictation");
}
