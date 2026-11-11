#!/usr/bin/env python3
"""Cloud LLM benchmark: send Voisu's real post-STT request to several models.

Two subcommands:

``run``    builds the production request for each corpus record from two
           stored Source Transcripts, POSTs it to each chosen model, and writes
           one row per record x model x repeat.
``score``  compares each model's final text with the human reference.

The prompt, schema and sampling parameters are NOT copied here.  A Rust helper
(``crates/voisu-cloud/examples/dump_llm_request.rs`` for Intent Reconstruction,
``crates/voisu-app/examples/dump_llm_request.rs`` for Minimal Grammar) emits the
exact production request body and runs the provider response through the same
parser and validation production uses.  This tool only swaps the ``model`` field
and the few reasoning controls listed in ``MODELS``.

Reproduced: request body (prompt, schema, temperature, response_format), the
source sanitisation and eligibility gate (a record production would not send
to a model is skipped), the response parser, and the acceptance guards
(``fallback`` text when production would reject or fail the model output).
Not reproduced: dictionary terms, user vocabulary and word confidences (empty
here, production feeds them from the user's dictionary and the providers),
the production transport (curl, 5 s ``--max-time``; this tool uses urllib with
``--timeout-seconds``), and the production grammar English-eligibility and
command-span gates.

LLM_RUN.json holds transcript text: it must be written to a git-ignored path.
REPORT.json holds aggregates and per-record numbers only.  Python 3.11+ stdlib.
"""

from __future__ import annotations

import argparse
import collections
import copy
import importlib.util
import json
import os
from pathlib import Path
import shlex
import socket
import subprocess
import sys
import time
import urllib.error
import urllib.request
from typing import Any, Mapping, Sequence

_BENCHMARK_PATH = Path(__file__).with_name("benchmark.py")
_SPEC = importlib.util.spec_from_file_location("voisu_local_asr_benchmark", _BENCHMARK_PATH)
benchmark = importlib.util.module_from_spec(_SPEC)
assert _SPEC.loader is not None
sys.modules.setdefault("voisu_local_asr_benchmark", benchmark)
_SPEC.loader.exec_module(benchmark)

BenchmarkError = benchmark.BenchmarkError
REPO_ROOT = Path(__file__).resolve().parents[2]
RUN_SCHEMA_VERSION = 1
REPORT_SCHEMA_VERSION = 1
MAX_RETRIES = 3
MAX_RETRY_WAIT_S = 60.0
USER_AGENT = "voisu-llm-bench/1"
SCORED_LANGUAGES = ("english", "hinglish")

# Indirection so tests never really sleep.
SLEEP = time.sleep

PROVIDERS: dict[str, dict[str, str]] = {
    "groq": {
        "url": "https://api.groq.com/openai/v1/chat/completions",
        "url_env": "LLM_BENCH_GROQ_URL",
        "key_env": "GROQ_API_KEY",
    },
    "cerebras": {
        "url": "https://api.cerebras.ai/v1/chat/completions",
        "url_env": "LLM_BENCH_CEREBRAS_URL",
        "key_env": "CEREBRAS_API_KEY",
    },
}

# id -> provider, API model name, request fields that differ from production.
# Production sends reasoning_effort "none", which only Qwen accepts; the
# gpt-oss models accept low/medium/high ("low" is what Voisu's grammar call
# already uses), so they get "low".
MODELS: dict[str, dict[str, Any]] = {
    "groq:qwen/qwen3.8-27b": {"provider": "groq", "api_model": "qwen/qwen3.8-27b", "overrides": {}},
    "groq:openai/gpt-oss-20b": {"provider": "groq", "api_model": "openai/gpt-oss-20b", "overrides": {"reasoning_effort": "low"}},
    "groq:openai/gpt-oss-120b": {"provider": "groq", "api_model": "openai/gpt-oss-120b", "overrides": {"reasoning_effort": "low"}},
    "cerebras:qwen-3.8-27b": {"provider": "cerebras", "api_model": "qwen-3.8-27b", "overrides": {}},
    "cerebras:gpt-oss-120b": {"provider": "cerebras", "api_model": "gpt-oss-120b", "overrides": {"reasoning_effort": "low"}},
}

