#!/usr/bin/env python3
"""One-command accuracy bench for local speech-to-text models.

A thin wrapper over `benchmark.py` (inventory -> freeze -> replay -> score).
It adds no scoring logic of its own: it imports those functions, files every
run under the private evaluation directory, and prints a compact comparison.
Everything it reads or writes lives under ~/.local/state/voisu/, never in the
repository.  Run it through `tools/dev bench ...` or directly.

  bench.py models
      List the model manifests it can find and whether the files they name
      are present.  Only "ready" ones can be replayed.

  bench.py run --model ID=MANIFEST [--model ID2=MANIFEST2 ...] [--baseline ID]
      Replay the frozen corpus through the protected `voisu` seam for each
      model, score it, and write replay.json, report.json, run.json and
      table.txt to <eval-dir>/runs/<UTC timestamp>/.  `--model ID` alone uses
      the ready manifest that `models` lists for ID.  The table shows raw and
      delivered WER, per-language WER, words invented on silence (FP), critical
      content misses, and latency.  Changes are shown against the latest
      earlier run (same corpus) of --baseline, or of the model with the most
      runs.

  bench.py add-captures [--since DAYS] [--dry-run]
      Turn recent daemon debug captures (kept 7 days under
      <state>/diagnostics/) into new corpus candidates: the audio is copied
      to <eval-dir>/audio/cap-<id>.wav and a draft entry prefilled with the
      delivered transcript goes into <eval-dir>/references.draft.json.  The
      transcript is the pipeline's guess, not ground truth.  Each new entry
      has language "unreviewed" so `benchmark.py inventory` refuses it until
      you correct the text and set the language by hand.  Existing audio and
      references are never overwritten; duplicates (by audio hash) are skipped.

Tests: python3 -m unittest discover -s tools/local-asr/tests
"""

from __future__ import annotations

import argparse
import hashlib
import io
import json
import os
from pathlib import Path
import re
import shlex
import sys
import tempfile
import time
import wave
from typing import Any, Mapping, Sequence

sys.path.insert(0, str(Path(__file__).resolve().parent))
import benchmark  # noqa: E402  (sibling module; the single source of replay/score logic)

BenchmarkError = benchmark.BenchmarkError


def _require(condition: bool, message: str) -> None:
    if not condition:
        raise BenchmarkError(message)


DEFAULT_CORPUS_NAME = "corpus.json"
DRAFT_REFERENCES_NAME = "references.draft.json"
UNREVIEWED_LANGUAGE = "unreviewed"
MIN_CAPTURE_MS = 300
CAPTURE_PCM_RE = re.compile(r"^[A-Za-z0-9._-]+\.pcm$")
SAFE_ID_RE = re.compile(r"[^A-Za-z0-9._-]+")
# Manifest locations, canonical first.  consented-worker/ holds the daemon's own
# throwaway per-start manifests and is deliberately not scanned.
MANIFEST_DIRS = ("local-dev/manifests", "private-manifests/current", "private-manifests")


def state_root(override: Path | None = None) -> Path:
    if override is not None:
        return override.expanduser()
    base = os.environ.get("XDG_STATE_HOME")
    return (Path(base) if base else Path.home() / ".local" / "state") / "voisu"


def _utc_stamp() -> str:
    return time.strftime("%Y%m%dT%H%M%SZ", time.gmtime())


def _write_private_json(path: Path, value: Any) -> None:
    """Atomic JSON write, owner-only, for files that may hold transcript text."""

    path.parent.mkdir(parents=True, exist_ok=True)
    handle, temp_name = tempfile.mkstemp(dir=path.parent, prefix=f".{path.name}.", suffix=".tmp")
    try:
        with os.fdopen(handle, "w", encoding="utf-8") as stream:
            stream.write(json.dumps(value, ensure_ascii=False, indent=2) + "\n")
        os.chmod(temp_name, path.stat().st_mode & 0o777 if path.exists() else 0o600)
        os.replace(temp_name, path)
    except BaseException:
        Path(temp_name).unlink(missing_ok=True)
        raise


