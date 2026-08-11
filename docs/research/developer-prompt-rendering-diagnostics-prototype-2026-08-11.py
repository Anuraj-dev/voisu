#!/usr/bin/env python3
"""Hermetic #142 diagnostics + fallback-feedback package proof.

Validates the diagnostic event timeline, user-facing feedback catalog,
evaluation vs production late-result rules, payload bounds, and unsent
Delivery flags. Standard-library only. Exit 0 iff healthy.
"""

from __future__ import annotations

import copy
import json
import re
import sys
from pathlib import Path
from typing import Any

HERE = Path(__file__).resolve().parent
CORPUS_PATH = HERE / "developer-prompt-rendering-diagnostics-corpus-2026-08-11.json"
SCHEMA_PATH = HERE / "developer-prompt-rendering-diagnostics-schema-2026-08-11.json"

CORPUS_ID = "voisu-developer-prompt-rendering-diagnostics-2026-08-11"
FIXTURE_ID_RE = re.compile(r"^DX-[0-9]{2}$")
FINGERPRINT_RE = re.compile(r"^sha256:[0-9a-f]{64}$")

MODES = ("evaluation", "production")
FEEDBACK_KINDS = ("silent", "minimal_status")
EVENT_NAMES = (
    "route_selected",
    "cloud_skipped",
    "cloud_request_started",
    "cloud_response_received",
    "cloud_deadline_exceeded",
    "provider_failed",
    "schema_validation_failed",
    "source_derivation_failed",
    "composition_accepted",
    "fallback_baseline_selected",
    "delivery_emitted",
    "late_result_retained",
    "late_result_discarded",
)
COMPOSITION_DECISIONS = (
    "accept",
    "accept_preserve_words",
    "accept_natural_layout",
    "fallback_baseline",
)
HARD_FALLBACK_TRIGGERS = frozenset(
    {
        "deadline_exceeded",
        "provider_failure",
        "response_schema_failure",
        "unverifiable_source_derivation",
        "unsafe_semantics",
        "invalid_fixed_label",
    }
)
SOFT_TRIGGERS = frozenset({"uncertain_backtracking", "uncertain_layout"})
MINIMAL_MESSAGE = "Local formatting used"

TOP_REQUIRED = [
    "corpus_id",
    "version",
    "issue",
    "language",
    "governing",
    "modes",
    "feedback_kinds",
    "feedback_catalog",
    "event_names",
    "composition_decisions",
    "fallback_triggers",
    "error_codes",
    "thresholds",
    "invariants",
    "production_cleanup",
    "fixtures",
    "dataset_counts",
]
FIXTURE_KEYS = {
    "id",
    "title",
    "mode",
    "roles",
    "related_behavior_fixture_ids",
    "related_combined_call_ids",
    "route",
    "cloud_request",
    "composition_decision",
    "fallback_trigger",
    "error_codes",
    "feedback",
    "delivery",
    "correlation_id",
    "events",
    "late_result",
    "rationale",
}
DELIVERY_KEYS = {"state", "auto_send", "live_type", "replace_delivered"}
FEEDBACK_KEYS = {"feedback_kind", "message"}
EVENT_KEYS = {
    "name",
    "t_ms",
    "detail",
    "error_codes",
    "fallback_trigger",
    "composition_decision",
    "http_body_snippet",
    "candidate_fingerprint",
    "candidate_text_clamped",
    "arrived_t_ms",
    "would_have_decision",
    "route",
    "cloud_request",
}
LATE_KEYS = {
    "lane",
    "retained",
    "arrived_t_ms",
    "candidate_fingerprint",
    "candidate_text_clamped",
    "would_have_decision",
    "compare_to_delivered",
}

DEFAULT_THRESHOLDS = {
    "delivery_deadline_ms": 1500,
    "max_events_per_recording": 24,
    "max_event_detail_utf8_bytes": 256,
    "max_feedback_message_utf8_bytes": 64,
    "max_http_body_snippet_utf8_bytes": 256,
    "max_retained_late_text_utf8_bytes": 2048,
    "max_late_results_per_recording": 1,
    "max_correlation_id_utf8_bytes": 128,
}