TASKS: dict[str, dict[str, Any]] = {
    "intent-reconstruction": {
        "rust_cmd": "cargo run -q -p voisu-cloud --example dump_llm_request --",
        "needs_b": True,
    },
    "grammar": {
        "rust_cmd": "cargo run -q -p voisu-app --example dump_llm_request --",
        "needs_b": False,
    },
}


# --------------------------------------------------------------------- utils


def _atomic_write(path: Path, value: Any) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temp = path.with_name(f".{path.name}.tmp")
    fd = os.open(temp, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(fd, "w", encoding="utf-8") as handle:
        handle.write(json.dumps(value, ensure_ascii=False, indent=1) + "\n")
    os.replace(temp, path)


def _git(args: Sequence[str], cwd: Path) -> subprocess.CompletedProcess[str] | None:
    try:
        return subprocess.run(["git", *args], cwd=cwd, capture_output=True, text=True, check=False)
    except OSError:
        return None


def require_untracked_ignored(path: Path) -> None:
    """Refuse a path inside a git work tree unless it is ignored and untracked."""

    path = path.resolve()
    anchor = path.parent
    while not anchor.exists() and anchor != anchor.parent:
        anchor = anchor.parent
    inside = _git(["rev-parse", "--is-inside-work-tree"], anchor)
    if inside is None or inside.returncode != 0 or inside.stdout.strip() != "true":
        return
    ignored = _git(["check-ignore", "-q", "--", str(path)], anchor)
    tracked = _git(["ls-files", "--error-unmatch", "--", str(path)], anchor)
    if ignored is None or ignored.returncode != 0 or (tracked is not None and tracked.returncode == 0):
        raise BenchmarkError(
            f"refusing to write transcript text to {path}: it is inside a git work tree and is not git-ignored "
            "(choose a path outside the repository or an ignored one)"
        )


def _load_stt_texts(stt_run: Mapping[str, Any]) -> dict[tuple[str, str], str]:
    rows = stt_run.get("rows")
    if not isinstance(rows, list):
        raise BenchmarkError("STT run has no rows list")
    texts: dict[tuple[str, str], str] = {}
    for row in rows:
        if row.get("repeat", 0) == 0 and row.get("status") == "ok" and isinstance(row.get("text"), str):
            texts[(str(row.get("candidate")), str(row.get("record_id")))] = row["text"]
    return texts


def _corpus_records(corpus: Mapping[str, Any], split: str) -> list[Mapping[str, Any]]:
    records = corpus.get("records")
    if not isinstance(records, list) or not records:
        raise BenchmarkError("corpus has no records")
    chosen = []
    for record in records:
        if record.get("language") not in SCORED_LANGUAGES:
            continue
        if split != "all" and record.get("split") != split:
            continue
        chosen.append(record)
    return chosen


# ------------------------------------------------------------- rust bridge


class RustBridge:
    """One long-lived Rust helper process speaking a JSON-line protocol."""

    def __init__(self, command: Sequence[str], cwd: Path) -> None:
        try:
            self.process = subprocess.Popen(
                list(command), cwd=cwd, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                text=True, encoding="utf-8", bufsize=1,
            )
        except OSError as exc:
            raise BenchmarkError(f"cannot start Rust helper {command[0]!r}: {exc}") from exc

    def call(self, message: Mapping[str, Any]) -> dict[str, Any]:
        assert self.process.stdin is not None and self.process.stdout is not None
        try:
            self.process.stdin.write(json.dumps(message) + "\n")
            self.process.stdin.flush()
            line = self.process.stdout.readline()
        except OSError as exc:
            raise BenchmarkError(f"Rust helper pipe failed: {exc}") from exc
        if not line:
            raise BenchmarkError("Rust helper exited without answering (build failed? see its stderr above)")
        try:
            answer = json.loads(line)
        except json.JSONDecodeError as exc:
            raise BenchmarkError(f"Rust helper returned invalid JSON: {exc}") from exc
        if not answer.get("ok"):
            raise BenchmarkError(f"Rust helper error for {message.get('id')}: {answer.get('error')}")
        return answer

    def close(self) -> None:
        try:
            if self.process.stdin:
                self.process.stdin.close()
            self.process.wait(timeout=10)
        except (OSError, subprocess.TimeoutExpired):
            self.process.kill()


def _inputs(task: str, text_a: str, text_b: str | None) -> dict[str, str]:
    if task == "intent-reconstruction":
        # source A plays the Deepgram role, source B the Groq role.
        return {"deepgram": text_a, "groq": text_b or ""}
    return {"text": text_a}


# ----------------------------------------------------------------- HTTP


class RequestTimeout(Exception):
    pass


class TransportFailure(Exception):
    pass


def post_json(url: str, key: str, body: Mapping[str, Any], timeout: float) -> tuple[int, Mapping[str, str], str]:
    request = urllib.request.Request(
        url,
        data=json.dumps(body).encode("utf-8"),
        method="POST",
        headers={
            "Authorization": f"Bearer {key}",
            "Content-Type": "application/json",
            "Accept": "application/json",
            "User-Agent": USER_AGENT,
        },
    )
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            return response.status, response.headers, response.read().decode("utf-8", "replace")
    except urllib.error.HTTPError as exc:
        try:
            text = exc.read().decode("utf-8", "replace")
        except (OSError, TimeoutError):
            text = ""
        return exc.code, exc.headers, text
    except (socket.timeout, TimeoutError) as exc:
        raise RequestTimeout() from exc
    except urllib.error.URLError as exc:
        if isinstance(exc.reason, (socket.timeout, TimeoutError)):
            raise RequestTimeout() from exc
        raise TransportFailure(type(exc.reason).__name__) from exc
    except OSError as exc:
        raise TransportFailure(type(exc).__name__) from exc


def _retry_wait(headers: Mapping[str, str], attempt: int) -> float:
    raw = headers.get("Retry-After") if headers else None
    try:
        wait = float(raw) if raw is not None else float(2**attempt)
    except ValueError:
        wait = float(2**attempt)
    return max(0.0, min(wait, MAX_RETRY_WAIT_S))


def _usage(response_text: str) -> dict[str, Any]:
    try:
        usage = json.loads(response_text).get("usage") or {}
    except (json.JSONDecodeError, AttributeError):
        return {}
    details = usage.get("completion_tokens_details") or {}
    return {
        "prompt_tokens": usage.get("prompt_tokens"),
        "completion_tokens": usage.get("completion_tokens"),
        "reasoning_tokens": details.get("reasoning_tokens", usage.get("reasoning_tokens")),
    }


class Throttle:
    def __init__(self, interval_ms: float) -> None:
        self.interval = interval_ms / 1000.0
        self.last = 0.0

    def wait(self) -> None:
        if self.interval > 0 and self.last:
            remaining = self.interval - (time.monotonic() - self.last)
            if remaining > 0:
                SLEEP(remaining)
        self.last = time.monotonic()


def call_model(url: str, key: str, body: Mapping[str, Any], timeout: float, throttle: Throttle) -> dict[str, Any]:
    """POST with 429 handling. latency_ms is the final attempt only, waits excluded."""

    retries = 0
    while True:
        throttle.wait()
        started = time.monotonic()
        try:
            status, headers, text = post_json(url, key, body, timeout)
        except RequestTimeout:
            return {"http_status": None, "error": "timeout", "retries": retries, "latency_ms": None, "response_body": None}
        except TransportFailure as exc:
            return {"http_status": None, "error": f"transport:{exc}", "retries": retries, "latency_ms": None, "response_body": None}
        latency_ms = (time.monotonic() - started) * 1000.0
        if status == 429 and retries < MAX_RETRIES:
            SLEEP(_retry_wait(headers, retries))
            retries += 1
            continue
        result = {"http_status": status, "retries": retries, "latency_ms": latency_ms}
        if status == 200:
            return {**result, "error": None, "response_body": text, **_usage(text)}
        return {**result, "error": f"http_{status}", "response_body": None, "error_body": text[:300]}


# ------------------------------------------------------------------ run


def _row_key(row: Mapping[str, Any]) -> tuple[str, str, int]:
    return (row["record_id"], row["model"], row["repeat"])


def run_command(args: argparse.Namespace) -> int:
    task = args.task
    task_cfg = TASKS[task]
    for model in args.model:
        if model not in MODELS:
            raise BenchmarkError(f"unknown model {model!r}; known: {', '.join(MODELS)}")
    if task_cfg["needs_b"] and not args.source_b:
        raise BenchmarkError("--source-b is required for intent-reconstruction")
    if args.repeats < 1:
        raise BenchmarkError("--repeats must be at least 1")
    output = Path(args.output)
    require_untracked_ignored(output)

    keys: dict[str, str] = {}
    for provider in sorted({MODELS[m]["provider"] for m in args.model}):
        env = PROVIDERS[provider]["key_env"]
        value = os.environ.get(env, "").strip()
        if not value:
            raise BenchmarkError(f"{env} is not set (needed for {provider} models)")
        keys[provider] = value

    corpus = benchmark.read_json(Path(args.corpus))
    texts = _load_stt_texts(benchmark.read_json(Path(args.stt_run)))
    records = _corpus_records(corpus, args.split)
    selected = []
    missing_source = 0
    for record in records:
        rid = record["record_id"]
        text_a = texts.get((args.source_a, rid))
        text_b = texts.get((args.source_b, rid)) if task_cfg["needs_b"] else None
        if text_a is None or (task_cfg["needs_b"] and text_b is None):
            missing_source += 1
            continue
        selected.append((record, text_a, text_b))
    if not selected:
        raise BenchmarkError("no corpus record has ok repeat-0 text for the chosen source candidates")
    if args.limit:
        selected = selected[: args.limit]

    config = {
        "task": task, "source_a": args.source_a, "source_b": args.source_b if task_cfg["needs_b"] else None,
        "corpus_id": corpus.get("corpus_id"),
    }
    document: dict[str, Any] = {"schema_version": RUN_SCHEMA_VERSION, "kind": "llm_bench_run", **config, "rows": []}
    if output.exists():
        previous = benchmark.read_json(output)
        for field, value in config.items():
            if previous.get(field) != value:
                raise BenchmarkError(f"existing {output} was made with {field}={previous.get(field)!r}; refusing to resume with {value!r}")
        document["rows"] = previous.get("rows", [])
    done = {_row_key(row) for row in document["rows"] if row.get("status") == "ok"}
    rows_by_key = {_row_key(row): row for row in document["rows"]}
    document["models"] = sorted(set(document.get("models", [])) | set(args.model))

    command = shlex.split(args.rust_cmd) if args.rust_cmd else shlex.split(task_cfg["rust_cmd"])
    bridge = RustBridge([*command, "--task", task], REPO_ROOT)
    throttle = Throttle(args.min_interval_ms)
    not_eligible = 0
    try:
        planned = []
        for record, text_a, text_b in selected:
            answer = bridge.call({"op": "request", "id": record["record_id"], **_inputs(task, text_a, text_b)})
            if not answer.get("eligible"):
                not_eligible += 1
                continue
            planned.append((record, text_a, text_b, answer))
        document["skipped"] = {"missing_source": missing_source, "not_eligible": not_eligible}
        for record, text_a, text_b, answer in planned:
            rid = record["record_id"]
            for model in args.model:
                spec = MODELS[model]
                provider = PROVIDERS[spec["provider"]]
                url = os.environ.get(provider["url_env"], provider["url"])
                for repeat in range(args.repeats):
                    key = (rid, model, repeat)
                    if key in done:
                        continue
                    body = copy.deepcopy(answer["body"])
                    body["model"] = spec["api_model"]
                    body.update(spec["overrides"])
                    result = call_model(url, keys[spec["provider"]], body, args.timeout_seconds, throttle)
                    row = {
                        "record_id": rid, "model": model, "repeat": repeat, "language": record["language"],
                        "eligibility": answer.get("eligibility"), "http_status": result["http_status"],
                        "retries": result["retries"], "latency_ms": result["latency_ms"],
                        "prompt_tokens": result.get("prompt_tokens"), "completion_tokens": result.get("completion_tokens"),
                        "reasoning_tokens": result.get("reasoning_tokens"),
                    }
                    if result["error"]:
                        row.update(status="error", text=None, raw_status=result["error"], error=_redact(result["error"] + ":" + result.get("error_body", ""), keys))
                    else:
                        accepted = bridge.call({
                            "op": "accept", "id": rid, **_inputs(task, text_a, text_b),
                            "response_body": result["response_body"],
                        })
                        row.update(status="ok", text=accepted["text"], raw_status=accepted.get("outcome"), error=None)
                    rows_by_key[key] = row
                    document["rows"] = list(rows_by_key.values())
                    _atomic_write(output, document)
                    print(f"{rid} {model} #{repeat}: {row['status']} {row['raw_status']}"
                          + (f" {row['latency_ms']:.0f} ms" if row["latency_ms"] is not None else ""), file=sys.stderr)
    finally:
        bridge.close()
    document["skipped"] = {"missing_source": missing_source, "not_eligible": not_eligible}
    _atomic_write(output, document)
    ok = sum(1 for row in document["rows"] if row["status"] == "ok")
    print(f"wrote {output}: {ok}/{len(document['rows'])} rows ok; skipped missing_source={missing_source} not_eligible={not_eligible}")
    return 0


def _redact(text: str, keys: Mapping[str, str]) -> str:
    for key in keys.values():
        text = text.replace(key, "***")
    return text


# ---------------------------------------------------------------- score


def _tokens(text: str, mapping: Mapping[str, str]) -> list[str]:
    return benchmark._map_tokens(benchmark.strict_tokens(text), mapping)


def _critical_hits(record: Mapping[str, Any], hypothesis: Sequence[str], mapping: Mapping[str, str]) -> tuple[int, int]:
    total = hits = 0
    for token in record.get("critical_tokens", []) or []:
        expected = _tokens(token, mapping)
        if not expected:
            continue
        total += 1
        hits += all(part in hypothesis for part in expected)
    return hits, total


class _Pool:
    """Pooled WER counters (distance / reference tokens)."""

    def __init__(self) -> None:
        self.distance = 0
        self.reference_tokens = 0

    def add(self, result: Mapping[str, Any]) -> None:
        self.distance += result["distance"]
        self.reference_tokens += result["reference_tokens"]

    def wer(self) -> float | None:
        return self.distance / self.reference_tokens if self.reference_tokens else None


def _mean(values: Sequence[float]) -> float | None:
    return sum(values) / len(values) if values else None


def _stratum(rows: Sequence[Mapping[str, Any]], corpus_by_id: Mapping[str, Mapping[str, Any]],
             source_texts: Mapping[str, Sequence[str | None]], sources: Sequence[str], mapping: Mapping[str, str]) -> dict[str, Any]:
    pools = {"model": _Pool(), **{source: _Pool() for source in sources}}
    verdicts = collections.Counter()
    invented = dropped = records_invented = records_dropped = fallback = 0
    crit = {"model": [0, 0], **{source: [0, 0] for source in sources}}
    per_record = []
    for row in rows:
        record = corpus_by_id[row["record_id"]]
        reference = _tokens(record["reference"], mapping)
        hyp = _tokens(row["text"], mapping)
        source_tokens = [_tokens(text, mapping) for text in source_texts[row["record_id"]]]
        model_result = benchmark.wer(reference, hyp)
        pools["model"].add(model_result)
        distances = {}
        for source, tokens in zip(sources, source_tokens):
            result = benchmark.wer(reference, tokens)
            pools[source].add(result)
            distances[source] = result["distance"]
        best = min(distances.values())
        verdict = "worse" if model_result["distance"] > best else "better" if model_result["distance"] < best else "unchanged"
        verdicts[verdict] += 1
        seen = set().union(*source_tokens)
        row_invented = sum(1 for token in hyp if token not in seen)
        agreed = collections.Counter(reference)
        for tokens in source_tokens:
            agreed &= collections.Counter(tokens)
        output_counts = collections.Counter(hyp)
        row_dropped = sum(max(0, count - output_counts[token]) for token, count in agreed.items())
        invented += row_invented
        dropped += row_dropped
        records_invented += row_invented > 0
        records_dropped += row_dropped > 0
        fallback += row.get("raw_status") in ("rejected", "failed")
        for name, tokens in (("model", hyp), *zip(sources, source_tokens)):
            hits, total = _critical_hits(record, tokens, mapping)
            crit[name][0] += hits
            crit[name][1] += total
        per_record.append({
            "record_id": row["record_id"], "language": record["language"], "repeat": row["repeat"],
            "reference_tokens": model_result["reference_tokens"], "distance_model": model_result["distance"],
            **{f"distance_{source}": distance for source, distance in distances.items()},
            "verdict": verdict, "invented_tokens": row_invented, "dropped_agreed_tokens": row_dropped,
            "outcome": row.get("raw_status"), "latency_ms": row.get("latency_ms"),
        })
    return {
        "samples": len(rows),
        "wer": {name: pool.wer() for name, pool in pools.items()},
        "verdicts": {key: verdicts.get(key, 0) for key in ("better", "worse", "unchanged")},
        "invented_tokens": invented, "records_with_invented": records_invented,
        "dropped_agreed_tokens": dropped, "records_with_dropped": records_dropped,
        "fallback_count": fallback,
        "critical_token_hit_rate": {name: (hits / total if total else None) for name, (hits, total) in crit.items()},
        "critical_token_total": crit["model"][1],
        "_per_record": per_record,
    }


def score_report(corpus: Mapping[str, Any], llm_run: Mapping[str, Any], stt_run: Mapping[str, Any],
                 deadline_ms: float, normalizer_path: Path | None = None) -> dict[str, Any]:
    _, mapping = benchmark.load_spelling_normalizer(normalizer_path)
    corpus_by_id = {record["record_id"]: record for record in corpus.get("records", [])}
    task = llm_run.get("task")
    sources = [llm_run["source_a"]] + ([llm_run["source_b"]] if llm_run.get("source_b") else [])
    texts = _load_stt_texts(stt_run)
    models: dict[str, Any] = {}
    per_record: list[dict[str, Any]] = []
    for model in sorted({row["model"] for row in llm_run.get("rows", [])}):
        model_rows = [row for row in llm_run["rows"] if row["model"] == model and row["record_id"] in corpus_by_id]
        ok_rows = []
        source_texts: dict[str, list[str | None]] = {}
        for row in model_rows:
            if row["status"] != "ok":
                continue
            found = [texts.get((source, row["record_id"])) for source in sources]
            if any(text is None for text in found):
                continue
            source_texts[row["record_id"]] = found
            ok_rows.append(row)
        strata: dict[str, Any] = {}
        for name in (*SCORED_LANGUAGES, "all"):
            subset = [row for row in ok_rows if name == "all" or corpus_by_id[row["record_id"]]["language"] == name]
            if not subset:
                continue
            result = _stratum(subset, corpus_by_id, source_texts, sources, mapping)
            for item in result.pop("_per_record"):
                if name == "all":
                    per_record.append({"model": model, **item})
            strata[name] = result
        latencies = [row["latency_ms"] for row in ok_rows if row.get("latency_ms") is not None]
        completion = [row["completion_tokens"] for row in ok_rows if row.get("completion_tokens") is not None]
        reasoning = [row["reasoning_tokens"] for row in ok_rows if row.get("reasoning_tokens") is not None]
        errors = [row for row in model_rows if row["status"] != "ok"]
        models[model] = {
            "rows": len(model_rows), "ok": len(ok_rows), "errors": len(errors),
            "timeouts": sum(1 for row in errors if row.get("raw_status") == "timeout"),
            "retries_429": sum(row.get("retries") or 0 for row in model_rows),
            "latency_ms": {
                "p50": benchmark._percentile(latencies, 0.50), "p90": benchmark._percentile(latencies, 0.90),
                "max": max(latencies) if latencies else None, "sample_count": len(latencies),
                "over_deadline": sum(1 for value in latencies if value > deadline_ms),
            },
            "mean_completion_tokens": _mean(completion), "mean_reasoning_tokens": _mean(reasoning),
            "strata": strata,
        }
    return {
        "schema_version": REPORT_SCHEMA_VERSION, "kind": "llm_bench_report", "task": task,
        "sources": sources, "deadline_ms": deadline_ms, "models": models, "per_record": per_record,
    }


def _fmt(value: float | None, digits: int = 3) -> str:
    return "-" if value is None else f"{value:.{digits}f}"


def render_table(report: Mapping[str, Any]) -> str:
    sources = report["sources"]
    header = ["model", "stratum", "n", "WER", *[f"WER:{s}" for s in sources], "better/worse/same",
              "invented", "dropped", "crit", "fallback", "err", "p50", "p90", "max", "ctok"]
    lines = [header]
    for model, data in report["models"].items():
        for name, stratum in data["strata"].items():
            latency = data["latency_ms"]
            crit = stratum["critical_token_hit_rate"]["model"]
            v = stratum["verdicts"]
            lines.append([
                model, name, str(stratum["samples"]), _fmt(stratum["wer"]["model"]),
                *[_fmt(stratum["wer"][s]) for s in sources],
                f"{v['better']}/{v['worse']}/{v['unchanged']}",
                str(stratum["invented_tokens"]), str(stratum["dropped_agreed_tokens"]),
                _fmt(crit, 2), str(stratum["fallback_count"]), str(data["errors"]),
                _fmt(latency["p50"], 0), _fmt(latency["p90"], 0), _fmt(latency["max"], 0),
                _fmt(data["mean_completion_tokens"], 1),
            ])
    widths = [max(len(row[i]) for row in lines) for i in range(len(header))]
    return "\n".join("  ".join(cell.ljust(widths[i]) for i, cell in enumerate(row)).rstrip() for row in lines)


def score_command(args: argparse.Namespace) -> int:
    report = score_report(
        benchmark.read_json(Path(args.corpus)), benchmark.read_json(Path(args.llm_run)),
        benchmark.read_json(Path(args.stt_run)), args.deadline_ms,
        Path(args.normalization) if args.normalization else None,
    )
    benchmark.write_json(Path(args.output), report)
    print(render_table(report))
    return 0


# ------------------------------------------------------------------ CLI


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    sub = parser.add_subparsers(dest="command", required=True)
    run = sub.add_parser("run", help="send the production request to each model")
    run.add_argument("--corpus", required=True)
    run.add_argument("--stt-run", required=True)
    run.add_argument("--task", choices=sorted(TASKS), required=True)
    run.add_argument("--source-a", required=True, help="STT candidate id playing the Deepgram role")
    run.add_argument("--source-b", help="STT candidate id playing the Groq role (intent-reconstruction)")
    run.add_argument("--model", action="append", required=True, help=f"one of: {', '.join(MODELS)}")
    run.add_argument("--output", required=True, help="LLM_RUN.json (holds text; must be git-ignored)")
    run.add_argument("--split", choices=("development", "heldout", "all"), default="all")
    run.add_argument("--limit", type=int, default=0)
    run.add_argument("--repeats", type=int, default=1)
    run.add_argument("--timeout-seconds", type=float, default=15.0)
    run.add_argument("--min-interval-ms", type=float, default=0.0)
    run.add_argument("--rust-cmd", help="override the Rust helper command (the --task flag is appended)")
    run.set_defaults(func=run_command)
    score = sub.add_parser("score", help="score a run against the corpus references")
    score.add_argument("--corpus", required=True)
    score.add_argument("--llm-run", required=True)
    score.add_argument("--stt-run", required=True)
    score.add_argument("--output", required=True, help="REPORT.json (no transcript text)")
    score.add_argument("--normalization", help="optional spelling normalization JSON, as benchmark.py")
    score.add_argument("--deadline-ms", type=float, default=5000.0, help="latency above this counts as over deadline")
    score.set_defaults(func=score_command)
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    args = _parser().parse_args(argv)
    try:
        return args.func(args)
    except BenchmarkError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