# --------------------------------------------------------------------------
# models
# --------------------------------------------------------------------------

def _missing_paths(manifest: Mapping[str, Any]) -> list[str]:
    """Names of files a manifest points at that are not on disk."""

    missing: list[str] = []
    model_path = manifest.get("model_path")
    if isinstance(model_path, str) and not Path(model_path).exists():
        missing.append("model_path")
    runtime = manifest.get("runtime")
    runtime_files = runtime if isinstance(runtime, dict) else {"python": runtime}
    for key in ("python", "worker"):
        value = runtime_files.get(key)
        if isinstance(value, str) and not Path(value).exists():
            missing.append(f"runtime.{key}")
    files = manifest.get("files")
    if isinstance(files, list):
        gone = sum(1 for item in files if isinstance(item, dict) and isinstance(item.get("path"), str) and not Path(item["path"]).exists())
        if gone:
            missing.append(f"{gone} model file(s)")
        # A pinned file whose size moved (the worker sources are pinned too) makes
        # replay fail with "file pin size/type changed"; the manifest needs regenerating.
        stale = sum(1 for item in files if isinstance(item, dict) and isinstance(item.get("path"), str) and isinstance(item.get("bytes"), int)
                    and Path(item["path"]).is_file() and Path(item["path"]).stat().st_size != item["bytes"])
        if stale:
            missing.append(f"{stale} stale pin(s): regenerate the manifest")
    return missing


def discover_models(root: Path) -> list[dict[str, Any]]:
    """Model manifests found under the state root, one per model id."""

    found: dict[str, dict[str, Any]] = {}
    for relative in MANIFEST_DIRS:
        directory = root / relative
        if not directory.is_dir():
            continue
        for path in sorted(directory.glob("*.json")):
            if path.name.endswith(".fingerprints.json"):
                continue
            try:
                manifest = json.loads(path.read_text(encoding="utf-8"))
            except (OSError, json.JSONDecodeError):
                continue
            model_id = manifest.get("model_id") if isinstance(manifest, dict) else None
            if not isinstance(model_id, str) or not benchmark.MODEL_ID_RE.fullmatch(model_id) or model_id in found:
                continue
            missing = _missing_paths(manifest)
            found[model_id] = {"model_id": model_id, "path": str(path), "device": manifest.get("device"), "ready": not missing, "missing": missing}
    return sorted(found.values(), key=lambda item: (not item["ready"], item["model_id"]))


def resolve_models(values: Sequence[str], root: Path) -> list[str]:
    """Turn --model values into the ID=ABSOLUTE_PATH form replay expects."""

    installed = {item["model_id"]: item for item in discover_models(root)}
    result = []
    for value in values:
        if "=" in value:
            model_id, raw_path = value.split("=", 1)
            path = Path(raw_path).expanduser().resolve()
        elif Path(value).expanduser().is_file():
            path = Path(value).expanduser().resolve()
            model_id = benchmark.read_json(path).get("model_id")
        else:
            model_id, item = value, installed.get(value)
            if item is None:
                raise BenchmarkError(f"no manifest found for model {value!r}; run `bench.py models` or pass --model {value}=/path/to/manifest.json")
            if not item["ready"]:
                raise BenchmarkError(f"model {value!r} is not ready ({', '.join(item['missing'])} missing): {item['path']}")
            path = Path(item["path"]).resolve()
        result.append(f"{model_id}={path}")
    return result