class CheckError(Exception):
    pass


def load_json(path: Path) -> Any:
    if not path.is_file():
        raise CheckError(f"missing file: {path}")
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except json.JSONDecodeError as exc:
        raise CheckError(f"invalid JSON {path}: {exc}") from exc


def exact_keys(obj: Any, allowed: set[str], where: str, errors: list[str]) -> bool:
    if not isinstance(obj, dict):
        errors.append(f"{where}: must be object")
        return False
    keys = set(obj)
    extra = keys - allowed
    missing = allowed - keys
    # Allow subset for events (optional fields) — callers pass max allowed set.
    if extra:
        errors.append(f"{where}: undeclared keys {sorted(extra)}")
        return False
    return True


def utf8_len(value: Any) -> int:
    if value is None:
        return 0
    if not isinstance(value, str):
        return 0
    return len(value.encode("utf-8"))


def validate_delivery(delivery: Any, where: str, errors: list[str]) -> None:
    if not isinstance(delivery, dict):
        errors.append(f"{where}: delivery must be object")
        return
    if set(delivery) != DELIVERY_KEYS:
        errors.append(f"{where}: delivery keys must be exactly {sorted(DELIVERY_KEYS)}")
    if delivery.get("state") != "unsent":
        errors.append(f"{where}: delivery.state must be unsent")
    for flag in ("auto_send", "live_type", "replace_delivered"):
        if delivery.get(flag) is not False:
            errors.append(f"{where}: delivery.{flag} must be false")


def expected_feedback(
    composition_decision: str,
    fallback_trigger: str | None,
    cloud_attempted: bool,
) -> dict[str, Any]:
    if composition_decision == "fallback_baseline" and cloud_attempted and (
        fallback_trigger in HARD_FALLBACK_TRIGGERS
        or (fallback_trigger is None and cloud_attempted is False)
    ):
        # hard fallback after cloud attempt
        if fallback_trigger in HARD_FALLBACK_TRIGGERS:
            return {"feedback_kind": "minimal_status", "message": MINIMAL_MESSAGE}
    if composition_decision == "fallback_baseline" and not cloud_attempted:
        return {"feedback_kind": "silent", "message": None}
    if composition_decision in {
        "accept",
        "accept_preserve_words",
        "accept_natural_layout",
    }:
        return {"feedback_kind": "silent", "message": None}
    if composition_decision == "fallback_baseline" and fallback_trigger in HARD_FALLBACK_TRIGGERS:
        return {"feedback_kind": "minimal_status", "message": MINIMAL_MESSAGE}
    return {"feedback_kind": "silent", "message": None}


def cloud_attempted_from_events(events: list[dict[str, Any]]) -> bool:
    return any(e.get("name") == "cloud_request_started" for e in events)


