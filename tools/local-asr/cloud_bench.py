#!/usr/bin/env python3
"""Private WER and latency benchmark for cloud speech-to-text models.

`transcribe` uploads the frozen corpus audio (see `benchmark.py freeze`) to each
candidate provider and records the returned text and wall-clock latency in a
RUN file. `score` turns a RUN file into a numeric REPORT using the same tokenizer
and WER as the local benchmark. The RUN file holds hypothesis text, so it is
refused unless it is outside any git work tree or gitignored. The REPORT holds
aggregates and numeric per-record results only.

Stdlib only. API keys come from environment variables and are never written to
disk. Provider base URLs can be overridden with CLOUD_BENCH_<PROVIDER>_URL.

Adding a candidate is one `Candidate(...)` entry in CANDIDATES plus one small
function `fn(job) -> str` that builds the request(s) with `job.call` and returns
the transcript text.
"""

from __future__ import annotations

import argparse
import base64
import concurrent.futures
from dataclasses import dataclass
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import sys
import threading
import time
from typing import Any, Callable, Mapping, Sequence
import urllib.error
import urllib.parse
import urllib.request
import uuid

sys.path.insert(0, str(Path(__file__).resolve().parent))
import benchmark  # noqa: E402  (sibling module; scorer is reused, not reimplemented)

RUN_SCHEMA_VERSION = 1
RUN_KIND = "cloud_bench_run"
REPORT_KIND = "cloud_bench_report"
MAX_RETRIES = 3
RETRY_AFTER_CAP_S = 60.0
ERROR_CHARS = 200

_sleep = time.sleep  # patched in tests


class CloudBenchError(ValueError):
    """A user-actionable configuration or evidence error."""


class ProviderError(Exception):
    def __init__(self, message: str, http_status: int | None = None):
        super().__init__(message)
        self.http_status = http_status


# --- HTTP -------------------------------------------------------------------


def multipart(fields: Sequence[tuple[str, str]], file: tuple[str, str, str, bytes]) -> tuple[bytes, str]:
    """Encode form fields plus one file part as multipart/form-data."""

    boundary = f"----cloudbench{uuid.uuid4().hex}"
    parts: list[bytes] = []
    for name, value in fields:
        parts.append(f'--{boundary}\r\nContent-Disposition: form-data; name="{name}"\r\n\r\n{value}\r\n'.encode())
    name, filename, content_type, data = file
    parts.append(f'--{boundary}\r\nContent-Disposition: form-data; name="{name}"; filename="{filename}"\r\nContent-Type: {content_type}\r\n\r\n'.encode() + data + b"\r\n")
    parts.append(f"--{boundary}--\r\n".encode())
    return b"".join(parts), f"multipart/form-data; boundary={boundary}"


def _retry_after(headers: Any, attempt: int) -> float:
    raw = headers.get("Retry-After") if headers else None
    try:
        wait = float(raw)
    except (TypeError, ValueError):
        wait = float(attempt)  # no usable header: linear backoff
    return max(0.0, min(wait, RETRY_AFTER_CAP_S))


class Call:
    """HTTP for one transcription. `latency_ms` sums the final attempt of each
    request (a multi-step provider reports total time); the wait before a 429
    retry and the failed attempt itself are excluded."""

    def __init__(self, timeout: float, secrets: Sequence[str]):
        self.timeout = timeout
        self.secrets = [s for s in secrets if s]
        self.latency_s = 0.0
        self.retries = 0
        self.retry_wait_s = 0.0
        self.http_status: int | None = None

    def redact(self, text: str) -> str:
        for secret in self.secrets:
            text = text.replace(secret, "[redacted]")
        return text

    def request(self, method: str, url: str, headers: Mapping[str, str], body: bytes | None = None) -> bytes:
        for attempt in range(1, MAX_RETRIES + 2):
            req = urllib.request.Request(url, data=body, method=method, headers=dict(headers))
            started = time.monotonic()
            status, payload, response_headers = self._send(req)
            elapsed = time.monotonic() - started
            self.http_status = status
            if status == 429 and attempt <= MAX_RETRIES:
                wait = _retry_after(response_headers, attempt)
                self.retries += 1
                self.retry_wait_s += wait
                _sleep(wait)
                continue
            self.latency_s += elapsed
            if 200 <= status < 300:
                return payload
            detail = payload.decode("utf-8", "replace").strip().replace("\n", " ")[:ERROR_CHARS]
            raise ProviderError(self.redact(f"HTTP {status}: {detail}" if detail else f"HTTP {status}"), status)
        raise AssertionError("unreachable")

    def _send(self, req: urllib.request.Request) -> tuple[int, bytes, Any]:
        try:
            with urllib.request.urlopen(req, timeout=self.timeout) as response:
                return response.status, response.read(), response.headers
        except urllib.error.HTTPError as exc:
            return exc.code, exc.read(), exc.headers
        except (urllib.error.URLError, socket.timeout, TimeoutError, ConnectionError, OSError) as exc:
            raise ProviderError(self.redact(f"request failed: {getattr(exc, 'reason', exc)}")) from exc

    def json(self, method: str, url: str, headers: Mapping[str, str], body: bytes | None = None) -> Any:
        payload = self.request(method, url, headers, body)
        try:
            return json.loads(payload)
        except json.JSONDecodeError as exc:
            raise ProviderError("response was not JSON", self.http_status) from exc