def cmd_models(args: argparse.Namespace) -> int:
    root = state_root(args.state_dir)
    models = discover_models(root)
    eval_dir = (args.eval_dir or root / "local-asr-eval-v1").expanduser()
    runs = _runs_per_model(load_history(eval_dir)) if eval_dir.is_dir() else {}
    if not models:
        print(f"no model manifests found under {root} ({', '.join(MANIFEST_DIRS)})")
        return 1
    rows = [["model", "ready", "device", "runs", "manifest"]]
    for item in models:
        rows.append([item["model_id"], "yes" if item["ready"] else "no: " + ", ".join(item["missing"]), str(item["device"] or "-"), str(len(runs.get(item["model_id"], []))), item["path"]])
    print(_format_rows(rows))
    ready = [item["model_id"] for item in models if item["ready"]]
    print(f"\n{len(ready)} of {len(models)} ready." + (f"  Try: tools/dev bench run --model {ready[0]}" if ready else ""))
    return 0


# --------------------------------------------------------------------------
# history and the comparison table
# --------------------------------------------------------------------------

def load_history(eval_dir: Path) -> list[dict[str, Any]]:
    """Earlier score reports: pre-bench results/report*.json, then runs/*/report.json."""

    paths = sorted((eval_dir / "results").glob("report*.json")) + sorted((eval_dir / "runs").glob("*/report.json"))
    history = []
    for path in paths:
        try:
            report = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError):
            continue
        if isinstance(report, dict) and report.get("kind") == "benchmark_report" and isinstance(report.get("models"), list):
            label = path.relative_to(eval_dir).as_posix()
            history.append({"label": label, "generated_at": str(report.get("generated_at", "")), "report": report})
    history.sort(key=lambda item: (item["generated_at"], item["label"]))
    return history


def _runs_per_model(history: Sequence[Mapping[str, Any]]) -> dict[str, list[dict[str, Any]]]:
    runs: dict[str, list[dict[str, Any]]] = {}
    for entry in history:
        for model in entry["report"]["models"]:
            if model.get("status") in {"measured", "measured_partial"}:
                runs.setdefault(model["model_id"], []).append({"label": entry["label"], "corpus": entry["report"].get("corpus_manifest_sha256"), "evidence_class": model.get("evidence_class"), "model": model})
    return runs


def pick_baseline(history: Sequence[Mapping[str, Any]], corpus_sha: str, evidence_class: str, wanted: str | None, current: Sequence[str] = ()) -> tuple[dict[str, Any] | None, str | None]:
    """Latest comparable earlier result for the baseline model, plus a note."""

    comparable = {model_id: [run for run in runs if run["corpus"] == corpus_sha and run["evidence_class"] == evidence_class] for model_id, runs in _runs_per_model(history).items()}
    comparable = {model_id: runs for model_id, runs in comparable.items() if runs}
    if wanted is not None:
        if wanted not in comparable:
            return None, f"no earlier run of baseline {wanted} on this corpus; no deltas shown"
        chosen = wanted
    elif comparable:
        # Most runs wins; a tie goes to a model in this run, then to the most recent.
        chosen = max(comparable, key=lambda model_id: (len(comparable[model_id]), model_id in current, comparable[model_id][-1]["label"]))
    else:
        return None, None
    run = comparable[chosen][-1]
    return {"model_id": chosen, "label": run["label"], "model": run["model"], "runs": len(comparable[chosen])}, None


def _pct(value: Any) -> str:
    return f"{value * 100:.1f}%" if isinstance(value, (int, float)) and not isinstance(value, bool) else "-"


def _num(value: Any, digits: int = 0) -> str:
    return f"{value:.{digits}f}" if isinstance(value, (int, float)) and not isinstance(value, bool) else "-"


def _delta(now: Any, then: Any, scale: float, unit: str, digits: int) -> str:
    if isinstance(now, (int, float)) and isinstance(then, (int, float)):
        return f"{(now - then) * scale:+.{digits}f}{unit}"
    return "-"


