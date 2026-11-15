"""Run records, pairwise comparisons and the winner-rule verdict.

One run record per (model, runtime, device/threads, window config): built from
a frozen corpus, the replay evidence and its `benchmark.py score` report.  The
record holds only aggregates and per-clip error counts keyed by record_id; no
transcript text, so two records can be compared without the evidence files.

Metric slots are filled by `metrics.py`.  A scope whose reference cannot be
scored (an English reference that normalizes to nothing, e.g. "um", or a
critical-token entry that tokenizes to nothing) gets `"status":
"invalid_reference"` for that metric instead of failing the run; the slot
carries no text.

Winner rule (local-accuracy spec, P0b), judged on the `english_heldout` scope:

1. the corpus passes `corpus_lock.check_corpus` (`corpus_lock_met`);
2. complete coverage (`complete_coverage`): both records measured every
   decision-scope clip and every negative clip, and no paired comparison
   dropped a clip;
3. the paired bootstrap CI of the primary metric (`PRIMARY_METRIC`,
   candidate minus incumbent) lies entirely below zero;
4. the candidate inserts 0 words on negative clips;
5. no critical-token regression: the candidate's critical-token error rate is
   not above the incumbent's on the same clips (point estimate).

An unmet corpus lock makes the verdict `pending` whatever the other checks
say: a verdict on a corpus below the lock is not evidence.  Otherwise a failed
check makes it `not_winner` (the candidate missing clips counts as failed; an
incumbent missing clips leaves coverage unjudged), and a check that cannot be
judged yet makes it `pending`.

Critical-token limitation: an entry counts as hit when its tokens occur
anywhere in the hypothesis, not only in the hypothesis span aligned to it, so
a token heard in the wrong place still counts as hit.
"""

from __future__ import annotations

from pathlib import Path
import sys
from typing import Any, Callable, Iterable, Mapping, Sequence

sys.path.insert(0, str(Path(__file__).resolve().parent))
import corpus_lock  # noqa: E402  (sibling module)
import metrics  # noqa: E402  (sibling module)

RUN_RECORD_SCHEMA_VERSION = 1
SCOPES: dict[str, Callable[[Mapping[str, Any]], bool]] = {
    # All English speech: reproduces the 2026-10-12 table rows (EN WER).
    "english_all": lambda record: record["language"] == "english",
    # The decision scope for comparisons and the winner rule.
    "english_heldout": lambda record: record["language"] == "english" and record.get("split") == "heldout",
}
DECISION_SCOPE = "english_heldout"
METRICS = ("strict_wer", "normalized_wer", "critical_token_error_rate")
PRIMARY_METRIC = "normalized_wer"
# What the worker does today (voisu_asr/windowing.py): 30 s windows, no overlap.
WORKER_DEFAULT_WINDOW = {"window_s": 30, "overlap_s": 0, "merge": "none", "source": "worker default (voisu_asr/windowing.py)"}
RUN_RECORD_FIELDS = (
    "schema_version", "kind", "run_key", "label", "status", "corpus_manifest_sha256", "evidence_sha256",
    "identity", "scopes", "latency_ms", "memory", "negatives", "limitations",
)
SCOPE_FIELDS = ("clips", "bands", *METRICS)
BAND_FIELDS = ("expected", "measured", "reference_tokens", "strict_wer")
NOT_MEASURED_SYSTEM_MEMORY = "not_measured (deferred: the protected replay does not report system_memory_delta_bytes)"
NOT_MEASURED_THREADS = "not_measured (no thread count declared for this run)"
METRIC_FIELDS = ("status", "estimate", "numerator", "denominator", "clips", "ci", "per_clip")
CI_FIELDS = ("status", "low", "high", "level", "iterations", "seed", "unit", "method")


def _ok(row: Mapping[str, Any]) -> bool:
    """The rows `benchmark.py score` counts as quality rows."""

    status = str(row.get("raw_response", {}).get("status", row.get("status", "")))
    return row.get("raw_text") is not None and status not in {"unavailable", "error"}