# --- candidates -------------------------------------------------------------


@dataclass(frozen=True)
class Job:
    candidate: "Candidate"
    call: Call
    audio: bytes
    upload_format: str  # "wav" or "flac"
    language: str  # "" means provider auto-detect

    @property
    def content_type(self) -> str:
        return f"audio/{self.upload_format}"

    @property
    def filename(self) -> str:
        return f"recording.{self.upload_format}"

    def key(self, name: str) -> str:
        return os.environ[name]

    def url(self, path: str) -> str:
        return self.candidate.base_url().rstrip("/") + path


@dataclass(frozen=True)
class Candidate:
    id: str
    provider: str
    model: str
    keys: tuple[str, ...]
    default_base_url: str
    fn: Callable[[Job], str]
    mode: str = "request_response"
    wav_only: bool = False  # provider has no documented FLAC input

    def base_url(self) -> str:
        return os.environ.get(f"CLOUD_BENCH_{self.provider.upper()}_URL", self.default_base_url)


def _groq(job: Job) -> str:
    # Mirrors production (crates/voisu-cloud/src/provider_http.rs): verbose_json
    # with word and segment timestamps, temperature 0, explicit language. The
    # production vocabulary prompt is per-user, so none is sent here.
    fields = [("model", job.candidate.model), ("response_format", "verbose_json"), ("timestamp_granularities[]", "word"), ("timestamp_granularities[]", "segment"), ("temperature", "0")]
    if job.language:
        fields.append(("language", job.language))
    body, content_type = multipart(fields, ("file", job.filename, job.content_type, job.audio))
    data = job.call.json("POST", job.url("/audio/transcriptions"), {"Authorization": f"Bearer {job.key('GROQ_API_KEY')}", "Content-Type": content_type}, body)
    return data["text"]


def _deepgram(job: Job) -> str:
    # Production streams over WebSocket (crates/voisu-cloud/src/deepgram.rs);
    # this is the prerecorded endpoint with the same formatting parameters, so
    # its latency is NOT streaming latency.
    params = {"model": job.candidate.model, "smart_format": "true", "punctuate": "true"}
    if job.language:
        params["language"] = job.language
    url = job.url("/v1/listen") + "?" + urllib.parse.urlencode(params)
    data = job.call.json("POST", url, {"Authorization": f"Token {job.key('DEEPGRAM_API_KEY')}", "Content-Type": job.content_type}, job.audio)
    return data["results"]["channels"][0]["alternatives"][0]["transcript"]


def _elevenlabs(job: Job) -> str:
    # https://elevenlabs.io/docs/api-reference/speech-to-text/convert
    fields = [("model_id", job.candidate.model), ("tag_audio_events", "false")]
    if job.language:
        fields.append(("language_code", job.language))
    body, content_type = multipart(fields, ("file", job.filename, job.content_type, job.audio))
    data = job.call.json("POST", job.url("/v1/speech-to-text"), {"xi-api-key": job.key("ELEVENLABS_API_KEY"), "Content-Type": content_type}, body)
    return data["text"]


