#!/usr/bin/env python3
"""#140 Developer Prompt Rendering combined-call model benchmark harness.

Live-compares model candidates under the #139 v1.1.2 compose gates + #138 oracles.
Stdlib only (+ subprocess curl for Groq). Never persists secrets.

Usage:
  python3 ...harness-2026-08-11.py --self-check
  python3 ...harness-2026-08-11.py --provider groq --live
  python3 ...harness-2026-08-11.py --provider all --live
  python3 ...harness-2026-08-11.py --provider groq --dry-run
  python3 ...harness-2026-08-11.py --offline-replay
"""

from __future__ import annotations

import argparse
import copy
import hashlib
import importlib.util
import json
import math
import os
import re
import subprocess
import sys
import tempfile
import time
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

HERE = Path(__file__).resolve().parent
CC_CORPUS_PATH = HERE / "developer-prompt-rendering-combined-call-corpus-2026-08-11.json"
CC_PROTO_PATH = HERE / "developer-prompt-rendering-combined-call-prototype-2026-08-11.py"
BEHAVIOR_CORPUS_PATH = HERE / "developer-prompt-rendering-behavior-corpus-2026-08-11.json"
RESULTS_JSON_PATH = HERE / "developer-prompt-rendering-model-benchmark-2026-08-11.json"
RESULTS_MD_PATH = HERE / "developer-prompt-rendering-model-benchmark-2026-08-11.md"

GROQ_URL = "https://api.groq.com/openai/v1/chat/completions"
GROQ_MODEL_ID = "openai/gpt-oss-20b"
GEMINI_MODELS = ("gemini-3.5-flash-lite", "gemini-3.6-flash")

DELIVERY_DEADLINE_MS = 1500
# Production-boundary curl process limit (matches #97 harness style).
CURL_MAX_TIME_S = 2.0
# Slightly higher wall budget for complex structured vectors so content quality
# is still measured; production deadline is applied as a separate gate.
LIVE_CURL_MAX_TIME_S = 8.0
MAX_TOKENS = 3500
TRIALS_DEFAULT = 3
RAW_RESPONSE_TRUNCATE = 4000
TEMPERATURE = 0.0
GROQ_REASONING_EFFORT = "low"  # verified supported; reduces reasoning tokens vs default

# Bounded live matrix (~16 content vectors + local synthetic gates).
LIVE_VECTORS: list[dict[str, Any]] = [
    {
        "vector_id": "V-sym-exclamation",
        "fixture_id": "CC-01",
        "category": "symbol_conversion",
        "notes": "exclamation point → !",
    },
    {
        "vector_id": "V-sym-period-newline",
        "fixture_id": "CC-02",
        "category": "symbol_conversion",
        "notes": "period + new line stack",
    },
    {
        "vector_id": "V-filler-clear",
        "fixture_id": "CC-03",
        "category": "filler_removal",
        "notes": "clear um/uh removal",
    },
    {
        "vector_id": "V-backtrack-clear",
        "fixture_id": "CC-04",
        "category": "backtrack_removal",
        "notes": "clear friday→monday backtrack",
    },
    {
        "vector_id": "V-backtrack-uncertain",
        "fixture_id": "CC-05",
        "category": "backtrack_uncertain",
        "notes": "uncertain backtrack → preserve words soft salvage",
    },
    {
        "vector_id": "V-structured-multi",
        "fixture_id": "CC-07",
        "category": "structured_multi_label",
        "notes": "CC-07 / DPR structured multi-label developer prompt",
    },
    {
        "vector_id": "V-dual-stt-name",
        "fixture_id": "CC-16",
        "category": "dual_stt_reconciliation",
        "notes": "voisu vs voice so; primary rank + protected name",
    },
    {
        "vector_id": "V-protected-command-url",
        "fixture_id": "CC-17",
        "category": "protected_tokens",
        "notes": "curl flags URL status_code 404",
    },
    {
        "vector_id": "V-everyday-short",
        "fixture_id": "CC-18",
        "category": "everyday_organize",
        "notes": "short everyday message; organize-only",
    },
    {
        "vector_id": "V-structured-goal",
        "fixture_id": "CC-20",
        "category": "structured_label",
        "notes": "simple Structured Goal label",
    },
    {
        "vector_id": "V-quote-convert",
        "fixture_id": "CC-21",
        "category": "symbol_conversion",
        "notes": "quote…unquote with protected interior",
    },
    {
        "vector_id": "V-protected-name",
        "fixture_id": "CC-14",
        "category": "protected_tokens",
        "notes": "live: preserve name Anuraj (corpus candidate was unsafe flip)",
        "live_expect_accept": True,
    },
    {
        "vector_id": "V-negation-dual",
        "fixture_id": "CC-15",
        "category": "protected_negation_dual_stt",
        "notes": "preserve Do not enable under dual-STT disagreement",
        "live_expect_accept": True,
    },
    {
        "vector_id": "V-multiparagraph",
        "fixture_id": "CC-24",
        "category": "layout_multiparagraph",
        "notes": "clear multi-paragraph layout",
    },
]

SYNTHETIC_VECTORS: list[dict[str, Any]] = [
    {
        "vector_id": "V-schema-invalid",
        "fixture_id": "CC-18",
        "category": "schema_invalid",
        "mode": "synthetic_malformed",
        "notes": "local malformed candidate → fallback_baseline + E_SCHEMA/E_MALFORMED",
    },
    {
        "vector_id": "V-deadline-outcome",
        "fixture_id": "CC-13",
        "category": "deadline_fallback",
        "mode": "synthetic_hard_outcome",
        "cloud_outcome": "deadline_exceeded",
        "notes": "hard cloud_outcome deadline_exceeded → baseline + E_DEADLINE",
    },
    {
        "vector_id": "V-provider-failure",
        "fixture_id": "CC-12",
        "category": "provider_failure",
        "mode": "synthetic_hard_outcome",
        "cloud_outcome": "provider_failure",
        "notes": "hard provider_failure → baseline + E_PROVIDER",
    },
    {
        "vector_id": "V-skipped-cloud",
        "fixture_id": "CC-23",
        "category": "cloud_skipped",
        "mode": "synthetic_hard_outcome",
        "cloud_outcome": "skipped",
        "notes": "cloud skipped → baseline without error",
    },
]


def utc_now() -> str:
    return datetime.now(timezone.utc).replace(microsecond=0).isoformat()


def load_json(path: Path) -> Any:
    return json.loads(path.read_text(encoding="utf-8"))


def load_compose_module() -> Any:
    spec = importlib.util.spec_from_file_location("cc_proto_139", CC_PROTO_PATH)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"cannot load prototype {CC_PROTO_PATH}")
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


def percentile(values: list[float], p: float) -> float | None:
    if not values:
        return None
    xs = sorted(values)
    if len(xs) == 1:
        return xs[0]
    k = (len(xs) - 1) * (p / 100.0)
    f = math.floor(k)
    c = math.ceil(k)
    if f == c:
        return xs[int(k)]
    return xs[f] + (xs[c] - xs[f]) * (k - f)


def truncate(text: str | None, n: int = RAW_RESPONSE_TRUNCATE) -> str | None:
    if text is None:
        return None
    if len(text) <= n:
        return text
    return text[:n] + f"…[truncated {len(text) - n} chars]"


def normalize_for_oracle(text: str) -> str:
    """Light normalize for oracle compare (collapse runs of whitespace, strip)."""
    return re.sub(r"[ \t]+", " ", text.replace("\r\n", "\n")).strip()


def oracle_match(rendered: str, expected: str) -> bool:
    if rendered == expected:
        return True
    return normalize_for_oracle(rendered) == normalize_for_oracle(expected)


def fixture_by_id(corpus: dict[str, Any], fid: str) -> dict[str, Any]:
    for f in corpus["fixtures"]:
        if f["id"] == fid:
            return f
    raise KeyError(fid)


def behavior_oracle_map(behavior: dict[str, Any]) -> dict[str, str]:
    out: dict[str, str] = {}
    for f in behavior.get("fixtures") or []:
        if isinstance(f, dict) and isinstance(f.get("id"), str) and isinstance(
            f.get("expected_final"), str
        ):
            out[f["id"]] = f["expected_final"]
    return out


def expected_oracle_for_fixture(
    fx: dict[str, Any],
    behavior_oracles: dict[str, str],
) -> str:
    """Prefer #138 expected_final when linked; else #139 expected.rendered."""
    for rid in fx.get("related_behavior_fixture_ids") or []:
        if rid in behavior_oracles:
            return behavior_oracles[rid]
    exp = fx.get("expected") or {}
    return str(exp.get("rendered") or fx.get("local_baseline") or "")