def validate_events(
    events: Any,
    mode: str,
    thresholds: dict[str, int],
    where: str,
    errors: list[str],
) -> None:
    if not isinstance(events, list) or not events:
        errors.append(f"{where}: events must be non-empty array")
        return
    if len(events) > thresholds["max_events_per_recording"]:
        errors.append(
            f"{where}: events count {len(events)} exceeds "
            f"{thresholds['max_events_per_recording']}"
        )

    names = [e.get("name") for e in events if isinstance(e, dict)]
    if names and names[0] != "route_selected":
        errors.append(f"{where}: first event must be route_selected")
    if names.count("delivery_emitted") != 1:
        errors.append(f"{where}: exactly one delivery_emitted required")
    if names.count("cloud_request_started") > 1:
        errors.append(f"{where}: at most one cloud_request_started (I-ONE-CALL)")
    if names.count("composition_accepted") + names.count("fallback_baseline_selected") != 1:
        errors.append(
            f"{where}: exactly one of composition_accepted or fallback_baseline_selected"
        )

    delivery_idx = next(
        (i for i, e in enumerate(events) if isinstance(e, dict) and e.get("name") == "delivery_emitted"),
        -1,
    )
    start_idx = next(
        (
            i
            for i, e in enumerate(events)
            if isinstance(e, dict) and e.get("name") == "cloud_request_started"
        ),
        -1,
    )

    prev_t = -1
    retained_count = 0
    for i, event in enumerate(events):
        prefix = f"{where}.events[{i}]"
        if not isinstance(event, dict):
            errors.append(f"{prefix}: must be object")
            continue
        extra = set(event) - EVENT_KEYS
        if extra:
            errors.append(f"{prefix}: undeclared keys {sorted(extra)}")
        name = event.get("name")
        if name not in EVENT_NAMES:
            errors.append(f"{prefix}: unknown event name {name!r}")
        t_ms = event.get("t_ms")
        if not isinstance(t_ms, int) or t_ms < 0:
            errors.append(f"{prefix}: t_ms must be int >= 0")
        else:
            if t_ms < prev_t:
                errors.append(f"{prefix}: t_ms must be non-decreasing")
            prev_t = t_ms

        detail = event.get("detail")
        if detail is not None and utf8_len(detail) > thresholds["max_event_detail_utf8_bytes"]:
            errors.append(f"{prefix}: detail exceeds max_event_detail_utf8_bytes")
        snippet = event.get("http_body_snippet")
        if snippet is not None and utf8_len(snippet) > thresholds["max_http_body_snippet_utf8_bytes"]:
            errors.append(f"{prefix}: http_body_snippet exceeds cap")
        clamped = event.get("candidate_text_clamped")
        if clamped is not None and utf8_len(clamped) > thresholds["max_retained_late_text_utf8_bytes"]:
            errors.append(f"{prefix}: candidate_text_clamped exceeds cap")
        fp = event.get("candidate_fingerprint")
        if fp is not None and not (isinstance(fp, str) and FINGERPRINT_RE.match(fp)):
            errors.append(f"{prefix}: candidate_fingerprint must be sha256:64hex")

        # secret-ish redaction heuristics
        for field in ("detail", "http_body_snippet", "candidate_text_clamped"):
            val = event.get(field)
            if isinstance(val, str) and re.search(
                r"(api[_-]?key|authorization\s*:|bearer\s+[a-z0-9._-]+)",
                val,
                flags=re.I,
            ):
                errors.append(f"{prefix}: possible secret material in {field}")

        if name in {"late_result_retained", "late_result_discarded"}:
            if delivery_idx < 0 or i <= delivery_idx:
                errors.append(f"{prefix}: late_result_* must occur after delivery_emitted")
        if name == "late_result_retained":
            retained_count += 1
            if mode != "evaluation":
                errors.append(f"{prefix}: late_result_retained only allowed in evaluation mode")

        if name in {
            "cloud_response_received",
            "cloud_deadline_exceeded",
            "provider_failed",
        }:
            if start_idx < 0 or i < start_idx:
                errors.append(f"{prefix}: cloud end/failure requires prior cloud_request_started")

        if name == "composition_accepted" and delivery_idx >= 0 and i > delivery_idx:
            errors.append(f"{prefix}: composition_accepted must precede delivery_emitted")
        if name == "fallback_baseline_selected" and delivery_idx >= 0 and i > delivery_idx:
            errors.append(
                f"{prefix}: fallback_baseline_selected must precede delivery_emitted"
            )

    if retained_count > thresholds["max_late_results_per_recording"]:
        errors.append(f"{where}: too many late_result_retained events")


