//! Per-case delta table between two `score-corpus` run JSONs: the recipe for
//! one-number-per-change comparisons across slices.

use crate::score::{CaseStatus, ScoreRun};

/// The snake_case status label used in compare output.
fn status_label(status: CaseStatus) -> &'static str {
    match status {
        CaseStatus::Scored => "scored",
        CaseStatus::NoFinal => "no_final",
        CaseStatus::Skipped => "skipped",
    }
}

/// Renders the human delta table for two runs joined on case id.
pub fn render_compare(a: &ScoreRun, b: &ScoreRun) -> String {
    let mut ids: Vec<&str> = a
        .cases
        .iter()
        .chain(b.cases.iter())
        .map(|row| row.id.as_str())
        .collect();
    ids.sort_unstable();
    ids.dedup();
    let width = ids.iter().map(|id| id.len()).max().unwrap_or(4).max(4);
    let mut out = String::new();
    out.push_str(&format!(
        "{:<width$}  {:>10}  {:>5}  {:>5}  {:>5}  status\n",
        "case",
        "dWER",
        "dI",
        "dD",
        "dS",
        width = width
    ));
    for id in &ids {
        let in_a = a.cases.iter().find(|row| row.id == *id);
        let in_b = b.cases.iter().find(|row| row.id == *id);
        let delta = |wer: Option<f64>| -> String {
            match wer {
                Some(delta) if delta >= 0.0 => format!("+{delta:.4}"),
                Some(delta) => format!("{delta:.4}"),
                None => "-".to_owned(),
            }
        };
        let (a_wer, a_i, a_d, a_s) = ops(in_a);
        let (b_wer, b_i, b_d, b_s) = ops(in_b);
        let dwer = match (a_wer, b_wer) {
            (Some(a), Some(b)) => Some(b - a),
            _ => None,
        };
        let count = |a: Option<usize>, b: Option<usize>| -> String {
            match (a, b) {
                (Some(a), Some(b)) => format!("{}{}", signed(b as i64 - a as i64), ""),
                (Some(a), None) => format!("{} -> -", a),
                (None, Some(b)) => format!("- -> {}", b),
                (None, None) => "-".to_owned(),
            }
        };
        let status = match (in_a, in_b) {
            (Some(a), Some(b)) if a.status == b.status => status_label(b.status).to_owned(),
            (Some(a), Some(b)) => {
                format!("{} -> {}", status_label(a.status), status_label(b.status))
            }
            (Some(_), None) => "only in a".to_owned(),
            (None, Some(_)) => "only in b".to_owned(),
            (None, None) => unreachable!("id came from one of the runs"),
        };
        out.push_str(&format!(
            "{:<width$}  {:>10}  {:>5}  {:>5}  {:>5}  {}\n",
            id,
            delta(dwer),
            count(a_i, b_i),
            count(a_d, b_d),
            count(a_s, b_s),
            status,
            width = width
        ));
    }
    out.push_str("aggregate:\n");
    for (name, a_value, b_value) in [
        ("corpus_wer", a.aggregate.corpus_wer, b.aggregate.corpus_wer),
        (
            "mean_case_wer",
            a.aggregate.mean_case_wer,
            b.aggregate.mean_case_wer,
        ),
        (
            "source_corpus_wer",
            a.aggregate.source_corpus_wer,
            b.aggregate.source_corpus_wer,
        ),
        (
            "delivery_rate",
            a.aggregate.delivery_rate,
            b.aggregate.delivery_rate,
        ),
    ] {
        out.push_str(&format!(
            "  {name}: {} -> {}\n",
            option(a_value),
            option(b_value)
        ));
    }
    out.push_str(&format!(
        "  scored: {} -> {}  no_final: {} -> {}  skipped: {} -> {}\n",
        a.aggregate.scored,
        b.aggregate.scored,
        a.aggregate.no_final,
        b.aggregate.no_final,
        a.aggregate.skipped,
        b.aggregate.skipped,
    ));
    out.push_str(&format!(
        "  median_stop_to_delivered_ms: {} -> {}\n",
        option(a.aggregate.median_stop_to_delivered_ms),
        option(b.aggregate.median_stop_to_delivered_ms),
    ));
    out
}

fn ops(
    row: Option<&crate::score::CaseRow>,
) -> (Option<f64>, Option<usize>, Option<usize>, Option<usize>) {
    match row.and_then(|row| row.wer.as_ref()) {
        Some(wer) => (
            Some(wer.error_rate),
            Some(wer.insertions),
            Some(wer.deletions),
            Some(wer.substitutions),
        ),
        None => (None, None, None, None),
    }
}

fn signed(delta: i64) -> String {
    if delta >= 0 {
        format!("+{delta}")
    } else {
        format!("{delta}")
    }
}

fn option(value: Option<f64>) -> String {
    match value {
        Some(value) => format!("{value:.4}"),
        None => "n/a".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::WordError;
    use crate::score::{Aggregate, CaseRow, CaseStatus, RUN_SCHEMA};

    fn run(case_rows: Vec<CaseRow>, corpus_wer: Option<f64>) -> ScoreRun {
        ScoreRun {
            schema: RUN_SCHEMA.to_owned(),
            corpus_dir: "/tmp".to_owned(),
            run_fingerprint: "sha256:x".to_owned(),
            aggregate: Aggregate {
                cases_total: case_rows.len(),
                corpus_wer,
                delivered: 0,
                delivery_denominator: 0,
                delivery_rate: None,
                mean_case_wer: None,
                median_stop_to_delivered_ms: None,
                no_final: 0,
                scored: case_rows.len(),
                skipped: 0,
                source_corpus_wer: None,
                source_mean_case_wer: None,
                total_deletions: 0,
                total_insertions: 0,
                total_reference_tokens: 0,
                total_substitutions: 0,
            },
            cases: case_rows,
        }
    }

    fn scored(id: &str, rate: f64, i: usize, d: usize, s: usize) -> CaseRow {
        CaseRow {
            id: id.to_owned(),
            tags: Vec::new(),
            notes: None,
            status: CaseStatus::Scored,
            reason: None,
            wer: Some(WordError {
                deletions: d,
                error_rate: rate,
                insertions: i,
                reference_tokens: 4,
                substitutions: s,
            }),
            source_wer: None,
            selected_source: None,
            delivery: "delivered".to_owned(),
            delivery_method: None,
            critical_error_count: None,
            section_loss: None,
            telemetry: None,
        }
    }

    #[test]
    fn compare_reports_per_case_and_status_deltas() {
        let a = run(
            vec![
                scored("kept", 0.25, 1, 0, 0),
                scored("dropped", 0.5, 0, 2, 0),
            ],
            Some(0.375),
        );
        let b = run(vec![scored("kept", 0.0, 0, 0, 0)], Some(0.0));
        let text = render_compare(&a, &b);
        assert!(text.contains("-0.2500"), "dWER must render: {text}");
        assert!(text.contains("only in a"), "dropped case: {text}");
        assert!(text.contains("corpus_wer: 0.3750 -> 0.0000"), "{text}");
    }
}
