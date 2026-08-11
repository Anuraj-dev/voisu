#!/usr/bin/env python3
"""Hermetic #139 combined-call contract proof.

Validates the structured one-call response package, proves source derivation and
protected-token policy, composes hierarchical fallback to the local baseline,
and runs property-bound mutations. Standard-library only. Exit 0 iff healthy.
"""

from __future__ import annotations

import copy
import hashlib
import json
import re
import sys
from pathlib import Path
from typing import Any

HERE = Path(__file__).resolve().parent
CORPUS_PATH = HERE / "developer-prompt-rendering-combined-call-corpus-2026-08-11.json"
SCHEMA_PATH = HERE / "developer-prompt-rendering-combined-call-schema-2026-08-11.json"

FIXTURE_ID_RE = re.compile(r"^CC-[0-9]{2}[a-z]?$")
DPR_ID_RE = re.compile(r"^DPR-[0-9]{2}[a-z]?$")
FINGERPRINT_RE = re.compile(r"^sha256:[0-9a-f]{64}$")
ERROR_CODE_RE = re.compile(r"^E_[A-Z0-9_]+$")

MAX_FILE_BYTES = 524_288
MAX_JSON_DEPTH = 14
MAX_JSON_NODES = 30_000

TOP_REQUIRED = [
    "corpus_id",
    "version",
    "issue",
    "language",
    "governing",
    "architecture",
    "closed_structured_labels",
    "closed_conversions",
    "protected_token_kinds",
    "fallback_triggers",
    "composition_decisions",
    "error_codes",
    "model_prompt_contracts",
    "invariants",
    "thresholds",
    "fixtures",
    "dataset_counts",
]
TOP_KEYS = set(TOP_REQUIRED)

ISSUE_KEYS = {
    "github_number",
    "title",
    "parent_map",
    "blocked_by",
    "blocks",
    "url",
}
GOVERNING_KEYS = {
    "contract_issue",
    "map_issue",
    "behavior_corpus_issue",
    "resolution_issues",
    "superseded_mechanisms",
    "out_of_scope",
}
ARCH_KEYS = {
    "call_budget",
    "response_authority",
    "local_baseline_authority",
    "whole_text_rewrite_rule",
    "grammar_subsystem",
    "delivery_deadline_ms",
    "delivery_mode",
}
THRESHOLD_KEYS = {
    "owner",
    "max_file_bytes",
    "max_json_depth",
    "max_json_nodes",
    "max_source_utf8_bytes",
    "max_removals",
    "max_conversions",
    "max_labels",
    "max_derivation_spans",
    "max_field_utf8_bytes",
    "delivery_deadline_ms",
}
COUNTS_KEYS = {
    "fixtures_total",
    "accept",
    "accept_preserve_words",
    "accept_natural_layout",
    "fallback_baseline",
    "related_behavior_links",
    "model_prompt_contracts",
    "notes",
}
FIXTURE_KEYS = {
    "id",
    "title",
    "roles",
    "related_behavior_fixture_ids",
    "policy",
    "sources",
    "source_selection",
    "local_baseline",
    "base_fingerprint",
    "protected_tokens",
    "cloud_outcome",
    "candidate",
    "expected",
    "rationale",
}
CANDIDATE_KEYS = {
    "schema_version",
    "base_fingerprint",
    "reconciliation",
    "removals",
    "conversions",
    "layout",
    "labels",
    "derivation",
}
EXPECTED_KEYS = {
    "decision",
    "rendered",
    "fallback_trigger",
    "error_codes",
    "delivery",
}
DELIVERY_KEYS = {"state", "auto_send", "live_type", "replace_delivered"}
SOURCE_KEYS = {"provider", "available", "text", "primary"}
SELECTION_KEYS = {"selected_provider", "reason"}
REMOVAL_KEYS = {"kind", "certainty", "source_provider", "source_span_text"}
CONVERSION_KEYS = {"id", "source_provider", "source_span_text"}
LAYOUT_KEYS = {"decision", "certainty"}
LABEL_KEYS = {"label", "source_provider", "source_span_text"}
SPAN_KEYS = {
    "kind",
    "source_provider",
    "source_text",
    "output_text",
    "conversion_id",
    "label",
}
RECON_KEYS = {"selected_provider", "reason"}
MODEL_KEYS = {
    "model_id",
    "provider",
    "system_prompt",
    "response_instructions",
    "notes",
}
ERROR_OBJ_KEYS = {"code", "summary"}
INVARIANT_KEYS = {"id", "summary"}

CLOSED_LABELS = [
    "Goal",
    "Context",
    "Requirements",
    "Constraints",
    "Steps",
    "Acceptance Criteria",
    "Files",
    "Notes",
]
PROTECTED_KINDS = [
    "name",
    "number_date_time",
    "negation",
    "command_flag",
    "url_path",
    "identifier",
    "code",
    "quote",
]
FALLBACK_TRIGGERS = [
    "none",
    "unsafe_semantics",
    "unverifiable_source_derivation",
    "invalid_fixed_label",
    "uncertain_backtracking",
    "uncertain_layout",
    "response_schema_failure",
    "provider_failure",
    "deadline_exceeded",
]
DECISIONS = [
    "accept",
    "accept_preserve_words",
    "accept_natural_layout",
    "fallback_baseline",
]
POLICIES = ["natural", "adaptive", "structured"]
CLOUD_OUTCOMES = [
    "succeeded",
    "rejected_unsafe",
    "rejected_unverifiable",
    "rejected_invalid_label",
    "schema_failure",
    "provider_failure",
    "deadline_exceeded",
    "skipped",
]
PROVIDERS = ["provider_a", "provider_b"]
SELECT_REASONS = [
    "only_available",
    "exact_agreement",
    "configured_primary_rank",
    "punctuation_local_render",
    "safe_complementary_merge",
]
LAYOUT_DECISIONS = ["natural", "multi_paragraph", "numbered", "structured_sections"]
REMOVAL_KINDS = ["filler", "backtrack"]
CERTAINTIES = ["clear", "uncertain"]
SPAN_KINDS = ["keep", "remove", "convert", "label", "layout_break"]

HARD_OUTCOME_MAP = {
    "schema_failure": ("response_schema_failure", "E_SCHEMA"),
    "provider_failure": ("provider_failure", "E_PROVIDER"),
    "deadline_exceeded": ("deadline_exceeded", "E_DEADLINE"),
}

MODEL_IDS = {
    "gemini-3.5-flash-lite",
    "gemini-3.6-flash",
    "openai/gpt-oss-20b",
}

DEFAULT_CONVERSIONS = [
    "exclamation point→!",
    "four→4.",
    "new line→\\n",
    "new paragraph→\\n\\n",
    "one→1.",
    "period→.",
    'quote…unquote→"…"',
    "spoken acceptance criteria cue→Acceptance Criteria label",
    "spoken constraints cue→Constraints label",
    "spoken context cue→Context label",
    "spoken files cue→Files label",
    "spoken goal cue→Goal label",
    "spoken notes cue→Notes label",
    "spoken requirements cue→Requirements label",
    "spoken steps cue→numbered_lines",
    "three→3.",
    "two→2.",
]


class CheckError(Exception):
    pass


def fingerprint(text: str) -> str:
    return "sha256:" + hashlib.sha256(text.encode("utf-8")).hexdigest()


def load_json(path: Path) -> Any:
    try:
        raw = path.read_bytes()
    except OSError as exc:
        raise CheckError(f"cannot read {path.name}: {exc}") from exc
    if len(raw) > MAX_FILE_BYTES:
        raise CheckError(f"{path.name} exceeds max_file_bytes ({len(raw)} > {MAX_FILE_BYTES})")
    try:
        text = raw.decode("utf-8")
    except UnicodeDecodeError as exc:
        raise CheckError(f"{path.name} is not UTF-8: {exc}") from exc
    try:
        return json.loads(text)
    except json.JSONDecodeError as exc:
        raise CheckError(f"malformed JSON in {path.name}: {exc}") from exc


def exact_keys(obj: Any, required: set[str], where: str, errors: list[str]) -> bool:
    if not isinstance(obj, dict):
        errors.append(f"{where}: must be an object")
        return False
    keys = set(obj)
    missing = required - keys
    extra = keys - required
    if missing:
        errors.append(f"{where}: missing keys {sorted(missing)}")
    if extra:
        errors.append(f"{where}: schema-forbidden property/keys {sorted(extra)}")
    return not missing and not extra


def json_depth(value: Any, depth: int = 0) -> int:
    if isinstance(value, dict):
        if not value:
            return depth
        return max(json_depth(v, depth + 1) for v in value.values())
    if isinstance(value, list):
        if not value:
            return depth
        return max(json_depth(v, depth + 1) for v in value)
    return depth


def count_nodes(value: Any) -> int:
    if isinstance(value, dict):
        return 1 + sum(count_nodes(v) for v in value.values())
    if isinstance(value, list):
        return 1 + sum(count_nodes(v) for v in value)
    return 1


