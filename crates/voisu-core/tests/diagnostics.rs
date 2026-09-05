use std::time::Duration;

use tempfile::TempDir;
use voisu_core::{
    AudioChunk, BoundaryFuture, CapturedAudio, ConfidenceArbitrationDiagnostic,
    ConfidenceArbitrationRejection, DEFAULT_MAX_AGE, DEFAULT_MAX_RECORDS, DebugAudioRecord,
    DiagnosticRecord, DiagnosticStore, EnglishEligibilityOutcome, IntentReconstructionDiagnostic,
    IntentReconstructionEligibility, IntentReconstructionEvidence, IntentReconstructionOutcome,
    LifecycleStage, MAX_MODEL_ID_UTF8_BYTES, MAX_SMART_WRITING_DIAGNOSTIC_EDITS,
    MAX_SMART_WRITING_DIAGNOSTIC_TEXT_UTF8_BYTES, MAX_SMART_WRITING_EDIT_FIELD_UTF8_BYTES,
    MAX_SMART_WRITING_FREE_TEXT_UTF8_BYTES, PreparedTranscriptDecision, Provider,
    ProviderCoordinator, ProviderFailure, ProviderFailureStage, ProviderStream, ProviderStreams,
    REDACTED, RetentionPolicy, SMART_WRITING_DIAGNOSTIC_VERSION, SmartWritingDiagnostic,
    SmartWritingEditEvidence, SmartWritingMode, SmartWritingOutcome, SmartWritingReasonCode,
    SourceTranscript, TEXT_SHA256_FINGERPRINT_LEN, Transcript, TranscriptDecision,
    TranscriptSelection, TranscriptValidator, clamp_utf8_bytes, correlation_id, export_record,
    is_text_sha256_fingerprint, replay_capture, text_sha256_fingerprint, unix_millis_now,
};

fn record_at(id: u64, recorded_at_unix_ms: u64) -> DiagnosticRecord {
    let mut record = DiagnosticRecord::new(format!("rec-{id}"), id);
    record.recorded_at_unix_ms = recorded_at_unix_ms;
    record
}

/// The store's on-disk log, for tests that assert on how it is written rather
/// than on what it contains.
fn history_file(store_dir: &std::path::Path) -> std::path::PathBuf {
    store_dir.join("history.jsonl")
}

/// A whole-log rewrite renames a fresh temp file into place, so it always lands
/// on a new inode; an append writes into the file already there and keeps its
/// inode. That makes the inode an exact witness for which path a store operation
/// took — no clocks, no sleeps, nothing timing-dependent.
fn history_inode(store_dir: &std::path::Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(history_file(store_dir)).unwrap().ino()
}

fn history_len(store_dir: &std::path::Path) -> u64 {
    std::fs::metadata(history_file(store_dir)).unwrap().len()
}

#[test]
fn a_read_does_not_rewrite_a_history_it_did_not_prune() {
    // `Command::History` runs load-prune-rewrite-fsync INLINE in the lifecycle
    // actor, so a concurrent `voisu stop` queues behind it — measured at ~250 ms
    // at the default retention on a real disk once the store moved off tmpfs. A
    // read that prunes nothing has nothing to write, and must not write.
    let dir = TempDir::new().unwrap();
    let store_dir = dir.path().join("diag");
    let store = DiagnosticStore::open(store_dir.clone(), RetentionPolicy::default()).unwrap();
    store
        .record(DiagnosticRecord::new("corr-read".to_owned(), 1))
        .unwrap();

    let before_inode = history_inode(&store_dir);
    let before_len = history_len(&store_dir);
    for _ in 0..3 {
        assert_eq!(store.history().unwrap().len(), 1);
    }

    assert_eq!(
        history_inode(&store_dir),
        before_inode,
        "a read that pruned nothing must not rewrite the log"
    );
    assert_eq!(history_len(&store_dir), before_len);
}

#[test]
fn a_recording_appends_its_record_instead_of_rewriting_the_whole_log() {
    // The rewrite's cost scales with the ENTIRE retained history, not with the
    // record being added: ~250 ms per dictation at the default retention on a
    // real disk, on the completion path of every Recording.
    let dir = TempDir::new().unwrap();
    let store_dir = dir.path().join("diag");
    let store = DiagnosticStore::open(store_dir.clone(), RetentionPolicy::default()).unwrap();
    store
        .record(DiagnosticRecord::new("corr-1".to_owned(), 1))
        .unwrap();

    let before_inode = history_inode(&store_dir);
    let before_len = history_len(&store_dir);
    store
        .record(DiagnosticRecord::new("corr-2".to_owned(), 2))
        .unwrap();

    assert_eq!(
        history_inode(&store_dir),
        before_inode,
        "an append must write into the existing log, not rename a rewrite over it"
    );
    assert!(
        history_len(&store_dir) > before_len,
        "the appended record must still reach the log"
    );
    let kept = store.history().unwrap();
    assert_eq!(kept.len(), 2);
    assert_eq!(kept[0].correlation_id, "corr-2", "newest first");

    // The inode assertions above are not vacuous: once the log drifts past its
    // compaction slack a record DOES rewrite it, and that path lands on a new
    // inode. The slack floor is 8, so a one-record policy compacts at the tenth.
    let pruning = DiagnosticStore::open(
        store_dir.clone(),
        RetentionPolicy {
            max_records: 1,
            ..RetentionPolicy::default()
        },
    )
    .unwrap();
    let before_inode = history_inode(&store_dir);
    let mut compacted_at = None;
    for id in 3..12u64 {
        pruning
            .record(DiagnosticRecord::new(format!("corr-{id}"), id))
            .unwrap();
        if compacted_at.is_none() && history_inode(&store_dir) != before_inode {
            compacted_at = Some(id);
        }
    }
    assert_eq!(
        compacted_at,
        Some(10),
        "the log must be compacted exactly once the slack is exceeded, not every record"
    );
    assert_eq!(
        pruning.history().unwrap().len(),
        1,
        "a drifted log still reads back as the retained set"
    );
}