def validate_late_result(
    late: Any,
    mode: str,
    events: list[dict[str, Any]],
    thresholds: dict[str, int],
    where: str,
    errors: list[str],
) -> None:
    if late is None:
        if any(
            isinstance(e, dict) and e.get("name") == "late_result_retained" for e in events
        ):
            errors.append(f"{where}: late_result_retained event requires late_result object")
        return
    if not isinstance(late, dict):
        errors.append(f"{where}.late_result: must be object or null")
        return
    extra = set(late) - LATE_KEYS
    if extra:
        errors.append(f"{where}.late_result: undeclared keys {sorted(extra)}")
    if late.get("lane") != "late_result_evaluation":
        errors.append(f"{where}.late_result.lane must be late_result_evaluation")
    retained = late.get("retained")
    if not isinstance(retained, bool):
        errors.append(f"{where}.late_result.retained must be bool")
    if mode == "production" and retained is True:
        errors.append(f"{where}: production must not retain late results")
    if mode == "production" and late.get("candidate_text_clamped") not in (None, ""):
        errors.append(f"{where}: production must not store late candidate_text_clamped")
    if retained is True:
        if mode != "evaluation":
            errors.append(f"{where}: retained=true only in evaluation")
        text = late.get("candidate_text_clamped")
        if not isinstance(text, str) or not text:
            errors.append(f"{where}: retained late result needs candidate_text_clamped")
        elif utf8_len(text) > thresholds["max_retained_late_text_utf8_bytes"]:
            errors.append(f"{where}: retained text exceeds cap")
        fp = late.get("candidate_fingerprint")
        if not (isinstance(fp, str) and FINGERPRINT_RE.match(fp)):
            errors.append(f"{where}: retained late result needs valid fingerprint")
        if late.get("compare_to_delivered") is not True:
            errors.append(f"{where}: retained late result should set compare_to_delivered true")
        if not any(
            isinstance(e, dict) and e.get("name") == "late_result_retained" for e in events
        ):
            errors.append(f"{where}: retained late_result requires late_result_retained event")


def validate_fixture(
    fixture: dict[str, Any],
    thresholds: dict[str, int],
    where: str,
    errors: list[str],
) -> None:
    missing = FIXTURE_KEYS - set(fixture)
    extra = set(fixture) - FIXTURE_KEYS
    if missing:
        errors.append(f"{where}: missing keys {sorted(missing)}")
    if extra:
        errors.append(f"{where}: undeclared keys {sorted(extra)}")

    fid = fixture.get("id")
    if not isinstance(fid, str) or not FIXTURE_ID_RE.match(fid):
        errors.append(f"{where}: id must match DX-NN")

    mode = fixture.get("mode")
    if mode not in MODES:
        errors.append(f"{where}: mode invalid")

    decision = fixture.get("composition_decision")
    if decision not in COMPOSITION_DECISIONS:
        errors.append(f"{where}: composition_decision invalid")

    trigger = fixture.get("fallback_trigger")
    if trigger is not None and not isinstance(trigger, str):
        errors.append(f"{where}: fallback_trigger must be string or null")

    events = fixture.get("events") if isinstance(fixture.get("events"), list) else []
    validate_events(fixture.get("events"), str(mode), thresholds, where, errors)
    validate_delivery(fixture.get("delivery"), where, errors)

    feedback = fixture.get("feedback")
    if not isinstance(feedback, dict) or set(feedback) != FEEDBACK_KEYS:
        errors.append(f"{where}: feedback must have feedback_kind + message")
    else:
        kind = feedback.get("feedback_kind")
        if kind not in FEEDBACK_KINDS:
            errors.append(f"{where}: feedback_kind invalid")
        msg = feedback.get("message")
        if kind == "silent" and msg is not None:
            errors.append(f"{where}: silent feedback message must be null")
        if kind == "minimal_status":
            if msg != MINIMAL_MESSAGE:
                errors.append(f"{where}: minimal_status message must be exact catalog string")
            if utf8_len(msg) > thresholds["max_feedback_message_utf8_bytes"]:
                errors.append(f"{where}: feedback message exceeds cap")

        attempted = cloud_attempted_from_events(
            [e for e in events if isinstance(e, dict)]
        )
        expected = expected_feedback(str(decision), trigger if isinstance(trigger, str) or trigger is None else None, attempted)
        # Refine: hard fallback after cloud → minimal; else silent
        if decision == "fallback_baseline" and attempted and trigger in HARD_FALLBACK_TRIGGERS:
            expected = {"feedback_kind": "minimal_status", "message": MINIMAL_MESSAGE}
        elif decision in {"accept", "accept_preserve_words", "accept_natural_layout"}:
            expected = {"feedback_kind": "silent", "message": None}
        elif decision == "fallback_baseline" and not attempted:
            expected = {"feedback_kind": "silent", "message": None}
        elif decision == "fallback_baseline" and trigger in SOFT_TRIGGERS:
            expected = {"feedback_kind": "silent", "message": None}
        if feedback.get("feedback_kind") != expected["feedback_kind"] or feedback.get(
            "message"
        ) != expected["message"]:
            errors.append(
                f"{where}: feedback {feedback} does not match policy {expected} "
                f"(decision={decision} trigger={trigger} attempted={attempted})"
            )

    cid = fixture.get("correlation_id")
    if not isinstance(cid, str) or not cid:
        errors.append(f"{where}: correlation_id required")
    elif utf8_len(cid) > thresholds["max_correlation_id_utf8_bytes"]:
        errors.append(f"{where}: correlation_id exceeds cap")

    # Decision consistency with events
    names = [e.get("name") for e in events if isinstance(e, dict)]
    if decision in {"accept", "accept_preserve_words", "accept_natural_layout"}:
        if "composition_accepted" not in names:
            errors.append(f"{where}: accept-path requires composition_accepted event")
    if decision == "fallback_baseline" and "fallback_baseline_selected" not in names:
        errors.append(f"{where}: fallback requires fallback_baseline_selected event")

    validate_late_result(
        fixture.get("late_result"),
        str(mode),
        [e for e in events if isinstance(e, dict)],
        thresholds,
        where,
        errors,
    )