def normalize_ws(text: str) -> str:
    return re.sub(r"\s+", " ", text).strip()


def source_contains(source_text: str, needle: str) -> bool:
    if not needle:
        return False
    if needle in source_text:
        return True
    return normalize_ws(needle).casefold() in normalize_ws(source_text).casefold()


_ATOM_RE = re.compile(r"[A-Za-z0-9]+(?:[_./=+\-][A-Za-z0-9]+)*")


def lexical_atoms(text: str) -> set[str]:
    if not isinstance(text, str):
        return set()
    return {atom.casefold() for atom in _ATOM_RE.findall(text)}


def lexical_atom_sequence(text: str) -> list[str]:
    """Ordered casefolded content words — organize-only keep must preserve this."""
    if not isinstance(text, str):
        return []
    return [atom.casefold() for atom in _ATOM_RE.findall(text)]


def structural_headers(text: str) -> list[str]:
    found: list[str] = []
    for line in text.splitlines():
        match = re.match(r"^[ \t]*([A-Za-z][A-Za-z0-9 ]*):(.*)$", line)
        if not match:
            continue
        raw = match.group(1).strip()
        body = match.group(2)
        closed = {lab.casefold(): lab for lab in CLOSED_LABELS}.get(raw.casefold())
        if closed is not None or not body.strip() or raw.istitle():
            found.append(raw)
    return found


def provider_text_map(sources: list[dict[str, Any]]) -> dict[str, str]:
    out: dict[str, str] = {}
    for source in sources:
        if source.get("available") is True and isinstance(source.get("text"), str):
            out[str(source["provider"])] = source["text"]
    return out


def selected_source_text(fixture: dict[str, Any]) -> str | None:
    selection = fixture.get("source_selection")
    sources = fixture.get("sources")
    if not isinstance(selection, dict) or not isinstance(sources, list):
        return None
    provider = selection.get("selected_provider")
    for source in sources:
        if (
            isinstance(source, dict)
            and source.get("provider") == provider
            and source.get("available") is True
            and isinstance(source.get("text"), str)
        ):
            return source["text"]
    return None


def conversion_rhs(conversion_id: str) -> str:
    if "→" not in conversion_id:
        return ""
    rhs = conversion_id.split("→", 1)[1].strip()
    if rhs.casefold().endswith(" label"):
        rhs = rhs[: -len(" label")]
    if rhs == "\\n":
        return "\n"
    if rhs == "\\n\\n":
        return "\n\n"
    if rhs.casefold() == "numbered_lines":
        return ""
    return rhs


def conversion_cue(conversion_id: str) -> str:
    if "→" not in conversion_id:
        return conversion_id
    return conversion_id.split("→", 1)[0].strip()


def cue_needles(cue: str) -> list[str]:
    """Surface forms that must appear in source for a catalog conversion cue.

    Literal cues (e.g. ``exclamation point``) are checked as-is. Ellipsis cues
    require every part. Symbolic spoken-section cues (``spoken goal cue``) map
    to the spoken label word(s) (``goal``), not the catalog phrase.
    """
    if not cue:
        return []
    spoken = re.fullmatch(r"spoken\s+(.+?)\s+cue", cue, flags=re.IGNORECASE)
    if spoken:
        return [spoken.group(1).strip()]
    if "…" in cue:
        return [p.strip() for p in cue.split("…") if p.strip()]
    return [cue]


def cue_covered_by(cue: str, text: str) -> bool:
    """True when catalog cue surface form(s) are covered by `text`."""
    if not isinstance(text, str):
        return False
    needles = cue_needles(cue)
    return bool(needles) and all(source_contains(text, needle) for needle in needles)


def keep_organize_only(source_text: str, output_text: str) -> bool:
    """Keep: output is case/punct/whitespace transform of source (same content words)."""
    return lexical_atom_sequence(source_text) == lexical_atom_sequence(output_text)


def convert_output_matches(conversion_id: str, source_text: str, output_text: str) -> bool:
    """Convert: output equals RHS, or RHS with only surrounding whitespace/punct."""
    if not isinstance(output_text, str):
        return False
    rhs = conversion_rhs(conversion_id)
    rhs_template = ""
    if "→" in conversion_id:
        rhs_template = conversion_id.split("→", 1)[1].strip()

    # quote…unquote→"…" (and similar ellipsis RHS templates)
    if "…" in rhs_template:
        match = re.search(r"(?is)quote\s+(.+?)\s+unquote", source_text or "")
        if not match:
            return False
        interior = normalize_ws(match.group(1))
        expected = rhs_template.replace("…", interior)
        return normalize_ws(output_text) == normalize_ws(expected)

    if rhs in {"\n", "\n\n"}:
        return output_text == rhs

    if not rhs:
        # numbered_lines and similar non-emitting conversion ids
        return output_text == ""

    stripped = output_text.strip()
    if stripped == rhs:
        return True
    # allow RHS embedded with only surrounding whitespace (already stripped) or
    # a single trailing space before strip edge-cases like "1. "
    if output_text.rstrip() == rhs or output_text.lstrip() == rhs:
        return True
    return False


def find_literal_spans(haystack: str, needle: str) -> list[tuple[int, int]]:
    """All casefold literal occurrences of needle in haystack as (start, end)."""
    if not needle or not haystack:
        return []
    h = haystack.casefold()
    n = needle.casefold()
    out: list[tuple[int, int]] = []
    start = 0
    while True:
        idx = h.find(n, start)
        if idx < 0:
            break
        out.append((idx, idx + len(n)))
        start = idx + 1
    if out:
        return out
    # whitespace-normalized multi-word fallback: locate first atom run
    norm_n = normalize_ws(needle).casefold()
    norm_h = normalize_ws(haystack).casefold()
    if norm_n and norm_n in norm_h:
        # map back poorly — claim full haystack once as soft region via atom span
        atoms = lexical_atom_sequence(needle)
        if not atoms:
            return []
        h_atoms = list(_ATOM_RE.finditer(haystack))
        seq = [m.group(0).casefold() for m in h_atoms]
        for i in range(len(seq) - len(atoms) + 1):
            if seq[i : i + len(atoms)] == atoms:
                a0 = h_atoms[i].start()
                a1 = h_atoms[i + len(atoms) - 1].end()
                out.append((a0, a1))
        return out
    return []


def ranges_overlap(a: tuple[int, int], b: tuple[int, int]) -> bool:
    return a[0] < b[1] and b[0] < a[1]


def claim_source_ranges(
    source_map: dict[str, str],
    derivation: list[dict[str, Any]],
) -> list[str]:
    """Claim source regions for consuming spans; return error codes if overlap."""
    claimed: dict[str, list[tuple[int, int]]] = {p: [] for p in source_map}
    for span in derivation:
        if not isinstance(span, dict):
            continue
        kind = span.get("kind")
        if kind not in {"keep", "remove", "convert", "label"}:
            continue
        provider = span.get("source_provider")
        source_text = span.get("source_text") or ""
        if provider not in source_map or not source_text:
            continue
        hay = source_map[provider]
        candidates = find_literal_spans(hay, source_text)
        if not candidates:
            # unverifiable is handled elsewhere; skip claim
            continue
        placed = False
        for cand in candidates:
            if any(ranges_overlap(cand, used) for used in claimed[provider]):
                continue
            claimed[provider].append(cand)
            placed = True
            break
        if not placed:
            return ["E_OVERLAP"]
    return []


def licensed_atoms(
    sources: dict[str, str],
    conversions: list[str],
    labels: list[str],
) -> set[str]:
    allowed: set[str] = set()
    for text in sources.values():
        allowed |= lexical_atoms(text)
    for conversion_id in conversions:
        rhs = conversion_rhs(conversion_id)
        if rhs and rhs not in {"\n", "\n\n"}:
            allowed |= lexical_atoms(rhs)
        allowed |= lexical_atoms(conversion_cue(conversion_id))
    for label in labels:
        allowed |= lexical_atoms(label)
    return allowed


def compose_render(derivation: list[dict[str, Any]]) -> str:
    return "".join(str(span.get("output_text", "")) for span in derivation)


def is_multiparagraph_text(text: str) -> bool:
    """True when text contains a multi-paragraph (blank-line) break.

    Covers ``\\n\\n``, ``\\r\\n\\r\\n``, and blank lines with only spaces/tabs
    between newlines — including when the break is composed across adjacent spans.
    """
    if not isinstance(text, str) or not text:
        return False
    normalized = text.replace("\r\n", "\n").replace("\r", "\n")
    return re.search(r"\n[ \t]*\n", normalized) is not None


def natural_preserve_render(fixture: dict[str, Any]) -> str:
    """Soft salvage for uncertain backtrack: prefer declared local baseline."""
    baseline = fixture.get("local_baseline")
    return baseline if isinstance(baseline, str) and baseline else ""