#[test]
fn a_torn_final_line_costs_one_record_not_the_whole_history() {
    // A crash partway through an append leaves the final line incomplete. Every
    // record written whole must survive: one truncated JSON value would be
    // unparseable and take the entire ring with it.
    use std::io::Write;
    let dir = TempDir::new().unwrap();
    let store_dir = dir.path().join("diag");
    let store = DiagnosticStore::open(store_dir.clone(), RetentionPolicy::default()).unwrap();
    store
        .record(DiagnosticRecord::new("corr-1".to_owned(), 1))
        .unwrap();
    store
        .record(DiagnosticRecord::new("corr-2".to_owned(), 2))
        .unwrap();

    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(history_file(&store_dir))
        .unwrap();
    file.write_all(br#"{"correlation_id":"corr-3","recording_i"#)
        .unwrap();
    drop(file);

    let kept = store.history().unwrap();
    assert_eq!(
        kept.len(),
        2,
        "a torn tail must cost only the record it tore, not the history"
    );
    assert_eq!(kept[0].correlation_id, "corr-2");
    assert_eq!(kept[1].correlation_id, "corr-1");

    let returned = store
        .record(DiagnosticRecord::new("corr-4".to_owned(), 4))
        .unwrap();
    assert!(
        returned
            .iter()
            .any(|record| record.correlation_id == "corr-4"),
        "record() accepted the post-tear record"
    );
    assert!(
        store.find("corr-4").unwrap().is_some(),
        "the first complete record after a torn tail must remain readable"
    );
}

#[test]
fn correlation_id_is_unique_and_carries_the_recording_id() {
    let first = correlation_id(7);
    let second = correlation_id(7);
    assert_ne!(first, second, "correlation IDs must not collide");
    assert!(
        first.contains("-7-"),
        "correlation ID must carry recording id: {first}"
    );
}

#[test]
fn retention_drops_records_beyond_the_count_bound_newest_first() {
    let policy = RetentionPolicy {
        max_records: 2,
        max_age: Duration::from_secs(3600),
        debug_audio_ttl: Duration::from_secs(3600),
    };
    let now = 10_000;
    let records = vec![
        record_at(1, now - 300),
        record_at(2, now - 200),
        record_at(3, now - 100),
    ];
    let outcome = policy.prune(records, now);
    let kept: Vec<u64> = outcome
        .kept
        .iter()
        .map(|record| record.recording_id)
        .collect();
    assert_eq!(
        kept,
        vec![2, 3],
        "the two newest records are retained, in chronological order"
    );
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
    let kept: Vec<u64> = outcome
        .kept
        .iter()
        .map(|record| record.recording_id)
        .collect();
    assert_eq!(
        kept,
        vec![2],
        "only the fresh record survives the age bound"
    );
}

#[test]
fn default_retention_fills_the_count_bound_from_a_week_of_recordings() {
    // Both bounds must move together. Raising the count alone achieves nothing:
    // the age bound prunes FIRST, so at the observed ~36 Recordings a day a 24 h
    // age bound plateaus the ring near 36 and the count bound is never reached.
    // This pins the count at >= 200 AND the age at >= 7 days, and then proves
    // the consequence: feeding a week of Recordings in, the count bound — not
    // the age bound — is what trims, and >= 200 records are retained spanning
    // far more than a day. Time is injected, never slept on.
    // Compile-time pin (clippy: assertions_on_constants forbids the runtime
    // form): the count bound must retain at least 200 Recordings.
    const { assert!(DEFAULT_MAX_RECORDS >= 200) };
    assert!(
        DEFAULT_MAX_AGE >= Duration::from_secs(7 * 24 * 3600),
        "the age bound must retain at least seven days, got {DEFAULT_MAX_AGE:?}"
    );

    let policy = RetentionPolicy::default();
    let now: u64 = 40 * 24 * 3600 * 1000;
    let interval_ms: u64 = 24 * 3600 * 1000 / 36;
    let week_count: u64 = 7 * 36;
    // A week of Recordings at ~36 a day, oldest first.
    let week: Vec<DiagnosticRecord> = (0..week_count)
        .map(|index| record_at(index, now - (week_count - 1 - index) * interval_ms))
        .collect();

    let outcome = policy.prune(week, now);
    let kept: Vec<u64> = outcome
        .kept
        .iter()
        .map(|record| record.recording_id)
        .collect();
    assert_eq!(
        kept.len(),
        DEFAULT_MAX_RECORDS,
        "the ring must fill to its count bound instead of plateauing at a day's worth"
    );
    assert!(kept.len() >= 200, "at least 200 records are retained");
    let expected: Vec<u64> =
        (week_count - u64::try_from(DEFAULT_MAX_RECORDS).unwrap()..week_count).collect();
    assert_eq!(kept, expected, "the newest records are the ones retained");

    let oldest_kept_age_ms = (week_count - 1 - kept[0]) * interval_ms;
    assert!(
        oldest_kept_age_ms > 24 * 3600 * 1000,
        "the retained window must reach past a single day, got {oldest_kept_age_ms} ms"
    );
}

#[test]
fn default_retention_prunes_past_the_age_bound_and_keeps_everything_newer() {
    // Pins the age bound in absolute days AND on its exact edge: an eight-day
    // record is pruned, a six-day record survives, and a record one millisecond
    // past the bound is dropped while one exactly on it is kept. `prune` takes
    // `now_ms`, so this is wall-clock independent — no sleep, no scheduling
    // assumption.
    let policy = RetentionPolicy::default();
    let now: u64 = 100 * 24 * 3600 * 1000;
    let day: u64 = 24 * 3600 * 1000;
    let age_ms = u64::try_from(DEFAULT_MAX_AGE.as_millis()).unwrap();
    let records = vec![
        record_at(1, now - 8 * day),
        record_at(2, now - age_ms - 1),
        record_at(3, now - age_ms),
        record_at(4, now - 6 * day),
        record_at(5, now - day),
        record_at(6, now),
    ];

    let outcome = policy.prune(records, now);
    let kept: Vec<u64> = outcome
        .kept
        .iter()
        .map(|record| record.recording_id)
        .collect();
    assert_eq!(
        kept,
        vec![3, 4, 5, 6],
        "records past the age bound are pruned and everything newer — including a \
         six-day-old Recording — is kept"
    );
}

#[test]
fn default_retention_still_caps_the_ring_at_the_count_bound() {
    // The count bound remains the ceiling once the age bound stops pruning: one
    // record beyond it drops the oldest, so the ring never grows without limit
    // even when a user records far more than a week's worth inside the week.
    let policy = RetentionPolicy::default();
    let now: u64 = 100 * 24 * 3600 * 1000;
    let overfull: Vec<DiagnosticRecord> = (0..u64::try_from(DEFAULT_MAX_RECORDS).unwrap() + 1)
        .map(|index| record_at(index, now - 1_000))
        .collect();

    let outcome = policy.prune(overfull, now);
    assert_eq!(outcome.kept.len(), DEFAULT_MAX_RECORDS);
    assert_eq!(
        outcome.kept[0].recording_id, 1,
        "the oldest record beyond the count bound is the one dropped"
    );
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
    assert!(
        outcome.kept[0].debug_audio.is_none(),
        "expired audio is detached"
    );
    assert_eq!(
        outcome.expired_audio.len(),
        1,
        "the expired audio path is returned for deletion"
    );
}

#[test]
fn export_environment_is_an_explicit_allowlist_with_no_secret_keys() {
    let record = record_at(1, unix_millis_now());
    let environment = vec![
        ("VOISU_GROQ_API_KEY".to_owned(), "super-secret".to_owned()),
        (
            "VOISU_GROQ_TRANSCRIPTION_URL".to_owned(),
            "https://groq.test/v1".to_owned(),
        ),
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
        export
            .environment
            .get("VOISU_GROQ_TRANSCRIPTION_URL")
            .map(String::as_str),
        Some("https://groq.test/v1"),
    );
    assert!(
        !export.environment.contains_key("VOISU_CUSTOM_NOTE"),
        "unknown VOISU_* values are omitted, not trusted"
    );
    assert!(
        !export.environment.contains_key("HOME"),
        "unrelated env is dropped"
    );
    assert!(!export.environment.contains_key("AWS_SECRET_ACCESS_KEY"));
    let encoded = serde_json::to_string(&export).unwrap();
    assert!(
        !encoded.contains("super-secret"),
        "no credential value survives export: {encoded}"
    );
    assert!(
        !encoded.contains("maybe-a-secret"),
        "no unlisted value survives export: {encoded}"
    );
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
    let environment = vec![(
        "VOISU_GROQ_API_KEY".to_owned(),
        "sk-live-hostile-123".to_owned(),
    )];
    let export = export_record(record, environment);
    let encoded = serde_json::to_string(&export).unwrap();
    assert!(
        !encoded.contains("sk-live-hostile-123"),
        "a known secret value must not survive anywhere in an export: {encoded}"
    );
    assert!(
        encoded.contains(REDACTED),
        "the secret is masked, not silently dropped"
    );
}

#[test]
fn export_scrubs_and_bounds_intent_reconstruction_evidence() {
    let secret = "intent-secret";
    let mut record = DiagnosticRecord::new("intent-export".to_owned(), 9);
    record.intent_reconstruction = Some(IntentReconstructionDiagnostic {
        model: "m".repeat(MAX_MODEL_ID_UTF8_BYTES + 20),
        eligibility: IntentReconstructionEligibility::MaterialDisagreement,
        outcome: IntentReconstructionOutcome::Rejected,
        elapsed_ms: 42,
        candidate: Some(format!("before {secret} after")),
    });

    let export = export_record(
        record,
        [("VOISU_GROQ_API_KEY".to_owned(), secret.to_owned())],
    );
    let intent = export.record.intent_reconstruction.unwrap();
    assert_eq!(intent.model.len(), MAX_MODEL_ID_UTF8_BYTES);
    assert!(!intent.candidate.as_deref().unwrap().contains(secret));
    assert!(intent.candidate.as_deref().unwrap().contains(REDACTED));
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
    assert!(
        !encoded.contains("hunter2"),
        "URL userinfo must be stripped: {encoded}"
    );
    assert!(
        !encoded.contains("token=abc123"),
        "URL query secret must be stripped: {encoded}"
    );
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
fn url_scrubbing_handles_nested_json_uppercase_and_websocket_schemes() {
    use voisu_core::scrub_embedded_urls;
    // Finding 5: a non-URL "http" substring earlier in the text must NOT stop
    // the scan and mask a later signed URL.
    let nested = scrub_embedded_urls(
        r#"{"httpStatus":500,"url":"https://user:hunter2@h.test/listen?token=abc"}"#,
    );
    assert!(
        !nested.contains("hunter2"),
        "later URL must still be scrubbed: {nested}"
    );
    assert!(!nested.contains("token=abc"), "{nested}");
    assert!(
        nested.contains("https://h.test/listen"),
        "host/path preserved: {nested}"
    );
    // Uppercase scheme is caught.
    assert!(!scrub_embedded_urls("HTTPS://user:pw@h.test/x").contains("pw"));
    // Websocket schemes (Deepgram streaming) are caught.
    let ws = scrub_embedded_urls("connect wss://user:pw@stream.test/v1?token=xyz then go");
    assert!(!ws.contains("pw") && !ws.contains("token=xyz"), "{ws}");
    assert!(ws.contains("wss://stream.test/v1"), "{ws}");
}

#[test]
fn export_scrubs_secret_values_from_the_delivery_fallback_reason() {
    // Finding 6: delivery_fallback_reason is a free-form exported string (a
    // clipboard/xkbcommon fallback message) and must be scrubbed like the rest.
    let mut record = record_at(1, unix_millis_now());
    record.delivery_fallback_reason =
        Some("clipboard fallback: key sk-live-hostile-123 rejected".to_owned());
    let environment = vec![(
        "VOISU_GROQ_API_KEY".to_owned(),
        "sk-live-hostile-123".to_owned(),
    )];
    let export = export_record(record, environment);
    let encoded = serde_json::to_string(&export).unwrap();
    assert!(
        !encoded.contains("sk-live-hostile-123"),
        "a secret in delivery_fallback_reason must be scrubbed: {encoded}"
    );
    assert!(encoded.contains(REDACTED));
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
    let environment = vec![(
        "VOISU_DEEPGRAM_API_KEY".to_owned(),
        "sk-live-hostile-123".to_owned(),
    )];
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
    assert!(
        !encoded.contains("hunter2"),
        "URL userinfo must be stripped: {encoded}"
    );
    assert!(
        !encoded.contains("token=leak"),
        "URL query must be stripped: {encoded}"
    );
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
        record.stages = vec![
            LifecycleStage::CaptureStarted,
            LifecycleStage::DeliveryCompleted,
        ];
        record.set_final_transcript(format!("transcript {id}"));
        store.record(record).unwrap();
    }

    let history = store.history().unwrap();
    assert_eq!(history.len(), 2, "retention bounds the stored history");
    assert_eq!(history[0].correlation_id, "corr-3", "newest first");
    assert!(store.find("corr-3").unwrap().is_some());
    assert!(
        store.find("corr-1").unwrap().is_none(),
        "pruned record is gone"
    );
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
    let audio = store
        .store_debug_audio("corr-audio", &[1, 2, 3, 4])
        .unwrap();
    assert!(
        audio
            .file_name
            .contains(&format!("exp{}", audio.expires_at_unix_ms)),
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
    assert!(
        history[0].debug_audio.is_none(),
        "expired audio is detached from the record"
    );
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
    assert!(
        live_path.exists(),
        "a referenced, unexpired capture survives"
    );
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

    assert!(
        victim.exists(),
        "cleanup must never delete outside the audio directory"
    );
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
    assert_eq!(
        history.len(),
        100,
        "no record is lost to a concurrent stale rewrite"
    );
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
    fn validate(
        &mut self,
        sources: Vec<SourceTranscript>,
    ) -> BoundaryFuture<'_, TranscriptDecision> {
        Box::pin(async move {
            let text = sources
                .first()
                .map(|source| source.text.clone())
                .unwrap_or_default();
            Ok(TranscriptDecision {
                transcript: Transcript(text),
                selection: TranscriptSelection::NearIdenticalGroq,
                validation_reason: "fixture replay".to_owned(),
                fallback_reason: None,
                reconciliation_requested: false,
                recovery_attempted: false,
                source_selection_diagnostic: voisu_core::SourceSelectionDiagnostic {
                    sources: Vec::new(),
                    selected_provider: None,
                    confidence: None,
                },
                intent_reconstruction: None,
                confidence_arbitration: None,
            })
        })
    }
}

struct DelayedPrepareValidator;

impl TranscriptValidator for DelayedPrepareValidator {
    fn validate(
        &mut self,
        _sources: Vec<SourceTranscript>,
    ) -> BoundaryFuture<'_, TranscriptDecision> {
        Box::pin(async { panic!("replay must use prepare()") })
    }

    fn prepare(
        &mut self,
        sources: Vec<SourceTranscript>,
    ) -> BoundaryFuture<'_, PreparedTranscriptDecision> {
        Box::pin(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            let text = sources
                .first()
                .map(|source| source.text.clone())
                .unwrap_or_default();
            Ok(PreparedTranscriptDecision::Ready(TranscriptDecision {
                transcript: Transcript(text),
                selection: TranscriptSelection::NearIdenticalGroq,
                validation_reason: "delayed fixture preparation".to_owned(),
                fallback_reason: None,
                reconciliation_requested: false,
                recovery_attempted: false,
                source_selection_diagnostic: voisu_core::SourceSelectionDiagnostic {
                    sources: Vec::new(),
                    selected_provider: None,
                    confidence: None,
                },
                intent_reconstruction: Some(IntentReconstructionEvidence {
                    eligibility: IntentReconstructionEligibility::NearIdenticalHighConfidence,
                    outcome: IntentReconstructionOutcome::Skipped,
                    candidate: None,
                }),
                confidence_arbitration: None,
            }))
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
    let outcome = replay_capture(
        CapturedAudio::new(vec![0_u8; 3_200]),
        coordinator,
        &mut validator,
    )
    .await
    .expect("replay succeeds");
    assert_eq!(
        outcome.source_transcripts.len(),
        2,
        "both providers replayed the fixture"
    );
    assert_eq!(outcome.decision.transcript.0, "replayed dictation");
}

#[tokio::test]
async fn replay_reconstruction_clock_excludes_preparation() {
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
    let mut validator = DelayedPrepareValidator;
    let outcome = replay_capture(
        CapturedAudio::new(vec![0_u8; 3_200]),
        coordinator,
        &mut validator,
    )
    .await
    .expect("replay succeeds");
    assert!(
        outcome.reconstruction_elapsed_ms < 100,
        "preparation latency must not be attributed to reconstruction: {} ms",
        outcome.reconstruction_elapsed_ms
    );
}

#[test]
fn sanitize_url_fails_closed_on_malformed_and_unrecognized_inputs() {
    // Adversarial: scheme-less URLs still carry credentials — the naive parse
    // would pass "user:pass@host/path" straight through.
    assert_eq!(
        voisu_core::sanitize_url("user:hunter2@groq.test/v1"),
        REDACTED
    );
    assert_eq!(voisu_core::sanitize_url("groq.test/v1?key=leak"), REDACTED);
    assert_eq!(voisu_core::sanitize_url(""), REDACTED);
    // Unrecognized schemes are not reasoned about — redact entirely.
    assert_eq!(
        voisu_core::sanitize_url("ftp://user:pass@host/file"),
        REDACTED
    );
    assert_eq!(voisu_core::sanitize_url("javascript://alert(1)"), REDACTED);
    // Malformed: empty authority.
    assert_eq!(voisu_core::sanitize_url("http://"), REDACTED);
    assert_eq!(voisu_core::sanitize_url("https://user:pass@"), REDACTED);
    // Well-formed shapes still sanitize rather than redact.
    assert_eq!(
        voisu_core::sanitize_url("https://host.test:8443/v1/audio?k=leak"),
        "https://host.test:8443/v1/audio"
    );
    assert_eq!(
        voisu_core::sanitize_url("HTTPS://host.test"),
        "HTTPS://host.test"
    );
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
        export
            .environment
            .get("VOISU_GROQ_MODEL")
            .map(String::as_str),
        Some("whisper-large-v3"),
    );
}