def _assemblyai(job: Job) -> str:
    # https://www.assemblyai.com/docs/api-reference/files/upload and /transcripts
    # Upload, create, then poll; latency is the total of all steps.
    auth = {"Authorization": job.key("ASSEMBLYAI_API_KEY")}
    upload = job.call.json("POST", job.url("/v2/upload"), {**auth, "Content-Type": "application/octet-stream"}, job.audio)
    request = {"audio_url": upload["upload_url"], "speech_models": [job.candidate.model]}
    if job.language:
        request["language_code"] = job.language
    created = job.call.json("POST", job.url("/v2/transcript"), {**auth, "Content-Type": "application/json"}, json.dumps(request).encode())
    poll_url = job.url(f"/v2/transcript/{created['id']}")
    deadline = time.monotonic() + job.call.timeout * 10
    while True:
        data = job.call.json("GET", poll_url, auth)
        if data.get("status") == "completed":
            return data.get("text") or ""
        if data.get("status") == "error":
            raise ProviderError(job.call.redact(f"transcript failed: {str(data.get('error'))[:ERROR_CHARS]}"), job.call.http_status)
        if time.monotonic() > deadline:
            raise ProviderError("transcript polling timed out")
        started = time.monotonic()
        _sleep(float(os.environ.get("CLOUD_BENCH_POLL_SECONDS", "0.5")))
        job.call.latency_s += time.monotonic() - started  # polling delay is real latency


def _cloudflare(job: Job) -> str:
    # https://developers.cloudflare.com/workers-ai/models/whisper-large-v3-turbo/
    # `audio` is a base64 string; the REST envelope puts the output in `result`.
    request: dict[str, Any] = {"audio": base64.b64encode(job.audio).decode("ascii")}
    if job.language:
        request["language"] = job.language
    path = f"/accounts/{job.key('CLOUDFLARE_ACCOUNT_ID')}/ai/run/@cf/openai/{job.candidate.model}"
    data = job.call.json("POST", job.url(path), {"Authorization": f"Bearer {job.key('CLOUDFLARE_API_TOKEN')}", "Content-Type": "application/json"}, json.dumps(request).encode())
    return (data.get("result") or data)["text"]


_GROQ_URL, _DG_URL, _EL_URL = "https://api.groq.com/openai/v1", "https://api.deepgram.com", "https://api.elevenlabs.io"
_AAI_URL, _CF_URL = "https://api.assemblyai.com", "https://api.cloudflare.com/client/v4"
_CANDIDATE_LIST = (
    Candidate("groq-whisper-large-v3", "groq", "whisper-large-v3", ("GROQ_API_KEY",), _GROQ_URL, _groq),
    Candidate("groq-whisper-large-v3-turbo", "groq", "whisper-large-v3-turbo", ("GROQ_API_KEY",), _GROQ_URL, _groq),
    Candidate("deepgram-nova-3", "deepgram", "nova-3", ("DEEPGRAM_API_KEY",), _DG_URL, _deepgram, mode="prerecorded"),
    Candidate("elevenlabs-scribe-v2", "elevenlabs", "scribe_v2", ("ELEVENLABS_API_KEY",), _EL_URL, _elevenlabs),
    Candidate("assemblyai-universal", "assemblyai", "universal-2", ("ASSEMBLYAI_API_KEY",), _AAI_URL, _assemblyai, mode="upload_poll"),
    Candidate("assemblyai-universal-3-5-pro", "assemblyai", "universal-3-5-pro", ("ASSEMBLYAI_API_KEY",), _AAI_URL, _assemblyai, mode="upload_poll"),
    Candidate("cloudflare-whisper-large-v3-turbo", "cloudflare", "whisper-large-v3-turbo", ("CLOUDFLARE_API_TOKEN", "CLOUDFLARE_ACCOUNT_ID"), _CF_URL, _cloudflare, wav_only=True),
)
CANDIDATES: dict[str, Candidate] = {candidate.id: candidate for candidate in _CANDIDATE_LIST}


# --- run file ---------------------------------------------------------------