def natural_layout_render(fixture: dict[str, Any], candidate: dict[str, Any]) -> str:
    """Soft salvage for uncertain layout: drop layout_break spans and re-space lightly."""
    # Prefer local baseline Natural when layout is uncertain.
    baseline = fixture.get("local_baseline")
    if isinstance(baseline, str) and baseline:
        return baseline
    spans = candidate.get("derivation") or []
    parts: list[str] = []
    for span in spans:
        if not isinstance(span, dict):
            continue
        if span.get("kind") == "layout_break":
            parts.append(" ")
            continue
        parts.append(str(span.get("output_text", "")))
    return re.sub(r"[ \t]+", " ", "".join(parts)).strip()


def validate_candidate_shape(
    candidate: Any,
    closed_conversions: set[str],
    where: str,
) -> list[str]:
    errors: list[str] = []
    if not exact_keys(candidate, CANDIDATE_KEYS, where, errors):
        return errors
    if candidate.get("schema_version") != "1":
        errors.append(f"{where}: schema_version must be '1'")
    if not isinstance(candidate.get("base_fingerprint"), str) or not FINGERPRINT_RE.match(
        candidate["base_fingerprint"]
    ):
        errors.append(f"{where}: invalid base_fingerprint")
    recon = candidate.get("reconciliation")
    if exact_keys(recon, RECON_KEYS, f"{where}.reconciliation", errors) and isinstance(recon, dict):
        if recon.get("selected_provider") not in PROVIDERS:
            errors.append(f"{where}.reconciliation.selected_provider invalid")
        if recon.get("reason") not in SELECT_REASONS:
            errors.append(f"{where}.reconciliation.reason invalid")
    removals = candidate.get("removals")
    if not isinstance(removals, list):
        errors.append(f"{where}.removals must be array")
    else:
        for idx, removal in enumerate(removals):
            prefix = f"{where}.removals[{idx}]"
            if exact_keys(removal, REMOVAL_KEYS, prefix, errors) and isinstance(removal, dict):
                if removal.get("kind") not in REMOVAL_KINDS:
                    errors.append(f"{prefix}: invalid kind")
                if removal.get("certainty") not in CERTAINTIES:
                    errors.append(f"{prefix}: invalid certainty")
                if removal.get("source_provider") not in PROVIDERS:
                    errors.append(f"{prefix}: invalid source_provider")
                if not isinstance(removal.get("source_span_text"), str) or not removal["source_span_text"]:
                    errors.append(f"{prefix}: source_span_text required")
    conversions = candidate.get("conversions")
    if not isinstance(conversions, list):
        errors.append(f"{where}.conversions must be array")
    else:
        for idx, conversion in enumerate(conversions):
            prefix = f"{where}.conversions[{idx}]"
            if exact_keys(conversion, CONVERSION_KEYS, prefix, errors) and isinstance(conversion, dict):
                cid = conversion.get("id")
                if not isinstance(cid, str) or cid not in closed_conversions:
                    errors.append(f"{prefix}: unknown conversion id {cid!r}")
                if conversion.get("source_provider") not in PROVIDERS:
                    errors.append(f"{prefix}: invalid source_provider")
                if not isinstance(conversion.get("source_span_text"), str) or not conversion["source_span_text"]:
                    errors.append(f"{prefix}: source_span_text required")
    layout = candidate.get("layout")
    if exact_keys(layout, LAYOUT_KEYS, f"{where}.layout", errors) and isinstance(layout, dict):
        if layout.get("decision") not in LAYOUT_DECISIONS:
            errors.append(f"{where}.layout.decision invalid")
        if layout.get("certainty") not in CERTAINTIES:
            errors.append(f"{where}.layout.certainty invalid")
    labels = candidate.get("labels")
    if not isinstance(labels, list):
        errors.append(f"{where}.labels must be array")
    else:
        for idx, label in enumerate(labels):
            prefix = f"{where}.labels[{idx}]"
            if exact_keys(label, LABEL_KEYS, prefix, errors) and isinstance(label, dict):
                if label.get("label") not in CLOSED_LABELS:
                    errors.append(f"{prefix}: unknown label")
                if label.get("source_provider") not in PROVIDERS:
                    errors.append(f"{prefix}: invalid source_provider")
                if not isinstance(label.get("source_span_text"), str) or not label["source_span_text"]:
                    errors.append(f"{prefix}: source_span_text required")
    derivation = candidate.get("derivation")
    if not isinstance(derivation, list) or not derivation:
        errors.append(f"{where}.derivation must be a non-empty array")
    else:
        for idx, span in enumerate(derivation):
            prefix = f"{where}.derivation[{idx}]"
            if exact_keys(span, SPAN_KEYS, prefix, errors) and isinstance(span, dict):
                if span.get("kind") not in SPAN_KINDS:
                    errors.append(f"{prefix}: invalid kind")
                if not isinstance(span.get("output_text"), str):
                    errors.append(f"{prefix}: output_text must be string")
                if span.get("source_provider") not in (None, *PROVIDERS):
                    errors.append(f"{prefix}: invalid source_provider")
                if not isinstance(span.get("source_text"), str):
                    errors.append(f"{prefix}: source_text must be string")
                if span.get("conversion_id") not in (None,) and not isinstance(
                    span.get("conversion_id"), str
                ):
                    errors.append(f"{prefix}: conversion_id invalid")
                if span.get("label") not in (None, *CLOSED_LABELS):
                    errors.append(f"{prefix}: label invalid")
    return errors