#[test]
fn a_preplanted_colliding_temp_file_does_not_lose_the_record() {
    let dir = TempDir::new().unwrap();
    let store_dir = dir.path().join("diag");
    // A count bound of one still has a compaction slack floor of eight, so the
    // TENTH record is the first to force the atomic whole-log rewrite — the only
    // path that uses a temp file.
    let policy = RetentionPolicy {
        max_records: 1,
        ..RetentionPolicy::default()
    };
    let store = DiagnosticStore::open(store_dir.clone(), policy).unwrap();
    // Adversarial: crash leftovers after PID reuse occupy the first temp names
    // this store would pick.
    for nonce in 0..3 {
        std::fs::write(
            store_dir.join(format!("history.jsonl.tmp.{}.{nonce}", std::process::id())),
            b"stale",
        )
        .unwrap();
    }
    let mut before_inode = None;
    for id in 1..=10 {
        let correlation = if id == 10 {
            "corr-collide".to_owned()
        } else {
            format!("corr-{id}")
        };
        store
            .record(DiagnosticRecord::new(correlation, id))
            .expect("a temp-name collision must retry, not fail");
        if id == 1 {
            before_inode = Some(history_inode(&store_dir));
        }
    }
    assert_ne!(
        history_inode(&store_dir),
        before_inode.unwrap(),
        "the test must reach the atomic compaction path that creates a temp file"
    );
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
                .is_some_and(|name| name.starts_with("history.jsonl.tmp."))
        })
        .count();
    assert_eq!(leftovers, 0, "stale temp files are purged at startup");
}