def model_metrics(model: Mapping[str, Any]) -> dict[str, Any]:
    """The few numbers the table shows, read from one benchmark_report model."""

    if not model.get("sample_count"):
        return {key: None for key in ("samples", "expected", "raw", "delivered", "english", "hinglish", "silence_words", "critical", "warm_p50", "warm_p95", "rtf", "stop_to_delivery")} | {"samples": 0, "expected": model.get("expected_sample_count")}
    languages = model.get("languages", {})
    delivered = model.get("delivered_strict", {})
    timings = model.get("timings", {})
    critical = model.get("critical_content_errors", {})
    return {
        "samples": model.get("sample_count"),
        "expected": model.get("expected_sample_count"),
        "raw": model.get("raw_strict", {}).get("wer"),
        "delivered": delivered.get("wer") if isinstance(delivered, dict) else None,
        "english": languages.get("english", {}).get("raw_strict", {}).get("wer"),
        "hinglish": languages.get("hinglish", {}).get("raw_strict", {}).get("wer"),
        "silence_words": languages.get("negative", {}).get("raw_strict", {}).get("insertions"),
        "critical": sum(v for v in critical.values() if isinstance(v, int)) if critical else None,
        "warm_p50": timings.get("warm_inference_ms", {}).get("p50_ms"),
        "warm_p95": timings.get("warm_inference_ms", {}).get("p95_ms"),
        "rtf": timings.get("real_time_factor", {}).get("p50"),
        "stop_to_delivery": timings.get("stop_to_delivery_ms", {}).get("p50_ms"),
    }


def _format_rows(rows: Sequence[Sequence[str]]) -> str:
    widths = [max(len(row[col]) for row in rows) for col in range(len(rows[0]))]
    return "\n".join("  ".join(cell.ljust(width) for cell, width in zip(row, widths)).rstrip() for row in rows)


def build_table(report: Mapping[str, Any], baseline: Mapping[str, Any] | None) -> str:
    base = model_metrics(baseline["model"]) if baseline else None
    columns = [("model", "model"), ("n", "n"), ("raw WER", "raw"), ("d raw", "d_raw"), ("deliv WER", "delivered"), ("EN raw", "english"), ("HI raw", "hinglish"),
               ("silence FP", "silence_words"), ("crit miss", "critical"), ("warm p50/p95 ms", "warm"), ("d warm", "d_warm"), ("RTF p50", "rtf"), ("stop>deliv ms", "stop_to_delivery")]
    cells: list[dict[str, str]] = []

    def row_for(label: str, model: Mapping[str, Any], compare: Mapping[str, Any] | None) -> dict[str, str]:
        m = model_metrics(model)
        samples = f"{m['samples']}" if m["samples"] == m["expected"] else f"{m['samples']}/{m['expected']}"
        return {
            "model": label, "n": samples, "raw": _pct(m["raw"]), "d_raw": _delta(m["raw"], compare["raw"], 100, "pp", 1) if compare else "-",
            "delivered": _pct(m["delivered"]), "english": _pct(m["english"]), "hinglish": _pct(m["hinglish"]), "silence_words": _num(m["silence_words"]),
            "critical": _num(m["critical"]), "warm": f"{_num(m['warm_p50'])}/{_num(m['warm_p95'])}" if m["warm_p50"] is not None else "-",
            "d_warm": _delta(m["warm_p50"], compare["warm_p50"], 1, "ms", 0) if compare else "-", "rtf": _num(m["rtf"], 3), "stop_to_delivery": _num(m["stop_to_delivery"]),
        }

    for model in report["models"]:
        cells.append(row_for(model["model_id"], model, base))
    if baseline:
        cells.append(row_for(f"[base] {baseline['model_id']}", baseline["model"], None))
    kept = [(title, key) for title, key in columns if key == "model" or any(cell[key] != "-" for cell in cells)]
    table = _format_rows([[title for title, _ in kept]] + [[cell[key] for _, key in kept] for cell in cells])
    dropped = [title for title, key in columns if (title, key) not in kept]
    notes = []
    if report.get("quality_evidence") is False:
        notes.append("FIXTURE or unverified evidence: these numbers are not quality evidence.")
    if baseline:
        notes.append(f"[base] and d columns: {baseline['model_id']} from {baseline['label']} ({baseline['runs']} earlier run(s) on this corpus).")
    if dropped:
        notes.append("no data for: " + ", ".join(dropped) + ".")
    return table + ("\n" + "\n".join(notes) if notes else "")