def _clip_fn(metric: str, token_map: Mapping[str, str] | None) -> Callable[[list[Mapping[str, Any]], Mapping[str, Mapping[str, Any]]], list[metrics.ClipRatio]]:
    def strict(rows, corpus):
        return metrics.strict_wer_clips((row["record_id"], corpus[row["record_id"]]["reference"], str(row["raw_text"])) for row in rows)

    def normalized(rows, corpus):
        return metrics.normalized_wer_clips(((row["record_id"], corpus[row["record_id"]]["reference"], str(row["raw_text"])) for row in rows), token_map)

    def critical(rows, corpus):
        return metrics.critical_token_clips(((row["record_id"], corpus[row["record_id"]].get("critical_tokens") or [], str(row["raw_text"])) for row in rows),
                                            tokenizer=lambda text: metrics.normalize_tokens(text, token_map))

    return {"strict_wer": strict, "normalized_wer": normalized, "critical_token_error_rate": critical}[metric]


def _ci_slot(clips: Sequence[metrics.ClipRatio], iterations: int, seed: int, level: float) -> dict[str, Any]:
    base = {"low": None, "high": None, "level": level, "iterations": iterations, "seed": seed, "unit": "clip", "method": "percentile"}
    if not clips:
        return {"status": "no_clips", **base}
    interval = metrics.bootstrap_ci(clips, iterations=iterations, seed=seed, level=level)
    return {"status": "measured", **base, "low": interval.low, "high": interval.high, "method": interval.method}


def _ordered(slot: Mapping[str, Any]) -> dict[str, Any]:
    return {key: slot[key] for key in METRIC_FIELDS}


def metric_slot(metric: str, rows: list[Mapping[str, Any]], corpus_by_id: Mapping[str, Mapping[str, Any]], *,
                iterations: int, seed: int, level: float, token_map: Mapping[str, str] | None = None) -> dict[str, Any]:
    """One metric over one scope's quality rows.

    A reference the metric cannot score (metrics.py raises ValueError: a
    reference or critical-token entry that normalizes to nothing) yields
    `"status": "invalid_reference"`.  The error message is dropped because it
    quotes reference text.
    """

    empty = {"estimate": None, "numerator": None, "denominator": None, "clips": 0, "per_clip": []}
    try:
        clips = _clip_fn(metric, token_map)(rows, corpus_by_id)
    except ValueError:
        ci = {"status": "invalid_reference", "low": None, "high": None, "level": level, "iterations": iterations, "seed": seed, "unit": "clip", "method": "percentile"}
        return _ordered({"status": "invalid_reference", **empty, "ci": ci})
    clips = sorted(clips, key=lambda clip: clip.record_id)
    if not clips:
        return _ordered({"status": "no_clips", **empty, "ci": _ci_slot(clips, iterations, seed, level)})
    return {
        "status": "measured",
        "estimate": metrics.ratio_of_sums(clips),
        "numerator": sum(clip.numerator for clip in clips),
        "denominator": sum(clip.denominator for clip in clips),
        "clips": len(clips),
        "ci": _ci_slot(clips, iterations, seed, level),
        "per_clip": [[clip.record_id, clip.numerator, clip.denominator] for clip in clips],
    }


def run_key(model: Mapping[str, Any], config: Mapping[str, Any] | None = None) -> dict[str, Any]:
    """The identity a run record is one of: model, runtime, device, threads, window."""

    config = config or {}
    identity = model.get("identity") or {}
    window = {key: config.get(key, WORKER_DEFAULT_WINDOW[key]) for key in ("window_s", "overlap_s", "merge")}
    window["source"] = "declared" if any(key in config for key in ("window_s", "overlap_s", "merge")) else WORKER_DEFAULT_WINDOW["source"]
    return {
        "model_id": model["model_id"],
        "runtime": config.get("runtime", identity.get("runtime_revision")),
        "backend": identity.get("backend"),
        "device": identity.get("observed_device", identity.get("requested_device")),
        "threads": config.get("threads"),
        "threads_source": config.get("threads_source", "declared") if config.get("threads") is not None else NOT_MEASURED_THREADS,
        "window": window,
        "host_profile": identity.get("host_profile"),
    }