def candidate_json_schema() -> dict[str, Any]:
    """Strict JSON schema for Groq response_format binding."""
    span = {
        "type": "object",
        "additionalProperties": False,
        "required": [
            "kind",
            "source_provider",
            "source_text",
            "output_text",
            "conversion_id",
            "label",
        ],
        "properties": {
            "kind": {
                "type": "string",
                "enum": ["keep", "remove", "convert", "label", "layout_break"],
            },
            "source_provider": {"type": ["string", "null"]},
            "source_text": {"type": "string"},
            "output_text": {"type": "string"},
            "conversion_id": {"type": ["string", "null"]},
            "label": {"type": ["string", "null"]},
        },
    }
    return {
        "type": "object",
        "additionalProperties": False,
        "required": [
            "schema_version",
            "base_fingerprint",
            "reconciliation",
            "removals",
            "conversions",
            "layout",
            "labels",
            "derivation",
        ],
        "properties": {
            "schema_version": {"type": "string"},
            "base_fingerprint": {"type": "string"},
            "reconciliation": {
                "type": "object",
                "additionalProperties": False,
                "required": ["selected_provider", "reason"],
                "properties": {
                    "selected_provider": {"type": "string"},
                    "reason": {"type": "string"},
                },
            },
            "removals": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": False,
                    "required": [
                        "kind",
                        "certainty",
                        "source_provider",
                        "source_span_text",
                    ],
                    "properties": {
                        "kind": {"type": "string", "enum": ["filler", "backtrack"]},
                        "certainty": {"type": "string", "enum": ["clear", "uncertain"]},
                        "source_provider": {"type": "string"},
                        "source_span_text": {"type": "string"},
                    },
                },
            },
            "conversions": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": False,
                    "required": ["id", "source_provider", "source_span_text"],
                    "properties": {
                        "id": {"type": "string"},
                        "source_provider": {"type": "string"},
                        "source_span_text": {"type": "string"},
                    },
                },
            },
            "layout": {
                "type": "object",
                "additionalProperties": False,
                "required": ["decision", "certainty"],
                "properties": {
                    "decision": {
                        "type": "string",
                        "enum": [
                            "natural",
                            "multi_paragraph",
                            "numbered",
                            "structured_sections",
                        ],
                    },
                    "certainty": {"type": "string", "enum": ["clear", "uncertain"]},
                },
            },
            "labels": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": False,
                    "required": ["label", "source_provider", "source_span_text"],
                    "properties": {
                        "label": {"type": "string"},
                        "source_provider": {"type": "string"},
                        "source_span_text": {"type": "string"},
                    },
                },
            },
            "derivation": {"type": "array", "items": span},
        },
    }


def build_user_prompt(
    fx: dict[str, Any],
    corpus: dict[str, Any],
    *,
    few_shot: dict[str, Any] | None = None,
) -> str:
    sel = fx.get("source_selection") or {}
    parts = [
        "INPUT",
        f"selected_provider: {sel.get('selected_provider')}",
        f"selection_reason: {sel.get('reason')}",
        f"base_fingerprint: {fx.get('base_fingerprint')}",
        f"policy: {fx.get('policy')}",
        f"protected_tokens (must appear exactly in accepted render): "
        f"{json.dumps(fx.get('protected_tokens') or [], ensure_ascii=False)}",
        "sources (JSON):",
        json.dumps(fx.get("sources") or [], indent=2, ensure_ascii=False),
        "",
        "TASK",
        "Emit one CombinedCallCandidate JSON object with EXACT top-level keys:",
        "schema_version, base_fingerprint, reconciliation, removals, conversions, "
        "layout, labels, derivation.",
        "",
        "HARD RULES",
        '1. schema_version must be "1".',
        "2. base_fingerprint MUST equal the given base_fingerprint exactly.",
        "3. reconciliation.selected_provider MUST equal selected_provider; "
        "reconciliation.reason MUST equal selection_reason.",
        "4. derivation is ordered; concatenating derivation[].output_text reconstructs "
        "the proposal. There is no free-form final string authority.",
        "5. Completeness: every non-whitespace character of the SELECTED provider source "
        "must be covered by keep/remove/convert/label spans (in non-decreasing source order).",
        "6. keep: output_text may change case/punctuation/whitespace but must preserve "
        "ordered content words of source_text (organize-only; no paraphrase drops).",
        "7. remove: only for clear filler/backtrack; output_text must be \"\"; "
        "removals[].kind is filler|backtrack (NOT \"remove\"); each remove span must "
        "match a removals[] entry (same provider + span text).",
        "8. convert: id from closed catalog; conversion_id on span equals conversions[].id; "
        "output_text equals catalog RHS.",
        "9. label: only closed labels; output_text like \"Goal:\\n\" when using section headers; "
        "policy natural forbids structural labels; structured policy allows closed labels "
        "with source evidence.",
        "10. layout_break: output_text only whitespace newlines; multiparagraph requires "
        "layout.decision multi_paragraph or structured_sections (not clear natural).",
        "11. Protected tokens must survive exactly in the composed render.",
        "12. Uncertain backtrack: do NOT remove words (certainty=uncertain on removal only, "
        "or keep all words).",
        "13. Prefer minimal spans. Default layout natural/clear when no multi-paragraph intent.",
        "",
        "Closed conversions:",
        json.dumps(corpus.get("closed_conversions") or [], ensure_ascii=False),
        "Closed labels:",
        json.dumps(corpus.get("closed_structured_labels") or [], ensure_ascii=False),
        "",
    ]
    if few_shot is not None:
        parts.extend(
            [
                "FEW-SHOT EXAMPLE (different input; shape only — adapt to THIS input):",
                json.dumps(few_shot, indent=2, ensure_ascii=False),
                "",
            ]
        )
    parts.append("Return ONLY the JSON object for THIS input.")
    return "\n".join(parts)


def few_shot_example(corpus: dict[str, Any]) -> dict[str, Any]:
    """Static shape example from corpus CC-01 candidate (not the vector under test)."""
    fx = fixture_by_id(corpus, "CC-01")
    return copy.deepcopy(fx["candidate"])


def lookup_secret(provider: str) -> str | None:
    """Fetch secret via secret-tool; never print or write the value."""
    try:
        out = subprocess.check_output(
            ["secret-tool", "lookup", "voisu-provider", provider],
            text=True,
            stderr=subprocess.DEVNULL,
            timeout=10,
        )
        key = out.strip()
        return key or None
    except (subprocess.CalledProcessError, FileNotFoundError, subprocess.TimeoutExpired):
        return None


def env_gemini_key() -> str | None:
    for name in ("GOOGLE_API_KEY", "GEMINI_API_KEY"):
        v = os.environ.get(name)
        if v and v.strip():
            return v.strip()
    return None


def parse_model_json(content: str) -> tuple[dict[str, Any] | None, str | None]:
    """Parse model content into candidate dict; return (cand, error)."""
    if not content or not content.strip():
        return None, "empty_content"
    text = content.strip()
    # Strip optional markdown fences
    if text.startswith("```"):
        text = re.sub(r"^```(?:json)?\s*", "", text)
        text = re.sub(r"\s*```$", "", text)
    try:
        obj = json.loads(text)
    except json.JSONDecodeError as e:
        # try extract first {...}
        m = re.search(r"\{.*\}", text, re.DOTALL)
        if not m:
            return None, f"json_decode: {e}"
        try:
            obj = json.loads(m.group(0))
        except json.JSONDecodeError as e2:
            return None, f"json_decode: {e2}"
    if not isinstance(obj, dict):
        return None, "not_object"
    # unwrap common nestings
    if "schema_version" not in obj:
        for key in ("CombinedCallCandidate", "candidate", "result", "data"):
            inner = obj.get(key)
            if isinstance(inner, dict) and (
                "derivation" in inner or "schema_version" in inner
            ):
                obj = inner
                break
    if "schema_version" not in obj:
        obj = {**obj, "schema_version": "1"}
    # coerce null source_text on spans to ""
    der = obj.get("derivation")
    if isinstance(der, list):
        for span in der:
            if isinstance(span, dict) and span.get("source_text") is None:
                span["source_text"] = ""
    return obj, None


def compose_with_candidate(
    mod: Any,
    fx: dict[str, Any],
    closed: set[str],
    candidate: dict[str, Any] | None,
    *,
    cloud_outcome: str = "succeeded",
) -> dict[str, Any]:
    fixture = copy.deepcopy(fx)
    fixture["cloud_outcome"] = cloud_outcome
    fixture["candidate"] = candidate
    return mod.compose_fixture(fixture, closed)