# --------------------------------------------------------------------------
# run
# --------------------------------------------------------------------------

def _new_run_dir(eval_dir: Path) -> Path:
    runs = eval_dir / "runs"
    runs.mkdir(mode=0o700, exist_ok=True)
    stamp, suffix = _utc_stamp(), 0
    while True:
        directory = runs / (stamp if not suffix else f"{stamp}-{suffix}")
        try:
            directory.mkdir(mode=0o700)
            return directory
        except FileExistsError:
            suffix += 1


def run_bench(eval_dir: Path, corpus_path: Path, model_args: Sequence[str], root: Path, rust_cli: str = "voisu", timeout_s: float = 120.0,
              normalizer: Path | None = None, baseline_id: str | None = None, fixture: bool = False) -> tuple[Path, dict[str, Any], str]:
    """Replay + score; returns (run dir, report, printable table)."""

    _require(eval_dir.is_dir(), f"evaluation directory does not exist: {eval_dir}")
    corpus = benchmark.read_json(corpus_path)
    benchmark.validate_frozen(corpus)
    models = resolve_models(model_args, root)
    model_ids = [item.split("=", 1)[0] for item in models]
    history = load_history(eval_dir)  # read before the new run exists, so "earlier" excludes it
    run_dir = _new_run_dir(eval_dir)
    started = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
    try:
        evidence = benchmark.replay(corpus, models, rust_cli, run_dir / "replay.json", timeout_s, fixture)
        report = benchmark.score(corpus, evidence, model_ids, normalizer, allow_fixture=fixture)
        benchmark.write_json(run_dir / "report.json", report)
    except BaseException:
        for leftover in run_dir.iterdir():
            if leftover.name in {"replay.json", "report.json"}:
                leftover.unlink()
        if not any(run_dir.iterdir()):
            run_dir.rmdir()
        raise
    evidence_class = "fixture" if fixture else "real"
    baseline, note = pick_baseline(history, corpus["manifest_sha256"], evidence_class, baseline_id, model_ids)
    table = build_table(report, baseline)
    if note:
        table += "\n" + note
    failures = _failure_notes(evidence)
    if failures:
        table += "\n" + failures
    meta = {
        "kind": "bench_run", "started_at": started, "finished_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "corpus": str(corpus_path), "corpus_manifest_sha256": corpus["manifest_sha256"], "fixture": fixture, "rust_cli": rust_cli,
        "timeout_seconds": timeout_s, "models": {item.split("=", 1)[0]: item.split("=", 1)[1] for item in models},
        "baseline": {"model_id": baseline["model_id"], "report": baseline["label"]} if baseline else None,
    }
    benchmark.write_json(run_dir / "run.json", meta)
    (run_dir / "table.txt").write_text(table + "\n", encoding="utf-8")
    return run_dir, report, table


def _failure_notes(evidence: Mapping[str, Any]) -> str:
    """Short, transcript-free reason per model whose replay did not fully succeed."""

    reasons: dict[str, str] = {}
    for row in evidence["records"]:
        response = row.get("raw_response", {})
        if response.get("status") in {"unavailable", "error"} and row["model_id"] not in reasons:
            reasons[row["model_id"]] = str(response.get("error", response.get("status")))[:200].replace("\n", " ")
    return "\n".join(f"{model_id}: replay failed: {reason}" for model_id, reason in reasons.items())