def record_label(key: Mapping[str, Any]) -> str:
    threads = f"{key['threads']}t" if key.get("threads") is not None else "?t"
    window = key.get("window") or {}
    return f"{key['model_id']} [{key.get('device') or '?'} {threads} w{window.get('window_s')}/{window.get('overlap_s')}]"


def build_run_record(model: Mapping[str, Any], rows: Sequence[Mapping[str, Any]], corpus: Mapping[str, Any], *,
                     config: Mapping[str, Any] | None = None, evidence_sha256: str | None = None,
                     iterations: int = metrics.BOOTSTRAP_ITERATIONS, seed: int = metrics.BOOTSTRAP_SEED,
                     level: float = metrics.CI_LEVEL, token_map: Mapping[str, str] | None = None) -> dict[str, Any]:
    """One run record from one `score` report model and that model's replay rows."""

    corpus_by_id = {record["record_id"]: record for record in corpus["records"]}
    quality = [row for row in rows if _ok(row) and row.get("record_id") in corpus_by_id]
    scopes = {}
    for name, keep in SCOPES.items():
        scoped = [row for row in quality if keep(corpus_by_id[row["record_id"]])]
        expected = sum(1 for record in corpus["records"] if keep(record))
        scopes[name] = {"clips": {"measured": len(scoped), "expected": expected}}
        for metric in METRICS:
            scopes[name][metric] = metric_slot(metric, scoped, corpus_by_id, iterations=iterations, seed=seed, level=level, token_map=token_map)
        scopes[name] = {"clips": scopes[name]["clips"], "bands": _bands(corpus["records"], keep, scopes[name]["strict_wer"]),
                        **{metric: scopes[name][metric] for metric in METRICS}}
    negatives = [row for row in quality if corpus_by_id[row["record_id"]]["language"] == "negative"]
    expected_negatives = sum(1 for record in corpus["records"] if record["language"] == "negative")
    inserted = [len(metrics.strict_tokens(str(row["raw_text"]))) for row in negatives]
    timings = model.get("timings") or {}
    resources = model.get("resources") or {}
    deltas = [row["system_memory_delta_bytes"] for row in rows if isinstance(row.get("system_memory_delta_bytes"), (int, float)) and not isinstance(row.get("system_memory_delta_bytes"), bool)]
    key = run_key(model, config)
    limitations = list(model.get("limitations") or [])
    if not deltas:
        limitations.append("system-wide memory delta " + NOT_MEASURED_SYSTEM_MEMORY)
    if key["threads"] is None:
        limitations.append("thread count " + NOT_MEASURED_THREADS)

    def latency(name: str) -> dict[str, Any]:
        summary = timings.get(name) or {}
        return {"p50": summary.get("p50_ms"), "p95": summary.get("p95_ms"), "samples": summary.get("sample_count", 0)}

    return {
        "schema_version": RUN_RECORD_SCHEMA_VERSION,
        "kind": "run_record",
        "run_key": key,
        "label": record_label(key),
        "status": model.get("status"),
        "corpus_manifest_sha256": corpus["manifest_sha256"],
        "evidence_sha256": evidence_sha256,
        "identity": dict(model.get("identity") or {}),
        "scopes": scopes,
        "latency_ms": {"warm_inference": latency("warm_inference_ms"), "stop_to_transcript": latency("stop_to_transcript_ms")},
        "memory": {
            "peak_rss_bytes": resources.get("peak_host_ram_bytes"),
            "peak_gpu_memory_bytes": resources.get("peak_gpu_memory_bytes"),
            "system_memory_delta_bytes": max(deltas) if deltas else None,
            "system_memory_delta_status": "measured" if deltas else NOT_MEASURED_SYSTEM_MEMORY,
        },
        "negatives": {"clips": len(negatives), "expected": expected_negatives, "insertions": sum(inserted) if negatives else None,
                      "clips_with_insertions": sum(1 for count in inserted if count)},
        "limitations": limitations,
    }


