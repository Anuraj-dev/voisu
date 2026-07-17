use std::time::Duration;

use tempfile::TempDir;
use voisu_core::{
    correlation_id, export_record, replay_capture, unix_millis_now, AudioChunk, BoundaryFuture,
    CapturedAudio, DebugAudioRecord, DiagnosticRecord, DiagnosticStore, LifecycleStage, Provider,
    ProviderCoordinator, ProviderFailure, ProviderFailureStage, ProviderStream, ProviderStreams,
    RetentionPolicy, SourceTranscript, Transcript, TranscriptDecision, TranscriptSelection,
    TranscriptValidator, REDACTED,
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
fn provider_failures_are_retained_and_surfaced_in_history() {
    // A provider that failed mid-stream and one that was absent/disabled must
    // both leave a visible entry in the retained history record — never a silent
    // missing Source Transcript.
    let dir = TempDir::new().unwrap();
    let store = DiagnosticStore::open(dir.path().to_owned(), RetentionPolicy::default()).unwrap();
    let mut record = DiagnosticRecord::new("corr-visible".to_owned(), 1);
    record.source_transcripts = vec![voisu_core::SourceTranscriptRecord {
        provider: Provider::Groq,
        text: "The async function returns a promise.".to_owned(),
    }];
    record.provider_failures = vec![
        ProviderFailure::new(
            Provider::Deepgram,
            ProviderFailureStage::Completion,
            "chunk 3 POST failed: connection reset",
        ),
        ProviderFailure::new(
            Provider::Deepgram,
            ProviderFailureStage::NotStarted,
            "Deepgram disabled for this Recording",
        ),
    ];
    let history = store.record(record).unwrap();
    assert_eq!(history.len(), 1);
    let failures = &history[0].provider_failures;
    assert_eq!(failures.len(), 2);
    assert_eq!(failures[0].stage, ProviderFailureStage::Completion);
    assert_eq!(failures[1].stage, ProviderFailureStage::NotStarted);
    // `voisu history` serializes the record verbatim, so the absence is visible.
    let encoded = serde_json::to_string(&history).unwrap();
    assert!(encoded.contains("connection reset"));
    assert!(encoded.contains("Deepgram disabled for this Recording"));
    assert!(encoded.contains("not_started"));
}

#[test]
fn export_structurally_scrubs_url_secrets_not_derived_from_secret_env_keys() {
    // Finding 5: a failure diagnostic echoes a signed provider URL whose secret
    // (userinfo + token query) comes from a NON-secret-named env key
    // (VOISU_DEEPGRAM_TRANSCRIPTION_URL). Name-based value scrubbing never sees
    // it, so export must strip URL userinfo and query/fragment structurally.
    let mut record = record_at(1, unix_millis_now());
    record.provider_failures = vec![ProviderFailure::new(
        Provider::Deepgram,
        ProviderFailureStage::Completion,
        "POST https://user:hunter2@api.deepgram.test/v1/listen?token=abc123 failed".to_owned(),
    )];
    // The URL env key is NOT classified secret by name (no API_KEY/TOKEN marker).
    let environment = vec![(
        "VOISU_DEEPGRAM_TRANSCRIPTION_URL".to_owned(),
        "https://api.deepgram.test/v1/listen".to_owned(),
    )];
    let export = export_record(record, environment);
    let encoded = serde_json::to_string(&export).unwrap();
    assert!(!encoded.contains("hunter2"), "URL userinfo must be stripped: {encoded}");
    assert!(!encoded.contains("token=abc123"), "URL query secret must be stripped: {encoded}");
    assert!(
        encoded.contains("https://api.deepgram.test/v1/listen"),
        "the non-secret host and path are preserved: {encoded}"
    );
    // The standalone scrubber is directly exercised too.
    assert_eq!(
        voisu_core::scrub_embedded_urls("see https://a:b@h.test/p?t=1 now"),
        "see https://h.test/p now"
    );
}

#[test]
fn export_scrubs_secret_values_from_provider_failure_diagnostics() {
    // A provider's boundary diagnostic can echo a secret (a signed URL, a header
    // value). Export must scrub it like every other free-form string.
    let mut record = record_at(1, unix_millis_now());
    record.provider_failures = vec![ProviderFailure::new(
        Provider::Deepgram,
        ProviderFailureStage::Completion,
        "auth failed with token sk-live-hostile-123".to_owned(),
    )];
    let environment = vec![("VOISU_DEEPGRAM_API_KEY".to_owned(), "sk-live-hostile-123".to_owned())];
    let export = export_record(record, environment);
    let encoded = serde_json::to_string(&export).unwrap();
    assert!(
        !encoded.contains("sk-live-hostile-123"),
        "a secret in a provider-failure diagnostic must be scrubbed: {encoded}"
    );
    assert!(encoded.contains(REDACTED));
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

#[test]
fn sanitize_url_fails_closed_on_malformed_and_unrecognized_inputs() {
    // Adversarial: scheme-less URLs still carry credentials — the naive parse
    // would pass "user:pass@host/path" straight through.
    assert_eq!(voisu_core::sanitize_url("user:hunter2@groq.test/v1"), REDACTED);
    assert_eq!(voisu_core::sanitize_url("groq.test/v1?key=leak"), REDACTED);
    assert_eq!(voisu_core::sanitize_url(""), REDACTED);
    // Unrecognized schemes are not reasoned about — redact entirely.
    assert_eq!(voisu_core::sanitize_url("ftp://user:pass@host/file"), REDACTED);
    assert_eq!(voisu_core::sanitize_url("javascript://alert(1)"), REDACTED);
    // Malformed: empty authority.
    assert_eq!(voisu_core::sanitize_url("http://"), REDACTED);
    assert_eq!(voisu_core::sanitize_url("https://user:pass@"), REDACTED);
    // Well-formed shapes still sanitize rather than redact.
    assert_eq!(
        voisu_core::sanitize_url("https://host.test:8443/v1/audio?k=leak"),
        "https://host.test:8443/v1/audio"
    );
    assert_eq!(voisu_core::sanitize_url("HTTPS://host.test"), "HTTPS://host.test");
}

#[test]
fn export_scrubs_even_a_one_character_secret_value() {
    let mut record = record_at(1, unix_millis_now());
    record.set_final_transcript("the code is 7 exactly".to_owned());
    let environment = vec![("VOISU_GROQ_API_KEY".to_owned(), "7".to_owned())];
    let export = export_record(record, environment);
    let transcript = export.record.final_transcript.as_deref().unwrap();
    assert!(
        !transcript.contains('7'),
        "a credential has no minimum length; even one character must be scrubbed: {transcript}"
    );
    assert!(transcript.contains(REDACTED));
}

#[test]
fn export_allowlist_passes_the_groq_model_name_through() {
    let environment = vec![("VOISU_GROQ_MODEL".to_owned(), "whisper-large-v3".to_owned())];
    let export = export_record(record_at(1, unix_millis_now()), environment);
    assert_eq!(
        export.environment.get("VOISU_GROQ_MODEL").map(String::as_str),
        Some("whisper-large-v3"),
    );
}

#[test]
fn a_preplanted_colliding_temp_file_does_not_lose_the_record() {
    let dir = TempDir::new().unwrap();
    let store_dir = dir.path().join("diag");
    let store = DiagnosticStore::open(store_dir.clone(), RetentionPolicy::default()).unwrap();
    // Adversarial: crash leftovers after PID reuse occupy the first temp names
    // this store would pick.
    for nonce in 0..3 {
        std::fs::write(
            store_dir.join(format!("history.json.tmp.{}.{nonce}", std::process::id())),
            b"stale",
        )
        .unwrap();
    }
    store
        .record(DiagnosticRecord::new("corr-collide".to_owned(), 1))
        .expect("a temp-name collision must retry, not fail");
    assert!(
        store.find("corr-collide").unwrap().is_some(),
        "the record persists despite the collision"
    );
    // Startup cleanup purges the stale leftovers.
    store.cleanup_expired().unwrap();
    let leftovers = std::fs::read_dir(&store_dir)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with("history.json.tmp."))
        })
        .count();
    assert_eq!(leftovers, 0, "stale temp files are purged at startup");
}