def decision_would_deliver_model_text(decision: str) -> bool:
    return decision in {"accept", "accept_preserve_words", "accept_natural_layout"}


def curl_groq(
    *,
    api_key: str,
    system: str,
    user: str,
    max_time_s: float,
    reasoning_effort: str | None = GROQ_REASONING_EFFORT,
) -> dict[str, Any]:
    """POST chat completions via curl; return transport envelope (no secrets)."""
    body: dict[str, Any] = {
        "model": GROQ_MODEL_ID,
        "temperature": TEMPERATURE,
        "max_tokens": MAX_TOKENS,
        "response_format": {
            "type": "json_schema",
            "json_schema": {
                "name": "combined_call_candidate",
                "strict": True,
                "schema": candidate_json_schema(),
            },
        },
        "messages": [
            {"role": "system", "content": system},
            {"role": "user", "content": user},
        ],
    }
    if reasoning_effort:
        body["reasoning_effort"] = reasoning_effort

    with tempfile.NamedTemporaryFile(
        "w", suffix=".json", delete=False, encoding="utf-8"
    ) as fh:
        json.dump(body, fh, ensure_ascii=False)
        body_path = fh.name

    # Write curl config without embedding the key in argv process list when possible.
    # Still pass Authorization via header file.
    header_path = body_path + ".hdr"
    try:
        Path(header_path).write_text(
            f"Authorization: Bearer {api_key}\nContent-Type: application/json\n",
            encoding="utf-8",
        )
        cmd = [
            "curl",
            "-sS",
            "-w",
            "\n__CURL_META__ http_code=%{http_code} time_total=%{time_total}",
            "--max-time",
            str(max_time_s),
            "-D",
            body_path + ".resp_headers",
            "-H",
            f"@{header_path}",
            "-d",
            f"@{body_path}",
            GROQ_URL,
        ]
        t0 = time.perf_counter()
        proc = subprocess.run(cmd, capture_output=True, text=True)
        wall_ms = (time.perf_counter() - t0) * 1000.0
        stdout = proc.stdout or ""
        stderr = proc.stderr or ""

        http_code = None
        curl_time_s = None
        body_text = stdout
        if "__CURL_META__" in stdout:
            body_text, _, meta = stdout.rpartition("__CURL_META__")
            body_text = body_text.rstrip("\n")
            m_code = re.search(r"http_code=(\d+)", meta)
            m_time = re.search(r"time_total=([0-9.]+)", meta)
            if m_code:
                http_code = int(m_code.group(1))
            if m_time:
                curl_time_s = float(m_time.group(1))

        rate_limited = False
        retry_after = None
        headers_path = body_path + ".resp_headers"
        header_blob = ""
        if Path(headers_path).exists():
            header_blob = Path(headers_path).read_text(encoding="utf-8", errors="replace")
            if "retry-after" in header_blob.lower() or http_code == 429:
                rate_limited = http_code == 429
            m_ra = re.search(r"(?i)retry-after:\s*(\S+)", header_blob)
            if m_ra:
                retry_after = m_ra.group(1)

        parsed_api: dict[str, Any] | None = None
        content = None
        error_obj = None
        usage = None
        try:
            parsed_api = json.loads(body_text) if body_text.strip() else None
        except json.JSONDecodeError:
            parsed_api = None

        if isinstance(parsed_api, dict):
            if "error" in parsed_api:
                error_obj = parsed_api.get("error")
            elif parsed_api.get("choices"):
                msg = (parsed_api["choices"][0] or {}).get("message") or {}
                content = msg.get("content")
                # some models put reasoning separately
                if content is None and "content" in msg:
                    content = msg["content"]
            usage = parsed_api.get("usage")

        timed_out = proc.returncode in (28,) or (
            http_code in (0, None) and wall_ms >= (max_time_s * 1000 - 50)
        )
        if http_code == 429:
            rate_limited = True

        return {
            "transport": "curl",
            "ok": proc.returncode == 0 and http_code == 200 and content is not None,
            "returncode": proc.returncode,
            "http_code": http_code,
            "latency_ms": round(wall_ms, 2),
            "curl_time_total_ms": None
            if curl_time_s is None
            else round(curl_time_s * 1000.0, 2),
            "timed_out": timed_out,
            "rate_limited": rate_limited,
            "retry_after": retry_after,
            "content": content,
            "error": error_obj,
            "usage": usage,
            "stderr_tail": truncate(stderr, 500),
            "raw_body_tail": truncate(body_text, 800) if content is None else None,
        }
    finally:
        for p in (body_path, header_path, body_path + ".resp_headers"):
            try:
                os.unlink(p)
            except OSError:
                pass


def contract_for_model(corpus: dict[str, Any], model_id: str) -> dict[str, Any]:
    for c in corpus.get("model_prompt_contracts") or []:
        if c.get("model_id") == model_id:
            return c
    raise KeyError(model_id)


def system_prompt_for(contract: dict[str, Any]) -> str:
    return (
        str(contract.get("system_prompt") or "").strip()
        + "\n\n"
        + str(contract.get("response_instructions") or "").strip()
    )


def run_synthetic_vector(
    mod: Any,
    corpus: dict[str, Any],
    vector: dict[str, Any],
    behavior_oracles: dict[str, str],
) -> dict[str, Any]:
    fx = fixture_by_id(corpus, vector["fixture_id"])
    closed = set(corpus["closed_conversions"])
    oracle = expected_oracle_for_fixture(fx, behavior_oracles)
    mode = vector["mode"]
    t0 = time.perf_counter()
    if mode == "synthetic_malformed":
        candidate = {"not": "a candidate"}
        compose = compose_with_candidate(
            mod, fx, closed, candidate, cloud_outcome="succeeded"
        )
        structured_parse_ok = False
        error_class = "schema_invalid_local"
    elif mode == "synthetic_hard_outcome":
        candidate = None
        compose = compose_with_candidate(
            mod,
            fx,
            closed,
            candidate,
            cloud_outcome=vector["cloud_outcome"],
        )
        structured_parse_ok = True  # no model parse required
        error_class = vector["cloud_outcome"]
    else:
        raise ValueError(mode)
    latency_ms = (time.perf_counter() - t0) * 1000.0
    decision = compose["decision"]
    semantic = oracle_match(compose["rendered"], oracle)
    return {
        "vector_id": vector["vector_id"],
        "fixture_id": vector["fixture_id"],
        "category": vector["category"],
        "mode": mode,
        "provider": "local",
        "model_id": None,
        "status": "ok",
        "trial": 1,
        "structured_parse_ok": structured_parse_ok,
        "compose_decision": decision,
        "production_decision": decision,
        "fallback_trigger": compose.get("fallback_trigger"),
        "error_codes": compose.get("error_codes") or [],
        "error_class": error_class,
        "rendered": compose.get("rendered"),
        "oracle_expected": oracle,
        "semantic_match_oracle": semantic,
        "latency_ms": round(latency_ms, 3),
        "within_1500ms": True,
        "would_deliver_model_text": decision_would_deliver_model_text(decision),
        "notes": vector.get("notes"),
    }