def _bands(records: Sequence[Mapping[str, Any]], keep: Callable[[Mapping[str, Any]], bool], strict: Mapping[str, Any]) -> dict[str, dict[str, Any]]:
    """Per duration band (corpus_lock.BANDS): clips expected/measured, reference tokens, strict WER."""

    per_clip = {item[0]: (item[1], item[2]) for item in strict.get("per_clip") or []}
    names = [name for name, _ in corpus_lock.BANDS] + [corpus_lock.OUT_OF_BAND]
    bands = {name: {"expected": 0, "measured": 0, "reference_tokens": 0, "errors": 0} for name in names}
    for record in records:
        if not keep(record):
            continue
        band = bands[corpus_lock.duration_band(int(record.get("duration_ms") or 0))]
        band["expected"] += 1
        band["reference_tokens"] += len(metrics.strict_tokens(str(record.get("reference") or "")))
        if record["record_id"] in per_clip:
            errors, tokens = per_clip[record["record_id"]]
            band["measured"] += 1
            band["errors"] += errors
            band["measured_tokens"] = band.get("measured_tokens", 0) + tokens
    return {name: {"expected": band["expected"], "measured": band["measured"], "reference_tokens": band["reference_tokens"],
                   "strict_wer": band["errors"] / band["measured_tokens"] if band.get("measured_tokens") else None}
            for name, band in bands.items() if band["expected"]}


def _complete(record: Mapping[str, Any]) -> bool:
    clips, negatives = record["scopes"][DECISION_SCOPE]["clips"], record["negatives"]
    return clips["measured"] == clips["expected"] and negatives["clips"] == negatives["expected"]


def _clips(slot: Mapping[str, Any]) -> dict[str, metrics.ClipRatio]:
    return {item[0]: metrics.ClipRatio(item[0], item[1], item[2]) for item in slot.get("per_clip") or []}


def compare(baseline: Mapping[str, Any], candidate: Mapping[str, Any], scope: str, metric: str, *,
            iterations: int = metrics.BOOTSTRAP_ITERATIONS, seed: int = metrics.BOOTSTRAP_SEED,
            level: float = metrics.CI_LEVEL) -> dict[str, Any]:
    """Paired comparison of two run records on one scope and metric (candidate minus baseline)."""

    if baseline["corpus_manifest_sha256"] != candidate["corpus_manifest_sha256"]:
        raise ValueError("run records from different corpora cannot be paired")
    a, b = baseline["scopes"][scope][metric], candidate["scopes"][scope][metric]
    result: dict[str, Any] = {
        "baseline": baseline["label"], "candidate": candidate["label"], "scope": scope, "metric": metric,
        "clips": 0, "dropped_clips": 0, "baseline_estimate": None, "candidate_estimate": None,
        "difference": {"estimate": None, "low": None, "high": None, "excludes_zero": None, "level": level, "iterations": iterations, "seed": seed, "unit": "clip"},
    }
    left, right = _clips(a), _clips(b)
    shared = sorted(set(left) & set(right))
    result["dropped_clips"] = len(set(left) ^ set(right))
    result["clips"] = len(shared)
    if not shared:
        return {**result, "status": "no_clips"}
    base_clips, cand_clips = [left[i] for i in shared], [right[i] for i in shared]
    base_value, cand_value = metrics.ratio_of_sums(base_clips), metrics.ratio_of_sums(cand_clips)
    result["baseline_estimate"], result["candidate_estimate"] = base_value, cand_value
    result["difference"]["estimate"] = cand_value - base_value if base_value is not None and cand_value is not None else None
    paired = metrics.paired_bootstrap_difference(base_clips, cand_clips, iterations=iterations, seed=seed, level=level)
    result["difference"].update({"estimate": paired.estimate, "low": paired.low, "high": paired.high, "excludes_zero": paired.excludes_zero})
    return {**result, "status": "measured"}


