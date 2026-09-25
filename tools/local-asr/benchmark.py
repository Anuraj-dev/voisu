#!/usr/bin/env python3
"""Private, reproducible local ASR corpus and benchmark tooling.

This module deliberately has no model or audio dependencies.  It inventories
consented WAV files, freezes a content-addressed corpus, asks the protected
Voisu Rust seam to replay exact audio, and scores the returned evidence.  A
Python worker is never launched by this file.

The files produced by this tool may contain private audio paths and transcript
text.  Keep them in a private evaluation directory outside the repository.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import re
import subprocess
import sys
import time
import uuid
import wave
from typing import Any, Iterable, Mapping, Sequence
import unicodedata


SCHEMA_VERSION = 1
REPORT_SCHEMA_VERSION = 1
MAX_REFERENCE_CHARS = 64 * 1024
CORPUS_LANGUAGES = ("english", "hinglish", "negative")
EXPECTED_MODELS = (
    "nemotron-3.5-0.6b",
    "parakeet-tdt-0.6b-v3",
    "qwen3-asr-0.6b",
    "qwen3-asr-1.7b",
    "fun-asr-nano-2512",
    "moonshine-streaming-medium",
    "cohere-transcribe-03-2026",
)
MODEL_ID_RE = re.compile(r"^[a-z0-9][a-z0-9.-]{1,96}$")
SHA256_RE = re.compile(r"^[0-9a-f]{64}$")


class BenchmarkError(ValueError):
    """A user-actionable corpus, evidence, or scoring error."""


def canonical_json(value: Any) -> bytes:
    return json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":")).encode("utf-8")


def sha256_bytes(value: bytes) -> str:
    return hashlib.sha256(value).hexdigest()


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    try:
        with path.open("rb") as handle:
            for chunk in iter(lambda: handle.read(1024 * 1024), b""):
                digest.update(chunk)
    except OSError as exc:
        raise BenchmarkError(f"cannot read {path}: {exc}") from exc
    return digest.hexdigest()


def read_json(path: Path) -> Any:
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as exc:
        raise BenchmarkError(f"cannot read JSON {path}: {exc}") from exc


def write_json(path: Path, value: Any) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")


def _without_hash(value: Mapping[str, Any], field: str) -> dict[str, Any]:
    result = dict(value)
    result.pop(field, None)
    return result


def content_hash(value: Mapping[str, Any], field: str) -> str:
    return sha256_bytes(canonical_json(_without_hash(value, field)))


def _require(condition: bool, message: str) -> None:
    if not condition:
        raise BenchmarkError(message)


def _absolute_file(value: Any, field: str) -> Path:
    _require(isinstance(value, str) and value, f"{field} must be a non-empty path")
    path = Path(value)
    _require(path.is_absolute(), f"{field} must be absolute: {value}")
    _require(path.is_file(), f"{field} does not exist: {value}")
    return path


def wav_info(path: Path) -> tuple[int, int]:
    """Return (duration_ms, PCM byte count) after enforcing replay format."""

    try:
        with wave.open(str(path), "rb") as handle:
            _require(handle.getnchannels() == 1, f"{path}: WAV must be mono PCM")
            _require(handle.getsampwidth() == 2, f"{path}: WAV must be signed 16-bit PCM")
            _require(handle.getframerate() == 16_000, f"{path}: WAV must be 16 kHz")
            frames = handle.getnframes()
            return round(frames * 1000 / 16_000), frames * 2
    except (OSError, EOFError, wave.Error) as exc:
        raise BenchmarkError(f"invalid WAV {path}: {exc}") from exc


def _normalise_language(value: Any) -> str:
    aliases = {"en": "english", "english": "english", "hi-latn": "hinglish", "hinglish": "hinglish", "negative": "negative", "noise": "negative"}
    _require(isinstance(value, str) and value.lower() in aliases, f"language must be english, hinglish, or negative: {value!r}")
    return aliases[value.lower()]


def _load_refs(path: Path) -> dict[str, dict[str, Any]]:
    raw = read_json(path)
    if isinstance(raw, list):
        rows = raw
    elif isinstance(raw, dict) and isinstance(raw.get("records"), list):
        rows = raw["records"]
    elif isinstance(raw, dict):
        rows = [{"id": key, **value} for key, value in raw.items() if isinstance(value, dict)]
    else:
        raise BenchmarkError("references JSON must be a record map or a records array")
    result: dict[str, dict[str, Any]] = {}
    for row in rows:
        _require(isinstance(row, dict), "each reference row must be an object")
        key = row.get("id") or row.get("audio") or row.get("path") or row.get("file")
        _require(isinstance(key, str) and key, "reference row needs id or audio")
        _require(key not in result, f"duplicate reference id: {key}")
        result[key] = dict(row)
    return result


def _load_consent(path: Path) -> dict[str, Any]:
    raw = read_json(path)
    _require(isinstance(raw, dict), "consent receipt must be an object")
    _require(raw.get("consent_id") or raw.get("id"), "consent receipt needs consent_id")
    return raw


def _consent_for(row: Mapping[str, Any], consent: Mapping[str, Any]) -> str:
    value = row.get("consent_id") or consent.get("consent_id") or consent.get("id")
    _require(isinstance(value, str) and value.strip(), "each recording needs an explicit consent_id")
    return value.strip()


def inventory(input_dir: Path, refs_path: Path, consent_path: Path, corpus_id: str) -> dict[str, Any]:
    """Inventory consented audio and adjudicated references without copying audio."""

    _require(corpus_id.strip() and corpus_id.lower() not in {"#267", "267", "bakeoff-267"}, "the frozen #267 corpus is immutable and cannot be imported")
    root = input_dir.resolve()
    _require(root.is_dir(), f"input directory does not exist: {root}")
    refs = _load_refs(refs_path)
    consent = _load_consent(consent_path)
    wavs = sorted((p for p in root.rglob("*") if p.is_file() and p.suffix.lower() == ".wav"), key=lambda p: p.relative_to(root).as_posix())
    _require(wavs, f"no WAV files found under {root}")
    records: list[dict[str, Any]] = []
    seen_ids: set[str] = set()
    for audio in wavs:
        rel = audio.relative_to(root).as_posix()
        row = refs.get(rel) or refs.get(audio.name) or refs.get(audio.stem)
        _require(row is not None, f"no adjudicated reference for {rel}")
        record_id = str(row.get("record_id") or row.get("id") or rel)
        _require(record_id not in seen_ids, f"duplicate record id: {record_id}")
        seen_ids.add(record_id)
        language = _normalise_language(row.get("language"))
        reference = row.get("reference", "")
        _require(isinstance(reference, str), f"reference for {record_id} must be text")
        _require(len(reference) <= MAX_REFERENCE_CHARS, f"reference for {record_id} is too long")
        if language == "negative":
            _require(not reference.strip(), f"negative sample {record_id} must have an empty reference")
        else:
            _require(reference.strip(), f"speech sample {record_id} needs an adjudicated reference")
        duration_ms, pcm_bytes = wav_info(audio)
        tags = row.get("tags", [])
        _require(isinstance(tags, list) and all(isinstance(tag, str) for tag in tags), f"tags for {record_id} must be strings")
        critical_tokens = row.get("critical_tokens", [])
        _require(isinstance(critical_tokens, list) and all(isinstance(token, str) for token in critical_tokens), f"critical_tokens for {record_id} must be strings")
        records.append({
            "record_id": record_id,
            "audio_path": str(audio.resolve()),
            "audio_relpath": rel,
            "audio_sha256": sha256_file(audio),
            "audio_bytes": pcm_bytes,
            "duration_ms": duration_ms,
            "language": language,
            "reference": reference,
            "tags": sorted(set(tags)),
            "critical_tokens": critical_tokens,
            "consent_id": _consent_for(row, consent),
            "split": None,
        })
    source = {"schema_version": SCHEMA_VERSION, "kind": "inventory", "corpus_id": corpus_id, "records": records}
    return {**source, "inventory_sha256": content_hash(source, "inventory_sha256")}


def validate_inventory(value: Mapping[str, Any]) -> None:
    _require(value.get("schema_version") == SCHEMA_VERSION, "unsupported inventory schema")
    _require(value.get("kind") == "inventory", "expected an inventory manifest")
    expected_hash = value.get("inventory_sha256")
    _require(isinstance(expected_hash, str) and expected_hash == content_hash(value, "inventory_sha256"), "inventory hash is stale or invalid")
    records = value.get("records")
    _require(isinstance(records, list) and records, "inventory has no records")
    ids: set[str] = set()
    for record in records:
        _validate_record(record, require_split=False)
        _require(record["record_id"] not in ids, f"duplicate record id: {record['record_id']}")
        ids.add(record["record_id"])


def _validate_record(record: Any, require_split: bool = True) -> None:
    _require(isinstance(record, dict), "corpus record must be an object")
    _require(isinstance(record.get("record_id"), str) and record["record_id"], "record_id is required")
    path = _absolute_file(record.get("audio_path"), f"audio_path for {record['record_id']}")
    digest = record.get("audio_sha256")
    _require(isinstance(digest, str) and SHA256_RE.fullmatch(digest), f"audio_sha256 for {record['record_id']} is invalid")
    _require(sha256_file(path) == digest, f"audio changed since manifest freeze: {path}")
    _require(record.get("language") in CORPUS_LANGUAGES, f"invalid language for {record['record_id']}")
    _require(isinstance(record.get("reference"), str), f"reference missing for {record['record_id']}")
    if record["language"] == "negative":
        _require(not record["reference"].strip(), f"negative sample {record['record_id']} has non-empty reference")
    if require_split:
        _require(record.get("split") in {"development", "heldout"}, f"invalid split for {record['record_id']}")


def freeze(inventory_value: Mapping[str, Any], seed: str, heldout_fraction: float) -> dict[str, Any]:
    validate_inventory(inventory_value)
    _require(0 < heldout_fraction < 1, "heldout fraction must be between 0 and 1")
    rows = []
    for row in sorted(inventory_value["records"], key=lambda item: item["record_id"]):
        copy = dict(row)
        key = f"{seed}\0{copy['language']}\0{copy['record_id']}".encode("utf-8")
        bucket = int.from_bytes(hashlib.sha256(key).digest()[:8], "big") / 2**64
        copy["split"] = "heldout" if bucket < heldout_fraction else "development"
        rows.append(copy)
    # A one-item stratum cannot be both tuned and held out. Keep the split
    # deterministic and state the limitation in the manifest metadata.
    strata: dict[str, dict[str, int]] = {}
    for row in rows:
        strata.setdefault(row["language"], {}).setdefault(row["split"], 0)
        strata[row["language"]][row["split"]] += 1
    source = {
        "schema_version": SCHEMA_VERSION,
        "kind": "frozen_corpus",
        "corpus_id": inventory_value["corpus_id"],
        "seed": seed,
        "source_inventory_sha256": inventory_value["inventory_sha256"],
        "heldout_fraction": heldout_fraction,
        "strata": strata,
        "records": rows,
        "quality_evidence": False,
    }
    return {**source, "manifest_sha256": content_hash(source, "manifest_sha256")}


def validate_frozen(value: Mapping[str, Any]) -> None:
    _require(value.get("schema_version") == SCHEMA_VERSION, "unsupported frozen corpus schema")
    _require(value.get("kind") == "frozen_corpus", "expected a frozen corpus manifest")
    expected_hash = value.get("manifest_sha256")
    _require(isinstance(expected_hash, str) and expected_hash == content_hash(value, "manifest_sha256"), "corpus manifest hash is stale or invalid")
    records = value.get("records")
    _require(isinstance(records, list) and records, "frozen corpus has no records")
    ids: set[str] = set()
    for record in records:
        _validate_record(record)
        _require(record["record_id"] not in ids, f"duplicate record id: {record['record_id']}")
        ids.add(record["record_id"])


def strict_tokens(text: str) -> list[str]:
    """Tokenize without translation or transliteration.

    Case, Unicode compatibility forms, punctuation, and repeated whitespace are
    normalised.  Spoken words, numbers, and script remain the supplied tokens.
    """

    normalized = unicodedata.normalize("NFKC", text).casefold()
    return re.findall(r"[\w]+(?:['’][\w]+)?", normalized, flags=re.UNICODE)


def load_spelling_normalizer(path: Path | None) -> tuple[str | None, dict[str, str]]:
    if path is None:
        return None, {}
    raw = read_json(path)
    _require(isinstance(raw, dict), "normalization file must be an object")
    version = raw.get("version")
    mapping = raw.get("token_map")
    _require(isinstance(version, str) and version.strip(), "normalization file needs a version")
    _require(isinstance(mapping, dict) and all(isinstance(k, str) and isinstance(v, str) for k, v in mapping.items()), "token_map must map strings to strings")
    return version, {strict_tokens(key)[0]: strict_tokens(value)[0] for key, value in mapping.items() if len(strict_tokens(key)) == 1 and len(strict_tokens(value)) == 1}


def _map_tokens(tokens: Sequence[str], mapping: Mapping[str, str]) -> list[str]:
    return [mapping.get(token, token) for token in tokens]


def wer(reference: Sequence[str], hypothesis: Sequence[str]) -> dict[str, Any]:
    """Return deterministic Levenshtein WER counts, including insertions."""

    n, m = len(reference), len(hypothesis)
    dp = [[(0, 0, 0, 0) for _ in range(m + 1)] for _ in range(n + 1)]
    for i in range(1, n + 1):
        dp[i][0] = (i, i, 0, 0)
    for j in range(1, m + 1):
        dp[0][j] = (j, 0, 0, j)
    for i in range(1, n + 1):
        for j in range(1, m + 1):
            if reference[i - 1] == hypothesis[j - 1]:
                dp[i][j] = dp[i - 1][j - 1]
                continue
            deletion = dp[i - 1][j][0] + 1, dp[i - 1][j][1] + 1, dp[i - 1][j][2], dp[i - 1][j][3]
            insertion = dp[i][j - 1][0] + 1, dp[i][j - 1][1], dp[i][j - 1][2], dp[i][j - 1][3] + 1
            substitution = dp[i - 1][j - 1][0] + 1, dp[i - 1][j - 1][1], dp[i - 1][j - 1][2] + 1, dp[i - 1][j - 1][3]
            # Prefer substitution, then deletion, then insertion on ties. This
            # makes S/D/I stable without changing the total edit distance.
            dp[i][j] = min((substitution, 0), (deletion, 1), (insertion, 2), key=lambda value: (value[0][0], value[1]))[0]
    distance, deletions, substitutions, insertions = dp[n][m]
    return {"wer": (distance / n) if n else (1.0 if m else 0.0), "reference_tokens": n, "hypothesis_tokens": m, "distance": distance, "deletions": deletions, "substitutions": substitutions, "insertions": insertions}


def _percentile(values: Sequence[float], percentile: float) -> float | None:
    if not values:
        return None
    ordered = sorted(values)
    index = max(0, min(len(ordered) - 1, math.ceil(percentile * len(ordered)) - 1))
    return ordered[index]


def timing_summary(values: Sequence[float]) -> dict[str, Any]:
    return {"p50_ms": _percentile(values, 0.50), "p95_ms": _percentile(values, 0.95), "sample_count": len(values)}


def _resolve_model_manifests(values: Sequence[str]) -> list[tuple[str, Path]]:
    result = []
    for value in values:
        if "=" in value:
            model_id, raw_path = value.split("=", 1)
        else:
            raw_path = value
            manifest = read_json(Path(raw_path))
            model_id = manifest.get("model_id")
        _require(isinstance(model_id, str) and MODEL_ID_RE.fullmatch(model_id), f"invalid model id in --model-manifest: {model_id!r}")
        path = _absolute_file(raw_path, f"model manifest for {model_id}")
        result.append((model_id, path))
    _require(len({model_id for model_id, _ in result}) == len(result), "duplicate model manifests")
    return result


def build_replay_command(rust_cli: str, model_manifest: Path, audio: Path) -> list[str]:
    """Build the protected Rust invocation agreed by the local-dev seam."""

    return [rust_cli, "local-dev", "replay", "--json", "--manifest", str(model_manifest), "--audio", str(audio)]


def _validate_rust_cli(rust_cli: str, allow_fixture: bool) -> None:
    name = Path(rust_cli).name.lower()
    _require(allow_fixture or "voisu" in name, "replay must use the protected Voisu Rust CLI; pass --fixture only for synthetic tests")


def replay(corpus: Mapping[str, Any], model_manifests: Sequence[str], rust_cli: str, output: Path, timeout_s: float, fixture: bool = False) -> dict[str, Any]:
    validate_frozen(corpus)
    _validate_rust_cli(rust_cli, fixture)
    models = _resolve_model_manifests(model_manifests)
    rows: list[dict[str, Any]] = []
    run_id = str(uuid.uuid4())
    for model_id, model_manifest in models:
        private_manifest = read_json(model_manifest)
        _require(private_manifest.get("model_id") == model_id, f"model manifest identity mismatch for {model_id}")
        for record in corpus["records"]:
            command = build_replay_command(rust_cli, model_manifest, Path(record["audio_path"]))
            started = time.time()
            try:
                completed = subprocess.run(command, check=False, capture_output=True, text=True, timeout=timeout_s)
            except (OSError, subprocess.TimeoutExpired) as exc:
                response = {"status": "unavailable", "error": f"protected replay failed: {exc}"}
                completed = None
            else:
                if completed.returncode != 0:
                    response = {"status": "unavailable", "error": f"protected replay exit {completed.returncode}: {completed.stderr[-1000:]}"}
                else:
                    try:
                        response = json.loads(completed.stdout)
                    except json.JSONDecodeError as exc:
                        raise BenchmarkError(f"Rust replay returned invalid JSON for {model_id}/{record['record_id']}: {exc}") from exc
                    _require(isinstance(response, dict), f"Rust replay response must be an object for {model_id}/{record['record_id']}")
            elapsed_ms = round((time.time() - started) * 1000, 3)
            row = {
                "schema_version": SCHEMA_VERSION,
                "run_id": run_id,
                "record_id": record["record_id"],
                "audio_sha256": record["audio_sha256"],
                "corpus_manifest_sha256": corpus["manifest_sha256"],
                "language": record["language"],
                "split": record["split"],
                "model_id": model_id,
                "protected_execution": not fixture,
                "execution_seam": "fixture" if fixture else "rust-protected",
                "evidence_class": "fixture" if fixture else response.get("evidence_class", "unverified"),
                "wall_time_ms": elapsed_ms,
                "raw_response": response,
                "raw_text": response.get("raw_text", response.get("text")) if response.get("status") not in {"unavailable", "error"} else None,
                "delivered_text": response.get("delivered_text"),
            }
            for key in ("model_revision", "runtime_revision", "receipt_sha256", "artifact_hashes", "host_profile", "duration_profile", "backend", "requested_device", "observed_device", "resident", "load_ms", "inference_ms", "stop_to_transcript_ms", "stop_to_delivery_ms", "phase", "session_id", "peak_host_ram_bytes", "peak_gpu_memory_bytes", "duration_ms"):
                if key in response:
                    row[key] = response[key]
            rows.append(row)
    envelope = {
        "schema_version": SCHEMA_VERSION,
        "kind": "replay_evidence",
        "run_id": run_id,
        "corpus_manifest_sha256": corpus["manifest_sha256"],
        "models": [model_id for model_id, _ in models],
        "fixture": fixture,
        "records": rows,
        "generated_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
    }
    envelope["evidence_sha256"] = content_hash(envelope, "evidence_sha256")
    write_json(output, envelope)
    return envelope


def _load_replay_rows(path: Path) -> Mapping[str, Any]:
    raw = read_json(path)
    _require(isinstance(raw, dict) and raw.get("kind") == "replay_evidence", "expected replay evidence JSON")
    _require(raw.get("evidence_sha256") == content_hash(raw, "evidence_sha256"), "replay evidence hash is stale or invalid")
    return raw


def _require_identity(row: Mapping[str, Any], allow_fixture: bool) -> None:
    if row.get("evidence_class") == "fixture":
        _require(allow_fixture, "fixture evidence cannot be scored as quality evidence without --allow-fixture")
        return
    _require(row.get("execution_seam") == "rust-protected" and row.get("protected_execution") is True, "replay evidence did not come through the protected Rust seam")
    for key in ("model_revision", "runtime_revision", "host_profile", "backend", "requested_device", "observed_device"):
        _require(row.get(key) not in (None, "", []), f"replay evidence is missing {key}")
    _require(row.get("model_id") == row.get("raw_response", {}).get("model_id", row.get("model_id")), "model identity changed in response")


def _response_status(row: Mapping[str, Any]) -> str:
    status = row.get("raw_response", {}).get("status", row.get("status", ""))
    return str(status)


def _score_field(rows: Sequence[Mapping[str, Any]], corpus_by_id: Mapping[str, Mapping[str, Any]], field: str, mapping: Mapping[str, str] | None = None) -> dict[str, Any]:
    mapping = mapping or {}
    total = {"distance": 0, "deletions": 0, "substitutions": 0, "insertions": 0, "reference_tokens": 0, "hypothesis_tokens": 0, "sample_count": 0}
    by_language: dict[str, dict[str, int]] = {}
    critical_errors = {"names": 0, "numbers": 0, "commands": 0}
    for row in rows:
        record = corpus_by_id[row["record_id"]]
        text = row.get(field)
        if text is None:
            continue
        reference = strict_tokens(record["reference"])
        hypothesis = strict_tokens(str(text))
        reference = _map_tokens(reference, mapping)
        hypothesis = _map_tokens(hypothesis, mapping)
        result = wer(reference, hypothesis)
        for key in total:
            if key != "sample_count":
                total[key] += result[key]
        total["sample_count"] += 1
        language = record["language"]
        bucket = by_language.setdefault(language, {"distance": 0, "deletions": 0, "substitutions": 0, "insertions": 0, "reference_tokens": 0, "hypothesis_tokens": 0, "sample_count": 0})
        for key in bucket:
            if key != "sample_count":
                bucket[key] += result[key]
        bucket["sample_count"] += 1
        for token in record.get("critical_tokens", []):
            expected = _map_tokens(strict_tokens(token), mapping)
            present = all(token in hypothesis for token in expected)
            if not present:
                for category in ("names", "numbers", "commands"):
                    if category in record.get("tags", []):
                        critical_errors[category] += 1
                        break

    def finish(bucket: Mapping[str, Any]) -> dict[str, Any]:
        return {**bucket, "wer": (bucket["distance"] / bucket["reference_tokens"]) if bucket["reference_tokens"] else (1.0 if bucket["insertions"] else 0.0)}

    return {"overall": finish(total), "by_language": {language: finish(bucket) for language, bucket in sorted(by_language.items())}, "critical_content_errors": critical_errors}


def _metric_samples(rows: Iterable[Mapping[str, Any]], key: str, phases: set[str]) -> list[float]:
    values = []
    for row in rows:
        phase = str(row.get("phase", row.get("raw_response", {}).get("phase", ""))).lower()
        value = row.get(key, row.get("raw_response", {}).get(key))
        if phase in phases and isinstance(value, (int, float)) and value >= 0:
            values.append(float(value))
    return values


def _report_model(model_id: str, rows: Sequence[Mapping[str, Any]], corpus: Mapping[str, Any], normalizer_version: str | None, normalizer: Mapping[str, str], allow_fixture: bool) -> dict[str, Any]:
    statuses = {_response_status(row) for row in rows}
    quality_rows = [row for row in rows if row.get("raw_text") is not None and _response_status(row) not in {"unavailable", "error"}]
    covered = {row["record_id"] for row in quality_rows}
    expected = {record["record_id"] for record in corpus["records"]}
    if not rows:
        status = "untested"
    elif statuses <= {"unavailable", "error"}:
        status = "unavailable"
    elif covered != expected:
        status = "measured_partial"
    else:
        status = "measured"
    languages = {}
    strict_raw = _score_field(quality_rows, {record["record_id"]: record for record in corpus["records"]}, "raw_text")
    normalized_raw = _score_field(quality_rows, {record["record_id"]: record for record in corpus["records"]}, "raw_text", normalizer)
    delivered_available = bool(quality_rows) and all(row.get("delivered_text") is not None for row in quality_rows)
    delivered = _score_field(quality_rows if delivered_available else [], {record["record_id"]: record for record in corpus["records"]}, "delivered_text")
    for language in CORPUS_LANGUAGES:
        languages[language] = {
            "raw_strict": strict_raw["by_language"].get(language, {"sample_count": 0, "wer": None}),
            "raw_spelling_normalized": normalized_raw["by_language"].get(language, {"sample_count": 0, "wer": None}),
            "delivered_strict": delivered["by_language"].get(language, {"sample_count": 0, "wer": None}) if delivered_available else {"status": "unavailable", "reason": "protected replay did not provide delivered_text"},
        }
    model_identity = {}
    for key in ("model_revision", "runtime_revision", "receipt_sha256", "host_profile", "backend", "requested_device", "observed_device"):
        values = sorted({json.dumps(row.get(key), sort_keys=True) for row in rows if row.get(key) not in (None, "", [])})
        if values:
            model_identity[key] = json.loads(values[0]) if len(values) == 1 else [json.loads(item) for item in values]
    cold_load = _metric_samples(rows, "load_ms", {"cold", "cold_load"})
    warm_inference = _metric_samples(rows, "inference_ms", {"warm"})
    stop_transcript = _metric_samples(rows, "stop_to_transcript_ms", {"cold", "warm", ""})
    stop_delivery = _metric_samples(rows, "stop_to_delivery_ms", {"cold", "warm", ""})
    peak_ram = [float(row["peak_host_ram_bytes"]) for row in rows if isinstance(row.get("peak_host_ram_bytes"), (int, float))]
    peak_gpu = [float(row["peak_gpu_memory_bytes"]) for row in rows if isinstance(row.get("peak_gpu_memory_bytes"), (int, float))]
    return {
        "model_id": model_id,
        "status": status,
        "evidence_class": "fixture" if any(row.get("evidence_class") == "fixture" for row in rows) else "real",
        "quality_evidence": status in {"measured", "measured_partial"} and not any(row.get("evidence_class") == "fixture" for row in rows),
        "sample_count": len(quality_rows),
        "expected_sample_count": len(expected),
        "identity": model_identity,
        "languages": languages,
        "raw_strict": strict_raw["overall"],
        "raw_spelling_normalized": {**normalized_raw["overall"], "normalization_version": normalizer_version},
        "delivered_strict": delivered["overall"] if delivered_available else {"status": "unavailable", "reason": "protected replay did not provide delivered_text"},
        "critical_content_errors": strict_raw["critical_content_errors"],
        "timings": {
            "cold_load_ms": timing_summary(cold_load),
            "warm_inference_ms": timing_summary(warm_inference),
            "stop_to_transcript_ms": timing_summary(stop_transcript),
            "stop_to_delivery_ms": timing_summary(stop_delivery),
        },
        "resources": {
            "peak_host_ram_bytes": max(peak_ram) if peak_ram else None,
            "peak_gpu_memory_bytes": max(peak_gpu) if peak_gpu else None,
        },
        "limitations": (["warm metrics unavailable: protected seam did not label a resident warm phase"] if not warm_inference else []) + (["delivered transcript metrics unavailable"] if not delivered_available else []),
    }


def score(corpus: Mapping[str, Any], evidence: Mapping[str, Any], expected_models: Sequence[str], normalizer_path: Path | None, allow_fixture: bool = False) -> dict[str, Any]:
    validate_frozen(corpus)
    _require(evidence.get("corpus_manifest_sha256") == corpus["manifest_sha256"], "replay evidence belongs to a different or stale corpus manifest")
    _require(evidence.get("schema_version") == SCHEMA_VERSION, "unsupported replay evidence schema")
    version, mapping = load_spelling_normalizer(normalizer_path)
    corpus_by_id = {record["record_id"]: record for record in corpus["records"]}
    rows = evidence.get("records")
    _require(isinstance(rows, list), "replay evidence has no records")
    expected = set(expected_models)
    _require(expected and all(MODEL_ID_RE.fullmatch(model_id) for model_id in expected), "expected model IDs are invalid")
    seen: set[tuple[str, str]] = set()
    grouped: dict[str, list[Mapping[str, Any]]] = {model_id: [] for model_id in expected}
    for row in rows:
        _require(isinstance(row, dict), "replay evidence row must be an object")
        model_id, record_id = row.get("model_id"), row.get("record_id")
        _require(model_id in expected, f"unexpected model in replay evidence: {model_id}")
        _require(record_id in corpus_by_id, f"replay evidence references unknown record: {record_id}")
        _require((model_id, record_id) not in seen, f"duplicate replay evidence for {model_id}/{record_id}")
        seen.add((model_id, record_id))
        _require(row.get("corpus_manifest_sha256") == corpus["manifest_sha256"], f"stale replay evidence for {model_id}/{record_id}")
        _require(row.get("audio_sha256") == corpus_by_id[record_id]["audio_sha256"], f"audio hash mismatch for {model_id}/{record_id}")
        _require(row.get("language") == corpus_by_id[record_id]["language"] and row.get("split") == corpus_by_id[record_id]["split"], f"language or split drift for {model_id}/{record_id}")
        _require_identity(row, allow_fixture)
        grouped[model_id].append(row)
    model_reports = [_report_model(model_id, grouped[model_id], corpus, version, mapping, allow_fixture) for model_id in sorted(expected)]
    report = {
        "schema_version": REPORT_SCHEMA_VERSION,
        "kind": "benchmark_report",
        "corpus_id": corpus["corpus_id"],
        "corpus_manifest_sha256": corpus["manifest_sha256"],
        "evidence_sha256": evidence["evidence_sha256"],
        "quality_evidence": not allow_fixture and all(item["quality_evidence"] for item in model_reports if item["status"] in {"measured", "measured_partial"}),
        "normalization": {"strict": "unicode-nfkc-casefold-token-v1", "spelling_normalized_version": version},
        "models": model_reports,
        "recommendation_scope": {"host_profile": "from measured identity per model", "language": "must be selected explicitly", "duration_profile": "must be supplied by the protected replay evidence"},
        "generated_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
    }
    return {**report, "report_sha256": content_hash(report, "report_sha256")}


def recommend(report: Mapping[str, Any], language: str, duration_profile: str | None = None, host_profile: str | None = None) -> dict[str, Any]:
    language = _normalise_language(language)
    candidates = []
    for model in report.get("models", []):
        if model.get("status") not in {"measured", "measured_partial"} or not model.get("quality_evidence"):
            continue
        identity = model.get("identity", {})
        if host_profile is not None and identity.get("host_profile") != host_profile:
            continue
        profile = identity.get("duration_profile")
        if duration_profile is not None and profile != duration_profile:
            continue
        metrics = model.get("languages", {}).get(language, {}).get("raw_strict", {})
        if not metrics.get("sample_count"):
            continue
        candidates.append({"model_id": model["model_id"], "strict_wer": metrics.get("wer"), "warm_p95_ms": model.get("timings", {}).get("warm_inference_ms", {}).get("p95_ms"), "peak_host_ram_bytes": model.get("resources", {}).get("peak_host_ram_bytes"), "host_profile": identity.get("host_profile"), "duration_profile": profile, "measured_samples": metrics.get("sample_count")})
    candidates.sort(key=lambda row: row["model_id"])
    def best(key: str) -> str | None:
        available = [row for row in candidates if isinstance(row.get(key), (int, float))]
        return min(available, key=lambda row: (row[key], row["model_id"]))["model_id"] if available else None
    return {
        "schema_version": REPORT_SCHEMA_VERSION,
        "kind": "measured_recommendation",
        "language": language,
        "duration_profile": duration_profile,
        "host_profile": host_profile,
        "candidates": candidates,
        "labels": {"highest_tested_accuracy": best("strict_wer"), "fastest_warm": best("warm_p95_ms"), "lowest_measured_memory": best("peak_host_ram_bytes")},
        "limitations": (["no measured candidate matches the requested language, host, and duration scope"] if not candidates else []) + (["a missing label means the corresponding metric was not measured"]),
    }


def _command_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    inv = sub.add_parser("inventory", help="inventory consented WAVs and adjudicated references")
    inv.add_argument("--input-dir", type=Path, required=True)
    inv.add_argument("--references", type=Path, required=True)
    inv.add_argument("--consent-receipt", type=Path, required=True)
    inv.add_argument("--corpus-id", required=True)
    inv.add_argument("--output", type=Path, required=True)
    freeze_parser = sub.add_parser("freeze", help="freeze hashes and deterministic development/heldout splits")
    freeze_parser.add_argument("--inventory", type=Path, required=True)
    freeze_parser.add_argument("--seed", required=True)
    freeze_parser.add_argument("--heldout-fraction", type=float, default=0.2)
    freeze_parser.add_argument("--output", type=Path, required=True)
    replay_parser = sub.add_parser("replay", help="replay exact audio through protected Rust")
    replay_parser.add_argument("--corpus", type=Path, required=True)
    replay_parser.add_argument("--model-manifest", action="append", required=True, metavar="MODEL_ID=PATH")
    replay_parser.add_argument("--rust-cli", default="voisu")
    replay_parser.add_argument("--timeout-seconds", type=float, default=120.0)
    replay_parser.add_argument("--fixture", action="store_true", help="label synthetic seam evidence; never quality evidence")
    replay_parser.add_argument("--output", type=Path, required=True)
    score_parser = sub.add_parser("score", help="score raw and delivered replay text")
    score_parser.add_argument("--corpus", type=Path, required=True)
    score_parser.add_argument("--evidence", type=Path, required=True)
    score_parser.add_argument("--model", action="append", dest="models", required=True)
    score_parser.add_argument("--normalizer", type=Path)
    score_parser.add_argument("--allow-fixture", action="store_true")
    score_parser.add_argument("--output", type=Path, required=True)
    rec_parser = sub.add_parser("recommend", help="make measured-only, scoped recommendations")
    rec_parser.add_argument("--report", type=Path, required=True)
    rec_parser.add_argument("--language", required=True)
    rec_parser.add_argument("--duration-profile")
    rec_parser.add_argument("--host-profile")
    rec_parser.add_argument("--output", type=Path)
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    args = _command_parser().parse_args(argv)
    try:
        if args.command == "inventory":
            write_json(args.output, inventory(args.input_dir, args.references, args.consent_receipt, args.corpus_id))
        elif args.command == "freeze":
            write_json(args.output, freeze(read_json(args.inventory), args.seed, args.heldout_fraction))
        elif args.command == "replay":
            replay(read_json(args.corpus), args.model_manifest, args.rust_cli, args.output, args.timeout_seconds, args.fixture)
        elif args.command == "score":
            write_json(args.output, score(read_json(args.corpus), _load_replay_rows(args.evidence), args.models, args.normalizer, args.allow_fixture))
        elif args.command == "recommend":
            result = recommend(read_json(args.report), args.language, args.duration_profile, args.host_profile)
            if args.output:
                write_json(args.output, result)
            else:
                print(json.dumps(result, ensure_ascii=False, indent=2))
        return 0
    except BenchmarkError as exc:
        print(f"benchmark: {exc}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