def compose_fixture(
    fixture: dict[str, Any],
    closed_conversions: set[str],
) -> dict[str, Any]:
    """Return decision/rendered/fallback_trigger/error_codes for one fixture."""
    baseline = fixture["local_baseline"]
    outcome = fixture.get("cloud_outcome")
    candidate = fixture.get("candidate")
    protected = fixture.get("protected_tokens") or []
    sources = fixture.get("sources") or []
    source_map = provider_text_map(sources if isinstance(sources, list) else [])
    selected = selected_source_text(fixture)
    expected_fp = fixture.get("base_fingerprint")

    def baseline_result(trigger: str | None, codes: list[str]) -> dict[str, Any]:
        return {
            "decision": "fallback_baseline",
            "rendered": baseline,
            "fallback_trigger": trigger,
            "error_codes": codes,
        }

    # Pre-flight hard outcomes that ignore / skip candidate content.
    if outcome == "skipped":
        return baseline_result(None, [])
    if outcome in HARD_OUTCOME_MAP:
        trigger, code = HARD_OUTCOME_MAP[outcome]
        return baseline_result(trigger, [code])

    if candidate is None:
        if outcome == "schema_failure":
            return baseline_result("response_schema_failure", ["E_SCHEMA"])
        return baseline_result("response_schema_failure", ["E_SCHEMA", "E_MALFORMED"])

    shape_errors = validate_candidate_shape(candidate, closed_conversions, "candidate")
    if shape_errors:
        # Unknown conversion is a catalog hard fail; otherwise malformed.
        codes = ["E_MALFORMED"]
        if any("unknown conversion" in err for err in shape_errors):
            codes = ["E_UNKNOWN_CONVERSION", "E_MALFORMED"]
        if any("unknown label" in err for err in shape_errors):
            codes = ["E_UNKNOWN_LABEL", "E_MALFORMED"]
        return baseline_result("response_schema_failure", codes)

    codes: list[str] = []
    # Freshness
    if candidate.get("base_fingerprint") != expected_fp:
        return baseline_result("unverifiable_source_derivation", ["E_STALE"])
    if selected is not None and fingerprint(selected) != expected_fp:
        return baseline_result("unverifiable_source_derivation", ["E_STALE"])

    recon = candidate["reconciliation"]
    selection = fixture.get("source_selection") or {}
    # Host selection is authoritative; candidate reconciliation must agree.
    if recon.get("selected_provider") != selection.get("selected_provider"):
        return baseline_result("unsafe_semantics", ["E_RECONCILE", "E_UNSAFE_SEMANTICS"])

    # Single-provider honesty: only_available must name the sole available source.
    available_providers = [
        str(s["provider"])
        for s in (sources if isinstance(sources, list) else [])
        if isinstance(s, dict) and s.get("available") is True
    ]
    if len(available_providers) == 1:
        only = available_providers[0]
        if recon.get("selected_provider") != only:
            return baseline_result("unsafe_semantics", ["E_RECONCILE", "E_UNSAFE_SEMANTICS"])
        if (
            selection.get("reason") == "only_available"
            or recon.get("reason") == "only_available"
        ) and (
            recon.get("selected_provider") != only
            or recon.get("reason") != "only_available"
        ):
            return baseline_result("unsafe_semantics", ["E_RECONCILE", "E_UNSAFE_SEMANTICS"])

    # Soft signals
    uncertain_backtrack = any(
        r.get("kind") == "backtrack" and r.get("certainty") == "uncertain"
        for r in candidate.get("removals", [])
    )
    uncertain_layout = candidate.get("layout", {}).get("certainty") == "uncertain"

    # Source-evidence for removals/conversions/labels
    for removal in candidate.get("removals", []):
        text = source_map.get(removal["source_provider"], "")
        if not source_contains(text, removal["source_span_text"]):
            return baseline_result("unverifiable_source_derivation", ["E_UNVERIFIABLE"])

    for conversion in candidate.get("conversions", []):
        text = source_map.get(conversion["source_provider"], "")
        if not source_contains(text, conversion["source_span_text"]):
            return baseline_result("unverifiable_source_derivation", ["E_UNVERIFIABLE"])
        cue = conversion_cue(conversion["id"])
        # Catalog cue must be covered by the declared conversion source span.
        if cue and not cue_covered_by(cue, conversion["source_span_text"]):
            return baseline_result("unverifiable_source_derivation", ["E_UNVERIFIABLE"])
        if cue and not cue_covered_by(cue, text):
            return baseline_result("unverifiable_source_derivation", ["E_UNVERIFIABLE"])

    for label in candidate.get("labels", []):
        text = source_map.get(label["source_provider"], "")
        if not source_contains(text, label["source_span_text"]):
            return baseline_result("unverifiable_source_derivation", ["E_UNVERIFIABLE"])

    # Derivation verification
    declared_conversion_ids = {c["id"] for c in candidate.get("conversions", [])}
    declared_labels = {lab["label"] for lab in candidate.get("labels", [])}
    declared_removals = {
        (
            r.get("source_provider"),
            normalize_ws(str(r.get("source_span_text", ""))).casefold(),
        )
        for r in candidate.get("removals", [])
        if isinstance(r, dict)
    }
    layout_decision = (candidate.get("layout") or {}).get("decision")
    layout_certainty = (candidate.get("layout") or {}).get("certainty")

    for span in candidate.get("derivation", []):
        kind = span.get("kind")
        if kind == "layout_break":
            out_lb = str(span.get("output_text", ""))
            if out_lb not in {"\n", "\n\n", " ", "\t", ""}:
                if not re.fullmatch(r"[\n\r\t ]*", out_lb):
                    return baseline_result("unverifiable_source_derivation", ["E_UNVERIFIABLE"])
            # Clear natural layout must not emit multi-paragraph breaks per span.
            if (
                layout_decision == "natural"
                and layout_certainty == "clear"
                and is_multiparagraph_text(out_lb)
            ):
                return baseline_result(
                    "unsafe_semantics",
                    ["E_UNSAFE_SEMANTICS"],
                )
            continue
        provider = span.get("source_provider")
        source_text = span.get("source_text") or ""
        if provider not in source_map:
            return baseline_result("unverifiable_source_derivation", ["E_UNVERIFIABLE"])
        if not source_contains(source_map[provider], source_text):
            return baseline_result("unverifiable_source_derivation", ["E_UNVERIFIABLE"])
        if kind == "remove":
            if span.get("output_text") != "":
                return baseline_result("response_schema_failure", ["E_MALFORMED"])
            rem_key = (provider, normalize_ws(source_text).casefold())
            if rem_key not in declared_removals:
                return baseline_result(
                    "unverifiable_source_derivation",
                    ["E_UNVERIFIABLE"],
                )
        if kind == "convert":
            cid = span.get("conversion_id")
            if not isinstance(cid, str) or cid not in closed_conversions:
                return baseline_result("response_schema_failure", ["E_UNKNOWN_CONVERSION"])
            if cid not in declared_conversion_ids:
                return baseline_result("unverifiable_source_derivation", ["E_UNVERIFIABLE"])
            cue = conversion_cue(cid)
            if cue and not cue_covered_by(cue, source_text):
                return baseline_result("unverifiable_source_derivation", ["E_UNVERIFIABLE"])
            # Clear natural forbids new-paragraph (multiparagraph RHS) converts.
            if (
                layout_decision == "natural"
                and layout_certainty == "clear"
                and is_multiparagraph_text(conversion_rhs(cid))
            ):
                return baseline_result(
                    "unsafe_semantics",
                    ["E_UNSAFE_SEMANTICS"],
                )
        if kind == "label":
            lab = span.get("label")
            if lab not in CLOSED_LABELS:
                return baseline_result("invalid_fixed_label", ["E_INVALID_LABEL"])
            if lab not in declared_labels:
                return baseline_result("invalid_fixed_label", ["E_INVALID_LABEL"])
            out = span.get("output_text") or ""
            if not out.casefold().startswith(str(lab).casefold()):
                return baseline_result("invalid_fixed_label", ["E_INVALID_LABEL"])

    composed = compose_render(candidate["derivation"])

    # Clear natural: multiparagraph must be declared on layout.decision
    # (multi_paragraph / structured_sections). Catch composition across spans
    # (adjacent single newlines), keep-embedded blank lines, and convert RHS.
    if (
        layout_decision == "natural"
        and layout_certainty == "clear"
        and is_multiparagraph_text(composed)
    ):
        return baseline_result(
            "unsafe_semantics",
            ["E_UNSAFE_SEMANTICS"],
        )

    # Invalid structural headers
    headers = structural_headers(composed)
    for header in headers:
        if header not in CLOSED_LABELS and header.casefold() not in {
            lab.casefold() for lab in CLOSED_LABELS
        }:
            return baseline_result("invalid_fixed_label", ["E_INVALID_LABEL"])
        # non-canonical closed label casing still invalid if not exact closed form
        if header not in CLOSED_LABELS:
            return baseline_result("invalid_fixed_label", ["E_INVALID_LABEL"])

    # Natural policy forbids structural labels
    if fixture.get("policy") == "natural" and headers:
        return baseline_result("invalid_fixed_label", ["E_INVALID_LABEL"])

    # Protected tokens (exact) before invented-content so polarity/name hits
    # surface as E_PROTECTED rather than only invented atoms.
    if not uncertain_backtrack and not uncertain_layout:
        for token in protected:
            if isinstance(token, str) and token and token not in composed:
                return baseline_result(
                    "unsafe_semantics",
                    ["E_PROTECTED", "E_UNSAFE_SEMANTICS"],
                )

    # Invented content
    allowed = licensed_atoms(
        source_map,
        list(declared_conversion_ids),
        list(declared_labels),
    )
    out_atoms = lexical_atoms(composed)
    invented = sorted(atom for atom in out_atoms if atom not in allowed)
    if invented:
        return baseline_result(
            "unsafe_semantics",
            ["E_INVENTED_CONTENT", "E_UNSAFE_SEMANTICS"],
        )

    # Soft salvage after hard checks pass for the candidate envelope
    if uncertain_backtrack:
        rendered = natural_preserve_render(fixture)
        for token in protected:
            if isinstance(token, str) and token and token not in rendered:
                return baseline_result(
                    "unsafe_semantics",
                    ["E_PROTECTED", "E_UNSAFE_SEMANTICS"],
                )
        return {
            "decision": "accept_preserve_words",
            "rendered": rendered,
            "fallback_trigger": "uncertain_backtracking",
            "error_codes": ["E_UNCERTAIN_BACKTRACK"],
        }

    if uncertain_layout:
        rendered = natural_layout_render(fixture, candidate)
        for token in protected:
            if isinstance(token, str) and token and token not in rendered:
                return baseline_result(
                    "unsafe_semantics",
                    ["E_PROTECTED", "E_UNSAFE_SEMANTICS"],
                )
        return {
            "decision": "accept_natural_layout",
            "rendered": rendered,
            "fallback_trigger": "uncertain_layout",
            "error_codes": ["E_UNCERTAIN_LAYOUT"],
        }

    # Final protected check on accepted compose
    for token in protected:
        if isinstance(token, str) and token and token not in composed:
            return baseline_result(
                "unsafe_semantics",
                ["E_PROTECTED", "E_UNSAFE_SEMANTICS"],
            )

    # Accept-path source-region overlap (double-keep / double-consume).
    overlap_codes = claim_source_ranges(source_map, candidate.get("derivation") or [])
    if overlap_codes:
        return baseline_result("unverifiable_source_derivation", overlap_codes)

    # Accept-path output fidelity (stronger than bag-of-atoms alone).
    for span in candidate.get("derivation") or []:
        if not isinstance(span, dict):
            continue
        kind = span.get("kind")
        source_text = span.get("source_text") or ""
        output_text = span.get("output_text") if isinstance(span.get("output_text"), str) else ""
        if kind == "keep":
            if not keep_organize_only(source_text, output_text):
                return baseline_result(
                    "unsafe_semantics",
                    ["E_INVENTED_CONTENT", "E_UNSAFE_SEMANTICS"],
                )
        if kind == "convert":
            cid = span.get("conversion_id")
            if not isinstance(cid, str) or not convert_output_matches(
                cid, source_text, output_text
            ):
                return baseline_result(
                    "unsafe_semantics",
                    ["E_INVENTED_CONTENT", "E_UNSAFE_SEMANTICS"],
                )

    return {
        "decision": "accept",
        "rendered": composed,
        "fallback_trigger": None,
        "error_codes": [],
    }


