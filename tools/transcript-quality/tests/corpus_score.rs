//! Offline tests for the audio-adjudicated corpus: loader validation, scoring
//! math, aggregate math, the stable run-JSON schema, skip semantics, and the
//! history capture path. No network, no provider keys, no daemon.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use transcript_quality::{
    CaseStatus, RESULT_SCHEMA, ReplayConfig, capture_results, ensure_corpus_path_allowed,
    load_corpus, load_run, score_corpus, write_run,
};
use voisu_core::{
    DeliveryMethod, DiagnosticRecord, Provider, SourceTranscriptRecord, TELEMETRY_SCHEMA,
};

static SEQ: AtomicU64 = AtomicU64::new(0);

fn scratch(label: &str) -> PathBuf {
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "voisu-tq-corpus-{}-{n}-{label}",
        std::process::id()
    ));
    fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn corpus_with(label: &str) -> (PathBuf, PathBuf) {
    let root = scratch(label);
    let corpus = root.join("corpus");
    fs::create_dir_all(&corpus).unwrap();
    (root, corpus)
}

fn case_dir(corpus: &Path, id: &str) -> PathBuf {
    let dir = corpus.join(id);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn write_case(
    corpus: &Path,
    id: &str,
    reference: &str,
    case_json: Option<&str>,
    result_json: Option<String>,
) -> PathBuf {
    let dir = case_dir(corpus, id);
    fs::write(dir.join("reference.txt"), reference).unwrap();
    if let Some(meta) = case_json {
        fs::write(dir.join("case.json"), meta).unwrap();
    }
    if let Some(result) = result_json {
        fs::write(dir.join("result.json"), result).unwrap();
    }
    dir
}

fn sidecar(
    case_id: &str,
    final_text: Option<&str>,
    sources: &[(&str, &str)],
    delivered: Option<bool>,
    stop_to_delivered_ms: Option<u64>,
) -> String {
    let sources: Vec<String> = sources
        .iter()
        .map(|(provider, text)| format!(r#"{{"provider": "{provider}", "text": "{text}"}}"#))
        .collect();
    let delivery = match delivered {
        Some(true) => {
            r#""delivery": { "delivered": true, "method": "clipboard_fallback" },"#.to_owned()
        }
        Some(false) => r#""delivery": { "delivered": false },"#.to_owned(),
        None => String::new(),
    };
    format!(
        r#"{{
  "schema": "{RESULT_SCHEMA}",
  "case_id": "{case_id}",
  "origin": "manual",
  "captured_at_unix_ms": 0,
  "source_transcripts": [{}],
  "final_transcript": {},
  "error": null,
  {}"telemetry": {{
    "telemetry_schema": {TELEMETRY_SCHEMA},
    "recording_duration_ms": 1000,
    "stop_to_finalized_ms": 900,
    "stop_to_delivered_ms": {}
  }}
}}"#,
        sources.join(", "),
        match final_text {
            Some(text) => format!(r#""{text}""#),
            None => "null".to_owned(),
        },
        delivery,
        match stop_to_delivered_ms {
            Some(value) => value.to_string(),
            None => "null".to_owned(),
        },
    )
}

fn no_replay() -> Option<&'static ReplayConfig> {
    None
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

// --- loader validation ---

#[test]
fn loader_reports_missing_corpus_clearly() {
    let root = scratch("no-corpus");
    let err = load_corpus(&root.join("absent")).unwrap_err();
    assert!(err.contains("cannot read corpus"), "{err}");
}

#[test]
fn loader_requires_a_reference_for_every_case() {
    let (_root, corpus) = corpus_with("missing-ref");
    case_dir(&corpus, "no-reference");
    let err = load_corpus(&corpus).unwrap_err();
    assert!(
        err.contains("case no-reference: missing reference.txt"),
        "{err}"
    );
}

#[test]
fn loader_rejects_an_empty_reference() {
    let (_root, corpus) = corpus_with("empty-ref");
    write_case(&corpus, "empty", "   \n", None, None);
    let err = load_corpus(&corpus).unwrap_err();
    assert!(err.contains("reference.txt is empty"), "{err}");
}

#[test]
fn loader_rejects_unsafe_case_names() {
    let (_root, corpus) = corpus_with("unsafe-name");
    let dir = corpus.join("has space");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("reference.txt"), "words").unwrap();
    let err = load_corpus(&corpus).unwrap_err();
    assert!(err.contains("not a safe component"), "{err}");
}

#[test]
fn loader_rejects_a_case_id_that_differs_from_the_directory() {
    let (_root, corpus) = corpus_with("id-mismatch");
    write_case(
        &corpus,
        "actual",
        "words",
        Some(r#"{ "id": "other" }"#),
        None,
    );
    let err = load_corpus(&corpus).unwrap_err();
    assert!(err.contains("must match the directory name"), "{err}");
}

#[test]
fn loader_rejects_a_foreign_result_schema_and_case_id() {
    let (_root, corpus) = corpus_with("schema");
    write_case(
        &corpus,
        "one",
        "words",
        None,
        Some(
            sidecar("one", Some("words"), &[], Some(true), Some(10))
                .replace(RESULT_SCHEMA, "some-other-schema"),
        ),
    );
    let err = load_corpus(&corpus).unwrap_err();
    assert!(err.contains("schema"), "{err}");

    let (_root, corpus) = corpus_with("case-id");
    write_case(
        &corpus,
        "one",
        "words",
        None,
        Some(sidecar("two", Some("words"), &[], None, None)),
    );
    let err = load_corpus(&corpus).unwrap_err();
    assert!(err.contains("declares case_id"), "{err}");
}

// --- privacy guard ---

#[test]
fn corpus_guard_refuses_tracked_git_paths() {
    let tracked = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("corpus-guard-refusal");
    let err = ensure_corpus_path_allowed(&tracked).unwrap_err();
    assert!(err.contains("refusing corpus"), "{err}");
    assert!(!tracked.exists(), "guard must not create the path");
}

#[test]
fn corpus_guard_allows_gitignored_and_out_of_tree_paths() {
    let ignored = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("out/corpus-guard-ignored");
    ensure_corpus_path_allowed(&ignored).expect("gitignored corpus path is allowed");
    let outside = scratch("outside");
    ensure_corpus_path_allowed(&outside).expect("temp corpus path is allowed");
}

#[test]
fn corpus_guard_refuses_the_committed_example() {
    let example = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("corpus.example");
    let err = ensure_corpus_path_allowed(&example).unwrap_err();
    assert!(err.contains("refusing corpus"), "{err}");
}

// --- scoring math ---

#[test]
fn scoring_matches_known_wer_values() {
    let (_root, corpus) = corpus_with("wer");
    write_case(
        &corpus,
        "exact",
        "alpha bravo charlie delta",
        None,
        Some(sidecar(
            "exact",
            Some("alpha bravo charlie delta"),
            &[],
            Some(true),
            Some(500),
        )),
    );
    // ref: a b c d / hyp: a x c -> one substitution, one deletion.
    write_case(
        &corpus,
        "sub-del",
        "a b c d",
        None,
        Some(sidecar("sub-del", Some("a x c"), &[], Some(false), None)),
    );
    // ref: a b / hyp: a b c d -> two insertions.
    write_case(
        &corpus,
        "insert",
        "a b",
        None,
        Some(sidecar("insert", Some("a b c d"), &[], None, None)),
    );
    // Sources present: the completeness arm scores the fuller sibling too.
    write_case(
        &corpus,
        "sources",
        "open the board",
        None,
        Some(sidecar(
            "sources",
            Some("open the board"),
            &[("groq", "open the board"), ("deepgram", "open the")],
            Some(true),
            None,
        )),
    );

    let cases = load_corpus(&corpus).unwrap();
    let run = score_corpus(&cases, &corpus, no_replay()).unwrap();

    let row = |id: &str| run.cases.iter().find(|row| row.id == id).unwrap();
    let exact = row("exact");
    assert_eq!(exact.status, CaseStatus::Scored);
    let wer = exact.wer.as_ref().unwrap();
    assert_eq!(
        (wer.insertions, wer.deletions, wer.substitutions),
        (0, 0, 0)
    );
    assert_eq!(exact.delivery, "delivered");
    assert_eq!(exact.delivery_method.as_deref(), Some("clipboard_fallback"));

    let sub = row("sub-del").wer.as_ref().unwrap();
    assert_eq!(
        (sub.insertions, sub.deletions, sub.substitutions),
        (0, 1, 1)
    );
    assert_eq!(row("sub-del").delivery, "not_delivered");

    let ins = row("insert").wer.as_ref().unwrap();
    assert_eq!(
        (ins.insertions, ins.deletions, ins.substitutions),
        (2, 0, 0)
    );
    assert_eq!(row("insert").delivery, "unknown");

    let sources = row("sources");
    let source_wer = sources.source_wer.as_ref().unwrap();
    assert_eq!(sources.selected_source.as_deref(), Some("groq"));
    assert_eq!(
        (
            source_wer.insertions,
            source_wer.deletions,
            source_wer.substitutions
        ),
        (0, 0, 0)
    );
    assert!(sources.section_loss == Some(false));
}

#[test]
fn no_final_result_is_no_final_not_scored() {
    let (_root, corpus) = corpus_with("no-final");
    write_case(
        &corpus,
        "dead",
        "words",
        None,
        Some(sidecar("dead", None, &[], Some(false), None)),
    );
    let cases = load_corpus(&corpus).unwrap();
    let run = score_corpus(&cases, &corpus, no_replay()).unwrap();
    let row = &run.cases[0];
    assert_eq!(row.status, CaseStatus::NoFinal);
    assert!(row.wer.is_none());
    assert_eq!(row.delivery, "not_delivered");
    assert_eq!(run.aggregate.no_final, 1);
    assert_eq!(run.aggregate.scored, 0);
    assert_eq!(run.aggregate.corpus_wer, None);
    assert_eq!(run.aggregate.delivery_rate, Some(0.0));
}

// --- aggregate math on the committed synthetic example ---

#[test]
fn corpus_example_scores_to_expected_numbers() {
    let example = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("corpus.example");
    let cases = load_corpus(&example).unwrap();
    assert_eq!(cases.len(), 3);
    let run = score_corpus(&cases, &example, no_replay()).unwrap();
    let alpha = run
        .cases
        .iter()
        .find(|row| row.id == "alpha-intro")
        .unwrap();
    assert!((alpha.wer.as_ref().unwrap().error_rate - 0.2).abs() < 1e-9);
    let names = run.cases.iter().find(|row| row.id == "names-goal").unwrap();
    assert!((names.wer.as_ref().unwrap().error_rate - 1.0 / 7.0).abs() < 1e-9);
    assert!(
        names.critical_error_count.unwrap() >= 1,
        "dropped fake name is critical"
    );
    assert_eq!(names.delivery, "unknown");

    let aggregate = &run.aggregate;
    assert_eq!(
        (aggregate.scored, aggregate.no_final, aggregate.skipped),
        (3, 0, 0)
    );
    assert!((aggregate.corpus_wer.unwrap() - 3.0 / 22.0).abs() < 1e-9);
    assert!((aggregate.mean_case_wer.unwrap() - (0.2 + 0.1 + 1.0 / 7.0) / 3.0).abs() < 1e-9);
    assert!((aggregate.source_corpus_wer.unwrap() - 1.0 / 22.0).abs() < 1e-9);
    assert_eq!(
        (aggregate.delivered, aggregate.delivery_denominator),
        (1, 2)
    );
    assert!((aggregate.delivery_rate.unwrap() - 0.5).abs() < 1e-9);
    assert_eq!(aggregate.median_stop_to_delivered_ms, Some(1176.0));
}

// --- JSON schema stability ---

#[test]
fn run_json_field_order_and_schema_are_stable() {
    let (_root, corpus) = corpus_with("schema-order");
    write_case(
        &corpus,
        "one",
        "alpha bravo",
        Some(r#"{ "tags": ["smoke"], "notes": "note" }"#),
        Some(sidecar(
            "one",
            Some("alpha bravo"),
            &[("groq", "alpha bravo")],
            Some(true),
            Some(700),
        )),
    );
    write_case(&corpus, "two", "words", None, None);
    let cases = load_corpus(&corpus).unwrap();
    let run = score_corpus(&cases, &corpus, no_replay()).unwrap();

    let json = serde_json::to_string_pretty(&run).unwrap();
    let order = |needle: &str| {
        json.find(needle)
            .unwrap_or_else(|| panic!("{needle} missing"))
    };
    assert!(order("\"schema\"") < order("\"corpus_dir\""));
    assert!(order("\"corpus_dir\"") < order("\"cases\""));
    assert!(order("\"cases\"") < order("\"aggregate\""));
    assert!(order("\"aggregate\"") < order("\"run_fingerprint\""));
    let row = json.find("\"id\": \"one\"").expect("case row");
    let row_keys = [
        "\"tags\"",
        "\"notes\"",
        "\"status\"",
        "\"reason\"",
        "\"wer\"",
        "\"source_wer\"",
        "\"selected_source\"",
        "\"delivery\"",
        "\"delivery_method\"",
        "\"critical_error_count\"",
        "\"section_loss\"",
        "\"telemetry\"",
    ];
    let mut previous = row;
    for key in row_keys {
        let at = json[previous..].find(key).map(|at| previous + at).unwrap();
        assert!(previous < at, "{key} out of order");
        previous = at;
    }
    assert!(json.contains("\"telemetry_schema\": 2"), "{json}");

    let path = scratch("schema-roundtrip").join("run.json");
    write_run(&run, &path).unwrap();
    let reloaded = load_run(&path).unwrap();
    assert_eq!(reloaded, run);
}

// --- skip semantics ---

#[test]
fn a_case_without_a_result_skips_without_replay() {
    let (_root, corpus) = corpus_with("skip");
    write_case(&corpus, "pending", "words", None, None);
    let cases = load_corpus(&corpus).unwrap();
    let run = score_corpus(&cases, &corpus, no_replay()).unwrap();
    let row = &run.cases[0];
    assert_eq!(row.status, CaseStatus::Skipped);
    assert!(
        row.reason.as_deref().unwrap().contains("no result.json"),
        "{:?}",
        row.reason
    );
    assert_eq!(run.aggregate.skipped, 1);
    assert_eq!(run.aggregate.scored, 0);
}

fn write_script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    let mut perms = fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&path, perms).unwrap();
    path
}

fn replay(corpus: &Path, root: &Path, script: &Path) -> transcript_quality::ScoreRun {
    let cases = load_corpus(corpus).unwrap();
    let config = ReplayConfig {
        voisu_bin: script.display().to_string(),
        diagnostics_dir: root.join("diagnostics"),
    };
    score_corpus(&cases, corpus, Some(&config)).unwrap()
}

#[test]
fn replay_with_missing_binary_skips_never_fakes() {
    let (root, corpus) = corpus_with("replay-no-bin");
    let dir = write_case(&corpus, "wanted", "words", None, None);
    fs::write(dir.join("fixture.pcm"), b"raw-pcm-bytes").unwrap();
    let run = replay(&corpus, &root, Path::new("/voisu-b1-not-a-binary"));
    let row = &run.cases[0];
    assert_eq!(row.status, CaseStatus::Skipped);
    assert!(row.wer.is_none());
    assert!(
        row.reason
            .as_deref()
            .unwrap()
            .contains("voisu binary not found"),
        "{:?}",
        row.reason
    );
}

#[test]
fn replay_success_stages_the_fixture_then_reports_the_daemon_gap() {
    let (root, corpus) = corpus_with("replay-gap");
    let dir = write_case(&corpus, "voiced", "words", None, None);
    fs::write(dir.join("fixture.pcm"), b"raw-pcm-bytes").unwrap();
    let script = write_script(
        &root,
        "voisu",
        "echo 'replayed fixture through 2 Source Transcript(s)'",
    );
    let run = replay(&corpus, &root, &script);
    let row = &run.cases[0];
    assert_eq!(row.status, CaseStatus::Skipped);
    assert!(row.wer.is_none());
    assert!(
        row.reason
            .as_deref()
            .unwrap()
            .contains("no machine-readable transcript"),
        "{:?}",
        row.reason
    );
    // The staged fixture copy was removed after the replay.
    let staged = root.join("diagnostics/fixtures/voiced.pcm");
    assert!(!staged.exists(), "staged fixture must be cleaned up");
    assert!(root.join("diagnostics/fixtures").is_dir());
}

#[test]
fn replay_replaces_a_stale_staged_fixture() {
    let (root, corpus) = corpus_with("replay-stale");
    let dir = write_case(&corpus, "voiced", "words", None, None);
    fs::write(dir.join("fixture.pcm"), b"fresh-pcm").unwrap();
    let fixtures = root.join("diagnostics/fixtures");
    fs::create_dir_all(&fixtures).unwrap();
    fs::write(fixtures.join("voiced.pcm"), b"stale").unwrap();
    let script = write_script(
        &root,
        "voisu",
        "echo 'replayed fixture through 1 Source Transcript(s)'",
    );
    let run = replay(&corpus, &root, &script);
    assert!(
        run.cases[0]
            .reason
            .as_deref()
            .unwrap()
            .contains("no machine-readable transcript")
    );
    assert!(
        !fixtures.join("voiced.pcm").exists(),
        "stale staged copy must be replaced then removed"
    );
}

#[test]
fn replay_with_unavailable_daemon_skips() {
    let (root, corpus) = corpus_with("replay-daemon");
    let dir = write_case(&corpus, "voiced", "words", None, None);
    fs::write(dir.join("fixture.pcm"), b"raw-pcm-bytes").unwrap();
    let script = write_script(&root, "voisu", "echo 'daemon unavailable'; exit 3");
    let run = replay(&corpus, &root, &script);
    let row = &run.cases[0];
    assert_eq!(row.status, CaseStatus::Skipped);
    assert!(
        row.reason
            .as_deref()
            .unwrap()
            .contains("daemon unavailable"),
        "{:?}",
        row.reason
    );
}

#[test]
fn replay_failure_is_a_skip_not_an_error() {
    let (root, corpus) = corpus_with("replay-fail");
    let dir = write_case(&corpus, "voiced", "words", None, None);
    fs::write(dir.join("fixture.pcm"), b"raw-pcm-bytes").unwrap();
    let script = write_script(&root, "voisu", "echo 'provider failed' >&2; exit 1");
    let run = replay(&corpus, &root, &script);
    assert_eq!(run.cases[0].status, CaseStatus::Skipped);
    assert!(
        run.cases[0]
            .reason
            .as_deref()
            .unwrap()
            .contains("replay failed")
    );
}

#[test]
fn replay_without_a_fixture_skips() {
    let (root, corpus) = corpus_with("replay-no-fixture");
    write_case(&corpus, "bare", "words", None, None);
    let script = write_script(&root, "voisu", "exit 0");
    let run = replay(&corpus, &root, &script);
    let row = &run.cases[0];
    assert!(
        row.reason.as_deref().unwrap().contains("no fixture.pcm"),
        "{:?}",
        row.reason
    );
}

// --- capture-result ---

fn history_record(correlation_id: &str) -> DiagnosticRecord {
    let mut record = DiagnosticRecord::new(correlation_id.to_owned(), 1);
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
    record.final_transcript = Some("the final transcript".to_owned());
    record.delivery_count = 1;
    record.delivery_method = Some(DeliveryMethod::ClipboardFallback);
    record.recording_duration_ms = Some(4321);
    record.stop_to_finalized_ms = Some(880);
    record.stop_to_delivered_ms = Some(1230);
    record
}

fn write_history(path: &Path, body: &str) {
    fs::write(path, body).unwrap();
}

#[test]
fn capture_result_writes_a_private_complete_sidecar() {
    let root = scratch("capture");
    let corpus = root.join("corpus");
    fs::create_dir_all(&corpus).unwrap();
    let history = root.join("history.jsonl");
    let mut line = serde_json::to_vec(&history_record("rec-42")).unwrap();
    line.push(b'\n');
    write_history(&history, std::str::from_utf8(&line).unwrap());

    let summary = capture_results(&history, &corpus, &[], 77).unwrap();
    assert_eq!(summary.written, vec!["rec-42".to_owned()]);
    assert!(
        summary
            .warnings
            .iter()
            .any(|warning| warning.contains("no reference.txt")),
        "{:?}",
        summary.warnings
    );

    let sidecar_path = corpus.join("rec-42").join("result.json");
    let text = fs::read_to_string(&sidecar_path).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(parsed["schema"], RESULT_SCHEMA);
    assert_eq!(parsed["case_id"], "rec-42");
    assert_eq!(parsed["origin"], "history");
    assert_eq!(parsed["captured_at_unix_ms"], 77);
    assert_eq!(parsed["correlation_id"], "rec-42");
    assert_eq!(parsed["source_transcripts"][0]["provider"], "groq");
    assert_eq!(parsed["final_transcript"], "the final transcript");
    assert_eq!(parsed["delivery"]["delivered"], true);
    assert_eq!(parsed["delivery"]["method"], "clipboard_fallback");
    assert_eq!(parsed["telemetry"]["telemetry_schema"], TELEMETRY_SCHEMA);
    assert_eq!(parsed["telemetry"]["stop_to_delivered_ms"], 1230);
    assert_eq!(mode(&corpus.join("rec-42")), 0o700);
    assert_eq!(mode(&sidecar_path), 0o600);

    // The scored corpus can consume the captured sidecar directly.
    case_dir(&corpus, "rec-42");
    fs::write(
        corpus.join("rec-42").join("reference.txt"),
        "the final transcript",
    )
    .unwrap();
    let cases = load_corpus(&corpus).unwrap();
    let run = score_corpus(&cases, &corpus, no_replay()).unwrap();
    let row = run.cases.iter().find(|row| row.id == "rec-42").unwrap();
    assert_eq!(row.status, CaseStatus::Scored);
    assert_eq!(row.wer.as_ref().unwrap().error_rate, 0.0);
    assert_eq!(
        row.telemetry.as_ref().unwrap().stop_to_delivered_ms,
        Some(1230)
    );
}

#[test]
fn capture_result_accepts_a_voisu_history_json_array() {
    let root = scratch("capture-array");
    let corpus = root.join("corpus");
    fs::create_dir_all(&corpus).unwrap();
    let history = root.join("history.json");
    write_history(
        &history,
        &serde_json::to_string_pretty(&vec![history_record("rec-array")]).unwrap(),
    );
    let summary = capture_results(&history, &corpus, &["rec-array".to_owned()], 5).unwrap();
    assert_eq!(summary.written, vec!["rec-array".to_owned()]);
}

#[test]
fn capture_result_filters_by_id_and_fails_on_unknown_ids() {
    let root = scratch("capture-filter");
    let corpus = root.join("corpus");
    fs::create_dir_all(&corpus).unwrap();
    let history = root.join("history.jsonl");
    let mut body = serde_json::to_vec(&history_record("rec-a")).unwrap();
    body.push(b'\n');
    body.extend(serde_json::to_vec(&history_record("rec-b")).unwrap());
    body.push(b'\n');
    write_history(&history, std::str::from_utf8(&body).unwrap());

    let summary = capture_results(&history, &corpus, &["rec-b".to_owned()], 1).unwrap();
    assert_eq!(summary.written, vec!["rec-b".to_owned()]);
    assert!(corpus.join("rec-b").join("result.json").is_file());
    assert!(!corpus.join("rec-a").exists());

    let err = capture_results(&history, &corpus, &["rec-z".to_owned()], 1).unwrap_err();
    assert!(err.contains("no history record for \"rec-z\""), "{err}");
}

#[test]
fn capture_result_ignores_unusable_lines_but_not_silently_for_no_records() {
    let root = scratch("capture-empty");
    let corpus = root.join("corpus");
    fs::create_dir_all(&corpus).unwrap();
    let history = root.join("history.jsonl");
    write_history(&history, "");
    let err = capture_results(&history, &corpus, &[], 1).unwrap_err();
    assert!(err.contains("no usable records"), "{err}");
}