#[test]
fn sanitize_url_rejects_malformed_hosts_and_invalid_ports() {
    // Adversarial: hosts containing whitespace or backslashes are not DNS-safe
    // and must redact, not pass through.
    assert_eq!(voisu_core::sanitize_url("http://ho st.test/v1"), REDACTED);
    assert_eq!(
        voisu_core::sanitize_url("http://host\\evil.test/v1"),
        REDACTED
    );
    assert_eq!(voisu_core::sanitize_url("https://host.test\t/v1"), REDACTED);
    assert_eq!(voisu_core::sanitize_url("https://host_test/v1"), REDACTED);
    // Ports must parse as a non-zero u16.
    assert_eq!(voisu_core::sanitize_url("https://host.test:0/v1"), REDACTED);
    assert_eq!(
        voisu_core::sanitize_url("https://host.test:99999/v1"),
        REDACTED
    );
    assert_eq!(
        voisu_core::sanitize_url("https://host.test:+443/v1"),
        REDACTED
    );
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
    assert_eq!(
        voisu_core::sanitize_url("https://[2001:db8::1/v1"),
        REDACTED
    );
    assert_eq!(voisu_core::sanitize_url("https://[bad host]/v1"), REDACTED);
}

#[test]
fn startup_cleanup_survives_all_temp_name_candidates_being_preplanted() {
    let dir = TempDir::new().unwrap();
    let store_dir = dir.path().join("diag");
    let policy = RetentionPolicy {
        max_records: 1,
        ..RetentionPolicy::default()
    };
    let store = DiagnosticStore::open(store_dir.clone(), policy).unwrap();
    store
        .record(DiagnosticRecord::new("corr-before-purge".to_owned(), 1))
        .unwrap();
    // Adversarial: every one of the 32 bounded temp-name candidates is already
    // occupied by crash leftovers. Cleanup must purge them BEFORE any history
    // rewrite, or the rewrite exhausts its retries and cleanup fails.
    for nonce in 0..32 {
        std::fs::write(
            store_dir.join(format!("history.jsonl.tmp.{}.{nonce}", std::process::id())),
            b"stale",
        )
        .unwrap();
    }
    store
        .cleanup_expired()
        .expect("cleanup must purge stale temp files before rewriting history");
    // This record appends after cleanup; the assertion only verifies that
    // purging every candidate did not damage subsequent history operations.
    store
        .record(DiagnosticRecord::new("corr-after-purge".to_owned(), 2))
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
    assert_eq!(
        voisu_core::sanitize_url("https://[2001:db8::1::2]/v1"),
        REDACTED
    );
    // A well-formed literal still sanitizes rather than redacts.
    assert_eq!(
        voisu_core::sanitize_url("https://[2001:db8::1]/v1?k=leak"),
        "https://[2001:db8::1]/v1"
    );
}

// ---------------------------------------------------------------------------
// SW8 / #121 — Smart Writing diagnostic schema (§10)
// ---------------------------------------------------------------------------

fn smart_writing_for_outcome(outcome: SmartWritingOutcome) -> SmartWritingDiagnostic {
    let (mode, eligibility) = match outcome {
        SmartWritingOutcome::Literal
        | SmartWritingOutcome::LiteralCommands
        | SmartWritingOutcome::LiteralFallback => (
            SmartWritingMode::Literal,
            EnglishEligibilityOutcome::Eligible,
        ),
        SmartWritingOutcome::FormattingOnly
        | SmartWritingOutcome::FormattingAndGrammar
        | SmartWritingOutcome::IdentityFallback => {
            (SmartWritingMode::Smart, EnglishEligibilityOutcome::Eligible)
        }
    };
    SmartWritingDiagnostic::new(
        mode,
        eligibility,
        "formatter-contract-v1",
        "validated before text",
        "rendered after text",
        outcome,
    )
}

#[test]
fn smart_writing_outcome_enum_serializes_exactly_the_section_10_set() {
    // Spec §10 closed outcome set — wire names are snake_case and exact.
    let cases = [
        (SmartWritingOutcome::Literal, "literal"),
        (SmartWritingOutcome::LiteralCommands, "literal_commands"),
        (SmartWritingOutcome::LiteralFallback, "literal_fallback"),
        (SmartWritingOutcome::FormattingOnly, "formatting_only"),
        (
            SmartWritingOutcome::FormattingAndGrammar,
            "formatting_and_grammar",
        ),
        (SmartWritingOutcome::IdentityFallback, "identity_fallback"),
    ];
    for (outcome, wire) in cases {
        let encoded = serde_json::to_string(&outcome).unwrap();
        assert_eq!(encoded, format!("\"{wire}\""));
        let decoded: SmartWritingOutcome = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, outcome);
    }
}

#[test]
fn smart_writing_every_outcome_and_major_reason_path_round_trips_on_a_record() {
    // Hermetic path coverage: every outcome plus representative reason codes
    // for mode / eligibility / capability / oversize / HTTP / schema / safety /
    // formatter / cleanup — stored on a DiagnosticRecord and round-tripped.
    let outcomes = [
        SmartWritingOutcome::Literal,
        SmartWritingOutcome::LiteralCommands,
        SmartWritingOutcome::LiteralFallback,
        SmartWritingOutcome::FormattingOnly,
        SmartWritingOutcome::FormattingAndGrammar,
        SmartWritingOutcome::IdentityFallback,
    ];
    let reasons = [
        SmartWritingReasonCode::ModeLiteral,
        SmartWritingReasonCode::ModeSmart,
        SmartWritingReasonCode::EnglishIneligible,
        SmartWritingReasonCode::CapabilityUnavailable,
        SmartWritingReasonCode::InputOversize,
        SmartWritingReasonCode::ResponseOversize,
        SmartWritingReasonCode::HttpTimeout,
        SmartWritingReasonCode::HttpStatus,
        SmartWritingReasonCode::HttpTransport,
        SmartWritingReasonCode::Malformed,
        SmartWritingReasonCode::Schema,
        SmartWritingReasonCode::Stale,
        SmartWritingReasonCode::ProtectedSpan,
        SmartWritingReasonCode::RuleContext,
        SmartWritingReasonCode::Unmappable,
        SmartWritingReasonCode::Overlap,
        SmartWritingReasonCode::FormatterPanic,
        SmartWritingReasonCode::FormatterDeadline,
        SmartWritingReasonCode::SafetyPanic,
        SmartWritingReasonCode::SafetyDeadline,
        SmartWritingReasonCode::ComposePanic,
        SmartWritingReasonCode::ComposeDeadline,
        SmartWritingReasonCode::CleanupOverrun,
        SmartWritingReasonCode::EditAccepted,
        SmartWritingReasonCode::UnknownRule,
    ];

    let dir = TempDir::new().unwrap();
    let store = DiagnosticStore::open(dir.path().to_owned(), RetentionPolicy::default()).unwrap();

    for (index, outcome) in outcomes.iter().enumerate() {
        let mut smart = smart_writing_for_outcome(*outcome);
        smart.reason_codes = vec![reasons[index % reasons.len()]];
        smart.request_began = matches!(
            outcome,
            SmartWritingOutcome::FormattingAndGrammar | SmartWritingOutcome::FormattingOnly
        );
        if smart.request_began {
            smart.set_model_id("openai/gpt-oss-20b");
            smart.http_latency_ms = Some(42);
        }
        smart.formatter_latency_ms = Some(5);
        smart.safety_latency_ms = Some(3);
        smart.total_gate_latency_ms = Some(50);
        smart.credential_prep_latency_ms = Some(12);
        smart.reap_watchdog_crossed = *outcome == SmartWritingOutcome::IdentityFallback
            && smart
                .reason_codes
                .contains(&SmartWritingReasonCode::CleanupOverrun);

        let mut record = DiagnosticRecord::new(format!("sw-outcome-{index}"), index as u64 + 1);
        record.smart_writing = Some(smart.clone());
        store.record(record).unwrap();

        let loaded = store
            .find(&format!("sw-outcome-{index}"))
            .unwrap()
            .expect("record retained");
        let loaded_sw = loaded.smart_writing.expect("smart writing present");
        assert_eq!(loaded_sw.outcome, *outcome);
        assert_eq!(loaded_sw.version, SMART_WRITING_DIAGNOSTIC_VERSION);
        assert_eq!(loaded_sw.reason_codes, smart.reason_codes);
        assert_eq!(loaded_sw.model_id, smart.model_id);
        assert_eq!(loaded_sw.request_began, smart.request_began);
    }

    // Every reason code serializes as snake_case and deserializes back.
    for reason in reasons {
        let wire = serde_json::to_string(&reason).unwrap();
        assert!(
            wire.starts_with('"') && !wire.contains(char::is_uppercase),
            "reason codes are snake_case wire values: {wire}"
        );
        let decoded: SmartWritingReasonCode = serde_json::from_str(&wire).unwrap();
        assert_eq!(decoded, reason);
    }
}