def score_live_attempt(
    *,
    vector: dict[str, Any],
    fx: dict[str, Any],
    transport: dict[str, Any],
    compose: dict[str, Any] | None,
    candidate: dict[str, Any] | None,
    parse_error: str | None,
    oracle: str,
    model_id: str,
    provider: str,
    trial: int,
) -> dict[str, Any]:
    latency_ms = float(transport.get("latency_ms") or 0.0)
    within = latency_ms <= DELIVERY_DEADLINE_MS
    structured_parse_ok = candidate is not None and parse_error is None

    if transport.get("timed_out") and not transport.get("ok"):
        # Treat as provider/deadline transport failure
        content_decision = "fallback_baseline"
        error_codes = ["E_DEADLINE"] if transport.get("timed_out") else ["E_PROVIDER"]
        fallback_trigger = (
            "deadline_exceeded" if transport.get("timed_out") else "provider_failure"
        )
        rendered = fx["local_baseline"]
        error_class = "deadline_timeout" if transport.get("timed_out") else "provider_error"
    elif transport.get("rate_limited"):
        content_decision = "fallback_baseline"
        error_codes = ["E_PROVIDER"]
        fallback_trigger = "provider_failure"
        rendered = fx["local_baseline"]
        error_class = "rate_limited"
    elif not transport.get("ok"):
        content_decision = "fallback_baseline"
        error_codes = ["E_PROVIDER"]
        fallback_trigger = "provider_failure"
        rendered = fx["local_baseline"]
        error_class = "provider_error"
    elif not structured_parse_ok:
        content_decision = "fallback_baseline"
        error_codes = ["E_SCHEMA", "E_MALFORMED"]
        fallback_trigger = "response_schema_failure"
        rendered = fx["local_baseline"]
        error_class = f"parse_fail:{parse_error}"
    else:
        assert compose is not None
        content_decision = compose["decision"]
        error_codes = list(compose.get("error_codes") or [])
        fallback_trigger = compose.get("fallback_trigger")
        rendered = compose.get("rendered")
        error_class = None if content_decision.startswith("accept") else (
            (error_codes[0] if error_codes else "compose_fallback")
        )

    # Production gate: exceed 1.5s → deadline fallback even if content would accept.
    if not within:
        production_decision = "fallback_baseline"
        production_codes = sorted(set(error_codes + ["E_DEADLINE"]))
        production_trigger = "deadline_exceeded"
        production_rendered = fx["local_baseline"]
        deadline_overrode = content_decision != "fallback_baseline" or "E_DEADLINE" not in error_codes
    else:
        production_decision = content_decision
        production_codes = error_codes
        production_trigger = fallback_trigger
        production_rendered = rendered
        deadline_overrode = False

    semantic = oracle_match(str(production_rendered or ""), oracle)
    # Content-only semantic (ignoring deadline override) for analysis
    content_semantic = oracle_match(str(rendered or ""), oracle)

    return {
        "vector_id": vector["vector_id"],
        "fixture_id": vector["fixture_id"],
        "category": vector["category"],
        "mode": "live",
        "provider": provider,
        "model_id": model_id,
        "status": "ok" if transport.get("ok") or transport.get("timed_out") or transport.get("rate_limited") else "transport_error",
        "trial": trial,
        "http_code": transport.get("http_code"),
        "rate_limited": bool(transport.get("rate_limited")),
        "timed_out": bool(transport.get("timed_out")),
        "structured_parse_ok": structured_parse_ok,
        "parse_error": parse_error,
        "compose_decision": content_decision,
        "production_decision": production_decision,
        "deadline_overrode_accept": deadline_overrode and not within,
        "fallback_trigger": production_trigger,
        "error_codes": production_codes,
        "content_error_codes": error_codes,
        "error_class": error_class,
        "rendered": production_rendered,
        "content_rendered": rendered,
        "oracle_expected": oracle,
        "semantic_match_oracle": semantic,
        "content_semantic_match_oracle": content_semantic,
        "latency_ms": round(latency_ms, 2),
        "curl_time_total_ms": transport.get("curl_time_total_ms"),
        "within_1500ms": within,
        "would_deliver_model_text": decision_would_deliver_model_text(production_decision),
        "usage": transport.get("usage"),
        "raw_response_truncated": truncate(transport.get("content")),
        "transport_error": transport.get("error"),
        "notes": vector.get("notes"),
    }


def run_groq_live(
    mod: Any,
    corpus: dict[str, Any],
    behavior_oracles: dict[str, str],
    *,
    trials: int,
    dry_run: bool,
) -> list[dict[str, Any]]:
    rows: list[dict[str, Any]] = []
    closed = set(corpus["closed_conversions"])
    contract = contract_for_model(corpus, GROQ_MODEL_ID)
    system = system_prompt_for(contract)
    example = few_shot_example(corpus)

    api_key = None if dry_run else lookup_secret("groq")
    if not dry_run and not api_key:
        for v in LIVE_VECTORS:
            rows.append(
                {
                    "vector_id": v["vector_id"],
                    "fixture_id": v["fixture_id"],
                    "category": v["category"],
                    "mode": "live",
                    "provider": "groq",
                    "model_id": GROQ_MODEL_ID,
                    "status": "not_run_missing_credentials",
                    "trial": 1,
                    "structured_parse_ok": False,
                    "compose_decision": None,
                    "production_decision": None,
                    "error_codes": [],
                    "semantic_match_oracle": None,
                    "latency_ms": None,
                    "within_1500ms": None,
                    "would_deliver_model_text": False,
                    "notes": "secret-tool lookup voisu-provider groq failed",
                }
            )
        return rows

    for v in LIVE_VECTORS:
        fx = fixture_by_id(corpus, v["fixture_id"])
        oracle = expected_oracle_for_fixture(fx, behavior_oracles)
        user = build_user_prompt(fx, corpus, few_shot=example)
        for trial in range(1, trials + 1):
            if dry_run:
                rows.append(
                    {
                        "vector_id": v["vector_id"],
                        "fixture_id": v["fixture_id"],
                        "category": v["category"],
                        "mode": "live_dry_run",
                        "provider": "groq",
                        "model_id": GROQ_MODEL_ID,
                        "status": "dry_run",
                        "trial": trial,
                        "prompt_chars": len(system) + len(user),
                        "structured_parse_ok": None,
                        "compose_decision": None,
                        "production_decision": None,
                        "error_codes": [],
                        "semantic_match_oracle": None,
                        "latency_ms": None,
                        "within_1500ms": None,
                        "would_deliver_model_text": False,
                        "oracle_expected": oracle,
                        "notes": v.get("notes"),
                    }
                )
                continue

            # Retry rate limits a few times
            transport = None
            for attempt in range(1, 6):
                transport = curl_groq(
                    api_key=api_key or "",
                    system=system,
                    user=user,
                    max_time_s=LIVE_CURL_MAX_TIME_S,
                )
                if not transport.get("rate_limited"):
                    break
                # exponential backoff
                time.sleep(min(2**attempt, 20))
            assert transport is not None

            candidate = None
            parse_error = None
            compose = None
            if transport.get("ok") and transport.get("content") is not None:
                candidate, parse_error = parse_model_json(str(transport["content"]))
                if candidate is not None:
                    compose = compose_with_candidate(
                        mod, fx, closed, candidate, cloud_outcome="succeeded"
                    )
            row = score_live_attempt(
                vector=v,
                fx=fx,
                transport=transport,
                compose=compose,
                candidate=candidate,
                parse_error=parse_error,
                oracle=oracle,
                model_id=GROQ_MODEL_ID,
                provider="groq",
                trial=trial,
            )
            row["attempts"] = attempt
            rows.append(row)
            # small pacing to reduce 429s
            time.sleep(0.15)
    return rows


def gemini_blocked_rows() -> list[dict[str, Any]]:
    rows: list[dict[str, Any]] = []
    for model_id in GEMINI_MODELS:
        for v in LIVE_VECTORS:
            rows.append(
                {
                    "vector_id": v["vector_id"],
                    "fixture_id": v["fixture_id"],
                    "category": v["category"],
                    "mode": "live",
                    "provider": "google",
                    "model_id": model_id,
                    "status": "not_run_missing_credentials",
                    "trial": 1,
                    "structured_parse_ok": None,
                    "compose_decision": None,
                    "production_decision": None,
                    "error_codes": [],
                    "semantic_match_oracle": None,
                    "latency_ms": None,
                    "within_1500ms": None,
                    "would_deliver_model_text": False,
                    "notes": (
                        "Gemini path defined (prompt contract + same #139 compose gate) "
                        "but GOOGLE_API_KEY/GEMINI_API_KEY and secret-tool google keys "
                        "unavailable on this host. No fabricated scores."
                    ),
                }
            )
    return rows


def offline_replay_rows(
    mod: Any, corpus: dict[str, Any], behavior_oracles: dict[str, str]
) -> list[dict[str, Any]]:
    """Replay corpus candidates through compose; verify expected decisions."""
    closed = set(corpus["closed_conversions"])
    rows: list[dict[str, Any]] = []
    for fx in corpus["fixtures"]:
        t0 = time.perf_counter()
        result = mod.compose_fixture(fx, closed)
        latency_ms = (time.perf_counter() - t0) * 1000.0
        exp = fx["expected"]
        decision_ok = result["decision"] == exp["decision"]
        render_ok = result["rendered"] == exp["rendered"]
        oracle = expected_oracle_for_fixture(fx, behavior_oracles)
        rows.append(
            {
                "vector_id": f"offline-{fx['id']}",
                "fixture_id": fx["id"],
                "category": "offline_corpus_replay",
                "mode": "offline_replay",
                "provider": "local",
                "model_id": None,
                "status": "ok" if decision_ok and render_ok else "mismatch",
                "trial": 1,
                "structured_parse_ok": True,
                "compose_decision": result["decision"],
                "expected_decision": exp["decision"],
                "decision_match": decision_ok,
                "render_match": render_ok,
                "fallback_trigger": result.get("fallback_trigger"),
                "error_codes": result.get("error_codes") or [],
                "rendered": result.get("rendered"),
                "oracle_expected": oracle,
                "semantic_match_oracle": oracle_match(result["rendered"], oracle),
                "latency_ms": round(latency_ms, 3),
                "within_1500ms": True,
                "would_deliver_model_text": decision_would_deliver_model_text(
                    result["decision"]
                ),
            }
        )
    return rows


