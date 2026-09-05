//! Private full-pipeline Transcript evaluator.
//!
//! Not packaged. Completeness-aware source selection here is an evaluator
//! heuristic, not product behavior.

mod compare;
mod completeness;
mod corpus;
mod evaluate;
mod manifest;
mod mark;
mod metrics;
mod report;
mod score;

use std::path::PathBuf;

pub use compare::render_compare;
pub use completeness::{CompletenessChoice, SourceProvider, select_completeness_aware};
pub use corpus::{
    CaptureSummary, CaseDelivery, CaseMeta, CaseResult, CaseSource, CaseTelemetry, CorpusCase,
    REFERENCE_FILE, RESULT_FILE, RESULT_SCHEMA, ResultOrigin, capture_results,
    default_eval_corpus_dir, ensure_corpus_path_allowed, load_corpus, parse_case_result,
};
pub use evaluate::{EvalConfig, evaluate, evaluate_path};
pub use manifest::{LoadedRecording, Manifest, ManifestRecording, load_manifest, load_recordings};
pub use mark::{
    Label, LabelRecord, MarkConfig, Promotion, RecordingActivity, ReferenceMeta, attach_reference,
    default_corpus_dir, default_diagnostics_dir, mark_last, newest_completed,
    probe_daemon_activity, render_promotion, run_mark_last,
};
pub use metrics::{
    CriticalError, SectionLoss, WordError, align_words, detect_critical_errors,
    detect_section_loss, tokenize,
};
pub use report::{
    ArmName, ArmResult, EvaluationReport, RecordingReport, StableReport, VolatileReport,
};
pub use score::{
    Aggregate, CaseRow, CaseStatus, ReplayConfig, ScoreRun, TelemetryRow,
    aggregate as aggregate_rows, load_run, render_human as render_run, score_corpus, write_run,
};

const USAGE: &str = "\
transcript-quality - private Recording evaluator (not packaged)

USAGE:
    transcript-quality score-corpus [<corpus-dir>] [--json <path>] [--replay] [--voisu <path>]
    transcript-quality capture-result [--corpus <dir>] [--history <path>] [--id <correlation-id>]...
    transcript-quality compare <run-a.json> <run-b.json>
    transcript-quality --manifest <path> [--out <path>] [--deliver-scratch <path>]

score-corpus          Score the audio-adjudicated eval corpus (see the README)
                      Default corpus: $XDG_STATE_HOME/voisu/eval-corpus
--json                Write the stable voisu-private-score-corpus-v1 run JSON
--replay              Host-only: run `voisu replay` for cases without a result
                      sidecar. Requires the installed daemon and provider keys;
                      unavailable cases SKIP with a reason. Never use in CI.