def validate_package(corpus: Any, schema: Any) -> list[str]:
    errors: list[str] = []
    if not isinstance(corpus, dict):
        return ["corpus must be object"]
    if not isinstance(schema, dict):
        errors.append("schema must be object")

    missing = [k for k in TOP_REQUIRED if k not in corpus]
    extra = [k for k in corpus if k not in TOP_REQUIRED]
    if missing:
        errors.append(f"corpus missing keys: {missing}")
    if extra:
        errors.append(f"corpus undeclared keys: {extra}")

    if corpus.get("corpus_id") != CORPUS_ID:
        errors.append("corpus_id mismatch")
    if corpus.get("language") != "en":
        errors.append("language must be en")

    issue = corpus.get("issue")
    if isinstance(issue, dict):
        if issue.get("github_number") != 142:
            errors.append("issue.github_number must be 142")
        if issue.get("parent_map") != 133:
            errors.append("issue.parent_map must be 133")
        if issue.get("blocked_by") != 139:
            errors.append("issue.blocked_by must be 139")

    if corpus.get("modes") != list(MODES):
        errors.append("modes must be [evaluation, production]")
    if corpus.get("feedback_kinds") != list(FEEDBACK_KINDS):
        errors.append("feedback_kinds mismatch")
    if corpus.get("event_names") != list(EVENT_NAMES):
        errors.append("event_names must match closed catalog exactly")

    thresholds = corpus.get("thresholds")
    if not isinstance(thresholds, dict):
        errors.append("thresholds must be object")
        thresholds = dict(DEFAULT_THRESHOLDS)
    else:
        for key, expected in DEFAULT_THRESHOLDS.items():
            if thresholds.get(key) != expected:
                errors.append(f"thresholds.{key} must be {expected}")

    catalog = corpus.get("feedback_catalog")
    if not isinstance(catalog, list) or len(catalog) < 2:
        errors.append("feedback_catalog must list silent + minimal_status")
    else:
        messages = {
            (c.get("feedback_kind"), c.get("message"))
            for c in catalog
            if isinstance(c, dict)
        }
        if ("silent", None) not in messages:
            errors.append("feedback_catalog missing silent/null")
        if ("minimal_status", MINIMAL_MESSAGE) not in messages:
            errors.append("feedback_catalog missing exact minimal_status message")

    cleanup = corpus.get("production_cleanup")
    if not isinstance(cleanup, dict) or "evaluation_only" not in cleanup or "retained_forever" not in cleanup:
        errors.append("production_cleanup must list evaluation_only and retained_forever")
    else:
        if not cleanup.get("evaluation_only"):
            errors.append("production_cleanup.evaluation_only must be non-empty")
        if not cleanup.get("retained_forever"):
            errors.append("production_cleanup.retained_forever must be non-empty")

    fixtures = corpus.get("fixtures")
    if not isinstance(fixtures, list) or not fixtures:
        errors.append("fixtures must be non-empty array")
        return errors

    ids: set[str] = set()
    for idx, fixture in enumerate(fixtures):
        where = f"fixtures[{idx}]"
        if not isinstance(fixture, dict):
            errors.append(f"{where}: must be object")
            continue
        validate_fixture(fixture, thresholds if isinstance(thresholds, dict) else DEFAULT_THRESHOLDS, where, errors)
        fid = fixture.get("id")
        if isinstance(fid, str):
            if fid in ids:
                errors.append(f"duplicate fixture id {fid}")
            ids.add(fid)

    # dataset counts
    counts = corpus.get("dataset_counts")
    if isinstance(counts, dict) and isinstance(fixtures, list):
        total = len(fixtures)
        if counts.get("fixtures_total") != total:
            errors.append(
                f"dataset_counts.fixtures_total {counts.get('fixtures_total')} != {total}"
            )
        eval_n = sum(1 for f in fixtures if isinstance(f, dict) and f.get("mode") == "evaluation")
        prod_n = sum(1 for f in fixtures if isinstance(f, dict) and f.get("mode") == "production")
        if counts.get("evaluation") != eval_n:
            errors.append(f"dataset_counts.evaluation expected {eval_n}")
        if counts.get("production") != prod_n:
            errors.append(f"dataset_counts.production expected {prod_n}")

    return errors


