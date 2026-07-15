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
        path: "/tmp/does-not-matter.pcm".to_owned(),
        captured_at_unix_ms: now - 5_000,
        expires_at_unix_ms: now - 1_000,
    });
    let outcome = policy.prune(vec![record], now);
    assert_eq!(outcome.kept.len(), 1, "the record survives");
    assert!(outcome.kept[0].debug_audio.is_none(), "expired audio is detached");
    assert_eq!(outcome.expired_audio.len(), 1, "the expired audio path is returned for deletion");
}

#[test]
fn export_redacts_secrets_and_drops_unrelated_environment() {
    let record = record_at(1, unix_millis_now());
    let environment = vec![
        ("VOISU_GROQ_API_KEY".to_owned(), "super-secret".to_owned()),
        ("VOISU_GROQ_TRANSCRIPTION_URL".to_owned(), "https://groq.test".to_owned()),
        ("HOME".to_owned(), "/home/person".to_owned()),
        ("AWS_SECRET_ACCESS_KEY".to_owned(), "leak".to_owned()),
    ];
    let export = export_record(record, environment);
    assert_eq!(export.environment.get("VOISU_GROQ_API_KEY").map(String::as_str), Some(REDACTED));
    assert_eq!(
        export.environment.get("VOISU_GROQ_TRANSCRIPTION_URL").map(String::as_str),
        Some("https://groq.test"),
    );
    assert!(!export.environment.contains_key("HOME"), "unrelated env is dropped");
    assert!(!export.environment.contains_key("AWS_SECRET_ACCESS_KEY"), "non-VOISU secrets are dropped");
    let encoded = serde_json::to_string(&export).unwrap();
    assert!(!encoded.contains("super-secret"), "no credential value survives export: {encoded}");
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
    let path = std::path::PathBuf::from(&audio.path);
    assert!(path.exists(), "debug audio is written");

    let mut record = DiagnosticRecord::new("corr-audio".to_owned(), 1);
    record.debug_audio = Some(audio);
    store.record(record).unwrap();

    // With a zero TTL the next history read must expire and remove the capture.
    let history = store.history().unwrap();
    assert!(history[0].debug_audio.is_none(), "expired audio is detached from the record");
    assert!(!path.exists(), "expired debug audio file is removed safely");
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