def ensure_private_path(path: Path) -> None:
    """Refuse a path inside a git work tree unless git reports it ignored."""

    parent = path.resolve().parent
    while not parent.exists() and parent != parent.parent:
        parent = parent.parent
    try:
        inside = subprocess.run(["git", "-C", str(parent), "rev-parse", "--is-inside-work-tree"], capture_output=True, text=True)
    except OSError:
        return  # no git binary, so no work tree to leak into
    if inside.returncode != 0 or inside.stdout.strip() != "true":
        return
    resolved = path.resolve()
    ignored = subprocess.run(["git", "-C", str(parent), "check-ignore", "-q", "--", str(resolved)], capture_output=True)
    if ignored.returncode != 0:
        raise CloudBenchError(f"refusing to write {path}: it is inside a git work tree and not gitignored (the run file holds transcript text); use a path outside the repository")


def atomic_write_json(path: Path, value: Any) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_name(f".{path.name}.{os.getpid()}.tmp")
    fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(fd, "w", encoding="utf-8") as handle:
        handle.write(json.dumps(value, ensure_ascii=False, indent=2) + "\n")
    os.replace(tmp, path)


def _row_key(row: Mapping[str, Any]) -> tuple[str, str, int]:
    return row["record_id"], row["candidate"], int(row.get("repeat", 0))


# --- transcribe -------------------------------------------------------------


class Throttle:
    def __init__(self, min_interval_s: float):
        self.min_interval_s = min_interval_s
        self.lock = threading.Lock()
        self.next_start = 0.0

    def wait(self) -> None:
        if self.min_interval_s <= 0:
            return
        with self.lock:
            delay = self.next_start - time.monotonic()
            if delay > 0:
                _sleep(delay)
            self.next_start = time.monotonic() + self.min_interval_s


def _encode_flac(wav_path: Path) -> bytes:
    result = subprocess.run(["flac", "--silent", "--stdout", "--force", str(wav_path)], capture_output=True)
    if result.returncode != 0 or not result.stdout:
        raise ProviderError("flac encode failed")
    return result.stdout


def _upload_audio(candidate: Candidate, wav_path: Path, upload_format: str) -> tuple[bytes, str]:
    want_flac = upload_format == "flac" or (upload_format == "auto" and shutil.which("flac") is not None)
    if want_flac and not candidate.wav_only:
        return _encode_flac(wav_path), "flac"
    return wav_path.read_bytes(), "wav"


def _transcribe_one(candidate: Candidate, record: Mapping[str, Any], repeat: int, args: argparse.Namespace, throttle: Throttle, secrets: Sequence[str]) -> dict[str, Any]:
    row: dict[str, Any] = {"record_id": record["record_id"], "candidate": candidate.id, "repeat": repeat, "mode": candidate.mode, "split": record["split"], "language": record["language"], "audio_duration_ms": record["duration_ms"], "text": None, "status": "error", "error": None, "latency_ms": None, "http_status": None, "retries": 0, "retry_wait_ms": 0, "upload_format": None}
    call = Call(args.timeout_seconds, secrets)
    try:
        audio, upload_format = _upload_audio(candidate, Path(record["audio_path"]), args.upload_format)
        row["upload_format"] = upload_format
        throttle.wait()
        job = Job(candidate, call, audio, upload_format, "" if args.stt_language == "auto" else args.stt_language)
        text = candidate.fn(job)
        if not isinstance(text, str):
            raise ProviderError("response had no text", call.http_status)
        row.update(text=text, status="ok")
    except ProviderError as exc:
        row["error"] = str(exc)
    except (KeyError, IndexError, TypeError) as exc:
        row["error"] = call.redact(f"unexpected response shape: {type(exc).__name__} {exc}")[:ERROR_CHARS]
    row["http_status"] = call.http_status
    row["latency_ms"] = round(call.latency_s * 1000, 1)
    row["retries"] = call.retries
    row["retry_wait_ms"] = round(call.retry_wait_s * 1000, 1)
    return row


def _select_records(corpus: Mapping[str, Any], split: str, language: str, limit: int | None) -> list[dict[str, Any]]:
    records = [r for r in corpus["records"] if split in ("all", r["split"]) and language in ("all", r["language"])]
    return records[:limit] if limit else records


