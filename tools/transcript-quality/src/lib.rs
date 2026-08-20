//! Private full-pipeline Transcript evaluator.
//!
//! Not packaged. Completeness-aware source selection here is an evaluator
//! heuristic, not product behavior.

mod completeness;
mod evaluate;
mod manifest;
mod mark;
mod metrics;
mod report;

pub use completeness::{select_completeness_aware, CompletenessChoice, SourceProvider};
pub use evaluate::{evaluate, evaluate_path, EvalConfig};
pub use manifest::{load_manifest, load_recordings, LoadedRecording, Manifest, ManifestRecording};
pub use metrics::{
    align_words, detect_critical_errors, detect_section_loss, tokenize, CriticalError,
    SectionLoss, WordError,
};
pub use mark::{
    attach_reference, default_corpus_dir, default_diagnostics_dir, mark_last, newest_completed,
    probe_daemon_activity, render_promotion, run_mark_last, Label, LabelRecord, MarkConfig,
    Promotion, RecordingActivity, ReferenceMeta,
};
pub use report::{
    ArmName, ArmResult, EvaluationReport, RecordingReport, StableReport, VolatileReport,
};

const USAGE: &str = "\
transcript-quality - private Recording evaluator (not packaged)

USAGE:
    transcript-quality --manifest <path> [--out <path>] [--deliver-scratch <path>]

    cargo run --manifest-path tools/transcript-quality/Cargo.toml -- \\
      --manifest /path/to/manifest.json --out /path/to/report.json

--manifest            JSON or JSONL of saved Recordings (required)
--out                 Report JSON. Default: tools/transcript-quality/out/
                      transcript-quality-report.json when the manifest lives
                      in this git tree; otherwise next to the manifest.
                      Refuses a git-tracked path unless it is gitignored or
                      outside the repository.
--deliver-scratch     Request Delivery into that blank editor path.
                      Unimplemented: evaluation still runs; nothing is typed.
--help                Print this help
";

/// CLI entry used by the private `transcript-quality` binary.
pub fn run(args: impl IntoIterator<Item = impl AsRef<str>>) -> Result<(), String> {
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