def aggregate_model(
    rows: list[dict[str, Any]], model_id: str
) -> dict[str, Any]:
    live = [
        r
        for r in rows
        if r.get("model_id") == model_id and r.get("mode") == "live"
    ]
    if not live:
        blocked = [
            r
            for r in rows
            if r.get("model_id") == model_id
            and r.get("status") == "not_run_missing_credentials"
        ]
        if blocked:
            return {
                "model_id": model_id,
                "status": "not_run_missing_credentials",
                "n_rows": len(blocked),
                "note": "No live scores; credentials missing.",
            }
        return {"model_id": model_id, "status": "no_rows", "n_rows": 0}

    n = len(live)
    parse_ok = sum(1 for r in live if r.get("structured_parse_ok"))
    content_accept = sum(
        1
        for r in live
        if str(r.get("compose_decision") or "").startswith("accept")
    )
    prod_accept = sum(
        1
        for r in live
        if str(r.get("production_decision") or "").startswith("accept")
    )
    semantic = sum(1 for r in live if r.get("semantic_match_oracle") is True)
    content_semantic = sum(
        1 for r in live if r.get("content_semantic_match_oracle") is True
    )
    within = sum(1 for r in live if r.get("within_1500ms") is True)
    unsafe_deliver = sum(
        1
        for r in live
        if r.get("would_deliver_model_text")
        and r.get("semantic_match_oracle") is False
    )
    latencies = [
        float(r["latency_ms"])
        for r in live
        if isinstance(r.get("latency_ms"), (int, float))
    ]
    by_vector: dict[str, list[dict[str, Any]]] = {}
    for r in live:
        by_vector.setdefault(r["vector_id"], []).append(r)

    vector_summary = []
    for vid, trials in sorted(by_vector.items()):
        lats = [float(t["latency_ms"]) for t in trials if t.get("latency_ms") is not None]
        vector_summary.append(
            {
                "vector_id": vid,
                "fixture_id": trials[0].get("fixture_id"),
                "category": trials[0].get("category"),
                "n_trials": len(trials),
                "parse_ok": sum(1 for t in trials if t.get("structured_parse_ok")),
                "content_accept": sum(
                    1
                    for t in trials
                    if str(t.get("compose_decision") or "").startswith("accept")
                ),
                "production_accept": sum(
                    1
                    for t in trials
                    if str(t.get("production_decision") or "").startswith("accept")
                ),
                "semantic_match": sum(
                    1 for t in trials if t.get("semantic_match_oracle") is True
                ),
                "content_semantic_match": sum(
                    1
                    for t in trials
                    if t.get("content_semantic_match_oracle") is True
                ),
                "within_1500ms": sum(1 for t in trials if t.get("within_1500ms")),
                "unsafe_deliver": sum(
                    1
                    for t in trials
                    if t.get("would_deliver_model_text")
                    and t.get("semantic_match_oracle") is False
                ),
                "p50_ms": percentile(lats, 50),
                "p95_ms": percentile(lats, 95),
                "max_ms": max(lats) if lats else None,
                "sample_decision": trials[0].get("compose_decision"),
                "sample_error_codes": trials[0].get("error_codes"),
                "sample_rendered": truncate(str(trials[0].get("content_rendered") or trials[0].get("rendered") or ""), 200),
            }
        )

    return {
        "model_id": model_id,
        "status": "live_partial" if any(
            r.get("status") not in ("ok", "transport_error") for r in live
        ) else "live",
        "n_rows": n,
        "n_vectors": len(by_vector),
        "structured_parse_ok_rate": parse_ok / n if n else None,
        "content_accept_rate": content_accept / n if n else None,
        "production_accept_rate": prod_accept / n if n else None,
        "semantic_match_rate": semantic / n if n else None,
        "content_semantic_match_rate": content_semantic / n if n else None,
        "within_1500ms_rate": within / n if n else None,
        "unsafe_deliver_count": unsafe_deliver,
        "latency_p50_ms": percentile(latencies, 50),
        "latency_p95_ms": percentile(latencies, 95),
        "latency_max_ms": max(latencies) if latencies else None,
        "latency_min_ms": min(latencies) if latencies else None,
        "vectors": vector_summary,
        "config": {
            "reasoning_effort": GROQ_REASONING_EFFORT,
            "temperature": TEMPERATURE,
            "response_format": "json_schema.strict",
            "delivery_deadline_ms": DELIVERY_DEADLINE_MS,
            "curl_max_time_s": LIVE_CURL_MAX_TIME_S,
        },
    }


