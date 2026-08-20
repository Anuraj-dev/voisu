use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use transcript_quality::{
    attach_reference, mark_last, newest_completed, render_promotion, run_mark_last, Label,
    LabelRecord, MarkConfig, RecordingActivity,
};
use voisu_core::{
    DebugAudioRecord, DiagnosticRecord, LifecycleStage, Provider, SourceTranscriptRecord,
};

const AUDIO_BYTES: &[u8] = b"pcm-fixture-01";
const AUDIO_SHA256: &str =
    "sha256:3989e80db5a6dbcfb1f756275cf3da9215a1c53be31c18ac4a9fd7ac40986e88";

fn scratch_dir(label: &str) -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "voisu-mark-{}-{}-{label}",
        std::process::id(),
        n
    ));
    fs::create_dir_all(&dir).expect("temp dir");
    dir
}

struct Harness {
    root: PathBuf,
    diagnostics: PathBuf,
    corpus: PathBuf,
}

impl Harness {
    fn new(label: &str) -> Self {
        let root = scratch_dir(label);
        let diagnostics = root.join("diagnostics");
        let corpus = root.join("corpus");
        fs::create_dir_all(diagnostics.join("audio")).unwrap();
        Self {
            root,
            diagnostics,
            corpus,
        }
    }

    fn config(&self, activity: RecordingActivity) -> MarkConfig<'_> {
        MarkConfig {
            diagnostics_dir: &self.diagnostics,
            corpus_dir: &self.corpus,
            activity,
            now_ms: 1_700_000_000_000,
        }
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn completed(
    correlation_id: &str,
    recording_id: u64,
    recorded_at: u64,
    audio_name: Option<&str>,
) -> DiagnosticRecord {
    let mut record = DiagnosticRecord::new(correlation_id.to_owned(), recording_id);
    record.recorded_at_unix_ms = recorded_at;
    record.stages = vec![
        LifecycleStage::CaptureStarted,
        LifecycleStage::CaptureFinalized,
        LifecycleStage::DeliveryCompleted,
    ];
    record.source_transcripts = vec![
        SourceTranscriptRecord {
            provider: Provider::Groq,
            text: "groq source".to_owned(),
        },
        SourceTranscriptRecord {
            provider: Provider::Deepgram,
            text: "deepgram source".to_owned(),
        },
    ];
    record.final_transcript = Some("final transcript".to_owned());
    record.first_chunk_ms = Some(110);
    record.capture_finalized_ms = Some(800);
    record.release_to_text_ms = Some(1_200);
    if let Some(name) = audio_name {
        record.debug_audio = Some(DebugAudioRecord {
            file_name: name.to_owned(),
            captured_at_unix_ms: recorded_at,
            expires_at_unix_ms: recorded_at + 7 * 24 * 3600 * 1000,
        });
    }
    record
}

fn write_history(diagnostics: &Path, records: &[DiagnosticRecord]) {
    let mut encoded = Vec::new();
    for record in records {
        encoded.extend(serde_json::to_vec(record).unwrap());
        encoded.push(b'\n');
    }
    fs::write(diagnostics.join("history.jsonl"), encoded).unwrap();
}

fn write_audio(diagnostics: &Path, name: &str, bytes: &[u8]) {
    fs::write(diagnostics.join("audio").join(name), bytes).unwrap();
}

fn seed_newest(harness: &Harness) -> DiagnosticRecord {
    let old = completed("rec-old", 1, 1_000, Some("rec-old-exp999.pcm"));
    let newest = completed("rec-new", 2, 2_000, Some("rec-new-exp999.pcm"));
    write_audio(&harness.diagnostics, "rec-old-exp999.pcm", b"old-pcm");
    write_audio(&harness.diagnostics, "rec-new-exp999.pcm", AUDIO_BYTES);
    write_history(&harness.diagnostics, &[old, newest.clone()]);
    newest
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[test]
fn newest_completed_id_is_resolved_before_copy() {
    let harness = Harness::new("newest");
    seed_newest(&harness);
    let history: Vec<DiagnosticRecord> = fs::read_to_string(harness.diagnostics.join("history.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(
        newest_completed(&history).unwrap().correlation_id,
        "rec-new"
    );

    let promotion = mark_last(
        &harness.config(RecordingActivity::Idle),
        Label::Good,
        Some("keep this one".to_owned()),
    )
    .unwrap();
    assert_eq!(promotion.correlation_id, "rec-new");
    assert!(!promotion.already_present);
    assert_eq!(promotion.recording_count, 1);
    assert!(!harness.corpus.join("rec-old").join("audio.pcm").exists());
    assert!(harness.corpus.join("rec-new").join("audio.pcm").is_file());
    let rendered = render_promotion(&promotion);
    assert!(rendered.contains(&harness.corpus.display().to_string()), "{rendered}");
    assert!(rendered.contains("disk:"), "{rendered}");
    assert!(rendered.contains("checksum:"), "{rendered}");

    let snapshot: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(harness.corpus.join("rec-new").join("snapshot.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(snapshot["correlation_id"], "rec-new");
    assert_eq!(snapshot["source_transcripts"][0]["text"], "groq source");
    assert_eq!(snapshot["source_transcripts"][1]["text"], "deepgram source");
    assert_eq!(snapshot["final_transcript"], "final transcript");
    assert!(snapshot.get("reconstruction_candidate").is_none());
    assert_eq!(snapshot["timing"]["release_to_text_ms"], 1_200);
    assert_eq!(snapshot["audio"]["checksum"], AUDIO_SHA256);

    let label: LabelRecord = serde_json::from_str(
        &fs::read_to_string(harness.corpus.join("rec-new").join("label.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(label.label, Label::Good);
    assert_eq!(label.correlation_id, "rec-new");
    assert_eq!(label.marked_at_unix_ms, 1_700_000_000_000);
    assert_eq!(label.note.as_deref(), Some("keep this one"));
}

#[test]
fn rejects_an_active_recording() {
    let harness = Harness::new("active");
    seed_newest(&harness);
    let err = mark_last(
        &harness.config(RecordingActivity::Active),
        Label::Bad,
        None,
    )
    .unwrap_err();
    assert!(err.contains("active"), "{err}");
    assert!(!harness.corpus.exists());
}

#[test]
fn rejects_missing_audio() {
    let harness = Harness::new("missing");
    let present = completed("rec-old", 1, 1_000, Some("rec-old-exp999.pcm"));
    let missing = completed("rec-missing", 2, 2_000, Some("rec-missing-exp999.pcm"));
    write_audio(&harness.diagnostics, "rec-old-exp999.pcm", AUDIO_BYTES);
    write_history(&harness.diagnostics, &[present, missing]);
    let err = mark_last(&harness.config(RecordingActivity::Idle), Label::Good, None).unwrap_err();
    assert!(err.contains("missing"), "{err}");
    assert!(!harness.corpus.join("rec-missing").join("audio.pcm").exists());
}

#[test]
fn rejects_record_without_debug_audio() {
    let harness = Harness::new("no-debug");
    let record = completed("rec-none", 3, 3_000, None);
    write_history(&harness.diagnostics, &[record]);
    let err = mark_last(&harness.config(RecordingActivity::Idle), Label::Bad, None).unwrap_err();
    assert!(err.contains("missing"), "{err}");
}

#[test]
fn second_mark_is_idempotent() {
    let harness = Harness::new("idem");
    seed_newest(&harness);
    let first = mark_last(
        &harness.config(RecordingActivity::Idle),
        Label::Good,
        Some("first".to_owned()),
    )
    .unwrap();
    let snapshot_before =
        fs::read(harness.corpus.join("rec-new").join("snapshot.json")).unwrap();
    let audio_before = fs::read(harness.corpus.join("rec-new").join("audio.pcm")).unwrap();

    let second = mark_last(
        &harness.config(RecordingActivity::Idle),
        Label::Good,
        Some("first".to_owned()),
    )
    .unwrap();
    assert!(second.already_present);
    assert_eq!(second.correlation_id, first.correlation_id);
    assert_eq!(second.checksum, first.checksum);
    assert_eq!(second.recording_count, 1);
    let entries: Vec<_> = fs::read_dir(&harness.corpus)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(entries.len(), 1);
    assert_eq!(
        fs::read(harness.corpus.join("rec-new").join("snapshot.json")).unwrap(),
        snapshot_before
    );
    assert_eq!(
        fs::read(harness.corpus.join("rec-new").join("audio.pcm")).unwrap(),
        audio_before
    );
}

#[test]
fn promotion_records_checksum_and_private_mode() {
    let harness = Harness::new("cksum");
    seed_newest(&harness);
    let promotion = mark_last(&harness.config(RecordingActivity::Idle), Label::Bad, None).unwrap();
    assert_eq!(promotion.checksum, AUDIO_SHA256);
    let checksum_file =
        fs::read_to_string(harness.corpus.join("rec-new").join("checksum.sha256")).unwrap();
    assert!(checksum_file.contains(AUDIO_SHA256), "{checksum_file}");
    assert_eq!(
        fs::read(harness.corpus.join("rec-new").join("audio.pcm")).unwrap(),
        AUDIO_BYTES
    );
    assert_eq!(mode(&harness.corpus.join("rec-new").join("audio.pcm")), 0o600);
    assert_eq!(mode(&harness.corpus.join("rec-new")), 0o700);
    assert!(
        harness
            .diagnostics
            .join("audio")
            .join("rec-new-exp999.pcm")
            .is_file(),
        "rolling debug capture must remain"
    );
}

#[test]
fn refuses_git_work_tree_corpus_paths() {
    let harness = Harness::new("git");
    seed_newest(&harness);
    let git_corpus = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("should-not-write-audio");
    let err = mark_last(
        &MarkConfig {
            diagnostics_dir: &harness.diagnostics,
            corpus_dir: &git_corpus,
            activity: RecordingActivity::Idle,
            now_ms: 1,
        },
        Label::Good,
        None,
    )
    .unwrap_err();
    assert!(err.contains("git"), "{err}");
    assert!(!git_corpus.exists());
}

#[test]
fn adjudicated_reference_does_not_replace_raw_evidence() {
    let harness = Harness::new("ref");
    seed_newest(&harness);
    mark_last(&harness.config(RecordingActivity::Idle), Label::Good, None).unwrap();
    let audio_before = fs::read(harness.corpus.join("rec-new").join("audio.pcm")).unwrap();
    let snapshot_before =
        fs::read(harness.corpus.join("rec-new").join("snapshot.json")).unwrap();
    attach_reference(
        &harness.corpus,
        "rec-new",
        "the words Raja actually said",
        99,
    )
    .unwrap();
    assert_eq!(
        fs::read(harness.corpus.join("rec-new").join("audio.pcm")).unwrap(),
        audio_before
    );
    assert_eq!(
        fs::read(harness.corpus.join("rec-new").join("snapshot.json")).unwrap(),
        snapshot_before
    );
    assert_eq!(
        fs::read_to_string(harness.corpus.join("rec-new").join("reference.txt")).unwrap(),
        "the words Raja actually said"
    );
    let meta: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(harness.corpus.join("rec-new").join("reference.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(meta["kind"], "adjudicated");
}

#[test]
fn help_is_private_to_this_binary() {
    run_mark_last(["--help"]).unwrap();
}