def cmd_run(args: argparse.Namespace) -> int:
    root = state_root(args.state_dir)
    eval_dir = (args.eval_dir or root / "local-asr-eval-v1").expanduser()
    corpus_path = (args.corpus or eval_dir / DEFAULT_CORPUS_NAME).expanduser()
    run_dir, report, table = run_bench(eval_dir, corpus_path, args.model, root, args.rust_cli, args.timeout_seconds, args.normalizer, args.baseline, args.fixture)
    print(table)
    print(f"\nwritten to {run_dir}")
    return 0 if all(model["status"] == "measured" for model in report["models"]) else 1


# --------------------------------------------------------------------------
# add-captures
# --------------------------------------------------------------------------

def pcm_to_wav(pcm: bytes) -> bytes:
    buffer = io.BytesIO()
    with wave.open(buffer, "wb") as handle:
        handle.setnchannels(1)
        handle.setsampwidth(2)
        handle.setframerate(16_000)
        handle.writeframes(pcm)
    return buffer.getvalue()


def _known_audio_hashes(eval_dir: Path) -> set[str]:
    hashes: set[str] = set()
    for name in ("corpus.json", "inventory.json"):
        path = eval_dir / name
        if path.is_file():
            records = benchmark.read_json(path).get("records", [])
            hashes.update(r["audio_sha256"] for r in records if isinstance(r, dict) and isinstance(r.get("audio_sha256"), str))
    audio_dir = eval_dir / "audio"
    if audio_dir.is_dir():
        hashes.update(benchmark.sha256_file(p) for p in audio_dir.glob("*.wav"))
    return hashes


def _load_reference_map(path: Path) -> dict[str, Any]:
    if not path.exists():
        return {}
    raw = benchmark.read_json(path)
    _require(isinstance(raw, dict) and not isinstance(raw.get("records"), list), f"{path.name} is not a record map; add the entries by hand")
    return raw


def _capture_skip_reason(row: Mapping[str, Any], pcm_dir: Path, now_ms: float, since_ms: float) -> str | None:
    audio = row.get("debug_audio")
    if not isinstance(audio, dict) or not isinstance(audio.get("file_name"), str):
        return "no debug audio recorded"
    if not CAPTURE_PCM_RE.fullmatch(audio["file_name"]):
        return "unsafe audio file name"
    captured = audio.get("captured_at_unix_ms", row.get("recorded_at_unix_ms"))
    if not isinstance(captured, (int, float)) or captured < since_ms:
        return "older than --since window"
    if isinstance(audio.get("expires_at_unix_ms"), (int, float)) and audio["expires_at_unix_ms"] < now_ms:
        return "debug audio expired"
    if not (pcm_dir / audio["file_name"]).is_file():
        return "audio file already pruned"
    transcript = row.get("final_transcript")
    if not isinstance(transcript, str) or not transcript.strip():
        return "no delivered transcript (failed or empty dictation)"
    if not isinstance(row.get("correlation_id"), str) or not row["correlation_id"]:
        return "no correlation id"
    return None