def write_markdown(results: dict[str, Any]) -> None:
    models = results.get("aggregates") or {}
    groq = models.get(GROQ_MODEL_ID) or {}
    lines: list[str] = []
    lines.append("# Developer Prompt Rendering model benchmark (#140)")
    lines.append("")
    lines.append(f"**Date:** {results.get('generated_at')}")
    lines.append(f"**Issue:** [#140](https://github.com/Anuraj-dev/voisu/issues/140)")
    lines.append(
        "**Governing:** #139 combined-call contract v"
        f"{results.get('governing', {}).get('combined_call_version')} "
        "(completeness + source-order gates intact); #138 behavior oracles."
    )
    lines.append(
        f"**Companion JSON:** [`{RESULTS_JSON_PATH.name}`](./{RESULTS_JSON_PATH.name})"
    )
    lines.append(
        f"**Harness:** [`{Path(__file__).name}`](./{Path(__file__).name})"
    )
    lines.append("")
    lines.append("## Ticket question")
    lines.append("")
    lines.append(
        "Which of Gemini 3.5 Flash-Lite, Gemini 3.6 Flash, and Groq GPT-OSS-20B best "
        "satisfies the approved behavior/schema contract under semantic safety, source "
        "fidelity, structured-output validity, p50/p95 latency, 1.5-second fallback, "
        "quota, and provider-failure tests?"
    )
    lines.append("")
    lines.append("## Method")
    lines.append("")
    lines.append("- **Accept gate:** import/reuse `#139` `compose_fixture` "
                 f"from `{CC_PROTO_PATH.name}` (v1.1.2 completeness/order/protected/"
                 "invented-content gates). **No gate weakening.**")
    lines.append("- **Prompts:** `#139` `model_prompt_contracts` system + response "
                 "instructions; user payload carries sources, host selection, "
                 "fingerprint, policy, protected tokens, closed catalogs, few-shot shape.")
    lines.append("- **Oracles:** related `#138` `expected_final` when linked; else "
                 "`#139` `expected.rendered` / local baseline.")
    lines.append("- **Groq transport:** `curl` → `POST https://api.groq.com/openai/v1/chat/completions` "
                 "(Python urllib Cloudflare 403 on this host historically). "
                 "Auth: `secret-tool lookup voisu-provider groq` (never stored in assets).")
    lines.append(f"- **Groq body:** model `{GROQ_MODEL_ID}`, `temperature=0`, "
                 f"`reasoning_effort={GROQ_REASONING_EFFORT!r}`, strict `json_schema` binding.")
    lines.append(f"- **Deadlines:** content wall latency measured; production override if "
                 f"`latency_ms > {DELIVERY_DEADLINE_MS}` → `fallback_baseline` + `E_DEADLINE`. "
                 f"curl `--max-time {LIVE_CURL_MAX_TIME_S}` for content capture; "
                 "synthetic CC-13 exercises hard deadline outcome.")
    lines.append("- **Gemini:** harness paths + matrix defined; **not executed** — no credentials.")
    lines.append("- **Synthetic local:** schema-invalid, provider_failure, deadline_exceeded, skipped.")
    lines.append("- **Offline:** full #139 corpus candidate replay must match expected decisions.")
    lines.append("")
    lines.append("## Credentials reality")
    lines.append("")
    lines.append("| Provider | Available on this host | Live status |")
    lines.append("|---|---|---|")
    lines.append(f"| Groq `{GROQ_MODEL_ID}` | yes (secret-tool) | "
                 f"{groq.get('status', 'n/a')} |")
    lines.append("| Gemini 3.5 Flash-Lite | **no** | `not_run_missing_credentials` |")
    lines.append("| Gemini 3.6 Flash | **no** | `not_run_missing_credentials` |")
    lines.append("")
    lines.append("## Matrix")
    lines.append("")
    lines.append(f"Live content vectors: **{len(LIVE_VECTORS)}** × trials; "
                 f"synthetic: **{len(SYNTHETIC_VECTORS)}**; "
                 f"offline replay: all #139 fixtures.")
    lines.append("")
    lines.append("| Vector | Fixture | Category |")
    lines.append("|---|---|---|")
    for v in LIVE_VECTORS:
        lines.append(
            f"| `{v['vector_id']}` | {v['fixture_id']} | {v['category']} |"
        )
    for v in SYNTHETIC_VECTORS:
        lines.append(
            f"| `{v['vector_id']}` | {v['fixture_id']} | {v['category']} (local) |"
        )
    lines.append("")
    lines.append("## Live results — Groq `openai/gpt-oss-20b`")
    lines.append("")
    if groq.get("status") in (None, "no_rows", "not_run_missing_credentials"):
        lines.append(f"_No live Groq aggregate: {groq.get('status')}_")
    else:
        lines.append("| Metric | Value |")
        lines.append("|---|---:|")
        lines.append(f"| Rows (vector×trial) | {groq.get('n_rows')} |")
        lines.append(f"| Vectors | {groq.get('n_vectors')} |")
        lines.append(
            f"| Structured parse ok rate | {groq.get('structured_parse_ok_rate')} |"
        )
        lines.append(
            f"| Content accept rate (compose) | {groq.get('content_accept_rate')} |"
        )
        lines.append(
            f"| Production accept rate (≤1.5s gate) | {groq.get('production_accept_rate')} |"
        )
        lines.append(
            f"| Semantic match rate (production render) | {groq.get('semantic_match_rate')} |"
        )
        lines.append(
            f"| Content semantic match rate | {groq.get('content_semantic_match_rate')} |"
        )
        lines.append(
            f"| Within 1500ms rate | {groq.get('within_1500ms_rate')} |"
        )
        lines.append(
            f"| Unsafe delivers (would_deliver ∧ ¬semantic) | {groq.get('unsafe_deliver_count')} |"
        )
        lines.append(f"| Latency p50 ms | {groq.get('latency_p50_ms')} |")
        lines.append(f"| Latency p95 ms | {groq.get('latency_p95_ms')} |")
        lines.append(f"| Latency max ms | {groq.get('latency_max_ms')} |")
        lines.append(f"| Latency min ms | {groq.get('latency_min_ms')} |")
        lines.append("")
        lines.append("### Per-vector summary")
        lines.append("")
        lines.append(
            "| Vector | Fixture | parse | content_accept | prod_accept | "
            "semantic | ≤1500 | unsafe | p50 ms | p95 ms |"
        )
        lines.append("|---|---|---:|---:|---:|---:|---:|---:|---:|---:|")
        for vs in groq.get("vectors") or []:
            n = vs.get("n_trials") or 1
            lines.append(
                f"| `{vs['vector_id']}` | {vs.get('fixture_id')} | "
                f"{vs.get('parse_ok')}/{n} | {vs.get('content_accept')}/{n} | "
                f"{vs.get('production_accept')}/{n} | {vs.get('semantic_match')}/{n} | "
                f"{vs.get('within_1500ms')}/{n} | {vs.get('unsafe_deliver')}/{n} | "
                f"{_fmt(vs.get('p50_ms'))} | {_fmt(vs.get('p95_ms'))} |"
            )
        lines.append("")
        lines.append("### Config")
        lines.append("")
        lines.append("```json")
        lines.append(json.dumps(groq.get("config") or {}, indent=2))
        lines.append("```")
    lines.append("")
    lines.append("## Gemini status")
    lines.append("")
    lines.append(
        "Both Gemini models are **blocked on this host** "
        "(`not_run_missing_credentials`). The harness builds the same user payload "
        "and would validate candidates through `#139` `compose_fixture`. "
        "**No Gemini latency/quality numbers are invented.**"
    )
    lines.append("")
    lines.append("## Synthetic / fallback gates (local)")
    lines.append("")
    synth = [
        r
        for r in results.get("rows") or []
        if r.get("mode", "").startswith("synthetic")
        or r.get("category")
        in {
            "schema_invalid",
            "deadline_fallback",
            "provider_failure",
            "cloud_skipped",
        }
    ]
    # filter synthetic by vector list
    synth = [
        r
        for r in results.get("rows") or []
        if r.get("vector_id") in {v["vector_id"] for v in SYNTHETIC_VECTORS}
    ]
    lines.append("| Vector | Decision | Error codes | Oracle match |")
    lines.append("|---|---|---|---|")
    for r in synth:
        lines.append(
            f"| `{r.get('vector_id')}` | {r.get('compose_decision')} | "
            f"`{r.get('error_codes')}` | {r.get('semantic_match_oracle')} |"
        )
    lines.append("")
    lines.append("## Offline #139 corpus replay")
    lines.append("")
    off = results.get("offline_replay") or {}
    lines.append(
        f"Fixtures: {off.get('total')}; decision+render match: {off.get('pass')}; "
        f"fail: {off.get('fail')}."
    )
    lines.append("")
    lines.append("## Safety / fidelity notes (Groq live)")
    lines.append("")
    lines.append(
        "- **Unsafe deliver** counts production paths that would deliver model-composed "
        "text while failing the oracle string match. Hierarchical fallback still "
        "protects against invented content / protected-token hits / unverifiable "
        "derivation when compose rejects."
    )
    lines.append(
        "- Completeness and source-order gates from #139 v1.1.2 remain authoritative; "
        "models that omit unremoved source words fail `E_UNVERIFIABLE` → baseline."
    )
    lines.append(
        "- Deadline: any live trial with wall latency > 1500ms is forced to "
        "`fallback_baseline` for production_decision even if content compose would accept."
    )
    lines.append("")
    lines.append("## Quota / provider failure")
    lines.append("")
    lines.append(
        "- Rate-limited attempts (HTTP 429) are retried with backoff; final rows "
        "record `rate_limited` if still limited."
    )
    lines.append(
        "- Synthetic `V-provider-failure` confirms `E_PROVIDER` → baseline."
    )
    lines.append(
        "- Groq free/dev quotas may throttle under multi-trial matrices; this package "
        "records headers/flags without storing secrets."
    )
    lines.append("")
    lines.append("## Provisional recommendation")
    lines.append("")
    rec = results.get("recommendation") or {}
    lines.append(f"**Status:** {rec.get('status')}")
    lines.append("")
    lines.append(rec.get("summary") or "")
    lines.append("")
    if rec.get("caveats"):
        lines.append("### Caveats")
        lines.append("")
        for c in rec["caveats"]:
            lines.append(f"- {c}")
        lines.append("")
    if rec.get("follow_ups"):
        lines.append("### Follow-ups (blocks final pick)")
        lines.append("")
        for c in rec["follow_ups"]:
            lines.append(f"- {c}")
        lines.append("")
    lines.append("## Notes for #144")
    lines.append("")
    lines.append(
        "- Product binding should keep `#139` compose gates as the only accept path "
        "for model candidates (no free-form final string)."
    )
    lines.append(
        "- Prefer providers/models that clear structured parse + completeness under "
        f"{DELIVERY_DEADLINE_MS}ms p95; otherwise ship local baseline under deadline."
    )
    lines.append(
        "- Re-run this harness with Gemini credentials before choosing a default cloud model."
    )
    lines.append(
        f"- Groq config used here: `{GROQ_MODEL_ID}` + `reasoning_effort={GROQ_REASONING_EFFORT}`."
    )
    lines.append("")
    lines.append("## Verification commands")
    lines.append("")
    lines.append("```bash")
    lines.append(
        "python3 docs/research/developer-prompt-rendering-model-benchmark-harness-2026-08-11.py --self-check"
    )
    lines.append(
        "python3 docs/research/developer-prompt-rendering-model-benchmark-harness-2026-08-11.py --provider groq --live"
    )
    lines.append(
        "python3 -m json.tool docs/research/developer-prompt-rendering-model-benchmark-2026-08-11.json >/dev/null"
    )
    lines.append("```")
    lines.append("")
    RESULTS_MD_PATH.write_text("\n".join(lines) + "\n", encoding="utf-8")


def _fmt(x: Any) -> str:
    if x is None:
        return "—"
    if isinstance(x, float):
        return f"{x:.1f}"
    return str(x)