#[test]
fn smart_writing_text_and_model_clamps_preserve_full_fingerprints() {
    let long_text = "α".repeat(MAX_SMART_WRITING_DIAGNOSTIC_TEXT_UTF8_BYTES / 2 + 200);
    assert!(
        long_text.len() > MAX_SMART_WRITING_DIAGNOSTIC_TEXT_UTF8_BYTES,
        "fixture must exceed the diagnostic text clamp"
    );
    let smart = SmartWritingDiagnostic::new(
        SmartWritingMode::Smart,
        EnglishEligibilityOutcome::Eligible,
        "contract",
        &long_text,
        &long_text,
        SmartWritingOutcome::FormattingOnly,
    );
    assert!(smart.validated_before.len() <= MAX_SMART_WRITING_DIAGNOSTIC_TEXT_UTF8_BYTES);
    assert!(smart.rendered_after.len() <= MAX_SMART_WRITING_DIAGNOSTIC_TEXT_UTF8_BYTES);
    assert!(
        smart
            .validated_before
            .is_char_boundary(smart.validated_before.len())
    );
    // Fingerprints cover the unclamped source so equality remains inspectable.
    assert_eq!(
        smart.validated_before_sha256,
        text_sha256_fingerprint(&long_text)
    );
    assert_eq!(
        smart.rendered_after_sha256,
        text_sha256_fingerprint(&long_text)
    );
    assert_ne!(
        smart.validated_before_sha256,
        text_sha256_fingerprint(&smart.validated_before),
        "fingerprint must be of the full source, not the clamped tail-drop"
    );

    let mut smart = smart;
    let long_model = "m".repeat(MAX_MODEL_ID_UTF8_BYTES + 40);
    smart.set_model_id(long_model);
    assert_eq!(
        smart.model_id.as_ref().map(String::len),
        Some(MAX_MODEL_ID_UTF8_BYTES)
    );

    let long_error = "e".repeat(MAX_SMART_WRITING_FREE_TEXT_UTF8_BYTES + 50);
    smart.set_free_form_error(long_error);
    assert_eq!(
        smart.free_form_error.as_ref().map(String::len),
        Some(MAX_SMART_WRITING_FREE_TEXT_UTF8_BYTES)
    );
}

#[test]
fn smart_writing_edit_evidence_is_bounded_to_32_entries_and_field_limits() {
    let long_field = "x".repeat(MAX_SMART_WRITING_EDIT_FIELD_UTF8_BYTES + 80);
    let long_id = "i".repeat(MAX_SMART_WRITING_FREE_TEXT_UTF8_BYTES + 20);
    let edit = SmartWritingEditEvidence::new(
        long_id.clone(),
        long_id,
        0,
        5,
        &long_field,
        &long_field,
        SmartWritingReasonCode::EditAccepted,
    );
    assert!(edit.edit_id.len() <= MAX_SMART_WRITING_FREE_TEXT_UTF8_BYTES);
    assert!(edit.rule_id.len() <= MAX_SMART_WRITING_FREE_TEXT_UTF8_BYTES);
    assert!(edit.before.len() <= MAX_SMART_WRITING_EDIT_FIELD_UTF8_BYTES);
    assert!(edit.after.len() <= MAX_SMART_WRITING_EDIT_FIELD_UTF8_BYTES);

    let mut smart = smart_writing_for_outcome(SmartWritingOutcome::FormattingAndGrammar);
    let many: Vec<_> = (0..(MAX_SMART_WRITING_DIAGNOSTIC_EDITS + 10))
        .map(|i| {
            SmartWritingEditEvidence::new(
                format!("e{i}"),
                "G_DIDNT_APOSTROPHE",
                i as u64,
                i as u64 + 1,
                "didnt",
                "didn't",
                if i % 2 == 0 {
                    SmartWritingReasonCode::EditAccepted
                } else {
                    SmartWritingReasonCode::ProtectedSpan
                },
            )
        })
        .collect();
    smart.set_edits(many);
    assert_eq!(smart.edits.len(), MAX_SMART_WRITING_DIAGNOSTIC_EDITS);
    assert_eq!(smart.edits[0].edit_id, "e0");
    assert_eq!(
        smart.edits.last().unwrap().edit_id,
        format!("e{}", MAX_SMART_WRITING_DIAGNOSTIC_EDITS - 1)
    );
}

#[test]
fn smart_writing_export_scrubs_secrets_from_all_free_form_fields() {
    let secret = "sk-sw8-hostile-secret";
    let mut smart = SmartWritingDiagnostic::new(
        SmartWritingMode::Smart,
        EnglishEligibilityOutcome::Eligible,
        format!("contract-with-{secret}"),
        format!("validated contains {secret}"),
        format!("rendered contains {secret}"),
        SmartWritingOutcome::FormattingAndGrammar,
    );
    smart.set_model_id(format!("model-{secret}"));
    smart.set_free_form_error(format!("transport failed for {secret}"));
    smart.set_edits(vec![SmartWritingEditEvidence::new(
        format!("id-{secret}"),
        format!("rule-{secret}"),
        0,
        4,
        format!("bef-{secret}"),
        format!("aft-{secret}"),
        SmartWritingReasonCode::EditAccepted,
    )]);

    let mut record = record_at(1, unix_millis_now());
    record.smart_writing = Some(smart);
    let export = export_record(
        record,
        vec![("VOISU_GROQ_API_KEY".to_owned(), secret.to_owned())],
    );
    let encoded = serde_json::to_string(&export).unwrap();
    assert!(
        !encoded.contains(secret),
        "no Smart Writing free-form field may leak a known secret: {encoded}"
    );
    assert!(encoded.contains(REDACTED));

    let sw = export.record.smart_writing.as_ref().unwrap();
    // Fingerprints are digests, not secret material, and must survive export.
    assert!(sw.validated_before_sha256.starts_with("sha256:"));
    assert_eq!(sw.validated_before_sha256.len(), "sha256:".len() + 64);
}

#[test]
fn smart_writing_export_structurally_scrubs_url_secrets_in_free_form_error() {
    let mut smart = smart_writing_for_outcome(SmartWritingOutcome::FormattingOnly);
    smart.set_free_form_error(
        "grammar failed at https://user:pw@api.example/v1?token=xyz please retry",
    );
    let mut record = record_at(2, unix_millis_now());
    record.smart_writing = Some(smart);
    let export = export_record(record, Vec::<(String, String)>::new());
    let encoded = serde_json::to_string(&export).unwrap();
    assert!(
        !encoded.contains("user:pw"),
        "userinfo must be stripped: {encoded}"
    );
    assert!(
        !encoded.contains("token=xyz"),
        "query secrets must be stripped: {encoded}"
    );
}