def add_captures(eval_dir: Path, diagnostics: Path, since_days: float, dry_run: bool = False, now_ms: float | None = None) -> dict[str, Any]:
    """Copy recent captures into eval_dir/audio and add unreviewed draft references."""

    _require(eval_dir.is_dir(), f"evaluation directory does not exist: {eval_dir}")
    history_path, pcm_dir = diagnostics / "history.jsonl", diagnostics / "audio"
    _require(history_path.is_file() and pcm_dir.is_dir(), f"no diagnostics captures under {diagnostics}")
    now_ms = time.time() * 1000 if now_ms is None else now_ms
    since_ms = now_ms - since_days * 86_400_000
    audio_dir, references_path = eval_dir / "audio", eval_dir / DRAFT_REFERENCES_NAME
    known = _known_audio_hashes(eval_dir)
    references = _load_reference_map(references_path)
    skipped: dict[str, int] = {}
    pending: list[tuple[str, bytes, dict[str, Any]]] = []

    def skip(reason: str) -> None:
        skipped[reason] = skipped.get(reason, 0) + 1

    for line in history_path.read_text(encoding="utf-8").splitlines():
        try:
            row = json.loads(line)
        except json.JSONDecodeError:
            skip("unreadable history line")
            continue
        reason = _capture_skip_reason(row, pcm_dir, now_ms, since_ms) if isinstance(row, dict) else "unreadable history line"
        if reason:
            skip(reason)
            continue
        pcm = (pcm_dir / row["debug_audio"]["file_name"]).read_bytes()
        if len(pcm) % 2:
            skip("malformed audio (odd byte count)")
            continue
        if len(pcm) * 1000 // 32_000 < MIN_CAPTURE_MS:
            skip(f"shorter than {MIN_CAPTURE_MS} ms")
            continue
        if not any(pcm):
            skip("silent audio")
            continue
        wav = pcm_to_wav(pcm)
        digest = hashlib.sha256(wav).hexdigest()
        name = f"cap-{SAFE_ID_RE.sub('-', row['correlation_id'])}.wav"
        if digest in known:
            skip("duplicate audio (already in corpus or captures)")
        elif name in references or (audio_dir / name).exists():
            skip("name already used by different content (kept existing)")
        else:
            known.add(digest)
            captured = row["debug_audio"].get("captured_at_unix_ms", row.get("recorded_at_unix_ms"))
            entry = {
                "language": UNREVIEWED_LANGUAGE,
                "reference": row["final_transcript"],
                "review_status": "unreviewed",
                "tags": ["capture", "unreviewed"],
                "source": {
                    "kind": "debug-capture", "correlation_id": row["correlation_id"],
                    "captured_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(captured / 1000)),
                    "prefill": "delivered transcript from the live pipeline; not ground truth",
                },
            }
            pending.append((name, wav, entry))
            references[name] = entry
    added = [name for name, _, _ in pending]
    if pending and not dry_run:
        audio_dir.mkdir(mode=0o700, exist_ok=True)
        written: list[Path] = []
        try:
            for name, wav, _ in pending:
                target = audio_dir / name
                with open(target, "xb") as handle:  # exclusive: never overwrite existing audio
                    written.append(target)
                    handle.write(wav)
                os.chmod(target, 0o600)
            _write_private_json(references_path, references)
        except BaseException:
            for target in written:
                target.unlink(missing_ok=True)  # only files created in this call
            raise
    unreviewed = sum(1 for value in references.values() if isinstance(value, dict) and value.get("review_status") == "unreviewed")
    return {"added": added, "skipped": skipped, "dry_run": dry_run, "unreviewed_total": unreviewed, "references": references_path}


def next_steps(eval_dir: Path, stamp: str) -> str:
    corpus = {}
    if (eval_dir / DEFAULT_CORPUS_NAME).is_file():
        corpus = benchmark.read_json(eval_dir / DEFAULT_CORPUS_NAME)
    benchmark_py = Path(__file__).resolve().parent / "benchmark.py"
    inventory_out, corpus_out = eval_dir / f"inventory-{stamp}.json", eval_dir / f"corpus-{stamp}.json"
    quote = shlex.quote
    return "\n".join([
        f"python3 {quote(str(benchmark_py))} inventory --input-dir {quote(str(eval_dir / 'audio'))} --references {quote(str(eval_dir / DRAFT_REFERENCES_NAME))} "
        f"--consent-receipt {quote(str(eval_dir / 'consent.json'))} --corpus-id {quote(str(corpus.get('corpus_id', 'local-english-hinglish-v2')))} --output {quote(str(inventory_out))}",
        f"python3 {quote(str(benchmark_py))} freeze --inventory {quote(str(inventory_out))} --seed {quote(str(corpus.get('seed', 'local-asr-v1')))} "
        f"--heldout-fraction {corpus.get('heldout_fraction', 0.2)} --output {quote(str(corpus_out))}",
        f"tools/dev bench run --corpus {quote(str(corpus_out))} --model <ID>   # a new corpus has no earlier runs to compare with",
    ])