#[test]
fn sanitize_url_rejects_malformed_hosts_and_invalid_ports() {
    // Adversarial: hosts containing whitespace or backslashes are not DNS-safe
    // and must redact, not pass through.
    assert_eq!(voisu_core::sanitize_url("http://ho st.test/v1"), REDACTED);
    assert_eq!(voisu_core::sanitize_url("http://host\\evil.test/v1"), REDACTED);
    assert_eq!(voisu_core::sanitize_url("https://host.test\t/v1"), REDACTED);
    assert_eq!(voisu_core::sanitize_url("https://host_test/v1"), REDACTED);
    // Ports must parse as a non-zero u16.
    assert_eq!(voisu_core::sanitize_url("https://host.test:0/v1"), REDACTED);
    assert_eq!(voisu_core::sanitize_url("https://host.test:99999/v1"), REDACTED);
    assert_eq!(voisu_core::sanitize_url("https://host.test:+443/v1"), REDACTED);
    assert_eq!(voisu_core::sanitize_url("https://host.test:/v1"), REDACTED);
    // Valid shapes still sanitize rather than redact.
    assert_eq!(
        voisu_core::sanitize_url("https://host.test:65535/v1?k=leak"),
        "https://host.test:65535/v1"
    );
    assert_eq!(
        voisu_core::sanitize_url("https://[2001:db8::1]:8443/v1?k=leak"),
        "https://[2001:db8::1]:8443/v1"
    );
    assert_eq!(
        voisu_core::sanitize_url("https://user:pass@[2001:db8::1]/v1"),
        "https://[2001:db8::1]/v1"
    );
    // Malformed IPv6 literals redact.
    assert_eq!(voisu_core::sanitize_url("https://[2001:db8::1/v1"), REDACTED);
    assert_eq!(voisu_core::sanitize_url("https://[bad host]/v1"), REDACTED);
}