def run_mutations(corpus: dict[str, Any], schema: Any) -> list[str]:
    failures: list[str] = []
    thresholds = corpus.get("thresholds") if isinstance(corpus.get("thresholds"), dict) else DEFAULT_THRESHOLDS

    def mut(name: str, needles: list[str], apply) -> None:
        clone = copy.deepcopy(corpus)
        apply(clone)
        errs = validate_package(clone, schema)
        joined = " | ".join(errs)
        if not errs:
            failures.append(f"mutation {name}: expected diagnostics, got clean")
            return
        missing = [n for n in needles if n not in joined]
        if missing:
            failures.append(
                f"mutation {name}: missing diagnostics {missing}; got {errs[:6]}"
            )

    def find(c: dict[str, Any], pred):
        for f in c["fixtures"]:
            if pred(f):
                return f
        raise KeyError("fixture not found")

    mut(
        "replace_delivered_true",
        ["replace_delivered", "false"],
        lambda c: find(c, lambda x: x["id"] == "DX-01").__setitem__(
            "delivery",
            {
                "state": "unsent",
                "auto_send": False,
                "live_type": False,
                "replace_delivered": True,
            },
        ),
    )

    def prod_retain_late(c: dict[str, Any]) -> None:
        f = find(c, lambda x: x["id"] == "DX-09")
        f["late_result"]["retained"] = True
        f["late_result"]["candidate_text_clamped"] = "Should not retain in production"
        f["events"].append(
            {
                "name": "late_result_retained",
                "t_ms": 1700,
                "arrived_t_ms": 1700,
                "candidate_fingerprint": "sha256:" + ("c" * 64),
                "candidate_text_clamped": "Should not retain in production",
                "would_have_decision": "accept",
            }
        )

    mut(
        "production_retain_late",
        ["production", "retain"],
        prod_retain_late,
    )

    def reverse_event_order(c: dict[str, Any]) -> None:
        f = find(c, lambda x: x["id"] == "DX-01")
        # swap response before request
        f["events"] = [
            f["events"][0],
            f["events"][2],  # response
            f["events"][1],  # start
            f["events"][3],
            f["events"][4],
        ]

    mut(
        "cloud_response_before_start",
        ["cloud_request_started"],
        reverse_event_order,
    )

    def false_cloud_success_message(c: dict[str, Any]) -> None:
        f = find(c, lambda x: x["id"] == "DX-02")
        f["feedback"] = {
            "feedback_kind": "minimal_status",
            "message": "Cloud enhanced your text",
        }

    mut(
        "false_cloud_success_message",
        ["message", "Local formatting used"],
        false_cloud_success_message,
    )

    def late_before_delivery(c: dict[str, Any]) -> None:
        f = find(c, lambda x: x["id"] == "DX-08")
        # move late_result_retained before delivery
        events = f["events"]
        late = events.pop()
        # insert before delivery_emitted
        di = next(i for i, e in enumerate(events) if e["name"] == "delivery_emitted")
        events.insert(di, late)

    mut(
        "late_before_delivery",
        ["after delivery_emitted"],
        late_before_delivery,
    )

    def double_cloud_start(c: dict[str, Any]) -> None:
        f = find(c, lambda x: x["id"] == "DX-01")
        f["events"].insert(
            2,
            {"name": "cloud_request_started", "t_ms": 20},
        )

    mut(
        "double_cloud_start",
        ["at most one cloud_request_started"],
        double_cloud_start,
    )

    def secret_in_snippet(c: dict[str, Any]) -> None:
        f = find(c, lambda x: x["id"] == "DX-04")
        for e in f["events"]:
            if e.get("name") == "cloud_response_received":
                e["http_body_snippet"] = "Authorization: Bearer super-secret-token-value"

    mut(
        "secret_in_http_snippet",
        ["secret"],
        secret_in_snippet,
    )

    def hard_fallback_silent(c: dict[str, Any]) -> None:
        f = find(c, lambda x: x["id"] == "DX-02")
        f["feedback"] = {"feedback_kind": "silent", "message": None}

    mut(
        "hard_fallback_must_minimal",
        ["feedback", "minimal_status"],
        hard_fallback_silent,
    )

    def decreasing_t(c: dict[str, Any]) -> None:
        f = find(c, lambda x: x["id"] == "DX-01")
        f["events"][-1]["t_ms"] = 0

    mut(
        "decreasing_timestamps",
        ["non-decreasing"],
        decreasing_t,
    )

    def oversize_detail(c: dict[str, Any]) -> None:
        f = find(c, lambda x: x["id"] == "DX-03")
        for e in f["events"]:
            if e.get("name") == "provider_failed":
                e["detail"] = "x" * 300

    mut(
        "oversize_detail",
        ["detail exceeds"],
        oversize_detail,
    )

    # thresholds mutation should fail counts/policy
    mut(
        "wrong_deadline",
        ["delivery_deadline_ms"],
        lambda c: c["thresholds"].__setitem__("delivery_deadline_ms", 5000),
    )

    return failures


def main(argv: list[str]) -> int:
    del argv
    try:
        corpus = load_json(CORPUS_PATH)
        schema = load_json(SCHEMA_PATH)
    except CheckError as exc:
        print(f"FAIL: {exc}", file=sys.stderr)
        return 1

    errors = validate_package(corpus, schema)
    if not isinstance(corpus, dict):
        print("FAIL: corpus shape", file=sys.stderr)
        return 1

    mutation_failures = run_mutations(corpus, schema)
    errors.extend(mutation_failures)

    if errors:
        print(f"FAIL: {len(errors)} error(s)", file=sys.stderr)
        for err in errors:
            print(f"  - {err}", file=sys.stderr)
        return 1

    counts = corpus["dataset_counts"]
    print("OK: developer-prompt-rendering-diagnostics package")
    print(f"  version: {corpus.get('version')}")
    print(f"  fixtures: {counts['fixtures_total']}")
    print(
        f"  modes: evaluation={counts['evaluation']} production={counts['production']}"
    )
    print(f"  events catalog: {len(corpus['event_names'])}")
    print(f"  invariants: {len(corpus['invariants'])}")
    print("  mutations: 11 property-bound")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