#[test]
fn pre_smart_writing_history_records_deserialize_without_smart_writing() {
    // Backward compatibility: a history line written before SW8 has no
    // smart_writing field and must load as None. JSONL is one record per line.
    // Timestamp must be recent so the default 7-day age bound does not prune it.
    let now = unix_millis_now();
    let pre_sw = format!(
        concat!(
            r#"{{"correlation_id":"pre-sw-1","recording_id":7,"recorded_at_unix_ms":{now},"#,
            r#""stages":[],"streamed_chunk_count":0,"source_transcripts":[],"#,
            r#""reconciliation_requested":false,"recovery_attempted":false,"#,
            r#""delivery_count":1,"provider_timings_ms":[]}}"#
        ),
        now = now
    );
    let record: DiagnosticRecord = serde_json::from_str(&pre_sw).unwrap();
    assert!(
        record.smart_writing.is_none(),
        "pre-SW records default smart_writing to None"
    );
    assert_eq!(record.correlation_id, "pre-sw-1");
    assert_eq!(record.recording_id, 7);

    // A store that already holds a pre-SW line still accepts and returns SW records.
    let dir = TempDir::new().unwrap();
    let store = DiagnosticStore::open(dir.path().to_owned(), RetentionPolicy::default()).unwrap();
    let history_path = dir.path().join("history.jsonl");
    std::fs::write(&history_path, format!("{pre_sw}\n")).unwrap();
    let history = store.history().unwrap();
    assert_eq!(history.len(), 1);
    assert!(history[0].smart_writing.is_none());

    let mut with_sw = DiagnosticRecord::new("post-sw".to_owned(), 8);
    with_sw.recorded_at_unix_ms = now;
    with_sw.smart_writing = Some(smart_writing_for_outcome(SmartWritingOutcome::Literal));
    store.record(with_sw).unwrap();
    let mixed = store.history().unwrap();
    assert_eq!(mixed.len(), 2);
    let post = mixed
        .iter()
        .find(|r| r.correlation_id == "post-sw")
        .unwrap();
    assert!(post.smart_writing.is_some());
    let pre = mixed
        .iter()
        .find(|r| r.correlation_id == "pre-sw-1")
        .unwrap();
    assert!(pre.smart_writing.is_none());
}

#[test]
fn smart_writing_diagnostic_omits_audio_and_never_requires_debug_capture() {
    // Audio remains opt-in via debug_audio only; Smart Writing does not attach it.
    let mut record = DiagnosticRecord::new("sw-no-audio".to_owned(), 1);
    record.smart_writing = Some(smart_writing_for_outcome(SmartWritingOutcome::Literal));
    assert!(record.debug_audio.is_none());
    let encoded = serde_json::to_string(&record).unwrap();
    assert!(!encoded.contains("debug_audio"));
    assert!(encoded.contains("smart_writing"));
    assert!(encoded.contains("\"literal\""));
}

#[test]
fn smart_writing_persistence_normalizes_publicly_mutated_oversized_fields() {
    // §10 budgets are construction-time clamps, but fields stay publicly
    // mutable. DiagnosticStore::record must re-normalize so oversized text /
    // model / error / edit fields and >32 edits cannot persist.
    let full_source = "Ω".repeat(MAX_SMART_WRITING_DIAGNOSTIC_TEXT_UTF8_BYTES / 2 + 300);
    assert!(full_source.len() > MAX_SMART_WRITING_DIAGNOSTIC_TEXT_UTF8_BYTES);
    let source_fp = text_sha256_fingerprint(&full_source);

    let mut smart = SmartWritingDiagnostic::new(
        SmartWritingMode::Smart,
        EnglishEligibilityOutcome::Eligible,
        "contract",
        "short-before",
        "short-after",
        SmartWritingOutcome::FormattingAndGrammar,
    );
    // Fingerprints of the unclamped source must survive normalize-on-record.
    smart.validated_before_sha256 = source_fp.clone();
    smart.rendered_after_sha256 = source_fp.clone();

    // Bypass constructors/setters: write oversized values directly.
    smart.validated_before = full_source.clone();
    smart.rendered_after = full_source.clone();
    smart.formatter_contract_id = "c".repeat(MAX_SMART_WRITING_FREE_TEXT_UTF8_BYTES + 40);
    smart.model_id = Some("m".repeat(MAX_MODEL_ID_UTF8_BYTES + 60));
    smart.free_form_error = Some("e".repeat(MAX_SMART_WRITING_FREE_TEXT_UTF8_BYTES + 70));
    smart.edits = (0..(MAX_SMART_WRITING_DIAGNOSTIC_EDITS + 12))
        .map(|i| SmartWritingEditEvidence {
            edit_id: format!(
                "e{i}-{}",
                "i".repeat(MAX_SMART_WRITING_FREE_TEXT_UTF8_BYTES)
            ),
            rule_id: "r".repeat(MAX_SMART_WRITING_FREE_TEXT_UTF8_BYTES + 30),
            start_utf8: i as u64,
            end_utf8: i as u64 + 1,
            before: "b".repeat(MAX_SMART_WRITING_EDIT_FIELD_UTF8_BYTES + 90),
            after: "a".repeat(MAX_SMART_WRITING_EDIT_FIELD_UTF8_BYTES + 90),
            code: SmartWritingReasonCode::EditAccepted,
        })
        .collect();
    assert!(smart.validated_before.len() > MAX_SMART_WRITING_DIAGNOSTIC_TEXT_UTF8_BYTES);
    assert!(smart.edits.len() > MAX_SMART_WRITING_DIAGNOSTIC_EDITS);

    let dir = TempDir::new().unwrap();
    let store = DiagnosticStore::open(dir.path().to_owned(), RetentionPolicy::default()).unwrap();
    let mut record = DiagnosticRecord::new("sw-oversized-persist".to_owned(), 1);
    record.smart_writing = Some(smart);
    store.record(record).unwrap();

    let loaded = store
        .find("sw-oversized-persist")
        .unwrap()
        .expect("record retained");
    let sw = loaded.smart_writing.expect("smart writing present");

    assert!(sw.validated_before.len() <= MAX_SMART_WRITING_DIAGNOSTIC_TEXT_UTF8_BYTES);
    assert!(sw.rendered_after.len() <= MAX_SMART_WRITING_DIAGNOSTIC_TEXT_UTF8_BYTES);
    assert!(sw.formatter_contract_id.len() <= MAX_SMART_WRITING_FREE_TEXT_UTF8_BYTES);
    assert!(
        sw.model_id
            .as_ref()
            .is_some_and(|m| m.len() <= MAX_MODEL_ID_UTF8_BYTES)
    );
    assert!(
        sw.free_form_error
            .as_ref()
            .is_some_and(|e| e.len() <= MAX_SMART_WRITING_FREE_TEXT_UTF8_BYTES)
    );
    assert_eq!(sw.edits.len(), MAX_SMART_WRITING_DIAGNOSTIC_EDITS);
    for edit in &sw.edits {
        assert!(edit.edit_id.len() <= MAX_SMART_WRITING_FREE_TEXT_UTF8_BYTES);
        assert!(edit.rule_id.len() <= MAX_SMART_WRITING_FREE_TEXT_UTF8_BYTES);
        assert!(edit.before.len() <= MAX_SMART_WRITING_EDIT_FIELD_UTF8_BYTES);
        assert!(edit.after.len() <= MAX_SMART_WRITING_EDIT_FIELD_UTF8_BYTES);
    }
    // Fingerprints stay of the unclamped source, not the clamped on-disk text.
    assert_eq!(sw.validated_before_sha256, source_fp);
    assert_eq!(sw.rendered_after_sha256, source_fp);
    assert_ne!(
        sw.validated_before_sha256,
        text_sha256_fingerprint(&sw.validated_before)
    );
}