#[test]
fn startup_cleanup_survives_all_temp_name_candidates_being_preplanted() {
    let dir = TempDir::new().unwrap();
    let store_dir = dir.path().join("diag");
    let store = DiagnosticStore::open(store_dir.clone(), RetentionPolicy::default()).unwrap();
    // Adversarial: every one of the 32 bounded temp-name candidates is already
    // occupied by crash leftovers. Cleanup must purge them BEFORE any history
    // rewrite, or the rewrite exhausts its retries and cleanup fails.
    for nonce in 0..32 {
        std::fs::write(
            store_dir.join(format!("history.json.tmp.{}.{nonce}", std::process::id())),
            b"stale",
        )
        .unwrap();
    }
    store
        .cleanup_expired()
        .expect("cleanup must purge stale temp files before rewriting history");
    store
        .record(DiagnosticRecord::new("corr-after-purge".to_owned(), 1))
        .unwrap();
    assert!(
        store.find("corr-after-purge").unwrap().is_some(),
        "history still works after the purge"
    );
}

#[test]
fn sanitize_url_validates_ipv6_structure_not_just_characters() {
    // Adversarial: hex-and-colon soup that a character check accepts but a
    // structural parse rejects.
    assert_eq!(voisu_core::sanitize_url("https://[deadbeef]/v1"), REDACTED);
    assert_eq!(voisu_core::sanitize_url("https://[2001:db8::1::2]/v1"), REDACTED);
    // A well-formed literal still sanitizes rather than redacts.
    assert_eq!(
        voisu_core::sanitize_url("https://[2001:db8::1]/v1?k=leak"),
        "https://[2001:db8::1]/v1"
    );
}