def build_recommendation(
    aggregates: dict[str, Any],
    offline: dict[str, Any],
    gemini_ran: bool,
) -> dict[str, Any]:
    groq = aggregates.get(GROQ_MODEL_ID) or {}
    caveats = [
        "Gemini 3.5 Flash-Lite and Gemini 3.6 Flash were **not live-run** "
        "(missing credentials). No comparative winner across the three ticket models "
        "can be finalized.",
        "Recommendation below is **provisional** and conditioned on Groq-only evidence "
        "plus local #139/#138 gates.",
    ]
    follow_ups = [
        "Obtain Google/Gemini API credentials and re-run "
        "`--provider gemini --live` (or `--provider all --live`).",
        "Compare Gemini Flash-Lite vs 3.6 Flash vs Groq on the same matrix "
        "(semantic, unsafe deliver, p50/p95, ≤1.5s rate).",
        "If Gemini wins quality but loses p95>1500ms, document Adaptive cloud skip "
        "vs Structured-only cloud policy for #144.",
    ]
    if offline.get("fail", 0) != 0:
        return {
            "status": "blocked_local_gates",
            "summary": "Offline #139 corpus replay failed; fix package before model pick.",
            "caveats": caveats,
            "follow_ups": follow_ups,
            "provisional_default": None,
        }

    if groq.get("status") not in ("live", "live_partial"):
        return {
            "status": "provisional_insufficient_live",
            "summary": "Groq live data missing; cannot recommend a cloud model yet.",
            "caveats": caveats,
            "follow_ups": follow_ups,
            "provisional_default": None,
        }

    parse_r = groq.get("structured_parse_ok_rate") or 0.0
    sem_r = groq.get("semantic_match_rate") or 0.0
    content_sem = groq.get("content_semantic_match_rate") or 0.0
    within_r = groq.get("within_1500ms_rate") or 0.0
    unsafe = groq.get("unsafe_deliver_count") or 0
    p95 = groq.get("latency_p95_ms")
    prod_acc = groq.get("production_accept_rate") or 0.0

    summary_parts = [
        f"Groq `{GROQ_MODEL_ID}` (reasoning_effort={GROQ_REASONING_EFFORT}) "
        f"achieved structured_parse_ok={parse_r:.2f}, "
        f"content_semantic={content_sem:.2f}, "
        f"production_semantic={sem_r:.2f}, "
        f"within_1500ms={within_r:.2f}, "
        f"production_accept={prod_acc:.2f}, "
        f"unsafe_deliver={unsafe}, "
        f"p50={_fmt(groq.get('latency_p50_ms'))}ms, "
        f"p95={_fmt(p95)}ms.",
    ]
    if unsafe == 0 and parse_r >= 0.8:
        summary_parts.append(
            "Under #139 hierarchical fallback, Groq is a **viable provisional cloud "
            "candidate** for organize-only structured calls when content accepts; "
            "deadline override remains mandatory for >1.5s walls."
        )
    elif unsafe == 0:
        summary_parts.append(
            "Unsafe delivers are zero (fallback protects), but accept/parse rates "
            "are incomplete — treat as **fallback-heavy** provisional path, not a "
            "quality winner."
        )
    else:
        summary_parts.append(
            "Non-zero unsafe delivers observed under production decision mirror — "
            "do **not** ship as default without guard tightening or prompt revision."
        )

    if not gemini_ran:
        summary_parts.append(
            "**Final three-way pick is blocked** until Gemini Flash-Lite and 3.6 Flash "
            "are live-benchmarked on this matrix."
        )

    return {
        "status": "provisional_groq_only",
        "summary": " ".join(summary_parts),
        "caveats": caveats,
        "follow_ups": follow_ups,
        "provisional_default": {
            "model_id": GROQ_MODEL_ID,
            "provider": "groq",
            "reasoning_effort": GROQ_REASONING_EFFORT,
            "condition": (
                "Only if Gemini remains unavailable and product accepts "
                "fallback-heavy behavior on vectors the model cannot prove; "
                "revisit when Gemini numbers exist."
            ),
        },
        "gemini_required_for_final": True,
    }