def cmd_add_captures(args: argparse.Namespace) -> int:
    root = state_root(args.state_dir)
    eval_dir = (args.eval_dir or root / "local-asr-eval-v1").expanduser()
    diagnostics = (args.diagnostics_dir or root / "diagnostics").expanduser()
    result = add_captures(eval_dir, diagnostics, args.since, args.dry_run)
    verb = "would add" if result["dry_run"] else "added"
    print(f"{verb} {len(result['added'])} capture(s); skipped {sum(result['skipped'].values())}")
    for reason, count in sorted(result["skipped"].items(), key=lambda item: -item[1]):
        print(f"  skipped {count}: {reason}")
    if result["added"]:
        shown = ", ".join(result["added"][:8]) + (f", ... (+{len(result['added']) - 8})" if len(result["added"]) > 8 else "")
        print(f"new clips: {shown}")
    print(f"{result['unreviewed_total']} unreviewed entr{'y' if result['unreviewed_total'] == 1 else 'ies'} in {result['references']}")
    if result["unreviewed_total"] and not result["dry_run"]:
        print(
            "\nReview before they count: edit the file above. For each entry with review_status \"unreviewed\", correct \"reference\" to exactly what was said,\n"
            "set \"language\" (english, hinglish, or negative with an empty reference), then set review_status to \"reviewed\".  To drop a clip, delete its entry\n"
            "and its WAV in audio/.  `inventory` rejects entries whose language is still \"unreviewed\".\n\n"
            "After review:"
        )
        print(next_steps(eval_dir, _utc_stamp()))
    return 0


# --------------------------------------------------------------------------
# CLI
# --------------------------------------------------------------------------

def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description="Quick local-ASR accuracy bench over the private frozen corpus.", epilog="See the module docstring (bench.py) for details.")
    sub = parser.add_subparsers(dest="command", required=True)

    def common(p: argparse.ArgumentParser) -> None:
        p.add_argument("--state-dir", type=Path, help="Voisu state directory (default: ~/.local/state/voisu)")
        p.add_argument("--eval-dir", type=Path, help="evaluation directory (default: <state-dir>/local-asr-eval-v1)")

    models = sub.add_parser("models", help="list installed model manifests and whether they can be replayed")
    common(models)
    run = sub.add_parser("run", help="replay and score models on the frozen corpus, then compare with an earlier run")
    common(run)
    run.add_argument("--model", action="append", required=True, metavar="ID=MANIFEST", help="model to test; ID alone uses the manifest `models` lists; repeatable")
    run.add_argument("--baseline", metavar="ID", help="compare against the latest earlier run of this model (default: the model with the most runs)")
    run.add_argument("--corpus", type=Path, help="frozen corpus (default: <eval-dir>/corpus.json)")
    run.add_argument("--rust-cli", default="voisu")
    run.add_argument("--timeout-seconds", type=float, default=120.0, help="per-clip replay timeout")
    run.add_argument("--normalizer", type=Path, help="optional spelling normalization file for benchmark.py score")
    run.add_argument("--fixture", action="store_true", help="synthetic seam evidence for tests; never quality evidence")
    captures = sub.add_parser("add-captures", help="turn recent debug captures into unreviewed corpus candidates")
    common(captures)
    captures.add_argument("--since", type=float, default=7.0, metavar="DAYS", help="only captures newer than this many days (default 7, the retention window)")
    captures.add_argument("--diagnostics-dir", type=Path, help="captures directory (default: <state-dir>/diagnostics)")
    captures.add_argument("--dry-run", action="store_true", help="report what would be added without writing")
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    args = _parser().parse_args(argv)
    try:
        return {"models": cmd_models, "run": cmd_run, "add-captures": cmd_add_captures}[args.command](args)
    except BenchmarkError as exc:
        print(f"bench: {exc}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