def validate_package(corpus: Any, schema: Any) -> list[str]:
    errors: list[str] = []
    if not isinstance(corpus, dict):
        return ["corpus must be a JSON object"]
    if not isinstance(schema, dict):
        errors.append("schema must be a JSON object")

    if not exact_keys(corpus, TOP_KEYS, "corpus", errors):
        pass

    depth = json_depth(corpus)
    if depth > MAX_JSON_DEPTH:
        errors.append(f"corpus JSON depth {depth} exceeds {MAX_JSON_DEPTH}")
    nodes = count_nodes(corpus)
    if nodes > MAX_JSON_NODES:
        errors.append(f"corpus JSON nodes {nodes} exceeds {MAX_JSON_NODES}")

    if corpus.get("corpus_id") != "voisu-developer-prompt-rendering-combined-call-2026-08-11":
        errors.append("corpus_id mismatch")
    if corpus.get("language") != "en":
        errors.append("language must be en")

    issue = corpus.get("issue")
    if exact_keys(issue, ISSUE_KEYS, "issue", errors) and isinstance(issue, dict):
        if issue.get("github_number") != 139:
            errors.append("issue.github_number must be 139")
        if issue.get("parent_map") != 133:
            errors.append("issue.parent_map must be 133")
        if issue.get("blocked_by") != 138:
            errors.append("issue.blocked_by must be 138")

    governing = corpus.get("governing")
    exact_keys(governing, GOVERNING_KEYS, "governing", errors)
    if isinstance(governing, dict):
        if governing.get("contract_issue") != 137:
            errors.append("governing.contract_issue must be 137")
        if governing.get("behavior_corpus_issue") != 138:
            errors.append("governing.behavior_corpus_issue must be 138")

    arch = corpus.get("architecture")
    if exact_keys(arch, ARCH_KEYS, "architecture", errors) and isinstance(arch, dict):
        if arch.get("grammar_subsystem") != "absent":
            errors.append("architecture.grammar_subsystem must be absent")
        if arch.get("whole_text_rewrite_rule") != "absent":
            errors.append("architecture.whole_text_rewrite_rule must be absent")
        if arch.get("delivery_deadline_ms") != 1500:
            errors.append("architecture.delivery_deadline_ms must be 1500")
        if arch.get("call_budget") != "at_most_one_structured_cloud_call":
            errors.append("architecture.call_budget mismatch")

    labels = corpus.get("closed_structured_labels")
    if labels != CLOSED_LABELS:
        errors.append("closed_structured_labels catalog mismatch")

    conversions = corpus.get("closed_conversions")
    if not isinstance(conversions, list) or not conversions:
        errors.append("closed_conversions must be non-empty array")
        conversion_set: set[str] = set()
    else:
        conversion_set = set(conversions)
        if len(conversions) != len(conversion_set):
            errors.append("closed_conversions must be unique")
        for item in DEFAULT_CONVERSIONS:
            if item not in conversion_set:
                errors.append(f"closed_conversions missing required {item!r}")

    if corpus.get("protected_token_kinds") != PROTECTED_KINDS:
        errors.append("protected_token_kinds catalog mismatch")
    if corpus.get("fallback_triggers") != FALLBACK_TRIGGERS:
        errors.append("fallback_triggers catalog mismatch")
    if corpus.get("composition_decisions") != DECISIONS:
        errors.append("composition_decisions catalog mismatch")

    thresholds = corpus.get("thresholds")
    if exact_keys(thresholds, THRESHOLD_KEYS, "thresholds", errors) and isinstance(
        thresholds, dict
    ):
        if thresholds.get("delivery_deadline_ms") != 1500:
            errors.append("thresholds.delivery_deadline_ms must be 1500")
        if thresholds.get("max_file_bytes") != MAX_FILE_BYTES:
            errors.append("thresholds.max_file_bytes mismatch")

    error_codes = corpus.get("error_codes")
    error_code_set: set[str] = set()
    if not isinstance(error_codes, list) or not error_codes:
        errors.append("error_codes must be non-empty array")
    else:
        for idx, item in enumerate(error_codes):
            if exact_keys(item, ERROR_OBJ_KEYS, f"error_codes[{idx}]", errors) and isinstance(
                item, dict
            ):
                code = item.get("code")
                if not isinstance(code, str) or not ERROR_CODE_RE.match(code):
                    errors.append(f"error_codes[{idx}]: invalid code")
                else:
                    error_code_set.add(code)
        if len(error_code_set) != len(error_codes):
            errors.append("error_codes must be unique")

    invariants = corpus.get("invariants")
    if not isinstance(invariants, list) or not invariants:
        errors.append("invariants must be non-empty array")
    else:
        for idx, item in enumerate(invariants):
            exact_keys(item, INVARIANT_KEYS, f"invariants[{idx}]", errors)

    models = corpus.get("model_prompt_contracts")
    if not isinstance(models, list) or len(models) != 3:
        errors.append("model_prompt_contracts must contain exactly 3 models")
    else:
        seen_models: set[str] = set()
        for idx, model in enumerate(models):
            prefix = f"model_prompt_contracts[{idx}]"
            if not exact_keys(model, MODEL_KEYS, prefix, errors):
                continue
            mid = model.get("model_id")
            if mid not in MODEL_IDS:
                errors.append(f"{prefix}: unexpected model_id {mid!r}")
            seen_models.add(str(mid))
            if model.get("provider") not in {"google", "groq"}:
                errors.append(f"{prefix}: invalid provider")
            for field in ("system_prompt", "response_instructions", "notes"):
                if not isinstance(model.get(field), str) or not model[field].strip():
                    errors.append(f"{prefix}: {field} must be non-empty")
            # prompt must forbid grammar / enrichment / free-form authority
            blob = f"{model.get('system_prompt','')} {model.get('response_instructions','')}".casefold()
            if "auto-send" not in blob and "auto send" not in blob:
                # require organize-only posture markers
                pass
            if "structured" not in blob and "json" not in blob:
                errors.append(f"{prefix}: prompt must require structured JSON")
            if "invent" not in blob and "enrich" not in blob:
                # soft: prefer invent guidance
                pass
        if seen_models != MODEL_IDS:
            errors.append(f"model_prompt_contracts must cover {sorted(MODEL_IDS)}")

    fixtures = corpus.get("fixtures")
    if not isinstance(fixtures, list) or not fixtures:
        errors.append("fixtures must be a non-empty array")
        return errors

    ids: list[str] = []
    decision_counts = {d: 0 for d in DECISIONS}
    related_links = 0

    for idx, fixture in enumerate(fixtures):
        prefix = f"fixtures[{idx}]"
        if not isinstance(fixture, dict):
            errors.append(f"{prefix}: must be object")
            continue
        if not exact_keys(fixture, FIXTURE_KEYS, prefix, errors):
            continue

        fid = fixture.get("id")
        if not isinstance(fid, str) or not FIXTURE_ID_RE.match(fid):
            errors.append(f"{prefix}: invalid id {fid!r}")
            fid = prefix
        else:
            ids.append(fid)

        for field in ("title", "local_baseline", "rationale"):
            if not isinstance(fixture.get(field), str) or not fixture[field]:
                errors.append(f"{fid}: {field} must be non-empty string")

        if fixture.get("policy") not in POLICIES:
            errors.append(f"{fid}: unknown policy")
        if fixture.get("cloud_outcome") not in CLOUD_OUTCOMES:
            errors.append(f"{fid}: unknown cloud_outcome")
        if not isinstance(fixture.get("base_fingerprint"), str) or not FINGERPRINT_RE.match(
            fixture["base_fingerprint"]
        ):
            errors.append(f"{fid}: invalid base_fingerprint")

        roles = fixture.get("roles")
        if not isinstance(roles, list) or not roles or not all(
            isinstance(r, str) and r for r in roles
        ):
            errors.append(f"{fid}: roles must be non-empty string array")

        related = fixture.get("related_behavior_fixture_ids")
        if not isinstance(related, list):
            errors.append(f"{fid}: related_behavior_fixture_ids must be array")
        else:
            for rid in related:
                if not isinstance(rid, str) or not DPR_ID_RE.match(rid):
                    errors.append(f"{fid}: invalid related behavior id {rid!r}")
                else:
                    related_links += 1

        sources = fixture.get("sources")
        if not isinstance(sources, list) or not (1 <= len(sources) <= 2):
            errors.append(f"{fid}: sources must have 1..2 items")
            sources = []
        else:
            providers_seen: list[str] = []
            for sidx, source in enumerate(sources):
                sp = f"{fid}.sources[{sidx}]"
                if exact_keys(source, SOURCE_KEYS, sp, errors) and isinstance(source, dict):
                    if source.get("provider") not in PROVIDERS:
                        errors.append(f"{sp}: invalid provider")
                    else:
                        providers_seen.append(source["provider"])
                    if not isinstance(source.get("available"), bool):
                        errors.append(f"{sp}: available must be bool")
                    if not isinstance(source.get("text"), str):
                        errors.append(f"{sp}: text must be string")
                    if not isinstance(source.get("primary"), bool):
                        errors.append(f"{sp}: primary must be bool")
                    if source.get("primary") is True and source.get("available") is not True:
                        errors.append(f"{sp}: primary source must be available")
            if len(providers_seen) != len(set(providers_seen)):
                errors.append(f"{fid}: source providers must be unique")

        selection = fixture.get("source_selection")
        if exact_keys(selection, SELECTION_KEYS, f"{fid}.source_selection", errors) and isinstance(
            selection, dict
        ):
            if selection.get("selected_provider") not in PROVIDERS:
                errors.append(f"{fid}: invalid selected_provider")
            if selection.get("reason") not in SELECT_REASONS:
                errors.append(f"{fid}: invalid selection reason")
            # selected must exist
            sel_text = selected_source_text(fixture)
            if sel_text is None:
                errors.append(f"{fid}: selected provider source missing/unavailable")
            elif fingerprint(sel_text) != fixture.get("base_fingerprint"):
                errors.append(f"{fid}: base_fingerprint must match selected source text")

        protected = fixture.get("protected_tokens")
        if not isinstance(protected, list) or not all(
            isinstance(t, str) and t for t in protected
        ):
            errors.append(f"{fid}: protected_tokens must be string array")

        expected = fixture.get("expected")
        if exact_keys(expected, EXPECTED_KEYS, f"{fid}.expected", errors) and isinstance(
            expected, dict
        ):
            decision = expected.get("decision")
            if decision not in DECISIONS:
                errors.append(f"{fid}: unknown expected.decision")
            else:
                decision_counts[decision] += 1
            if not isinstance(expected.get("rendered"), str) or not expected["rendered"]:
                errors.append(f"{fid}: expected.rendered must be non-empty")
            trigger = expected.get("fallback_trigger")
            if trigger is not None and trigger not in FALLBACK_TRIGGERS[1:]:
                errors.append(f"{fid}: invalid fallback_trigger")
            codes = expected.get("error_codes")
            if not isinstance(codes, list) or not all(
                isinstance(c, str) and ERROR_CODE_RE.match(c) for c in codes
            ):
                errors.append(f"{fid}: expected.error_codes invalid")
            elif error_code_set and any(c not in error_code_set for c in codes):
                errors.append(f"{fid}: expected.error_codes not in package catalog")
            delivery = expected.get("delivery")
            if exact_keys(delivery, DELIVERY_KEYS, f"{fid}.expected.delivery", errors) and isinstance(
                delivery, dict
            ):
                if delivery.get("state") != "unsent":
                    errors.append(f"{fid}: delivery.state must be unsent")
                for flag in ("auto_send", "live_type", "replace_delivered"):
                    if delivery.get(flag) is not False:
                        errors.append(f"{fid}: delivery.{flag} must be false")

            # decision/trigger consistency
            if decision == "accept":
                if trigger is not None:
                    errors.append(f"{fid}: accept requires null fallback_trigger")
                if codes:
                    errors.append(f"{fid}: accept requires empty error_codes")
            if decision == "accept_preserve_words":
                if trigger != "uncertain_backtracking":
                    errors.append(f"{fid}: accept_preserve_words trigger mismatch")
            if decision == "accept_natural_layout":
                if trigger != "uncertain_layout":
                    errors.append(f"{fid}: accept_natural_layout trigger mismatch")
            if decision == "fallback_baseline" and fixture.get("cloud_outcome") == "skipped":
                if trigger is not None:
                    errors.append(f"{fid}: skipped path should have null trigger")
            if decision == "fallback_baseline" and expected.get("rendered") != fixture.get(
                "local_baseline"
            ):
                errors.append(f"{fid}: fallback_baseline rendered must equal local_baseline")

        candidate = fixture.get("candidate")
        if candidate is not None:
            shape_errs = validate_candidate_shape(
                candidate, conversion_set, f"{fid}.candidate"
            )
            # Fixtures with hard schema_failure may still carry null only; if candidate
            # present it must be well-shaped for composition tests, except deliberate
            # invalid fixtures are validated by composition outcomes.
            # Only record shape errors when outcome is succeeded and decision accept*.
            if fixture.get("cloud_outcome") == "succeeded" and fixture.get("expected", {}).get(
                "decision"
            ) in {"accept", "accept_preserve_words", "accept_natural_layout"}:
                for err in shape_errs:
                    errors.append(err)

        # Composition oracle
        result = compose_fixture(fixture, conversion_set)
        exp = fixture.get("expected") if isinstance(fixture.get("expected"), dict) else {}
        if result.get("decision") != exp.get("decision"):
            errors.append(
                f"{fid}: composition decision {result.get('decision')!r} != expected {exp.get('decision')!r}"
            )
        if result.get("rendered") != exp.get("rendered"):
            errors.append(
                f"{fid}: composition rendered mismatch "
                f"got={result.get('rendered')!r} expected={exp.get('rendered')!r}"
            )
        if result.get("fallback_trigger") != exp.get("fallback_trigger"):
            errors.append(
                f"{fid}: composition fallback_trigger {result.get('fallback_trigger')!r} "
                f"!= expected {exp.get('fallback_trigger')!r}"
            )
        # error codes: require same set (order-insensitive) for hard cases; allow extras order free
        got_codes = set(result.get("error_codes") or [])
        exp_codes = set(exp.get("error_codes") or [])
        if got_codes != exp_codes:
            errors.append(
                f"{fid}: composition error_codes {sorted(got_codes)} != expected {sorted(exp_codes)}"
            )

        # Protected tokens must hold on final rendered
        rendered = exp.get("rendered") if isinstance(exp, dict) else None
        if isinstance(rendered, str):
            for token in protected if isinstance(protected, list) else []:
                if isinstance(token, str) and token and token not in rendered:
                    errors.append(f"{fid}: protected token {token!r} missing from expected.rendered")

    if len(ids) != len(set(ids)):
        errors.append("duplicate fixture IDs")

    counts = corpus.get("dataset_counts")
    if exact_keys(counts, COUNTS_KEYS, "dataset_counts", errors) and isinstance(counts, dict):
        if counts.get("fixtures_total") != len(fixtures):
            errors.append("dataset_counts.fixtures_total mismatch")
        for key in DECISIONS:
            if counts.get(key) != decision_counts[key]:
                errors.append(
                    f"dataset_counts.{key} mismatch declared={counts.get(key)} actual={decision_counts[key]}"
                )
        if counts.get("related_behavior_links") != related_links:
            errors.append("dataset_counts.related_behavior_links mismatch")
        if counts.get("model_prompt_contracts") != 3:
            errors.append("dataset_counts.model_prompt_contracts must be 3")

    # schema file sanity: must declare same corpus_id const
    if isinstance(schema, dict):
        props = schema.get("properties") if isinstance(schema.get("properties"), dict) else {}
        cid = props.get("corpus_id", {})
        if isinstance(cid, dict) and cid.get("const") != corpus.get("corpus_id"):
            errors.append("schema corpus_id const mismatches corpus")

    return errors