def run_self_check(mod: Any, corpus: dict[str, Any], behavior: dict[str, Any]) -> list[str]:
    errors: list[str] = []
    # version
    if corpus.get("version") != "1.1.2":
        errors.append(f"expected combined-call version 1.1.2, got {corpus.get('version')}")
    # offline replay
    closed = set(corpus["closed_conversions"])
    for fx in corpus["fixtures"]:
        r = mod.compose_fixture(fx, closed)
        exp = fx["expected"]
        if r["decision"] != exp["decision"] or r["rendered"] != exp["rendered"]:
            errors.append(
                f"offline mismatch {fx['id']}: got {r['decision']} "
                f"expected {exp['decision']}"
            )
    # synthetic
    behavior_oracles = behavior_oracle_map(behavior)
    for v in SYNTHETIC_VECTORS:
        row = run_synthetic_vector(mod, corpus, v, behavior_oracles)
        if v["vector_id"] == "V-schema-invalid":
            if row["compose_decision"] != "fallback_baseline":
                errors.append("schema-invalid should fallback_baseline")
        if v["vector_id"] == "V-deadline-outcome":
            if "E_DEADLINE" not in (row.get("error_codes") or []):
                errors.append("deadline synthetic missing E_DEADLINE")
        if v["vector_id"] == "V-provider-failure":
            if "E_PROVIDER" not in (row.get("error_codes") or []):
                errors.append("provider synthetic missing E_PROVIDER")
    # matrix fixtures exist
    for v in LIVE_VECTORS:
        try:
            fixture_by_id(corpus, v["fixture_id"])
        except KeyError:
            errors.append(f"missing fixture {v['fixture_id']}")
    # contracts
    for mid in (GROQ_MODEL_ID, *GEMINI_MODELS):
        try:
            contract_for_model(corpus, mid)
        except KeyError:
            errors.append(f"missing model_prompt_contract {mid}")
    # prompt builds
    example = few_shot_example(corpus)
    for v in LIVE_VECTORS[:3]:
        fx = fixture_by_id(corpus, v["fixture_id"])
        prompt = build_user_prompt(fx, corpus, few_shot=example)
        if fx["base_fingerprint"] not in prompt:
            errors.append(f"prompt missing fingerprint for {v['vector_id']}")
    # schema sanity
    schema = candidate_json_schema()
    if "derivation" not in schema["properties"]:
        errors.append("candidate schema missing derivation")
    return errors


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="#140 model benchmark harness")
    parser.add_argument(
        "--provider",
        choices=["groq", "gemini", "all", "none"],
        default="none",
        help="which live providers to exercise",
    )
    parser.add_argument("--live", action="store_true", help="perform live API calls")
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="build prompts/matrix without network",
    )
    parser.add_argument(
        "--self-check",
        action="store_true",
        help="offline package health (no network required)",
    )
    parser.add_argument(
        "--offline-replay",
        action="store_true",
        help="include full offline corpus replay rows in results",
    )
    parser.add_argument(
        "--trials",
        type=int,
        default=TRIALS_DEFAULT,
        help=f"live trials per vector (default {TRIALS_DEFAULT})",
    )
    parser.add_argument(
        "--write",
        action="store_true",
        default=True,
        help="write JSON+MD results (default true)",
    )
    parser.add_argument(
        "--no-write",
        action="store_true",
        help="do not write result assets",
    )
    args = parser.parse_args(argv)

    if not CC_CORPUS_PATH.is_file() or not CC_PROTO_PATH.is_file():
        print("missing #139 combined-call package", file=sys.stderr)
        return 2

    corpus = load_json(CC_CORPUS_PATH)
    behavior = load_json(BEHAVIOR_CORPUS_PATH) if BEHAVIOR_CORPUS_PATH.is_file() else {}
    mod = load_compose_module()
    behavior_oracles = behavior_oracle_map(behavior)

    if args.self_check:
        errs = run_self_check(mod, corpus, behavior)
        if errs:
            print("SELF-CHECK FAIL", file=sys.stderr)
            for e in errs:
                print(f"  - {e}", file=sys.stderr)
            return 1
        print(
            "SELF-CHECK OK: offline #139 replay "
            f"{len(corpus['fixtures'])}/{len(corpus['fixtures'])}, "
            f"synthetics {len(SYNTHETIC_VECTORS)}, "
            f"live matrix {len(LIVE_VECTORS)} vectors, "
            f"contracts {len(corpus.get('model_prompt_contracts') or [])}"
        )
        # If only self-check, still optionally refresh results with offline data
        if args.provider == "none" and not args.live and not args.dry_run:
            # write offline-only snapshot for package completeness
            offline_rows = offline_replay_rows(mod, corpus, behavior_oracles)
            synth_rows = [
                run_synthetic_vector(mod, corpus, v, behavior_oracles)
                for v in SYNTHETIC_VECTORS
            ]
            gemini_rows = gemini_blocked_rows()
            offline_pass = sum(1 for r in offline_rows if r["status"] == "ok")
            offline_meta = {
                "total": len(offline_rows),
                "pass": offline_pass,
                "fail": len(offline_rows) - offline_pass,
            }
            # Preserve any prior live rows if results exist
            prior_live: list[dict[str, Any]] = []
            if RESULTS_JSON_PATH.is_file():
                try:
                    prior = load_json(RESULTS_JSON_PATH)
                    prior_live = [
                        r
                        for r in (prior.get("rows") or [])
                        if r.get("mode") == "live"
                        and r.get("status")
                        not in ("not_run_missing_credentials", "dry_run")
                    ]
                except Exception:
                    prior_live = []
            rows = offline_rows + synth_rows + gemini_rows + prior_live
            aggregates = {
                GROQ_MODEL_ID: aggregate_model(rows, GROQ_MODEL_ID),
                "gemini-3.5-flash-lite": aggregate_model(rows, "gemini-3.5-flash-lite"),
                "gemini-3.6-flash": aggregate_model(rows, "gemini-3.6-flash"),
            }
            results = {
                "benchmark_id": "voisu-developer-prompt-rendering-model-benchmark-2026-08-11",
                "issue": 140,
                "generated_at": utc_now(),
                "governing": {
                    "combined_call_corpus": CC_CORPUS_PATH.name,
                    "combined_call_version": corpus.get("version"),
                    "behavior_corpus": BEHAVIOR_CORPUS_PATH.name,
                    "behavior_version": behavior.get("version"),
                    "delivery_deadline_ms": DELIVERY_DEADLINE_MS,
                    "compose_gates": "v1.1.2 completeness+source_order+protected+invented",
                },
                "models": [
                    {
                        "model_id": GROQ_MODEL_ID,
                        "provider": "groq",
                        "prompt_contract": True,
                    },
                    {
                        "model_id": "gemini-3.5-flash-lite",
                        "provider": "google",
                        "prompt_contract": True,
                    },
                    {
                        "model_id": "gemini-3.6-flash",
                        "provider": "google",
                        "prompt_contract": True,
                    },
                ],
                "matrix": {
                    "live_vectors": LIVE_VECTORS,
                    "synthetic_vectors": SYNTHETIC_VECTORS,
                    "trials_default": TRIALS_DEFAULT,
                },
                "rows": rows,
                "offline_replay": offline_meta,
                "aggregates": aggregates,
                "recommendation": build_recommendation(
                    aggregates, offline_meta, gemini_ran=False
                ),
                "limitations": [
                    "Self-check path may preserve prior live rows if present.",
                    "Gemini not run without credentials.",
                ],
            }
            if not args.no_write:
                RESULTS_JSON_PATH.write_text(
                    json.dumps(results, indent=2, ensure_ascii=False) + "\n",
                    encoding="utf-8",
                )
                write_markdown(results)
                print(f"Wrote {RESULTS_JSON_PATH.name} and {RESULTS_MD_PATH.name}")
            return 0

    do_live = args.live and not args.dry_run
    do_dry = args.dry_run
    provider = args.provider

    rows: list[dict[str, Any]] = []
    # always include offline replay + synthetics for package completeness
    offline_rows = offline_replay_rows(mod, corpus, behavior_oracles)
    rows.extend(offline_rows)
    for v in SYNTHETIC_VECTORS:
        rows.append(run_synthetic_vector(mod, corpus, v, behavior_oracles))

    offline_pass = sum(1 for r in offline_rows if r["status"] == "ok")
    offline_meta = {
        "total": len(offline_rows),
        "pass": offline_pass,
        "fail": len(offline_rows) - offline_pass,
    }

    gemini_ran = False
    if provider in ("groq", "all"):
        rows.extend(
            run_groq_live(
                mod,
                corpus,
                behavior_oracles,
                trials=max(1, args.trials),
                dry_run=do_dry or not do_live,
            )
        )
    if provider in ("gemini", "all"):
        # Live Gemini only if keys exist
        gkey = env_gemini_key()
        if do_live and gkey:
            # Not implemented against Google API in this host package — mark not_run
            # with explicit note rather than inventing an incomplete client.
            print(
                "Gemini key present in env but Google live client is intentionally "
                "minimal in this package; marking not_run_missing_transport "
                "to avoid fabricated scores. Prefer full Gemini path in follow-up.",
                file=sys.stderr,
            )
            for model_id in GEMINI_MODELS:
                for v in LIVE_VECTORS:
                    rows.append(
                        {
                            "vector_id": v["vector_id"],
                            "fixture_id": v["fixture_id"],
                            "category": v["category"],
                            "mode": "live",
                            "provider": "google",
                            "model_id": model_id,
                            "status": "not_run_missing_transport",
                            "trial": 1,
                            "structured_parse_ok": None,
                            "compose_decision": None,
                            "error_codes": [],
                            "semantic_match_oracle": None,
                            "latency_ms": None,
                            "within_1500ms": None,
                            "would_deliver_model_text": False,
                            "notes": "Key present but Gemini HTTP client not implemented in this research package; do not invent scores.",
                        }
                    )
        else:
            rows.extend(gemini_blocked_rows())
    elif provider == "groq":
        # still record gemini blocked for matrix honesty
        rows.extend(gemini_blocked_rows())

    aggregates = {
        GROQ_MODEL_ID: aggregate_model(rows, GROQ_MODEL_ID),
        "gemini-3.5-flash-lite": aggregate_model(rows, "gemini-3.5-flash-lite"),
        "gemini-3.6-flash": aggregate_model(rows, "gemini-3.6-flash"),
    }
    recommendation = build_recommendation(
        aggregates, offline_meta, gemini_ran=gemini_ran
    )

    results = {
        "benchmark_id": "voisu-developer-prompt-rendering-model-benchmark-2026-08-11",
        "issue": 140,
        "generated_at": utc_now(),
        "governing": {
            "combined_call_corpus": CC_CORPUS_PATH.name,
            "combined_call_version": corpus.get("version"),
            "behavior_corpus": BEHAVIOR_CORPUS_PATH.name,
            "behavior_version": behavior.get("version"),
            "delivery_deadline_ms": DELIVERY_DEADLINE_MS,
            "compose_gates": "v1.1.2 completeness+source_order+protected+invented",
            "prior_method": "groq-reconciliation-model-benchmark-2026-08-09.md",
        },
        "models": [
            {
                "model_id": GROQ_MODEL_ID,
                "provider": "groq",
                "prompt_contract": True,
                "live_config": {
                    "reasoning_effort": GROQ_REASONING_EFFORT,
                    "temperature": TEMPERATURE,
                    "response_format": "json_schema.strict",
                },
            },
            {
                "model_id": "gemini-3.5-flash-lite",
                "provider": "google",
                "prompt_contract": True,
            },
            {
                "model_id": "gemini-3.6-flash",
                "provider": "google",
                "prompt_contract": True,
            },
        ],
        "matrix": {
            "live_vectors": LIVE_VECTORS,
            "synthetic_vectors": SYNTHETIC_VECTORS,
            "trials": max(1, args.trials),
        },
        "rows": rows,
        "offline_replay": offline_meta,
        "aggregates": aggregates,
        "recommendation": recommendation,
        "limitations": [
            "Gemini models not live-run without credentials; recommendation provisional.",
            "Live wall latency includes queue_time; production must still enforce 1.5s.",
            "Oracle match is string equality (light whitespace normalize) against "
            "#138/#139 expected finals — not a soft BLEU score.",
            "No product Rust changes; research package only.",
        ],
    }

    if not args.no_write:
        RESULTS_JSON_PATH.write_text(
            json.dumps(results, indent=2, ensure_ascii=False) + "\n",
            encoding="utf-8",
        )
        write_markdown(results)
        print(f"Wrote {RESULTS_JSON_PATH}")
        print(f"Wrote {RESULTS_MD_PATH}")

    # Summary to stdout
    print("OFFLINE", offline_meta)
    for mid, agg in aggregates.items():
        print(
            "AGG",
            mid,
            agg.get("status"),
            "parse",
            agg.get("structured_parse_ok_rate"),
            "sem",
            agg.get("semantic_match_rate"),
            "p50",
            agg.get("latency_p50_ms"),
            "p95",
            agg.get("latency_p95_ms"),
            "unsafe",
            agg.get("unsafe_deliver_count"),
        )
    print("RECOMMENDATION", recommendation.get("status"))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