def winner_verdict(incumbent: Mapping[str, Any], candidate: Mapping[str, Any], comparisons: Iterable[Mapping[str, Any]], *,
                   corpus_lock_met: bool) -> dict[str, Any]:
    """The spec's winner rule for one candidate against the incumbent (see module docstring)."""

    by_metric = {item["metric"]: item for item in comparisons
                 if item["baseline"] == incumbent["label"] and item["candidate"] == candidate["label"] and item["scope"] == DECISION_SCOPE}
    if not _complete(candidate):
        coverage: bool | None = False
    elif not _complete(incumbent) or any(item.get("dropped_clips") for item in by_metric.values()):
        coverage = None
    else:
        coverage = True
    primary = by_metric.get(PRIMARY_METRIC)
    if primary is None or primary["status"] != "measured":
        better = None
    else:
        better = primary["difference"]["high"] < 0
    insertions = candidate["negatives"]["insertions"]
    no_insertions = None if insertions is None else insertions == 0
    critical = by_metric.get("critical_token_error_rate")
    if critical is None or critical["difference"]["estimate"] is None:
        no_regression = None
    else:
        no_regression = critical["difference"]["estimate"] <= 0
    checks = {"corpus_lock_met": corpus_lock_met, "complete_coverage": coverage, "paired_ci_better": better,
              "zero_negative_insertions": no_insertions, "no_critical_regression": no_regression}
    if not corpus_lock_met:
        verdict = "pending"
    elif any(value is False for value in checks.values()):
        verdict = "not_winner"
    elif all(value is True for value in checks.values()):
        verdict = "winner"
    else:
        verdict = "pending"
    return {"incumbent": incumbent["label"], "candidate": candidate["label"], "scope": DECISION_SCOPE, "primary_metric": PRIMARY_METRIC,
            "checks": checks, "verdict": verdict}


def build_run_report(corpus: Mapping[str, Any], evidence: Mapping[str, Any], report: Mapping[str, Any], *,
                     configs: Mapping[str, Mapping[str, Any]] | None = None, incumbent: str | None = None,
                     iterations: int = metrics.BOOTSTRAP_ITERATIONS, seed: int = metrics.BOOTSTRAP_SEED,
                     level: float = metrics.CI_LEVEL, token_map: Mapping[str, str] | None = None) -> dict[str, Any]:
    """Run records for every model in a `score` report, all pairwise comparisons, and verdicts.

    `configs` maps model_id to `{"threads", "runtime", "window_s", "overlap_s",
    "merge"}` (any subset).  `incumbent` is a model_id in this report; without
    it no verdicts are made.
    """

    if report.get("corpus_manifest_sha256") != corpus["manifest_sha256"] or report.get("evidence_sha256") != evidence.get("evidence_sha256"):
        raise ValueError("report, evidence and corpus do not belong together")
    configs = configs or {}
    lock = corpus_lock.check_corpus(corpus["records"])
    rows_by_model: dict[str, list[Mapping[str, Any]]] = {}
    for row in evidence.get("records") or []:
        rows_by_model.setdefault(row["model_id"], []).append(row)
    records = [build_run_record(model, rows_by_model.get(model["model_id"], []), corpus, config=configs.get(model["model_id"]),
                                evidence_sha256=evidence.get("evidence_sha256"), iterations=iterations, seed=seed, level=level, token_map=token_map)
               for model in report["models"]]
    comparisons = [compare(a, b, scope, metric, iterations=iterations, seed=seed, level=level)
                   for i, a in enumerate(records) for b in records[i + 1:] for scope in SCOPES for metric in METRICS]
    verdicts = []
    if incumbent is not None:
        chosen = [record for record in records if record["run_key"]["model_id"] == incumbent]
        if not chosen:
            raise ValueError(f"incumbent {incumbent} is not in this report")
        for record in records:
            if record is chosen[0]:
                continue
            # The rule reads incumbent -> candidate; build that direction explicitly.
            directed = [compare(chosen[0], record, DECISION_SCOPE, metric, iterations=iterations, seed=seed, level=level) for metric in METRICS]
            verdicts.append(winner_verdict(chosen[0], record, directed, corpus_lock_met=lock["ok"]))
    return {
        "schema_version": RUN_RECORD_SCHEMA_VERSION,
        "kind": "run_report",
        "corpus_manifest_sha256": corpus["manifest_sha256"],
        "evidence_sha256": evidence.get("evidence_sha256"),
        "report_sha256": report.get("report_sha256"),
        "incumbent": incumbent,
        "corpus_lock": {"ok": lock["ok"], "missing": lock["missing"]},
        "bootstrap": {"iterations": iterations, "seed": seed, "level": level, "unit": "clip"},
        "records": records,
        "comparisons": comparisons,
        "verdicts": verdicts,
    }