#[test]
fn smart_writing_export_reclamp_after_redaction_expansion_past_bound() {
    // REDACTED is longer than a short secret. Scrubbing a field already at its
    // §10 budget can expand past the bound unless export re-clamps afterward.
    const SECRET: &str = "Z"; // 1 UTF-8 byte
    assert!(REDACTED.len() > SECRET.len());

    // free_form_error budget is 128: fill to max ending with the secret so a
    // naive replace(SECRET, REDACTED) would exceed the budget by REDACTED.len()-1.
    let prefix_len = MAX_SMART_WRITING_FREE_TEXT_UTF8_BYTES - SECRET.len();
    let at_budget = format!("{}{}", "x".repeat(prefix_len), SECRET);
    assert_eq!(at_budget.len(), MAX_SMART_WRITING_FREE_TEXT_UTF8_BYTES);
    let expanded = at_budget.replace(SECRET, REDACTED);
    assert!(
        expanded.len() > MAX_SMART_WRITING_FREE_TEXT_UTF8_BYTES,
        "fixture must expand past the free-text budget without re-clamp"
    );

    let model_prefix = MAX_MODEL_ID_UTF8_BYTES - SECRET.len();
    let model_at_budget = format!("{}{}", "m".repeat(model_prefix), SECRET);
    assert_eq!(model_at_budget.len(), MAX_MODEL_ID_UTF8_BYTES);

    let text_prefix = MAX_SMART_WRITING_DIAGNOSTIC_TEXT_UTF8_BYTES - SECRET.len();
    let text_at_budget = format!("{}{}", "t".repeat(text_prefix), SECRET);
    assert_eq!(
        text_at_budget.len(),
        MAX_SMART_WRITING_DIAGNOSTIC_TEXT_UTF8_BYTES
    );
    let text_fp = text_sha256_fingerprint(&text_at_budget);

    let edit_prefix = MAX_SMART_WRITING_EDIT_FIELD_UTF8_BYTES - SECRET.len();
    let edit_at_budget = format!("{}{}", "b".repeat(edit_prefix), SECRET);

    let mut smart = SmartWritingDiagnostic::new(
        SmartWritingMode::Smart,
        EnglishEligibilityOutcome::Eligible,
        at_budget.clone(),
        &text_at_budget,
        &text_at_budget,
        SmartWritingOutcome::FormattingAndGrammar,
    );
    // new() clamps texts and fingerprints the unclamped args; keep fingerprints
    // of the at-budget source (already within the clamp so equal to stored text).
    assert_eq!(smart.validated_before_sha256, text_fp);
    smart.model_id = Some(model_at_budget);
    smart.free_form_error = Some(at_budget.clone());
    smart.edits = vec![SmartWritingEditEvidence {
        edit_id: at_budget.clone(),
        rule_id: at_budget,
        start_utf8: 0,
        end_utf8: 1,
        before: edit_at_budget.clone(),
        after: edit_at_budget,
        code: SmartWritingReasonCode::EditAccepted,
    }];

    let mut record = record_at(1, unix_millis_now());
    record.smart_writing = Some(smart);
    let export = export_record(
        record,
        vec![("VOISU_GROQ_API_KEY".to_owned(), SECRET.to_owned())],
    );
    let encoded = serde_json::to_string(&export).unwrap();
    assert!(
        !encoded.contains(SECRET),
        "secret must be scrubbed from export: {encoded}"
    );

    let sw = export.record.smart_writing.as_ref().unwrap();
    assert!(sw.formatter_contract_id.len() <= MAX_SMART_WRITING_FREE_TEXT_UTF8_BYTES);
    assert!(sw.validated_before.len() <= MAX_SMART_WRITING_DIAGNOSTIC_TEXT_UTF8_BYTES);
    assert!(sw.rendered_after.len() <= MAX_SMART_WRITING_DIAGNOSTIC_TEXT_UTF8_BYTES);
    assert!(
        sw.model_id
            .as_ref()
            .is_some_and(|m| m.len() <= MAX_MODEL_ID_UTF8_BYTES)
    );
    assert!(
        sw.free_form_error
            .as_ref()
            .is_some_and(|e| e.len() <= MAX_SMART_WRITING_FREE_TEXT_UTF8_BYTES)
    );
    assert!(sw.edits[0].edit_id.len() <= MAX_SMART_WRITING_FREE_TEXT_UTF8_BYTES);
    assert!(sw.edits[0].rule_id.len() <= MAX_SMART_WRITING_FREE_TEXT_UTF8_BYTES);
    assert!(sw.edits[0].before.len() <= MAX_SMART_WRITING_EDIT_FIELD_UTF8_BYTES);
    assert!(sw.edits[0].after.len() <= MAX_SMART_WRITING_EDIT_FIELD_UTF8_BYTES);
    // Scrub expanded (REDACTED is longer than SECRET) then re-clamped to exact
    // budgets. At a max-full field the clamp cuts inside REDACTED, so only a
    // leading prefix of the mask remains — still within budget, secret gone.
    let free = sw
        .free_form_error
        .as_ref()
        .expect("free_form_error present");
    assert_eq!(free.len(), MAX_SMART_WRITING_FREE_TEXT_UTF8_BYTES);
    assert!(
        free.contains('<'),
        "re-clamp after scrub should retain a redaction prefix within budget: {free:?}"
    );
    // Fingerprints are digests, not secret material, and must survive export.
    assert_eq!(sw.validated_before_sha256, text_fp);
    assert_eq!(sw.rendered_after_sha256, text_fp);
}

#[test]
fn smart_writing_export_normalizes_publicly_mutated_oversized_fields() {
    // Export is a second boundary: even without scrub hits, oversized public
    // mutation must not leave export payloads past §10 budgets.
    let mut smart = smart_writing_for_outcome(SmartWritingOutcome::FormattingOnly);
    smart.validated_before = "v".repeat(MAX_SMART_WRITING_DIAGNOSTIC_TEXT_UTF8_BYTES + 500);
    smart.rendered_after = "r".repeat(MAX_SMART_WRITING_DIAGNOSTIC_TEXT_UTF8_BYTES + 500);
    smart.model_id = Some("m".repeat(MAX_MODEL_ID_UTF8_BYTES + 20));
    smart.free_form_error = Some("e".repeat(MAX_SMART_WRITING_FREE_TEXT_UTF8_BYTES + 20));
    smart.edits = (0..(MAX_SMART_WRITING_DIAGNOSTIC_EDITS + 5))
        .map(|i| SmartWritingEditEvidence {
            edit_id: format!("id-{i}"),
            rule_id: "rule".to_owned(),
            start_utf8: 0,
            end_utf8: 1,
            before: "b".repeat(MAX_SMART_WRITING_EDIT_FIELD_UTF8_BYTES + 10),
            after: "a".repeat(MAX_SMART_WRITING_EDIT_FIELD_UTF8_BYTES + 10),
            code: SmartWritingReasonCode::EditAccepted,
        })
        .collect();
    let fingerprint = smart.validated_before_sha256.clone();

    let mut record = record_at(3, unix_millis_now());
    record.smart_writing = Some(smart);
    let export = export_record(record, Vec::<(String, String)>::new());
    let sw = export.record.smart_writing.as_ref().unwrap();
    assert!(sw.validated_before.len() <= MAX_SMART_WRITING_DIAGNOSTIC_TEXT_UTF8_BYTES);
    assert!(sw.rendered_after.len() <= MAX_SMART_WRITING_DIAGNOSTIC_TEXT_UTF8_BYTES);
    assert_eq!(
        sw.model_id.as_ref().map(String::len),
        Some(MAX_MODEL_ID_UTF8_BYTES)
    );
    assert_eq!(
        sw.free_form_error.as_ref().map(String::len),
        Some(MAX_SMART_WRITING_FREE_TEXT_UTF8_BYTES)
    );
    assert_eq!(sw.edits.len(), MAX_SMART_WRITING_DIAGNOSTIC_EDITS);
    assert_eq!(sw.validated_before_sha256, fingerprint);
}

#[test]
fn clamp_utf8_bytes_respects_scalar_boundaries() {
    // Multi-byte scalar at the cut must not produce invalid UTF-8.
    let text = "ab\u{1F600}cd"; // grinning face is 4 bytes
    let clamped = clamp_utf8_bytes(text, 3);
    assert_eq!(clamped, "ab");
    assert!(std::str::from_utf8(clamped.as_bytes()).is_ok());
}

#[test]
fn text_sha256_fingerprint_is_stable_and_prefixed() {
    let fp = text_sha256_fingerprint("hello");
    assert!(fp.starts_with("sha256:"));
    assert_eq!(fp.len(), TEXT_SHA256_FINGERPRINT_LEN);
    assert!(is_text_sha256_fingerprint(&fp));
    assert_eq!(fp, text_sha256_fingerprint("hello"));
    assert_ne!(fp, text_sha256_fingerprint("Hello"));
    // Known SHA-256 of "hello".
    assert_eq!(
        fp,
        "sha256:2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
    );
    // Form validator rejects free-form, wrong case, wrong length, and uppercase hex.
    assert!(!is_text_sha256_fingerprint("sk-not-a-fingerprint"));
    assert!(!is_text_sha256_fingerprint(&format!(
        "sha256:{}",
        "A".repeat(64)
    )));
    assert!(!is_text_sha256_fingerprint(&format!(
        "sha256:{}",
        "a".repeat(63)
    )));
    assert!(!is_text_sha256_fingerprint(&format!(
        "SHA256:{}",
        "a".repeat(64)
    )));
    assert!(!is_text_sha256_fingerprint(&format!(
        "sha256:{}",
        "g".repeat(64)
    )));
}