--voisu               voisu binary for --replay (default $VOISU_BIN or \"voisu\")
capture-result        Copy pipeline results from history.jsonl (or a
                      `voisu history --json` array) into <corpus>/<case>/result.json
                      sidecars. Never copies audio.
compare               Per-case delta table between two score-corpus JSON runs
--manifest            Legacy manifest evaluation (unchanged)
--help                Print this help
";

/// CLI entry used by the private `transcript-quality` binary.
pub fn run(args: impl IntoIterator<Item = impl AsRef<str>>) -> Result<(), String> {
    let all: Vec<String> = args
        .into_iter()
        .map(|arg| arg.as_ref().to_owned())
        .collect();
    match all.first().map(String::as_str) {
        Some("score-corpus") => run_score_corpus(all.into_iter().skip(1)),
        Some("capture-result") => run_capture_result(all.into_iter().skip(1)),
        Some("compare") => run_compare(all.into_iter().skip(1)),
        _ => run_manifest(all),
    }
}

fn run_manifest(args: impl IntoIterator<Item = impl AsRef<str>>) -> Result<(), String> {
    let parsed = parse_args(args)?;
    if parsed.help {
        print!("{USAGE}");
        return Ok(());
    }
    let manifest_path = parsed
        .manifest
        .ok_or_else(|| "missing --manifest <path>\n\n{USAGE}".replace("{USAGE}", USAGE))?;

    if parsed.deliver_scratch.is_some() {
        eprintln!(
            "transcript-quality: --deliver-scratch is unimplemented; nothing will be typed into any editor"
        );
    }

    let out = parsed
        .out
        .unwrap_or_else(|| report::default_report_path(&manifest_path));
    report::ensure_report_path_writable(&out)?;

    let config = EvalConfig {
        deliver_scratch: parsed.deliver_scratch,
    };
    let report = evaluate_path(&manifest_path, &config)?;
    let human = report::render_human(&report);
    print!("{human}");
    report::write_report(&report, &out)?;
    println!("wrote report {}", out.display());
    Ok(())
}

const SCORE_CORPUS_USAGE: &str = "\
USAGE:
    transcript-quality score-corpus [<corpus-dir>] [--json <path>] [--replay] [--voisu <path>]
";

struct ParsedScoreArgs {
    corpus: Option<std::path::PathBuf>,
    json: Option<std::path::PathBuf>,
    replay: bool,
    voisu: Option<String>,
    help: bool,
}

fn run_score_corpus(args: impl IntoIterator<Item = impl AsRef<str>>) -> Result<(), String> {
    let parsed = parse_score_args(args)?;
    if parsed.help {
        print!("{USAGE}");
        return Ok(());
    }
    let corpus_dir = match parsed.corpus {
        Some(dir) => dir,
        None => corpus::default_eval_corpus_dir()?,
    };
    corpus::ensure_corpus_path_allowed(&corpus_dir)?;
    let cases = corpus::load_corpus(&corpus_dir)?;
    let replay = if parsed.replay {
        let voisu_bin = parsed
            .voisu
            .unwrap_or_else(|| std::env::var("VOISU_BIN").unwrap_or_else(|_| "voisu".to_owned()));
        let diagnostics_dir = mark::default_diagnostics_dir()?;
        Some(score::ReplayConfig {
            voisu_bin,
            diagnostics_dir,
        })
    } else {
        None
    };
    let run = score::score_corpus(&cases, &corpus_dir, replay.as_ref())?;
    print!("{}", score::render_human(&run));
    if let Some(json) = &parsed.json {
        score::write_run(&run, json)?;
        println!("wrote run {} ({})", json.display(), run.run_fingerprint);
    }
    Ok(())
}

fn parse_score_args(
    args: impl IntoIterator<Item = impl AsRef<str>>,
) -> Result<ParsedScoreArgs, String> {
    let mut parsed = ParsedScoreArgs {
        corpus: None,
        json: None,
        replay: false,
        voisu: None,
        help: false,
    };
    let mut iter = args.into_iter();
    while let Some(raw) = iter.next() {
        let arg = raw.as_ref();
        match arg {
            "--help" | "-h" => parsed.help = true,
            "--json" => parsed.json = Some(PathBuf::from(value_of("--json", &mut iter)?)),
            "--replay" => parsed.replay = true,
            "--voisu" => parsed.voisu = Some(value_of("--voisu", &mut iter)?),
            other if other.starts_with("--json=") => {
                parsed.json = Some(std::path::PathBuf::from(&other["--json=".len()..]));
            }
            other if other.starts_with("--voisu=") => {
                parsed.voisu = Some(other["--voisu=".len()..].to_owned());
            }
            other if other.starts_with('-') => {
                return Err(format!("unknown argument: {other}\n\n{SCORE_CORPUS_USAGE}"));
            }
            other => {
                if parsed.corpus.is_some() {
                    return Err(format!(
                        "unexpected corpus argument: {other}\n\n{SCORE_CORPUS_USAGE}"
                    ));
                }
                parsed.corpus = Some(std::path::PathBuf::from(other));
            }
        }
    }
    Ok(parsed)
}

const CAPTURE_USAGE: &str = "\
USAGE:
    transcript-quality capture-result [--corpus <dir>] [--history <path>] [--id <correlation-id>]...
";

struct ParsedCaptureArgs {
    corpus: Option<std::path::PathBuf>,
    history: Option<std::path::PathBuf>,
    ids: Vec<String>,
    help: bool,
}

fn run_capture_result(args: impl IntoIterator<Item = impl AsRef<str>>) -> Result<(), String> {
    let parsed = parse_capture_args(args)?;
    if parsed.help {
        print!("{USAGE}");
        return Ok(());
    }
    let corpus_dir = match parsed.corpus {
        Some(dir) => dir,
        None => corpus::default_eval_corpus_dir()?,
    };
    corpus::ensure_corpus_path_allowed(&corpus_dir)?;
    let history = match parsed.history {
        Some(path) => path,
        None => mark::default_diagnostics_dir()?.join("history.jsonl"),
    };
    let summary = corpus::capture_results(
        &history,
        &corpus_dir,
        &parsed.ids,
        voisu_core::unix_millis_now(),
    )?;
    print!("{}", summary.render(&corpus_dir));
    println!("captured {} result sidecar(s)", summary.written.len());
    Ok(())
}

fn parse_capture_args(
    args: impl IntoIterator<Item = impl AsRef<str>>,
) -> Result<ParsedCaptureArgs, String> {
    let mut parsed = ParsedCaptureArgs {
        corpus: None,
        history: None,
        ids: Vec::new(),
        help: false,
    };
    let mut iter = args.into_iter();
    while let Some(raw) = iter.next() {
        let arg = raw.as_ref();
        match arg {
            "--help" | "-h" => parsed.help = true,
            "--corpus" => parsed.corpus = Some(PathBuf::from(value_of("--corpus", &mut iter)?)),
            "--history" => parsed.history = Some(PathBuf::from(value_of("--history", &mut iter)?)),
            "--id" => parsed.ids.push(value_of("--id", &mut iter)?),
            other if other.starts_with("--corpus=") => {
                parsed.corpus = Some(std::path::PathBuf::from(&other["--corpus=".len()..]));
            }
            other if other.starts_with("--history=") => {
                parsed.history = Some(std::path::PathBuf::from(&other["--history=".len()..]));
            }
            other if other.starts_with("--id=") => {
                parsed.ids.push(other["--id=".len()..].to_owned());
            }
            other if other.starts_with('-') => {
                return Err(format!("unknown argument: {other}\n\n{CAPTURE_USAGE}"));
            }
            other => {
                return Err(format!(
                    "unexpected argument: {other} (correlation IDs go after --id)\n\n{CAPTURE_USAGE}"
                ));
            }
        }
    }
    Ok(parsed)
}

const COMPARE_USAGE: &str = "\
USAGE:
    transcript-quality compare <run-a.json> <run-b.json>
";

fn run_compare(args: impl IntoIterator<Item = impl AsRef<str>>) -> Result<(), String> {
    let mut paths: Vec<std::path::PathBuf> = Vec::new();
    let mut help = false;
    for raw in args {
        let arg = raw.as_ref();
        match arg {
            "--help" | "-h" => help = true,
            other if other.starts_with('-') => {
                return Err(format!("unknown argument: {other}\n\n{COMPARE_USAGE}"));
            }
            other => paths.push(std::path::PathBuf::from(other)),
        }
    }
    if help {
        print!("{USAGE}");
        return Ok(());
    }
    let [path_a, path_b] = paths.as_slice() else {
        return Err(format!(
            "compare needs exactly two run JSONs\n\n{COMPARE_USAGE}"
        ));
    };
    let a = score::load_run(path_a)?;
    let b = score::load_run(path_b)?;
    print!("{}", compare::render_compare(&a, &b));
    Ok(())
}

fn value_of(
    flag: &str,
    iter: &mut impl Iterator<Item = impl AsRef<str>>,
) -> Result<String, String> {
    iter.next()
        .map(|value| value.as_ref().to_owned())
        .ok_or_else(|| format!("missing value for {flag}"))
}

struct ParsedArgs {
    manifest: Option<std::path::PathBuf>,
    out: Option<std::path::PathBuf>,
    deliver_scratch: Option<std::path::PathBuf>,
    help: bool,
}

fn parse_args(args: impl IntoIterator<Item = impl AsRef<str>>) -> Result<ParsedArgs, String> {
    let mut manifest = None;
    let mut out = None;
    let mut deliver_scratch = None;
    let mut help = false;
    let mut iter = args.into_iter();
    while let Some(raw) = iter.next() {
        let arg = raw.as_ref();
        match arg {
            "--help" | "-h" => help = true,
            "--manifest" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "missing value for --manifest".to_owned())?;
                manifest = Some(std::path::PathBuf::from(value.as_ref()));
            }
            "--out" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "missing value for --out".to_owned())?;
                out = Some(std::path::PathBuf::from(value.as_ref()));
            }
            "--deliver-scratch" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "missing value for --deliver-scratch".to_owned())?;
                deliver_scratch = Some(std::path::PathBuf::from(value.as_ref()));
            }
            other if other.starts_with("--manifest=") => {
                manifest = Some(std::path::PathBuf::from(&other["--manifest=".len()..]));
            }
            other if other.starts_with("--out=") => {
                out = Some(std::path::PathBuf::from(&other["--out=".len()..]));
            }
            other if other.starts_with("--deliver-scratch=") => {
                deliver_scratch = Some(std::path::PathBuf::from(
                    &other["--deliver-scratch=".len()..],
                ));
            }
            other => return Err(format!("unknown argument: {other}\n\n{USAGE}")),
        }
    }
    Ok(ParsedArgs {
        manifest,
        out,
        deliver_scratch,
        help,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_help() {
        let parsed = parse_args(["--help"]).unwrap();
        assert!(parsed.help);
    }

    #[test]
    fn parse_required_and_optional() {
        let parsed = parse_args([
            "--manifest",
            "/tmp/m.json",
            "--out",
            "/tmp/r.json",
            "--deliver-scratch",
            "/tmp/scratch.txt",
        ])
        .unwrap();
        assert_eq!(
            parsed.manifest.as_deref(),
            Some(std::path::Path::new("/tmp/m.json"))
        );
        assert_eq!(
            parsed.out.as_deref(),
            Some(std::path::Path::new("/tmp/r.json"))
        );
        assert_eq!(
            parsed.deliver_scratch.as_deref(),
            Some(std::path::Path::new("/tmp/scratch.txt"))
        );
    }
}