def transcribe(args: argparse.Namespace) -> int:
    corpus = benchmark.read_json(Path(args.corpus))
    benchmark.validate_frozen(corpus)
    unknown = [c for c in args.candidate if c not in CANDIDATES]
    if unknown:
        raise CloudBenchError(f"unknown candidate {unknown[0]}; available: {', '.join(CANDIDATES)}")
    candidates = [CANDIDATES[c] for c in dict.fromkeys(args.candidate)]
    for candidate in candidates:
        missing = [name for name in candidate.keys if not os.environ.get(name)]
        if missing:
            raise CloudBenchError(f"{candidate.id} needs {' and '.join(missing)} set in the environment")
    output = Path(args.output)
    ensure_private_path(output)
    secrets = [os.environ[name] for candidate in candidates for name in candidate.keys]

    rows: dict[tuple[str, str, int], dict[str, Any]] = {}
    if output.exists():
        previous = benchmark.read_json(output)
        if previous.get("kind") != RUN_KIND or previous.get("corpus_manifest_sha256") != corpus["manifest_sha256"]:
            raise CloudBenchError(f"{output} is not a run file for this corpus; refusing to resume into it")
        rows = {_row_key(row): row for row in previous["rows"] if row.get("status") == "ok"}

    records = _select_records(corpus, args.split, args.language, args.limit)
    todo = [(candidate, record, repeat) for repeat in range(args.repeats) for record in records for candidate in candidates if (record["record_id"], candidate.id, repeat) not in rows]
    throttles = {candidate.id: Throttle(args.min_interval_ms / 1000) for candidate in candidates}

    def save() -> None:
        ordered = sorted(rows.values(), key=lambda r: (r["candidate"], r["repeat"], r["record_id"]))
        atomic_write_json(output, {"schema_version": RUN_SCHEMA_VERSION, "kind": RUN_KIND, "corpus_id": corpus["corpus_id"], "corpus_manifest_sha256": corpus["manifest_sha256"], "settings": {"stt_language": args.stt_language, "upload_format": args.upload_format}, "rows": ordered})

    save()
    failures = 0
    pool = concurrent.futures.ThreadPoolExecutor(max_workers=max(1, args.concurrency))
    try:
        futures = [pool.submit(_transcribe_one, c, r, n, args, throttles[c.id], secrets) for c, r, n in todo]
        for future in concurrent.futures.as_completed(futures):
            row = future.result()
            rows[_row_key(row)] = row
            failures += row["status"] != "ok"
            save()
    finally:
        pool.shutdown(wait=True, cancel_futures=True)
    print(f"{len(todo)} requests, {failures} failed; wrote {output}", file=sys.stderr)
    return 1 if failures else 0


# --- score ------------------------------------------------------------------

_COUNT_KEYS = ("distance", "deletions", "substitutions", "insertions", "reference_tokens")


def _contains(hypothesis: Sequence[str], needle: Sequence[str]) -> bool:
    n = len(needle)
    return n > 0 and any(list(hypothesis[i : i + n]) == list(needle) for i in range(len(hypothesis) - n + 1))


def _score_row(row: Mapping[str, Any], record: Mapping[str, Any], mapping: Mapping[str, str]) -> dict[str, Any]:
    reference = benchmark._map_tokens(benchmark.strict_tokens(record["reference"]), mapping)
    hypothesis = benchmark._map_tokens(benchmark.strict_tokens(row["text"]), mapping)
    result = benchmark.wer(reference, hypothesis)
    critical = record.get("critical_tokens") or []
    critical_ok = all(_contains(hypothesis, benchmark._map_tokens(benchmark.strict_tokens(token), mapping)) for token in critical) if critical else None
    return {**{key: result[key] for key in _COUNT_KEYS}, "hypothesis_tokens": result["hypothesis_tokens"], "critical_ok": critical_ok}


def _stratum(rows: Sequence[Mapping[str, Any]], scored: Mapping[int, Mapping[str, Any]], scored_norm: Mapping[int, Mapping[str, Any]] | None) -> dict[str, Any]:
    ok = [r for r in rows if r["status"] == "ok"]
    total = {key: sum(scored[id(r)][key] for r in ok) for key in _COUNT_KEYS}
    out: dict[str, Any] = {"records": len({r["record_id"] for r in rows}), "rows": len(rows), "errors": len(rows) - len(ok), **total, "wer": _pooled(total)}
    if scored_norm is not None:
        norm = {key: sum(scored_norm[id(r)][key] for r in ok) for key in _COUNT_KEYS}
        out["spelling_normalized"] = {**norm, "wer": _pooled(norm)}
    latency = [float(r["latency_ms"]) for r in ok if isinstance(r.get("latency_ms"), (int, float))]
    out["latency_ms"] = {"p50": benchmark._percentile(latency, 0.50), "p90": benchmark._percentile(latency, 0.90), "max": max(latency) if latency else None}
    rtfs = [r["latency_ms"] / r["audio_duration_ms"] for r in ok if r.get("audio_duration_ms") and isinstance(r.get("latency_ms"), (int, float))]
    out["rtf"] = {"p50": benchmark._percentile(rtfs, 0.50), "p90": benchmark._percentile(rtfs, 0.90)}
    return out