#[test]
fn smart_writing_persistence_rejects_invalid_fingerprints() {
    // Fingerprint fields are publicly mutable strings. Persist must reject any
    // value that is not exact `sha256:` + 64 lowercase hex (including secrets
    // and oversized free-form) and regenerate from the clamped text.
    let secret = "sk-sw8-fingerprint-smuggle-secret";
    let mut smart = SmartWritingDiagnostic::new(
        SmartWritingMode::Smart,
        EnglishEligibilityOutcome::Eligible,
        "contract",
        "validated-source-text",
        "rendered-source-text",
        SmartWritingOutcome::FormattingOnly,
    );
    let valid_before = smart.validated_before_sha256.clone();
    let valid_after = smart.rendered_after_sha256.clone();
    assert!(is_text_sha256_fingerprint(&valid_before));
    assert!(is_text_sha256_fingerprint(&valid_after));

    // Valid form survives normalize-on-record (unclamped digests stay intact).
    smart.validated_before_sha256 = valid_before.clone();
    smart.rendered_after_sha256 = valid_after.clone();

    let dir = TempDir::new().unwrap();
    let store = DiagnosticStore::open(dir.path().to_owned(), RetentionPolicy::default()).unwrap();
    let mut record = DiagnosticRecord::new("sw-fp-valid".to_owned(), 1);
    record.smart_writing = Some(smart.clone());
    store.record(record).unwrap();
    let loaded = store.find("sw-fp-valid").unwrap().unwrap();
    let sw = loaded.smart_writing.as_ref().unwrap();
    assert_eq!(sw.validated_before_sha256, valid_before);
    assert_eq!(sw.rendered_after_sha256, valid_after);

    // Secret-bearing free-form, wrong case, and oversized garbage are rejected.
    smart.validated_before_sha256 = secret.to_owned();
    smart.rendered_after_sha256 = format!("sha256:{}", "A".repeat(64)); // uppercase hex
    let mut record = DiagnosticRecord::new("sw-fp-invalid".to_owned(), 2);
    record.smart_writing = Some(smart.clone());
    store.record(record).unwrap();
    let loaded = store.find("sw-fp-invalid").unwrap().unwrap();
    let sw = loaded.smart_writing.as_ref().unwrap();
    assert_eq!(
        sw.validated_before_sha256,
        text_sha256_fingerprint(&sw.validated_before)
    );
    assert_eq!(
        sw.rendered_after_sha256,
        text_sha256_fingerprint(&sw.rendered_after)
    );
    assert!(is_text_sha256_fingerprint(&sw.validated_before_sha256));
    assert!(is_text_sha256_fingerprint(&sw.rendered_after_sha256));
    let on_disk = std::fs::read_to_string(dir.path().join("history.jsonl")).unwrap();
    assert!(
        !on_disk.contains(secret),
        "secret must not persist in a fingerprint field: {on_disk}"
    );

    // Oversized free-form fingerprint cannot persist either.
    smart.validated_before_sha256 = format!("not-a-fp-{}", "x".repeat(4_000));
    smart.rendered_after_sha256 = format!("still-not-{}", "y".repeat(4_000));
    let mut record = DiagnosticRecord::new("sw-fp-oversized".to_owned(), 3);
    record.smart_writing = Some(smart);
    store.record(record).unwrap();
    let loaded = store.find("sw-fp-oversized").unwrap().unwrap();
    let sw = loaded.smart_writing.as_ref().unwrap();
    assert_eq!(
        sw.validated_before_sha256.len(),
        TEXT_SHA256_FINGERPRINT_LEN
    );
    assert_eq!(sw.rendered_after_sha256.len(), TEXT_SHA256_FINGERPRINT_LEN);
    assert!(is_text_sha256_fingerprint(&sw.validated_before_sha256));
    assert!(is_text_sha256_fingerprint(&sw.rendered_after_sha256));
}

#[test]
fn smart_writing_export_rejects_invalid_and_secret_bearing_fingerprints() {
    // Export is a second boundary: invalid fingerprints are regenerated, so a
    // secret smuggled into a fingerprint field cannot appear in the export.
    let secret = "sk-export-fingerprint-smuggle";
    let mut smart = SmartWritingDiagnostic::new(
        SmartWritingMode::Smart,
        EnglishEligibilityOutcome::Eligible,
        "contract",
        "before-text",
        "after-text",
        SmartWritingOutcome::FormattingOnly,
    );
    let valid_fp = smart.validated_before_sha256.clone();
    smart.validated_before_sha256 = secret.to_owned();
    smart.rendered_after_sha256 = format!("SHA256:{}", "a".repeat(64));

    let mut record = record_at(9, unix_millis_now());
    record.smart_writing = Some(smart);
    let export = export_record(
        record,
        vec![("VOISU_GROQ_API_KEY".to_owned(), secret.to_owned())],
    );
    let encoded = serde_json::to_string(&export).unwrap();
    assert!(
        !encoded.contains(secret),
        "export must not leak a secret via fingerprint fields: {encoded}"
    );

    let sw = export.record.smart_writing.as_ref().unwrap();
    assert!(is_text_sha256_fingerprint(&sw.validated_before_sha256));
    assert!(is_text_sha256_fingerprint(&sw.rendered_after_sha256));
    assert_eq!(
        sw.validated_before_sha256,
        text_sha256_fingerprint(&sw.validated_before)
    );
    assert_eq!(
        sw.rendered_after_sha256,
        text_sha256_fingerprint(&sw.rendered_after)
    );
    // Regenerated from clamped text, not the original constructor fingerprint
    // of the same short text — equal in this case because text was never oversized.
    assert_eq!(sw.validated_before_sha256, valid_fp);
}

#[test]
fn smart_writing_new_record_defaults_smart_writing_to_none() {
    let record = DiagnosticRecord::new("fresh".to_owned(), 1);
    assert!(record.smart_writing.is_none());
    // Serializing a fresh record must not force a smart_writing key.
    let encoded = serde_json::to_string(&record).unwrap();
    assert!(
        !encoded.contains("smart_writing"),
        "absent smart_writing is skipped on serialize: {encoded}"
    );
}

#[test]
fn confidence_arbitration_diagnostic_is_additive_on_the_record_wire() {
    // A pre-B4 history line (no confidence_arbitration field) must still
    // deserialize, and a B4 record round-trips its counts and closed reasons.
    let pre_b4 = r#"{
        "correlation_id": "rec-1-7-1",
        "recording_id": 7,
        "recorded_at_unix_ms": 1
    }"#;
    let decoded: DiagnosticRecord = serde_json::from_str(pre_b4).unwrap();
    assert!(decoded.confidence_arbitration.is_none());

    let mut record = DiagnosticRecord::new("rec-2-7-1".to_owned(), 7);
    record.confidence_arbitration = Some(ConfidenceArbitrationDiagnostic {
        regions_considered: 3,
        regions_flipped: 1,
        rejections: vec![
            ConfidenceArbitrationRejection::ConfidenceGapNotDecisive,
            ConfidenceArbitrationRejection::MeaningInvertingTokens,
        ],
    });
    let encoded = serde_json::to_string(&record).unwrap();
    assert!(encoded.contains("confidence_arbitration"));
    assert!(encoded.contains("meaning_inverting_tokens"));
    let decoded: DiagnosticRecord = serde_json::from_str(&encoded).unwrap();
    let arbitration = decoded.confidence_arbitration.expect("round-trips");
    assert_eq!(arbitration.regions_considered, 3);
    assert_eq!(arbitration.regions_flipped, 1);
    assert_eq!(
        arbitration.rejections,
        vec![
            ConfidenceArbitrationRejection::ConfidenceGapNotDecisive,
            ConfidenceArbitrationRejection::MeaningInvertingTokens,
        ]
    );
}