def _pct(value: Any) -> str:
    return f"{value * 100:.1f}%" if isinstance(value, (int, float)) and not isinstance(value, bool) else "-"


def _metric_cell(slot: Mapping[str, Any]) -> str:
    if slot["status"] == "invalid_reference":
        return "invalid ref"
    if slot["status"] != "measured":
        return "-"
    ci = slot["ci"]
    interval = f" [{_pct(ci['low'])}, {_pct(ci['high'])}]" if ci["status"] == "measured" else " [no CI]"
    return _pct(slot["estimate"]) + interval


def format_run_report(run_report: Mapping[str, Any]) -> str:
    """Compact text: one row per run record, then decision-scope comparisons and verdicts."""

    def num(value: Any) -> str:
        return f"{value:.0f}" if isinstance(value, (int, float)) and not isinstance(value, bool) else "-"

    def mib(value: Any) -> str:
        return f"{value / 2**20:.0f}" if isinstance(value, (int, float)) and not isinstance(value, bool) else "-"

    header = ["run", "EN n", "EN strict", "held-out n", "held-out strict", "held-out norm", "held-out crit", "p50/p95 ms", "peak RSS MiB", "sys dMiB", "neg ins"]
    rows = [header]
    for record in run_report["records"]:
        everything, held = record["scopes"]["english_all"], record["scopes"][DECISION_SCOPE]
        latency = record["latency_ms"]["warm_inference"]
        rows.append([
            record["label"], str(everything["clips"]["measured"]), _metric_cell(everything["strict_wer"]), str(held["clips"]["measured"]),
            _metric_cell(held["strict_wer"]), _metric_cell(held["normalized_wer"]), _metric_cell(held["critical_token_error_rate"]),
            f"{num(latency['p50'])}/{num(latency['p95'])}", mib(record["memory"]["peak_rss_bytes"]),
            mib(record["memory"]["system_memory_delta_bytes"]) if record["memory"]["system_memory_delta_status"] == "measured" else "n/m",
            num(record["negatives"]["insertions"]),
        ])
    widths = [max(len(row[i]) for row in rows) for i in range(len(header))]
    lines = ["  ".join(cell.ljust(width) for cell, width in zip(row, widths)).rstrip() for row in rows]
    lines.append(f"\nduration bands, {DECISION_SCOPE} (clips measured/expected, reference tokens, strict WER):")
    for record in run_report["records"]:
        bands = record["scopes"][DECISION_SCOPE]["bands"]
        cells = "; ".join(f"{name} n={band['measured']}/{band['expected']} tok={band['reference_tokens']} strict={_pct(band['strict_wer'])}" for name, band in bands.items())
        lines.append(f"  {record['label']}: {cells or 'no clips'}")
    decision = [item for item in run_report["comparisons"] if item["scope"] == DECISION_SCOPE]
    if decision:
        lines.append(f"\npaired differences, {DECISION_SCOPE} (B minus A; negative = B makes fewer errors):")
        for item in decision:
            diff = item["difference"]
            if item["status"] == "measured":
                detail = f"{diff['estimate'] * 100:+.2f}pp [{diff['low'] * 100:+.2f}, {diff['high'] * 100:+.2f}] {'excludes 0' if diff['excludes_zero'] else 'includes 0'}"
            else:
                detail = item["status"]
            lines.append(f"  {item['metric']}: A={item['baseline']} B={item['candidate']} n={item['clips']}: {detail}")
    for verdict in run_report["verdicts"]:
        checks = ", ".join(f"{name}={'-' if value is None else ('yes' if value else 'no')}" for name, value in verdict["checks"].items())
        lines.append(f"verdict {verdict['candidate']} vs {verdict['incumbent']}: {verdict['verdict']} ({checks})")
    lock = run_report.get("corpus_lock") or {}
    if lock and not lock.get("ok"):
        lines.append("corpus lock NOT met (verdicts stay pending): " + "; ".join(lock.get("missing") or []))
    lines.append("n/m = not measured.  ?t = thread count not declared.  System memory delta is deferred: the protected replay does not report it.")
    lines.append("critical tokens: an entry counts as hit anywhere in the hypothesis, not only in its aligned span.")
    return "\n".join(lines)