def _pooled(total: Mapping[str, int]) -> float:
    # Same convention as benchmark._score_field's finish().
    if total["reference_tokens"]:
        return total["distance"] / total["reference_tokens"]
    return 1.0 if total["insertions"] else 0.0


def build_report(corpus: Mapping[str, Any], run: Mapping[str, Any], normalizer_path: Path | None) -> dict[str, Any]:
    version, mapping = benchmark.load_spelling_normalizer(normalizer_path)
    by_id = {record["record_id"]: record for record in corpus["records"]}
    grouped: dict[str, list[Mapping[str, Any]]] = {}
    for row in run["rows"]:
        if row["record_id"] not in by_id:
            raise CloudBenchError(f"run references unknown record {row['record_id']}")
        grouped.setdefault(row["candidate"], []).append(row)
    candidates: dict[str, Any] = {}
    for candidate_id, rows in sorted(grouped.items()):
        record_of = lambda r: by_id[r["record_id"]]  # noqa: E731
        scored = {id(r): _score_row(r, record_of(r), {}) for r in rows if r["status"] == "ok"}
        scored_norm = {id(r): _score_row(r, record_of(r), mapping) for r in rows if r["status"] == "ok"} if mapping else None

        def stratum(subset: Sequence[Mapping[str, Any]]) -> dict[str, Any]:
            return _stratum(subset, scored, scored_norm)

        speech = [r for r in rows if record_of(r)["language"] != "negative"]
        negative_ok = [r for r in rows if record_of(r)["language"] == "negative" and r["status"] == "ok"]
        with_output = [r for r in negative_ok if scored[id(r)]["hypothesis_tokens"] > 0]
        critical = [r for r in rows if r["status"] == "ok" and scored[id(r)]["critical_ok"] is not None]
        present = sum(1 for r in critical if scored[id(r)]["critical_ok"])
        candidates[candidate_id] = {
            # Same pooling as benchmark.py: every record, negatives contribute insertions.
            "overall": stratum(rows),
            "speech": stratum(speech),
            "by_language": {lang: stratum([r for r in rows if record_of(r)["language"] == lang]) for lang in ("english", "hinglish", "negative")},
            "by_split": {split: stratum([r for r in rows if record_of(r)["split"] == split]) for split in ("development", "heldout")},
            "negative": {"rows": len([r for r in rows if record_of(r)["language"] == "negative"]), "ok_rows": len(negative_ok), "rows_with_output": len(with_output), "inserted_tokens": sum(scored[id(r)]["hypothesis_tokens"] for r in negative_ok), "hallucination_rate": (len(with_output) / len(negative_ok)) if negative_ok else None},
            "critical_tokens": {"records": len(critical), "all_present": present, "fraction": (present / len(critical)) if critical else None},
            "modes": sorted({r.get("mode") for r in rows if r.get("mode")}),
            "upload_formats": sorted({r.get("upload_format") for r in rows if r.get("upload_format")}),
            "retries": sum(int(r.get("retries") or 0) for r in rows),
            "records": [
                {"record_id": r["record_id"], "repeat": r.get("repeat", 0), "language": record_of(r)["language"], "split": record_of(r)["split"], "status": r["status"], "latency_ms": r.get("latency_ms"), "audio_duration_ms": r.get("audio_duration_ms"), **({k: scored[id(r)][k] for k in (*_COUNT_KEYS, "critical_ok")} if r["status"] == "ok" else {})}
                for r in sorted(rows, key=lambda r: (r.get("repeat", 0), r["record_id"]))
            ],
        }
    return {"schema_version": RUN_SCHEMA_VERSION, "kind": REPORT_KIND, "corpus_id": corpus["corpus_id"], "corpus_manifest_sha256": corpus["manifest_sha256"], "normalization": {"strict": "unicode-nfkc-casefold-token-v1", "spelling_normalized_version": version}, "candidates": candidates}