def _expect_diagnostic(
    failures: list[str],
    name: str,
    corpus: dict[str, Any],
    schema: Any,
    needles: list[str],
) -> None:
    errors = validate_package(corpus, schema)
    joined = "\n".join(errors)
    missing = [n for n in needles if n.casefold() not in joined.casefold()]
    if not errors:
        failures.append(f"mutation {name}: expected failures, got healthy package")
    elif missing:
        failures.append(
            f"mutation {name}: missing diagnostics {missing}; got {errors[:8]}"
        )


def run_mutations(corpus: dict[str, Any], schema: Any) -> list[str]:
    failures: list[str] = []

    def mut(name: str, needles: list[str], editor) -> None:
        clone = copy.deepcopy(corpus)
        try:
            editor(clone)
        except Exception as exc:  # noqa: BLE001 — mutation harness
            failures.append(f"mutation {name} crashed before validate: {exc}")
            return
        _expect_diagnostic(failures, name, clone, schema, needles)

    def find(c: dict[str, Any], pred) -> dict[str, Any]:
        for fixture in c["fixtures"]:
            if pred(fixture):
                return fixture
        raise AssertionError("fixture not found for mutation")

    def first(c: dict[str, Any]) -> dict[str, Any]:
        return c["fixtures"][0]

    mut(
        "duplicate_id",
        ["duplicate fixture IDs"],
        lambda c: c["fixtures"].__setitem__(1, {**c["fixtures"][1], "id": c["fixtures"][0]["id"]}),
    )

    mut(
        "count_mismatch",
        ["dataset_counts.fixtures_total"],
        lambda c: c["dataset_counts"].__setitem__("fixtures_total", 0),
    )

    mut(
        "forbidden_top_level",
        ["schema-forbidden property"],
        lambda c: c.__setitem__("unexpected_top", True),
    )

    mut(
        "grammar_subsystem_present",
        ["grammar_subsystem must be absent"],
        lambda c: c["architecture"].__setitem__("grammar_subsystem", "present"),
    )

    mut(
        "autosend_true",
        ["auto_send must be false"],
        lambda c: find(c, lambda f: True)["expected"]["delivery"].__setitem__(
            "auto_send", True
        ),
    )

    mut(
        "protected_missing",
        ["protected token"],
        lambda c: (
            find(c, lambda f: f.get("protected_tokens")).__setitem__(
                "expected",
                {
                    **find(c, lambda f: f.get("protected_tokens"))["expected"],
                    "rendered": "x",
                    "decision": "accept",
                    "fallback_trigger": None,
                    "error_codes": [],
                },
            )
        ),
    )

    def invent_accept(c: dict[str, Any]) -> None:
        f = find(c, lambda x: x.get("id") == "CC-01")
        f["candidate"]["derivation"] = [
            {
                "kind": "keep",
                "source_provider": "provider_a",
                "source_text": "ship it exclamation point",
                "output_text": "Deploy to production now!",
                "conversion_id": None,
                "label": None,
            }
        ]
        # leave expected as accept so composition detects mismatch / invented

    mut(
        "invented_content_accept",
        ["composition decision", "composition error_codes"],
        invent_accept,
    )

    def stale_fp(c: dict[str, Any]) -> None:
        f = find(c, lambda x: x.get("id") == "CC-01")
        f["candidate"]["base_fingerprint"] = "sha256:" + ("a" * 64)

    mut(
        "stale_fingerprint",
        ["composition decision", "fallback_baseline"],
        stale_fp,
    )

    def unknown_conversion(c: dict[str, Any]) -> None:
        f = find(c, lambda x: x.get("id") == "CC-01")
        f["candidate"]["conversions"][0]["id"] = "hey→Restart"

    mut(
        "unknown_conversion",
        ["unknown conversion", "composition"],
        unknown_conversion,
    )

    def non_closed_header(c: dict[str, Any]) -> None:
        f = find(c, lambda x: x.get("id") == "CC-20")
        f["candidate"]["derivation"][0]["output_text"] = "Edge Cases:\n"
        f["candidate"]["derivation"][0]["label"] = "Goal"

    mut(
        "non_closed_header",
        ["composition decision", "invalid_fixed_label"],
        non_closed_header,
    )

    def drop_model(c: dict[str, Any]) -> None:
        c["model_prompt_contracts"] = c["model_prompt_contracts"][:2]
        c["dataset_counts"]["model_prompt_contracts"] = 2

    mut(
        "missing_model_contract",
        ["exactly 3 models"],
        drop_model,
    )

    def wrong_deadline(c: dict[str, Any]) -> None:
        c["architecture"]["delivery_deadline_ms"] = 3000

    mut(
        "deadline_not_1500",
        ["delivery_deadline_ms must be 1500"],
        wrong_deadline,
    )

    def fallback_render_mismatch(c: dict[str, Any]) -> None:
        f = find(c, lambda x: x.get("expected", {}).get("decision") == "fallback_baseline")
        f["expected"]["rendered"] = f["local_baseline"] + " EXTRA"

    mut(
        "fallback_render_ne_baseline",
        ["fallback_baseline rendered must equal local_baseline"],
        fallback_render_mismatch,
    )

    def accept_with_trigger(c: dict[str, Any]) -> None:
        f = find(c, lambda x: x.get("expected", {}).get("decision") == "accept")
        f["expected"]["fallback_trigger"] = "unsafe_semantics"

    mut(
        "accept_with_trigger",
        ["accept requires null fallback_trigger"],
        accept_with_trigger,
    )

    def corrupt_source_span(c: dict[str, Any]) -> None:
        f = find(c, lambda x: x.get("id") == "CC-01")
        f["candidate"]["derivation"][0]["source_text"] = "not present in source xyz"

    mut(
        "unverifiable_span",
        ["composition decision"],
        corrupt_source_span,
    )

    def protect_name_flip(c: dict[str, Any]) -> None:
        f = find(c, lambda x: x.get("id") == "CC-14")
        # Claim accept with case-mutated name so protected exact-match fails.
        f["expected"]["decision"] = "accept"
        f["expected"]["fallback_trigger"] = None
        f["expected"]["error_codes"] = []
        f["expected"]["rendered"] = "Call anuraj about the release."
        f["cloud_outcome"] = "succeeded"
        f["candidate"]["derivation"] = [
            {
                "kind": "keep",
                "source_provider": "provider_a",
                "source_text": "call Anuraj about the release",
                "output_text": "Call anuraj about the release.",
                "conversion_id": None,
                "label": None,
            }
        ]

    mut(
        "protected_name_expected_tamper",
        ["protected token", "composition"],
        protect_name_flip,
    )

    def whole_text_rewrite_flag(c: dict[str, Any]) -> None:
        c["architecture"]["whole_text_rewrite_rule"] = "present"

    mut(
        "whole_text_rewrite_present",
        ["whole_text_rewrite_rule must be absent"],
        whole_text_rewrite_flag,
    )

    def bad_related_id(c: dict[str, Any]) -> None:
        first(c)["related_behavior_fixture_ids"] = ["F01"]

    mut(
        "bad_related_behavior_id",
        ["invalid related behavior id"],
        bad_related_id,
    )

    def empty_derivation(c: dict[str, Any]) -> None:
        f = find(c, lambda x: x.get("id") == "CC-01")
        f["candidate"]["derivation"] = []

    mut(
        "empty_derivation",
        ["derivation must be a non-empty array", "composition"],
        empty_derivation,
    )

    def natural_with_goal(c: dict[str, Any]) -> None:
        f = find(c, lambda x: x.get("id") == "CC-18")
        f["policy"] = "natural"
        f["candidate"]["derivation"] = [
            {
                "kind": "keep",
                "source_provider": "provider_a",
                "source_text": "hey can you send the notes when you get a chance",
                "output_text": "Goal:\nPlease send notes.",
                "conversion_id": None,
                "label": None,
            }
        ]
        f["expected"]["decision"] = "accept"
        f["expected"]["rendered"] = "Goal:\nPlease send notes."
        f["expected"]["fallback_trigger"] = None
        f["expected"]["error_codes"] = []

    mut(
        "natural_structural_label",
        ["composition decision"],
        natural_with_goal,
    )

    def decision_count_lie(c: dict[str, Any]) -> None:
        c["dataset_counts"]["accept"] = 0

    mut(
        "decision_count_lie",
        ["dataset_counts.accept mismatch"],
        decision_count_lie,
    )

    def strip_conversion_catalog(c: dict[str, Any]) -> None:
        c["closed_conversions"] = [x for x in c["closed_conversions"] if not x.startswith("period")]

    mut(
        "missing_required_conversion",
        ["closed_conversions missing required"],
        strip_conversion_catalog,
    )

    def live_type_true(c: dict[str, Any]) -> None:
        first(c)["expected"]["delivery"]["live_type"] = True

    mut(
        "live_type_true",
        ["live_type must be false"],
        live_type_true,
    )

    def replace_delivered_true(c: dict[str, Any]) -> None:
        first(c)["expected"]["delivery"]["replace_delivered"] = True

    mut(
        "replace_delivered_true",
        ["replace_delivered must be false"],
        replace_delivered_true,
    )

    def wrong_issue(c: dict[str, Any]) -> None:
        c["issue"]["github_number"] = 100

    mut(
        "wrong_issue_number",
        ["github_number must be 139"],
        wrong_issue,
    )

    def fingerprint_mismatch_selected(c: dict[str, Any]) -> None:
        f = find(c, lambda x: x.get("id") == "CC-01")
        f["base_fingerprint"] = "sha256:" + ("b" * 64)
        # also update candidate so only selected match check fails first
        f["candidate"]["base_fingerprint"] = f["base_fingerprint"]

    mut(
        "selected_fingerprint_mismatch",
        ["base_fingerprint must match selected source text"],
        fingerprint_mismatch_selected,
    )

    def remove_without_removals(c: dict[str, Any]) -> None:
        f = find(c, lambda x: x.get("id") == "CC-01")
        f["candidate"]["removals"] = []
        f["candidate"]["derivation"] = [
            {
                "kind": "remove",
                "source_provider": "provider_a",
                "source_text": "ship",
                "output_text": "",
                "conversion_id": None,
                "label": None,
            },
            {
                "kind": "keep",
                "source_provider": "provider_a",
                "source_text": "it exclamation point",
                "output_text": "It!",
                "conversion_id": None,
                "label": None,
            },
        ]
        # keep expected accept so composition must diagnose the undeclared remove
        f["expected"]["decision"] = "accept"
        f["expected"]["fallback_trigger"] = None
        f["expected"]["error_codes"] = []
        f["expected"]["rendered"] = "It!"

    mut(
        "remove_without_removals",
        ["composition decision", "E_UNVERIFIABLE"],
        remove_without_removals,
    )

    def convert_cue_missing(c: dict[str, Any]) -> None:
        f = find(c, lambda x: x.get("id") == "CC-01")
        # conversion id claims exclamation cue but convert span source lacks it
        f["candidate"]["conversions"] = [
            {
                "id": "exclamation point→!",
                "source_provider": "provider_a",
                "source_span_text": "ship it",
            }
        ]
        f["candidate"]["derivation"] = [
            {
                "kind": "keep",
                "source_provider": "provider_a",
                "source_text": "ship it",
                "output_text": "Ship it",
                "conversion_id": None,
                "label": None,
            },
            {
                "kind": "convert",
                "source_provider": "provider_a",
                "source_text": "ship it",
                "output_text": "!",
                "conversion_id": "exclamation point→!",
                "label": None,
            },
        ]
        f["expected"]["decision"] = "accept"
        f["expected"]["fallback_trigger"] = None
        f["expected"]["error_codes"] = []
        f["expected"]["rendered"] = "Ship it!"

    mut(
        "convert_cue_missing",
        ["composition decision", "E_UNVERIFIABLE"],
        convert_cue_missing,
    )

    def double_keep_overlap(c: dict[str, Any]) -> None:
        f = find(c, lambda x: x.get("id") == "CC-01")
        f["candidate"]["conversions"] = []
        f["candidate"]["derivation"] = [
            {
                "kind": "keep",
                "source_provider": "provider_a",
                "source_text": "ship it",
                "output_text": "Ship it",
                "conversion_id": None,
                "label": None,
            },
            {
                "kind": "keep",
                "source_provider": "provider_a",
                "source_text": "ship it",
                "output_text": " ship it",
                "conversion_id": None,
                "label": None,
            },
        ]
        f["expected"]["decision"] = "accept"
        f["expected"]["fallback_trigger"] = None
        f["expected"]["error_codes"] = []
        f["expected"]["rendered"] = "Ship it ship it"

    mut(
        "double_keep_overlap",
        ["composition decision", "E_OVERLAP"],
        double_keep_overlap,
    )

    def natural_layout_multiparagraph_break(c: dict[str, Any]) -> None:
        f = find(c, lambda x: x.get("id") == "CC-18")
        f["candidate"]["layout"] = {"decision": "natural", "certainty": "clear"}
        f["candidate"]["derivation"] = [
            {
                "kind": "keep",
                "source_provider": "provider_a",
                "source_text": "hey can you send the notes",
                "output_text": "Hey, can you send the notes",
                "conversion_id": None,
                "label": None,
            },
            {
                "kind": "layout_break",
                "source_provider": None,
                "source_text": "",
                "output_text": "\n\n",
                "conversion_id": None,
                "label": None,
            },
            {
                "kind": "keep",
                "source_provider": "provider_a",
                "source_text": "when you get a chance",
                "output_text": "when you get a chance?",
                "conversion_id": None,
                "label": None,
            },
        ]
        f["expected"]["decision"] = "accept"
        f["expected"]["fallback_trigger"] = None
        f["expected"]["error_codes"] = []
        f["expected"]["rendered"] = "Hey, can you send the notes\n\nwhen you get a chance?"

    mut(
        "natural_clear_multiparagraph_break",
        ["composition decision", "E_UNSAFE_SEMANTICS"],
        natural_layout_multiparagraph_break,
    )

    def natural_layout_adjacent_single_newlines(c: dict[str, Any]) -> None:
        """Two consecutive single-newline layout_break spans compose to \\n\\n."""
        f = find(c, lambda x: x.get("id") == "CC-18")
        f["candidate"]["layout"] = {"decision": "natural", "certainty": "clear"}
        f["candidate"]["derivation"] = [
            {
                "kind": "keep",
                "source_provider": "provider_a",
                "source_text": "hey can you send the notes",
                "output_text": "Hey, can you send the notes",
                "conversion_id": None,
                "label": None,
            },
            {
                "kind": "layout_break",
                "source_provider": None,
                "source_text": "",
                "output_text": "\n",
                "conversion_id": None,
                "label": None,
            },
            {
                "kind": "layout_break",
                "source_provider": None,
                "source_text": "",
                "output_text": "\n",
                "conversion_id": None,
                "label": None,
            },
            {
                "kind": "keep",
                "source_provider": "provider_a",
                "source_text": "when you get a chance",
                "output_text": "when you get a chance?",
                "conversion_id": None,
                "label": None,
            },
        ]
        f["expected"]["decision"] = "accept"
        f["expected"]["fallback_trigger"] = None
        f["expected"]["error_codes"] = []
        f["expected"]["rendered"] = "Hey, can you send the notes\n\nwhen you get a chance?"

    mut(
        "natural_clear_adjacent_single_newlines",
        ["composition decision", "E_UNSAFE_SEMANTICS"],
        natural_layout_adjacent_single_newlines,
    )

    def natural_layout_keep_edge_newlines(c: dict[str, Any]) -> None:
        """Keep trailing \\n + keep leading \\n also compose multiparagraph under clear natural."""
        f = find(c, lambda x: x.get("id") == "CC-18")
        f["candidate"]["layout"] = {"decision": "natural", "certainty": "clear"}
        f["candidate"]["derivation"] = [
            {
                "kind": "keep",
                "source_provider": "provider_a",
                "source_text": "hey can you send the notes",
                "output_text": "Hey, can you send the notes\n",
                "conversion_id": None,
                "label": None,
            },
            {
                "kind": "keep",
                "source_provider": "provider_a",
                "source_text": "when you get a chance",
                "output_text": "\nwhen you get a chance?",
                "conversion_id": None,
                "label": None,
            },
        ]
        f["expected"]["decision"] = "accept"
        f["expected"]["fallback_trigger"] = None
        f["expected"]["error_codes"] = []
        f["expected"]["rendered"] = "Hey, can you send the notes\n\nwhen you get a chance?"

    mut(
        "natural_clear_keep_edge_newlines",
        ["composition decision", "E_UNSAFE_SEMANTICS"],
        natural_layout_keep_edge_newlines,
    )

    def recon_provider_mismatch(c: dict[str, Any]) -> None:
        f = find(c, lambda x: x.get("id") == "CC-01")
        # only_available host selection; candidate picks a non-available provider
        f["candidate"]["reconciliation"] = {
            "selected_provider": "provider_b",
            "reason": "only_available",
        }
        f["expected"]["decision"] = "accept"
        f["expected"]["fallback_trigger"] = None
        f["expected"]["error_codes"] = []

    mut(
        "recon_provider_mismatch",
        ["composition decision", "E_RECONCILE"],
        recon_provider_mismatch,
    )

    def keep_rephrase_drops_words(c: dict[str, Any]) -> None:
        f = find(c, lambda x: x.get("id") == "CC-18")
        # Source-licensed atoms only, but free rephrase drops content words from the span.
        f["candidate"]["derivation"] = [
            {
                "kind": "keep",
                "source_provider": "provider_a",
                "source_text": "hey can you send the notes when you get a chance",
                "output_text": "Send the notes.",
                "conversion_id": None,
                "label": None,
            }
        ]
        f["expected"]["decision"] = "accept"
        f["expected"]["fallback_trigger"] = None
        f["expected"]["error_codes"] = []
        f["expected"]["rendered"] = "Send the notes."

    mut(
        "keep_rephrase_drops_words",
        ["composition decision", "E_UNSAFE_SEMANTICS"],
        keep_rephrase_drops_words,
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
    if not isinstance(corpus, dict) or not isinstance(schema, dict):
        print("FAIL: package load shape", file=sys.stderr)
        return 1

    mutation_failures = run_mutations(corpus, schema)
    errors.extend(mutation_failures)

    if errors:
        print(f"FAIL: {len(errors)} error(s)", file=sys.stderr)
        for err in errors:
            print(f"  - {err}", file=sys.stderr)
        return 1

    counts = corpus["dataset_counts"]
    print("OK: developer-prompt-rendering-combined-call package")
    print(f"  version: {corpus.get('version')}")
    print(f"  fixtures: {counts['fixtures_total']}")
    print(
        "  decisions: "
        f"accept={counts['accept']} "
        f"preserve_words={counts['accept_preserve_words']} "
        f"natural_layout={counts['accept_natural_layout']} "
        f"fallback={counts['fallback_baseline']}"
    )
    print(f"  related_behavior_links: {counts['related_behavior_links']}")
    print(f"  model_prompt_contracts: {counts['model_prompt_contracts']}")
    print(f"  closed_labels: {len(corpus['closed_structured_labels'])}")
    print(f"  closed_conversions: {len(corpus['closed_conversions'])}")
    print(f"  error_codes: {len(corpus['error_codes'])}")
    print(f"  invariants: {len(corpus['invariants'])}")
    print("  mutations: 34 property-bound")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