def _fmt(value: Any, spec: str, scale: float = 1.0) -> str:
    return "-" if value is None else format(value * scale, spec)


def format_table(report: Mapping[str, Any]) -> str:
    header = f"{'candidate':<34}{'stratum':<13}{'n':>4}{'err':>4}{'WER%':>7}{'S/D/I':>11}{'p50ms':>8}{'p90ms':>8}{'maxms':>8}{'RTFp50':>8}"
    lines = [header, "-" * len(header)]
    for candidate_id, cand in report["candidates"].items():
        strata = [("overall", cand["overall"]), ("speech", cand["speech"]), *[(k, v) for k, v in cand["by_language"].items() if v["rows"]], *[(k, v) for k, v in cand["by_split"].items() if v["rows"]]]
        for name, s in strata:
            sdi = f"{s['substitutions']}/{s['deletions']}/{s['insertions']}"
            lines.append(f"{candidate_id:<34}{name:<13}{s['records']:>4}{s['errors']:>4}{_fmt(s['wer'], '.1f', 100):>7}{sdi:>11}{_fmt(s['latency_ms']['p50'], '.0f'):>8}{_fmt(s['latency_ms']['p90'], '.0f'):>8}{_fmt(s['latency_ms']['max'], '.0f'):>8}{_fmt(s['rtf']['p50'], '.2f'):>8}")
        neg, crit = cand["negative"], cand["critical_tokens"]
        lines.append(f"{candidate_id:<34}negative: {neg['rows_with_output']}/{neg['ok_rows']} rows with output, {neg['inserted_tokens']} inserted tokens; critical tokens: {crit['all_present']}/{crit['records']} records ({_fmt(crit['fraction'], '.0f', 100)}%)")
        lines.append("")
    return "\n".join(lines).rstrip()


def score(args: argparse.Namespace) -> int:
    corpus = benchmark.read_json(Path(args.corpus))
    benchmark.validate_frozen(corpus)
    run = benchmark.read_json(Path(args.run))
    if run.get("kind") != RUN_KIND or run.get("corpus_manifest_sha256") != corpus["manifest_sha256"]:
        raise CloudBenchError("run file belongs to a different corpus or is not a cloud_bench run")
    report = build_report(corpus, run, Path(args.normalization) if args.normalization else None)
    benchmark.write_json(Path(args.output), report)
    print(format_table(report))
    return 0


# --- CLI --------------------------------------------------------------------


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    sub = parser.add_subparsers(dest="command", required=True)
    t = sub.add_parser("transcribe", help="send corpus audio to cloud providers and record text and latency")
    t.add_argument("--corpus", required=True)
    t.add_argument("--candidate", action="append", required=True, help=f"one of: {', '.join(CANDIDATES)}")
    t.add_argument("--output", required=True, help="RUN.json (holds transcript text; must be ignored or outside the repo)")
    t.add_argument("--split", choices=("development", "heldout", "all"), default="all")
    t.add_argument("--language", choices=("english", "hinglish", "negative", "all"), default="all", help="corpus record filter")
    t.add_argument("--limit", type=int)
    t.add_argument("--repeats", type=int, default=1)
    t.add_argument("--concurrency", type=int, default=1)
    t.add_argument("--timeout-seconds", type=float, default=30.0)
    t.add_argument("--min-interval-ms", type=float, default=0.0, help="minimum spacing between request starts per candidate")
    t.add_argument("--stt-language", default="en", help="language sent to providers; 'auto' sends none")
    t.add_argument("--upload-format", choices=("auto", "wav", "flac"), default="auto", help="auto uses FLAC when the flac binary exists, like production")
    s = sub.add_parser("score", help="score a RUN file into a numeric REPORT")
    s.add_argument("--corpus", required=True)
    s.add_argument("--run", required=True)
    s.add_argument("--output", required=True)
    s.add_argument("--normalization")
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    args = _parser().parse_args(argv)
    try:
        return transcribe(args) if args.command == "transcribe" else score(args)
    except (CloudBenchError, benchmark.BenchmarkError) as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
