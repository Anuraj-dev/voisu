#!/usr/bin/env python3
"""Invariant checker for the Developer Prompt Rendering behavior corpus (#138).

Standard-library only. Enforces the owned package shape published beside this
file (exact key sets and catalogs). Not a generic JSON Schema Draft 2020-12
engine, not the future runtime renderer, and not the #139 cloud validator.
"""

from __future__ import annotations

import copy
import json
import re
import sys
from pathlib import Path
from typing import Any

HERE = Path(__file__).resolve().parent
CORPUS_PATH = HERE / "developer-prompt-rendering-behavior-corpus-2026-08-11.json"
SCHEMA_PATH = HERE / "developer-prompt-rendering-behavior-schema-2026-08-11.json"

FIXTURE_ID_RE = re.compile(r"^DPR-[0-9]{2}[a-z]?$")

# Exact owned key sets (mirrors schema required/properties for owned objects).
TOP_REQUIRED = [
    "corpus_id",
    "version",
    "issue",
    "language",
    "governing",
    "closed_structured_labels",
    "kinds",
    "policies",
    "routes",
    "cloud_request_states",
    "cloud_outcomes",
    "provider_states",
    "fallback_triggers",
    "closed_tags",
    "coverage_requirements",
    "coverage_matrix",
    "fixtures",
    "dataset_counts",
    "invariants",
]
TOP_KEYS = set(TOP_REQUIRED)

ISSUE_KEYS = {"github_number", "title", "parent_map", "blocks", "url"}
GOVERNING_KEYS = {
    "contract_issue",
    "map_issue",
    "resolution_issues",
    "superseded_sources",
    "out_of_scope",
}
COUNTS_KEYS = {
    "fixtures_total",
    "by_kind",
    "by_policy",
    "by_route",
    "by_cloud_request",
    "fallback_fixtures",
    "coverage_requirement_count",
    "closed_tag_count",
}
FIXTURE_KEYS = {
    "id",
    "title",
    "kind",
    "tags",
    "policy",
    "surface_hint",
    "sources",
    "provider_state",
    "source_selection",
    "timing",
    "route",
    "cloud",
    "local_baseline",
    "expected_final",
    "allowed_operations",
    "label_evidence",
    "protected_tokens",
    "forbidden_outcomes",
    "fallback",
    "delivery",
    "rationale",
}
SOURCE_KEYS = {"provider", "available", "text", "primary"}
CLOUD_REQUIRED_KEYS = {
    "request_policy",
    "actual_requests",
    "outcome",
    "justification",
    "deadline_evidence",
}
CLOUD_OPTIONAL_KEYS = {"rejected_candidate_excerpt", "rejected_labels"}
CLOUD_KEYS = CLOUD_REQUIRED_KEYS | CLOUD_OPTIONAL_KEYS
DEADLINE_EVIDENCE_KEYS = {
    "deadline_ms",
    "observed_elapsed_ms",
    "local_baseline_delivery_start_by_ms",
}
TIMING_KEYS = {"certainty", "boundaries"}
BOUNDARY_KEYS = {"source_provider", "left_phrase", "right_phrase", "pause_ms"}
OPS_KEYS = {"removals", "conversions", "layout", "labels"}
LABEL_EV_KEYS = {"label", "source_provider", "source_span_text"}
SELECTION_KEYS = {"selected_provider", "reason"}
DELIVERY_KEYS = {"state", "auto_send", "live_type", "replace_delivered"}
FALLBACK_OBJ_KEYS = {"trigger", "result"}
INVARIANT_KEYS = {"id", "summary"}

KINDS = ["everyday_message", "developer_prompt"]
POLICIES = ["natural", "adaptive", "structured"]
ROUTES = ["literal_identity", "deterministic_local", "local_with_optional_cloud"]
CLOUD_POLICIES = ["not_allowed", "allowed", "required"]
CLOUD_OUTCOMES = [
    "not_attempted",
    "skipped",
    "succeeded",
    "rejected_unsafe",
    "rejected_unverifiable",
    "rejected_invalid_label",
    "schema_failure",
    "provider_failure",
    "deadline_exceeded",
]
PROVIDER_STATES = [
    "exact_agreement",
    "punctuation_only_agreement",
    "safe_complementary",
    "protected_token_disagreement",
    "semantic_disagreement",
    "single_provider",
]
FALLBACK_TRIGGERS = [
    "unsafe_semantics",
    "unverifiable_source_derivation",
    "invalid_fixed_label",
    "uncertain_backtracking",
    "uncertain_layout",
    "response_schema_failure",
    "provider_failure",
    "deadline_exceeded",
]
SOURCE_SELECT_REASONS = [
    "only_available",
    "exact_agreement",
    "configured_primary_rank",
    "punctuation_local_render",
    "safe_complementary_merge",
]
SURFACE_HINTS = [None, "coding_agent", "messaging", "shell", "neutral"]
PROVIDERS = ["provider_a", "provider_b"]
TIMING_CERTAINTIES = ["clear", "uncertain"]
DELIVERY_STATES = ["unsent"]
SAFE_CONVERSION_CUES = {
    "exclamation point→!": ("exclamation point",),
    "four→4.": ("four",),
    "new line→\\n": ("new line",),
    "new paragraph→\\n\\n": ("new paragraph",),
    "one→1.": ("one",),
    "period→.": ("period",),
    'quote…unquote→"…"': ("quote", "unquote"),
    "spoken acceptance criteria cue→Acceptance Criteria label": ("acceptance criteria",),
    "spoken constraints cue→Constraints label": ("constraints",),
    "spoken context cue→Context label": ("context",),
    "spoken files cue→Files label": ("files",),
    "spoken goal cue→Goal label": ("goal",),
    "spoken notes cue→Notes label": ("notes",),
    "spoken requirements cue→Requirements label": ("requirements",),
    "spoken steps cue→numbered_lines": ("steps",),
    "three→3.": ("three",),
    "two→2.": ("two",),
}
SAFE_CONVERSIONS = list(SAFE_CONVERSION_CUES)
SYMBOL_CONVERSIONS = {
    "exclamation point→!",
    "new line→\\n",
    "new paragraph→\\n\\n",
    "period→.",
    'quote…unquote→"…"',
}

POLICY_TAGS = {
    "natural": "policy-natural",
    "adaptive": "policy-adaptive",
    "structured": "policy-structured",
}
KIND_TAGS = {
    "everyday_message": ("kind-everyday", "everyday-message"),
    "developer_prompt": ("kind-developer", "developer-prompt"),
}
ROUTE_TAGS = {
    "literal_identity": "route-literal",
    "deterministic_local": "route-local",
    "local_with_optional_cloud": "route-cloud-optional",
}
PROVIDER_STATE_TAGS = {
    "exact_agreement": "dual-stt-exact",
    "punctuation_only_agreement": "dual-stt-punct",
    "safe_complementary": "dual-stt-complementary",
    "protected_token_disagreement": "dual-stt-protected-disagreement",
    "semantic_disagreement": "dual-stt-semantic-disagreement",
    "single_provider": "dual-stt-single-provider",
}
FALLBACK_TAGS = {
    "unsafe_semantics": "fallback-unsafe-semantics",
    "unverifiable_source_derivation": "fallback-unverifiable",
    "invalid_fixed_label": "fallback-invalid-label",
    "uncertain_backtracking": "fallback-uncertain-backtrack",
    "uncertain_layout": "fallback-uncertain-layout",
    "response_schema_failure": "fallback-schema",
    "provider_failure": "fallback-provider",
    "deadline_exceeded": "fallback-deadline",
}
LABEL_TAGS = {
    "Goal": "label-goal",
    "Context": "label-context",
    "Requirements": "label-requirements",
    "Constraints": "label-constraints",
    "Steps": "label-steps",
    "Acceptance Criteria": "label-acceptance-criteria",
    "Files": "label-files",
    "Notes": "label-notes",
}
LABEL_SOURCE_CUES = {label: label.casefold() for label in LABEL_TAGS}

FAIL_OR_SKIP = (
    "not_attempted",
    "skipped",
    "rejected_unsafe",
    "rejected_unverifiable",
    "rejected_invalid_label",
    "schema_failure",
    "provider_failure",
    "deadline_exceeded",
)
ATTEMPTED_FAILURE_FALLBACK = {
    "rejected_unsafe": "unsafe_semantics",
    "rejected_unverifiable": "unverifiable_source_derivation",
    "rejected_invalid_label": "invalid_fixed_label",
    "schema_failure": "response_schema_failure",
    "provider_failure": "provider_failure",
    "deadline_exceeded": "deadline_exceeded",
}


class CheckError(Exception):
    pass


def load_json(path: Path) -> Any:
    try:
        text = path.read_text(encoding="utf-8")
    except OSError as exc:
        raise CheckError(f"cannot read {path.name}: {exc}") from exc
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


def required_allowed_keys(
    obj: Any,
    required: set[str],
    allowed: set[str],
    where: str,
    errors: list[str],
) -> bool:
    """Require a core shape while permitting only explicitly optional keys."""
    if not isinstance(obj, dict):
        errors.append(f"{where}: must be an object")
        return False
    keys = set(obj)
    missing = required - keys
    extra = keys - allowed
    if missing:
        errors.append(f"{where}: missing keys {sorted(missing)}")
    if extra:
        errors.append(f"{where}: schema-forbidden property/keys {sorted(extra)}")
    return not missing and not extra


def is_str_list(val: Any, allow_empty_string_items: bool = False) -> bool:
    if not isinstance(val, list):
        return False
    for item in val:
        if not isinstance(item, str):
            return False
        if not allow_empty_string_items and item == "":
            return False
    return True


def structural_headers(text: str, labels: list[str]) -> list[tuple[str, str | None]]:
    """Return label-like line-start headers and their canonical closed label, if any."""
    if not isinstance(text, str):
        return []
    canonical = {label.casefold(): label for label in labels if isinstance(label, str)}
    found: list[tuple[str, str | None]] = []
    for line in text.splitlines():
        match = re.match(r"^[ \t]*([A-Za-z][A-Za-z0-9 ]*):(.*)$", line)
        if not match:
            continue
        raw = match.group(1).strip()
        body = match.group(2)
        closed = canonical.get(raw.casefold())
        # Closed labels are structural regardless of case or inline/block body. Unknown
        # headers are structural when block-shaped or conventionally title-cased.
        if closed is not None or not body.strip() or raw.istitle():
            found.append((raw, closed))
    return found


def closed_label_prefixes(text: str, labels: list[str]) -> list[str]:
    """Detect closed labels case-insensitively at line starts."""
    return [canonical for _, canonical in structural_headers(text, labels) if canonical]


def evidence_tokens(text: str) -> set[str]:
    """Tokenize source/final text for conservative complementary-evidence checks."""
    return {
        token.strip(".,!?;:").casefold()
        for token in re.findall(r"https?://\S+|[A-Za-z0-9_./=\-]+", text)
        if token.strip(".,!?;:")
    }


def lexical_atoms(text: str) -> set[str]:
    """Return case-insensitive ordinary lexical atoms, ignoring punctuation/layout."""
    if not isinstance(text, str):
        return set()
    return {
        atom.casefold()
        for atom in re.findall(r"[A-Za-z0-9]+(?:[_./=+\-][A-Za-z0-9]+)*", text)
    }


def ordered_lexical_tokens(text: Any) -> list[str]:
    """Return ordinary lexical tokens in source order, preserving duplicates."""
    if not isinstance(text, str):
        return []
    return [
        token.casefold()
        for token in re.findall(r"[A-Za-z0-9]+(?:[_./=+\-][A-Za-z0-9]+)*", text)
    ]


def ordinary_output_tokens(
    text: Any, labels: list[str], conversions: list[Any]
) -> list[str]:
    """Remove licensed structural outputs, leaving wording whose order must survive."""
    if not isinstance(text, str):
        return []
    ordinary = text
    for label in sorted((x for x in labels if isinstance(x, str)), key=len, reverse=True):
        ordinary = re.sub(
            rf"(?im)^[ \t]*{re.escape(label)}[ \t]*:", "", ordinary
        )
    if any(
        conversion in {
            "one→1.",
            "two→2.",
            "three→3.",
            "four→4.",
            "spoken steps cue→numbered_lines",
        }
        for conversion in conversions
    ):
        ordinary = re.sub(r"(?m)^[ \t]*[0-9]+\.[ \t]*", "", ordinary)
    return ordered_lexical_tokens(ordinary)


def is_subsequence(needles: list[str], haystack: list[str]) -> bool:
    """Return whether needles occur in order within haystack."""
    cursor = iter(haystack)
    return all(any(candidate == needle for candidate in cursor) for needle in needles)


def labeled_sections(text: str, labels: list[str]) -> dict[str, str]:
    """Return the body of each canonical closed-label section in final text."""
    canonical = {label.casefold(): label for label in labels if isinstance(label, str)}
    sections: dict[str, list[str]] = {}
    current: str | None = None
    for line in text.splitlines():
        match = re.match(r"^[ \t]*([A-Za-z][A-Za-z0-9 ]*):(.*)$", line)
        label = canonical.get(match.group(1).strip().casefold()) if match else None
        if label is not None:
            current = label
            sections.setdefault(label, [])
            inline_body = match.group(2).strip()
            if inline_body:
                sections[label].append(inline_body)
        elif current is not None:
            sections[current].append(line)
    return {label: "\n".join(lines).strip() for label, lines in sections.items()}


def cue_bearing_spans(source_text: str, labels: list[str]) -> dict[str, list[str]]:
    """Split spoken source text at declared closed-label cues."""
    matches: list[tuple[int, str]] = []
    for label in labels:
        cue = LABEL_SOURCE_CUES.get(label)
        if cue:
            matches.extend(
                (match.start(), label)
                for match in re.finditer(
                    rf"(?<!\w){re.escape(cue)}(?!\w)", source_text, re.IGNORECASE
                )
            )
    matches.sort()
    spans: dict[str, list[str]] = {}
    for idx, (start, label) in enumerate(matches):
        end = matches[idx + 1][0] if idx + 1 < len(matches) else len(source_text)
        spans.setdefault(label, []).append(source_text[start:end].strip())
    return spans


def punctuation_equivalence_key(text: Any) -> tuple[str, ...] | None:
    """Ignore sentence punctuation/case/spacing, but preserve lexical and technical tokens."""
    if not isinstance(text, str):
        return None
    token_pattern = re.compile(
        r"https?://[^\s,!?;:]+"
        r"|--?[A-Za-z0-9][A-Za-z0-9_-]*(?:=[^\s,!?;:]+)?"
        r"|[A-Za-z0-9]+(?:[_/=.()+\-][A-Za-z0-9_./=()+\-]*)*"
    )
    return tuple(token.casefold() for token in token_pattern.findall(text))


def safe_mapping_get(mapping: dict[str, Any], key: Any) -> Any:
    """Return a catalog mapping value only for a primitive string key."""
    return mapping.get(key) if isinstance(key, str) else None


def declared_output_atoms(ops: dict[str, Any], labels: list[str]) -> set[str]:
    """Collect literal lexical outputs licensed by the closed safe-conversion catalog."""
    allowed: set[str] = set()
    for label in ops.get("labels", []):
        if isinstance(label, str) and label in labels:
            allowed.update(lexical_atoms(label))
    for conversion in ops.get("conversions", []):
        if not isinstance(conversion, str) or conversion not in SAFE_CONVERSION_CUES:
            continue
        rhs = conversion.split("→", 1)[1].strip()
        if rhs.casefold().endswith(" label"):
            rhs = rhs[: -len(" label")]
        if rhs.casefold() == "numbered_lines" or "\\n" in rhs:
            continue
        allowed.update(lexical_atoms(rhs))
    return allowed


def provenance_source_text(fixture: dict[str, Any]) -> str:
    """Use selected source, except safe complementary merge may use both sources."""
    selection = fixture.get("source_selection")
    if isinstance(selection, dict) and selection.get("reason") == "safe_complementary_merge":
        return "\n".join(
            str(source.get("text", "")) for source in available_sources(fixture.get("sources"))
        )
    return selected_source_text(fixture) or ""


def selected_source_text(fixture: dict[str, Any]) -> str | None:
    sel = fixture.get("source_selection")
    sources = fixture.get("sources")
    if not isinstance(sel, dict) or not isinstance(sources, list):
        return None
    provider = sel.get("selected_provider")
    for source in sources:
        if (
            isinstance(source, dict)
            and source.get("provider") == provider
            and source.get("available") is True
        ):
            text = source.get("text")
            return text if isinstance(text, str) else None
    return None


def available_sources(sources: Any) -> list[dict[str, Any]]:
    if not isinstance(sources, list):
        return []
    out: list[dict[str, Any]] = []
    for source in sources:
        if isinstance(source, dict) and source.get("available") is True:
            out.append(source)
    return out


def validate_package(corpus: Any, schema: Any) -> list[str]:
    errors: list[str] = []

    if not isinstance(corpus, dict):
        return ["corpus must be a JSON object"]
    if not isinstance(schema, dict):
        errors.append("schema must be a JSON object")

    if not exact_keys(corpus, TOP_KEYS, "corpus", errors):
        # still continue where possible
        pass

    if corpus.get("corpus_id") != "voisu-developer-prompt-rendering-behavior-2026-08-11":
        errors.append("corpus_id must be voisu-developer-prompt-rendering-behavior-2026-08-11")
    if not isinstance(corpus.get("version"), str) or not corpus.get("version"):
        errors.append("version must be a non-empty string")
    if corpus.get("language") != "en":
        errors.append("language must be en")

    issue = corpus.get("issue")
    if exact_keys(issue, ISSUE_KEYS, "issue", errors) and isinstance(issue, dict):
        if issue.get("github_number") != 138:
            errors.append("issue.github_number must be 138")
        if issue.get("parent_map") != 133:
            errors.append("issue.parent_map must be 133")
        for field in ("title", "url"):
            if not isinstance(issue.get(field), str) or not issue.get(field):
                errors.append(f"issue.{field} must be a non-empty string")
        blocks = issue.get("blocks")
        if not isinstance(blocks, list) or not blocks or any(
            not isinstance(value, int) or isinstance(value, bool) for value in blocks
        ):
            errors.append("issue.blocks must be a non-empty integer array")
        if "blocked_by" in (issue or {}):
            errors.append("issue must not declare native blocked_by for #138")

    governing = corpus.get("governing")
    if exact_keys(governing, GOVERNING_KEYS, "governing", errors):
        assert isinstance(governing, dict)
        if governing.get("contract_issue") != 137:
            errors.append("governing.contract_issue must be 137")
        if governing.get("map_issue") != 133:
            errors.append("governing.map_issue must be 133")
        for field in ("resolution_issues",):
            values = governing.get(field)
            if not isinstance(values, list) or any(
                not isinstance(value, int) or isinstance(value, bool) for value in values
            ):
                errors.append(f"governing.{field} must be an integer array")
        for field, allow_empty in (("superseded_sources", True), ("out_of_scope", False)):
            values = governing.get(field)
            if not isinstance(values, list) or (not allow_empty and not values) or any(
                not isinstance(value, str) or (not allow_empty and not value)
                for value in values
            ):
                qualifier = "" if allow_empty else "non-empty "
                errors.append(f"governing.{field} must be a {qualifier}string array")

    labels = corpus.get("closed_structured_labels")
    if not isinstance(labels, list) or not labels:
        errors.append("closed_structured_labels must be a non-empty array")
        labels = []
    else:
        try:
            if len(labels) != len(set(labels)):
                errors.append("closed_structured_labels must be unique")
        except TypeError:
            errors.append("closed_structured_labels items must be hashable strings")
        for lab in labels:
            if not isinstance(lab, str) or not lab:
                errors.append("closed_structured_labels items must be non-empty strings")

    closed_tags = corpus.get("closed_tags")
    if not isinstance(closed_tags, list) or not closed_tags:
        errors.append("closed_tags must be a non-empty array")
        closed_tags = []
    else:
        try:
            if len(closed_tags) != len(set(closed_tags)):
                errors.append("closed_tags must be unique")
        except TypeError:
            errors.append("closed_tags items must be hashable strings")
        for tag in closed_tags:
            if not isinstance(tag, str) or not tag:
                errors.append("closed_tags items must be non-empty strings")
    closed_tag_set = set(t for t in closed_tags if isinstance(t, str))

    coverage_reqs = corpus.get("coverage_requirements")
    if not isinstance(coverage_reqs, list) or not coverage_reqs:
        errors.append("coverage_requirements must be a non-empty array")
        coverage_reqs = []
    else:
        invalid_reqs = [
            req for req in coverage_reqs if not isinstance(req, str) or not req
        ]
        if invalid_reqs:
            errors.append("coverage_requirements items must be non-empty strings")
        else:
            coverage_req_set = set(coverage_reqs)
            if len(coverage_reqs) != len(coverage_req_set):
                errors.append("coverage_requirements must be unique")
            if coverage_req_set != closed_tag_set:
                errors.append(
                    "coverage_requirements must equal closed_tags exactly "
                    f"(symmetric diff {sorted(coverage_req_set ^ closed_tag_set)})"
                )

    def check_catalog(name: str, expected: list[str]) -> None:
        values = corpus.get(name)
        if not isinstance(values, list):
            errors.append(f"{name} must be an array")
            return
        try:
            if values != expected:
                errors.append(f"{name} catalog mismatch: declared={values} expected={expected}")
            if len(values) != len(set(values)):
                errors.append(f"{name} must be unique")
        except TypeError:
            errors.append(f"{name} items must be hashable; unhashable values rejected")

    check_catalog("kinds", KINDS)
    check_catalog("policies", POLICIES)
    check_catalog("routes", ROUTES)
    check_catalog("cloud_request_states", CLOUD_POLICIES)
    check_catalog("cloud_outcomes", CLOUD_OUTCOMES)
    check_catalog("provider_states", PROVIDER_STATES)

    fb_catalog = corpus.get("fallback_triggers")
    expected_fb = ["none"] + FALLBACK_TRIGGERS
    if not isinstance(fb_catalog, list):
        errors.append("fallback_triggers must be an array")
    else:
        try:
            if fb_catalog != expected_fb:
                errors.append(
                    f"fallback_triggers catalog mismatch: declared={fb_catalog} expected={expected_fb}"
                )
        except TypeError:
            errors.append("fallback_triggers items must be hashable")

    matrix = corpus.get("coverage_matrix")
    if not isinstance(matrix, dict):
        errors.append("coverage_matrix must be an object")
        matrix = {}

    fixtures = corpus.get("fixtures")
    if not isinstance(fixtures, list) or not fixtures:
        errors.append("fixtures must be a non-empty array")
        return errors

    ids: list[str] = []
    fixture_by_id: dict[str, dict[str, Any]] = {}
    by_kind = {k: 0 for k in KINDS}
    by_policy = {p: 0 for p in POLICIES}
    by_route = {r: 0 for r in ROUTES}
    by_cloud = {c: 0 for c in CLOUD_POLICIES}
    fallback_n = 0
    labels_with_positive: set[str] = set()

    for idx, fixture in enumerate(fixtures):
        prefix = f"fixtures[{idx}]"
        if not isinstance(fixture, dict):
            errors.append(f"{prefix}: must be an object")
            continue
        if not exact_keys(fixture, FIXTURE_KEYS, prefix, errors):
            # Continue checks that do not depend on full shape where possible.
            pass

        fid = fixture.get("id")
        if not isinstance(fid, str) or not FIXTURE_ID_RE.match(fid):
            errors.append(f"{prefix}: invalid id {fid!r}")
            fid = f"fixtures[{idx}]"
        else:
            ids.append(fid)
            fixture_by_id[fid] = fixture

        for field in ("title", "local_baseline", "expected_final", "rationale"):
            val = fixture.get(field)
            if not isinstance(val, str) or not val:
                errors.append(f"{fid}: {field} must be a non-empty string")

        kind = fixture.get("kind")
        if not isinstance(kind, str) or kind not in KINDS:
            errors.append(f"{fid}: unknown kind {kind!r}")
        else:
            by_kind[kind] += 1

        policy = fixture.get("policy")
        if not isinstance(policy, str) or policy not in POLICIES:
            errors.append(f"{fid}: unknown policy {policy!r}")
        else:
            by_policy[policy] += 1

        route = fixture.get("route")
        if not isinstance(route, str) or route not in ROUTES:
            errors.append(f"{fid}: unknown route {route!r}")
        else:
            by_route[route] += 1

        surface_hint = fixture.get("surface_hint")
        if surface_hint is not None and (
            not isinstance(surface_hint, str) or surface_hint not in SURFACE_HINTS
        ):
            errors.append(f"{fid}: unknown surface_hint {surface_hint!r}")

        tags = fixture.get("tags")
        if not isinstance(tags, list) or not tags:
            errors.append(f"{fid}: tags must be a non-empty array")
            tags = []
        else:
            try:
                if len(tags) != len(set(tags)):
                    errors.append(f"{fid}: tags must be unique")
            except TypeError:
                errors.append(f"{fid}: tags must be a string array (unhashable item rejected)")
                tags = []
            for tag in tags:
                if not isinstance(tag, str):
                    errors.append(f"{fid}: tag items must be strings, got {type(tag).__name__}")
                elif tag not in closed_tag_set:
                    errors.append(f"{fid}: unknown fixture tag {tag!r}")
            if "no-autosend" not in tags:
                errors.append(f"{fid}: tags must include no-autosend")
            if kind == "everyday_message" and "kind-everyday" not in tags:
                errors.append(f"{fid}: everyday_message fixtures must tag kind-everyday")
            if kind == "developer_prompt" and "kind-developer" not in tags:
                errors.append(f"{fid}: developer_prompt fixtures must tag kind-developer")

        # Sources
        sources = fixture.get("sources")
        if not isinstance(sources, list) or not (1 <= len(sources) <= 2):
            errors.append(f"{fid}: sources must be an array of length 1 or 2")
            sources = []
        avail = []
        primaries = 0
        for s_idx, source in enumerate(sources):
            sp = f"{fid}.sources[{s_idx}]"
            if not exact_keys(source, SOURCE_KEYS, sp, errors):
                continue
            if source.get("provider") not in PROVIDERS:
                errors.append(f"{sp}: invalid provider")
            if not isinstance(source.get("available"), bool):
                errors.append(f"{sp}: available must be bool")
            if not isinstance(source.get("text"), str):
                errors.append(f"{sp}: text must be a string")
            elif source.get("available") is True and not source.get("text"):
                errors.append(f"{sp}: available source text must be non-empty")
            if not isinstance(source.get("primary"), bool):
                errors.append(f"{sp}: primary must be bool")
            elif source.get("primary") is True:
                primaries += 1
                if source.get("available") is not True:
                    errors.append(f"{sp}: primary source must be available")
            if source.get("available") is True:
                avail.append(source)
            # Superseded command-introducer pattern
            text = source.get("text") if isinstance(source.get("text"), str) else ""
            if re.search(
                r"\bcommand (exclamation point|period|new line|new paragraph|quote)\b",
                text,
            ):
                errors.append(f"{fid}: superseded explicit command-introducer pattern in source")

        if len(avail) < 1:
            errors.append(f"{fid}: at least one source must be available")
        if primaries != 1:
            errors.append(f"{fid}: exactly one source must have primary=true")
        if len(sources) == 2:
            provider_ids = [
                source.get("provider") for source in sources if isinstance(source, dict)
            ]
            if len(provider_ids) != 2 or any(
                not isinstance(provider, str) for provider in provider_ids
            ) or sorted(provider_ids) != sorted(PROVIDERS):
                errors.append(
                    f"{fid}: two-source fixtures require distinct provider identities "
                    "provider_a and provider_b"
                )

        pstate = fixture.get("provider_state")
        if not isinstance(pstate, str) or pstate not in PROVIDER_STATES:
            errors.append(f"{fid}: unknown provider_state {pstate!r}")
        else:
            if pstate == "single_provider" and len(avail) != 1:
                errors.append(f"{fid}: single_provider requires exactly one available source")
            if pstate != "single_provider" and len(avail) != 2:
                errors.append(f"{fid}: dual provider state {pstate!r} requires two available sources")
            if pstate == "exact_agreement":
                if len(avail) != 2:
                    errors.append(f"{fid}: exact_agreement requires two available Source Transcripts")
                elif avail[0].get("text") != avail[1].get("text"):
                    errors.append(f"{fid}: exact_agreement sources must have identical text")
            if pstate == "punctuation_only_agreement":
                if len(avail) == 2:
                    left_raw = avail[0].get("text")
                    right_raw = avail[1].get("text")
                    if left_raw == right_raw:
                        errors.append(
                            f"{fid}: punctuation_only_agreement must not have identical raw text"
                        )
                    elif punctuation_equivalence_key(left_raw) != punctuation_equivalence_key(
                        right_raw
                    ):
                        errors.append(
                            f"{fid}: punctuation_only_agreement lexical/semantic mismatch; "
                            "only punctuation, case, and spacing may differ"
                        )
            if pstate in {
                "safe_complementary",
                "protected_token_disagreement",
                "semantic_disagreement",
            } and len(avail) == 2 and avail[0].get("text") == avail[1].get("text"):
                errors.append(f"{fid}: provider state {pstate!r} requires divergent source text")
            if pstate == "safe_complementary" and len(avail) == 2:
                final_tokens = evidence_tokens(fixture.get("expected_final", ""))
                source_token_sets = [evidence_tokens(str(source.get("text", ""))) for source in avail]
                for source_idx, source_tokens in enumerate(source_token_sets):
                    other_tokens = source_token_sets[1 - source_idx]
                    if not ((source_tokens - other_tokens) & final_tokens):
                        errors.append(
                            f"{fid}: safe_complementary source {source_idx} contributes "
                            "no unique final evidence"
                        )
            if pstate in ("exact_agreement", "punctuation_only_agreement"):
                cloud_obj = fixture.get("cloud") if isinstance(fixture.get("cloud"), dict) else {}
                if route not in ("literal_identity", "deterministic_local") or cloud_obj.get(
                    "request_policy"
                ) != "not_allowed":
                    errors.append(
                        f"{fid}: exact/punctuation provider agreement must stay local"
                    )

        # Source selection
        selection = fixture.get("source_selection")
        if exact_keys(selection, SELECTION_KEYS, f"{fid}.source_selection", errors):
            assert isinstance(selection, dict)
            selected_provider = selection.get("selected_provider")
            selection_reason = selection.get("reason")
            if not isinstance(selected_provider, str) or selected_provider not in PROVIDERS:
                errors.append(f"{fid}: source_selection.selected_provider invalid")
            if not isinstance(selection_reason, str) or selection_reason not in SOURCE_SELECT_REASONS:
                errors.append(f"{fid}: unknown source_selection.reason {selection_reason!r}")
            expected_reason = safe_mapping_get(
                {
                    "single_provider": "only_available",
                    "exact_agreement": "exact_agreement",
                    "punctuation_only_agreement": "punctuation_local_render",
                    "safe_complementary": "safe_complementary_merge",
                    "protected_token_disagreement": "configured_primary_rank",
                    "semantic_disagreement": "configured_primary_rank",
                },
                pstate,
            )
            if expected_reason and selection.get("reason") != expected_reason:
                errors.append(
                    f"{fid}: provider_state {pstate!r} requires "
                    f"source_selection.reason {expected_reason!r}"
                )
            selected = selection.get("selected_provider")
            selected_obj = next(
                (
                    s
                    for s in sources
                    if isinstance(s, dict)
                    and s.get("provider") == selected
                    and s.get("available") is True
                ),
                None,
            )
            if selected_obj is None:
                errors.append(
                    f"{fid}: source_selection.selected_provider {selected!r} not available"
                )
            elif (
                selection.get("reason") == "configured_primary_rank"
                and selected_obj.get("primary") is not True
            ):
                errors.append(
                    f"{fid}: configured_primary_rank requires selected source primary=true"
                )
            elif selection.get("reason") == "only_available" and len(avail) != 1:
                errors.append(f"{fid}: only_available reason requires exactly one available source")

        # Cloud execution branch
        outcome: Any = None
        cloud = fixture.get("cloud")
        if required_allowed_keys(
            cloud, CLOUD_REQUIRED_KEYS, CLOUD_KEYS, f"{fid}.cloud", errors
        ):
            assert isinstance(cloud, dict)
            pol = cloud.get("request_policy")
            reqs = cloud.get("actual_requests")
            outcome = cloud.get("outcome")
            just = cloud.get("justification")
            deadline_evidence = cloud.get("deadline_evidence")
            rejected_candidate_excerpt = cloud.get("rejected_candidate_excerpt")
            rejected_labels = cloud.get("rejected_labels")
            rejected_labels_valid = (
                is_str_list(rejected_labels)
                and bool(rejected_labels)
                and len(set(rejected_labels)) == len(rejected_labels)
            )
            if not isinstance(pol, str) or pol not in CLOUD_POLICIES:
                errors.append(f"{fid}: unknown cloud.request_policy {pol!r}")
            else:
                by_cloud[pol] += 1
            if not isinstance(reqs, int) or isinstance(reqs, bool) or reqs not in (0, 1):
                errors.append(f"{fid}: cloud.actual_requests must be 0 or 1")
            if not isinstance(outcome, str) or outcome not in CLOUD_OUTCOMES:
                errors.append(f"{fid}: unknown cloud.outcome {outcome!r}")
            if not isinstance(just, str) or not just:
                errors.append(f"{fid}: cloud.justification must be a non-empty string")

            if outcome == "deadline_exceeded":
                if not exact_keys(
                    deadline_evidence,
                    DEADLINE_EVIDENCE_KEYS,
                    f"{fid}.cloud.deadline_evidence",
                    errors,
                ):
                    errors.append(
                        f"{fid}: deadline_exceeded requires typed numeric deadline evidence"
                    )
                else:
                    assert isinstance(deadline_evidence, dict)
                    deadline_ms = deadline_evidence.get("deadline_ms")
                    observed_ms = deadline_evidence.get("observed_elapsed_ms")
                    delivery_by_ms = deadline_evidence.get(
                        "local_baseline_delivery_start_by_ms"
                    )
                    if deadline_ms != 1500:
                        errors.append(f"{fid}: product Delivery deadline_ms must equal 1500")
                    if (
                        not isinstance(observed_ms, int)
                        or isinstance(observed_ms, bool)
                        or observed_ms <= 1500
                    ):
                        errors.append(
                            f"{fid}: observed_elapsed_ms must be an integer greater than 1500"
                        )
                    if (
                        not isinstance(delivery_by_ms, int)
                        or isinstance(delivery_by_ms, bool)
                        or not 0 <= delivery_by_ms <= 1500
                    ):
                        errors.append(
                            f"{fid}: local_baseline_delivery_start_by_ms must be within "
                            "0..1500"
                        )
                    if (
                        isinstance(deadline_ms, int)
                        and not isinstance(deadline_ms, bool)
                        and isinstance(observed_ms, int)
                        and not isinstance(observed_ms, bool)
                        and isinstance(delivery_by_ms, int)
                        and not isinstance(delivery_by_ms, bool)
                        and not delivery_by_ms <= deadline_ms < observed_ms
                    ):
                        errors.append(
                            f"{fid}: Delivery must start with the local baseline by the "
                            "deadline while cloud is still pending"
                        )
                if "deadline-local-baseline-start" not in tags:
                    errors.append(
                        f"{fid}: deadline_exceeded requires "
                        "deadline-local-baseline-start tag"
                    )
            elif deadline_evidence is not None:
                errors.append(
                    f"{fid}: cloud.deadline_evidence is allowed only for deadline_exceeded"
                )
            if (
                "deadline-local-baseline-start" in tags
                and outcome != "deadline_exceeded"
            ):
                errors.append(
                    f"{fid}: deadline-local-baseline-start tag requires deadline_exceeded"
                )

            if rejected_candidate_excerpt is not None and (
                not isinstance(rejected_candidate_excerpt, str)
                or not rejected_candidate_excerpt
            ):
                errors.append(
                    f"{fid}: cloud.rejected_candidate_excerpt must be a non-empty string"
                )
            if rejected_labels is not None and not rejected_labels_valid:
                errors.append(
                    f"{fid}: cloud.rejected_labels must be a non-empty unique string array"
                )

            if isinstance(outcome, str) and outcome.startswith("rejected_"):
                forbidden_candidates = fixture.get("forbidden_outcomes")
                forbidden_candidates = (
                    forbidden_candidates if isinstance(forbidden_candidates, list) else []
                )
                if rejected_candidate_excerpt is None and rejected_labels is None:
                    errors.append(
                        f"{fid}: cloud outcome {outcome!r} requires typed rejected candidate evidence"
                    )
                if outcome == "rejected_invalid_label":
                    if not rejected_labels_valid:
                        errors.append(
                            f"{fid}: rejected_invalid_label requires cloud.rejected_labels"
                        )
                    elif not any(label not in labels for label in rejected_labels):
                        errors.append(
                            f"{fid}: rejected_invalid_label must identify a non-closed label"
                        )
                    else:
                        for rejected_label in rejected_labels:
                            if not any(
                                isinstance(candidate, str)
                                and re.search(
                                    rf"(?im)^[ \t]*{re.escape(rejected_label)}[ \t]*:",
                                    candidate,
                                )
                                for candidate in forbidden_candidates
                            ):
                                errors.append(
                                    f"{fid}: rejected label {rejected_label!r} must be bound "
                                    "to a forbidden candidate"
                                )
                elif (
                    not isinstance(rejected_candidate_excerpt, str)
                    or not rejected_candidate_excerpt
                ):
                    errors.append(
                        f"{fid}: cloud outcome {outcome!r} requires rejected_candidate_excerpt"
                    )
                elif not any(
                    isinstance(candidate, str)
                    and rejected_candidate_excerpt in candidate
                    for candidate in forbidden_candidates
                ):
                    errors.append(
                        f"{fid}: rejected_candidate_excerpt must be bound to a forbidden candidate"
                    )
            elif rejected_candidate_excerpt is not None or rejected_labels is not None:
                errors.append(
                    f"{fid}: rejected candidate evidence is allowed only for rejected_* outcomes"
                )

            # Branch consistency
            if pol == "not_allowed":
                if reqs != 0:
                    errors.append(f"{fid}: cloud not_allowed requires actual_requests=0")
                if outcome not in ("not_attempted",):
                    errors.append(f"{fid}: cloud not_allowed requires outcome not_attempted")
            if reqs == 0 and outcome not in ("not_attempted", "skipped"):
                errors.append(
                    f"{fid}: actual_requests=0 requires outcome not_attempted or skipped"
                )
            if reqs == 1 and outcome in ("not_attempted", "skipped"):
                errors.append(f"{fid}: actual_requests=1 cannot use outcome {outcome!r}")
            if outcome in FAIL_OR_SKIP and fixture.get("expected_final") != fixture.get(
                "local_baseline"
            ):
                errors.append(
                    f"{fid}: non-success cloud branch requires expected_final == local_baseline"
                )
            if outcome == "succeeded":
                if reqs != 1:
                    errors.append(f"{fid}: succeeded cloud requires actual_requests=1")
                if pol == "not_allowed":
                    errors.append(f"{fid}: succeeded cloud cannot have request_policy not_allowed")

            # Coverage tags must describe the executed branch, not a nearby concept.
            if "cloud-skipped" in tags and not (
                pol in ("allowed", "required") and reqs == 0 and outcome == "skipped"
            ):
                errors.append(
                    f"{fid}: cloud-skipped tag requires an allowed/required branch "
                    "explicitly skipped"
                )
            if "cloud-failed" in tags and not (
                reqs == 1
                and outcome
                in {
                    "rejected_unsafe",
                    "rejected_unverifiable",
                    "rejected_invalid_label",
                    "schema_failure",
                    "provider_failure",
                    "deadline_exceeded",
                }
            ):
                errors.append(f"{fid}: cloud-failed tag requires one failed cloud request")
            if "fallback-provider" in tags and not (
                outcome == "provider_failure"
                and isinstance(fixture.get("fallback"), dict)
                and fixture["fallback"].get("trigger") == "provider_failure"
            ):
                errors.append(
                    f"{fid}: fallback-provider tag requires cloud provider_failure fallback"
                )

        if "dual-stt-single-provider" in tags and not (
            pstate == "single_provider" and len(avail) == 1
        ):
            errors.append(
                f"{fid}: dual-stt-single-provider tag requires exactly one available provider"
            )
        if "multi-paragraph" in tags and not (
            isinstance(fixture.get("expected_final"), str)
            and "\n\n" in fixture["expected_final"]
        ):
            errors.append(f"{fid}: multi-paragraph tag requires a blank-line paragraph break")
        if "protected-quote" in tags:
            available_text = "\n".join(
                str(source.get("text", "")) for source in avail if isinstance(source, dict)
            )
            if not (
                re.search(r"\bquote\b.*\bunquote\b", available_text, re.IGNORECASE)
                and isinstance(fixture.get("expected_final"), str)
                and '"' in fixture["expected_final"]
            ):
                errors.append(
                    f"{fid}: protected-quote tag requires paired source quote cues "
                    "and a quoted final span"
                )
        if "protected-negation" in tags:
            available_text = "\n".join(
                str(source.get("text", "")) for source in avail if isinstance(source, dict)
            )
            if not re.search(r"\b(?:not|never|no|don't|doesn't|isn't|can't|won't)\b", available_text, re.IGNORECASE):
                errors.append(f"{fid}: protected-negation tag requires source negation evidence")

        # High-risk coverage tags must be truthful about the fixture's actual fields/behavior.
        def require_exact_tag(
            family: str, expected_tag: str | None, known_tags: set[str]
        ) -> None:
            present = known_tags & set(tag for tag in tags if isinstance(tag, str))
            expected = {expected_tag} if expected_tag else set()
            if present != expected:
                errors.append(
                    f"{fid}: {family} coverage tag mismatch: present={sorted(present)} "
                    f"expected={sorted(expected)}"
                )

        require_exact_tag(
            "policy", safe_mapping_get(POLICY_TAGS, policy), set(POLICY_TAGS.values())
        )
        expected_kind_tags = safe_mapping_get(KIND_TAGS, kind)
        require_exact_tag(
            "kind",
            expected_kind_tags[0] if expected_kind_tags else None,
            {values[0] for values in KIND_TAGS.values()},
        )
        kind_alias_tags = {values[1] for values in KIND_TAGS.values()}
        if kind_alias_tags & set(tags):
            require_exact_tag(
                "kind-alias",
                expected_kind_tags[1] if expected_kind_tags else None,
                kind_alias_tags,
            )
        require_exact_tag(
            "route", safe_mapping_get(ROUTE_TAGS, route), set(ROUTE_TAGS.values())
        )

        provider_tag_set = set(PROVIDER_STATE_TAGS.values())
        present_provider_tags = provider_tag_set & set(tags)
        if present_provider_tags:
            require_exact_tag(
                "provider-state",
                safe_mapping_get(PROVIDER_STATE_TAGS, pstate),
                provider_tag_set,
            )

        timing_obj = fixture.get("timing")
        timing_tag = None
        if isinstance(timing_obj, dict):
            timing_tag = safe_mapping_get(
                {
                    "clear": "timing-clear",
                    "uncertain": "timing-uncertain",
                },
                timing_obj.get("certainty"),
            )
        present_timing_tags = {"timing-clear", "timing-uncertain"} & set(tags)
        if timing_obj is not None or present_timing_tags:
            require_exact_tag(
                "timing-certainty", timing_tag, {"timing-clear", "timing-uncertain"}
            )

        cloud_obj_for_tags = fixture.get("cloud") if isinstance(fixture.get("cloud"), dict) else {}
        cloud_outcome_for_tags = cloud_obj_for_tags.get("outcome")
        expected_cloud_tag = None
        if cloud_outcome_for_tags == "succeeded":
            expected_cloud_tag = "cloud-succeeded"
        elif cloud_outcome_for_tags == "skipped":
            expected_cloud_tag = "cloud-skipped"
        elif isinstance(cloud_outcome_for_tags, str) and cloud_outcome_for_tags in ATTEMPTED_FAILURE_FALLBACK:
            expected_cloud_tag = "cloud-failed"
        require_exact_tag(
            "cloud-outcome",
            expected_cloud_tag,
            {"cloud-succeeded", "cloud-skipped", "cloud-failed"},
        )

        local_only_actual = route in ("literal_identity", "deterministic_local")
        if ("local-only" in tags) != local_only_actual:
            errors.append(
                f"{fid}: local-only tag must exactly match literal/deterministic local routing"
            )

        fallback_obj_for_tags = fixture.get("fallback")
        expected_fallback_tag = (
            safe_mapping_get(FALLBACK_TAGS, fallback_obj_for_tags.get("trigger"))
            if isinstance(fallback_obj_for_tags, dict)
            else None
        )
        require_exact_tag(
            "fallback-trigger", expected_fallback_tag, set(FALLBACK_TAGS.values())
        )

        if "multi-paragraph" in tags and not (
            isinstance(fixture.get("expected_final"), str)
            and "\n\n" in fixture["expected_final"]
        ):
            errors.append(f"{fid}: multi-paragraph tag requires actual blank-line layout")
        if "numbered-sequence" in tags:
            numbered_lines = re.findall(
                r"(?m)^\s*[1-9][0-9]*\.\s+\S", str(fixture.get("expected_final", ""))
            )
            if len(numbered_lines) < 2:
                errors.append(
                    f"{fid}: numbered-sequence tag requires at least two actual numbered lines"
                )

        # Ticket-required semantic tags need typed/source/final/operation witnesses.
        source_text = "\n".join(
            str(source.get("text", "")) for source in avail if isinstance(source, dict)
        )
        source_fold = source_text.casefold()
        final_value = fixture.get("expected_final")
        final_text = final_value if isinstance(final_value, str) else ""
        protected_values = [
            value
            for value in fixture.get("protected_tokens", [])
            if isinstance(value, str) and value
        ]
        fixture_ops = fixture.get("allowed_operations")
        ops_for_witness = fixture_ops if isinstance(fixture_ops, dict) else {}
        removals = [
            value.casefold()
            for value in ops_for_witness.get("removals", [])
            if isinstance(value, str)
        ]
        conversions_for_witness = {
            value
            for value in ops_for_witness.get("conversions", [])
            if isinstance(value, str)
        }
        negation_re = re.compile(
            r"\b(?:not|never|no|don't|doesn't|isn't|can't|won't)\b", re.IGNORECASE
        )
        filler_re = re.compile(r"\b(?:um|uh)\b", re.IGNORECASE)
        semantic_witnesses = {
            "protected-name": any(
                value.casefold() in {"voisu", "voice so"} and value.casefold() in source_fold
                for value in protected_values
            ),
            "protected-number": any(
                any(char.isdigit() for char in value)
                and value in source_text
                and value in final_text
                for value in protected_values
            ),
            "protected-negation": bool(negation_re.search(source_text))
            and any(negation_re.search(value) for value in protected_values),
            "protected-command": fixture.get("surface_hint") == "shell"
            and route == "literal_identity"
            and any("--" in value and value in source_text for value in protected_values),
            "protected-url-path": any(
                (value.startswith(("http://", "https://")) or "/" in value)
                and value in source_text
                and value in final_text
                for value in protected_values
            ),
            "protected-identifier": any(
                "_" in value and value in source_text and value in final_text
                for value in protected_values
            ),
            "protected-code": any(
                re.search(r"[(){};]", value)
                and value in source_text
                and value in final_text
                for value in protected_values
            ),
            "protected-quote": bool(re.search(r"\bquote\b.*\bunquote\b", source_text, re.I))
            and 'quote…unquote→"…"' in conversions_for_witness
            and '"' in final_text,
            "slang-dialect": any(
                value.casefold() in {"gonna", "aint"}
                and value.casefold() in source_fold
                and value in final_text
                for value in protected_values
            ),
            "already-formatted": route == "literal_identity"
            and ("\n" in source_text or fixture.get("surface_hint") == "shell"),
            "filler-clear-remove": bool(filler_re.search(source_text))
            and any(value in {"um", "uh"} for value in removals)
            and not filler_re.search(final_text),
            "filler-preserve": bool(filler_re.search(source_text))
            and bool(filler_re.search(final_text))
            and not any(value in {"um", "uh"} for value in removals),
            "backtrack-clear": "no wait" in source_fold
            and "no wait" in removals
            and "no wait" not in final_text.casefold(),
            "backtrack-uncertain": isinstance(fallback_obj_for_tags, dict)
            and fallback_obj_for_tags.get("trigger") == "uncertain_backtracking"
            and not removals
            and lexical_atoms(source_text) <= lexical_atoms(final_text),
            "symbol-convert": bool(conversions_for_witness & SYMBOL_CONVERSIONS),
            "symbol-stacked": len(conversions_for_witness & SYMBOL_CONVERSIONS) >= 2,
            "symbol-malformed": "quote" in source_fold
            and not conversions_for_witness
            and "quote" in final_text.casefold(),
            "symbol-quoted-meta": not (
                conversions_for_witness
                & {
                    "exclamation point→!",
                    "new line→\\n",
                    "new paragraph→\\n\\n",
                    "period→.",
                }
            )
            and any(
                phrase in source_fold and phrase in final_text.casefold()
                for phrase in ("exclamation point", "period", "new line")
            ),
        }
        for semantic_tag, witnessed in semantic_witnesses.items():
            if semantic_tag in tags and not witnessed:
                errors.append(
                    f"{fid}: {semantic_tag} tag lacks required typed/source/final witness"
                )

        # Route invariants
        baseline = fixture.get("local_baseline")
        final = fixture.get("expected_final")
        if route == "deterministic_local":
            cloud_obj = fixture.get("cloud") if isinstance(fixture.get("cloud"), dict) else {}
            if cloud_obj.get("request_policy") != "not_allowed":
                errors.append(
                    f"{fid}: deterministic_local requires cloud.request_policy not_allowed"
                )
            if cloud_obj.get("actual_requests") not in (0, None) and cloud_obj.get(
                "actual_requests"
            ) != 0:
                errors.append(f"{fid}: deterministic_local requires cloud.actual_requests=0")
            if baseline != final:
                errors.append(
                    f"{fid}: deterministic_local final Transcript must equal local_baseline"
                )
        if route == "literal_identity":
            cloud_obj = fixture.get("cloud") if isinstance(fixture.get("cloud"), dict) else {}
            if cloud_obj.get("request_policy") != "not_allowed" or cloud_obj.get(
                "actual_requests"
            ) != 0:
                errors.append(f"{fid}: literal_identity requires no cloud")
            selected_text = selected_source_text(fixture)
            if selected_text is None:
                errors.append(f"{fid}: literal_identity missing selected source text")
            elif not (selected_text == baseline == final):
                errors.append(
                    f"{fid}: literal_identity requires selected source == local_baseline == expected_final"
                )
        if route == "local_with_optional_cloud":
            cloud_obj = fixture.get("cloud") if isinstance(fixture.get("cloud"), dict) else {}
            if cloud_obj.get("request_policy") not in ("allowed", "required"):
                errors.append(
                    f"{fid}: local_with_optional_cloud requires cloud allowed or required"
                )

        # Timing
        timing = fixture.get("timing")
        if timing is not None:
            if exact_keys(timing, TIMING_KEYS, f"{fid}.timing", errors):
                assert isinstance(timing, dict)
                certainty = timing.get("certainty")
                if not isinstance(certainty, str) or certainty not in TIMING_CERTAINTIES:
                    errors.append(f"{fid}: timing.certainty must be clear or uncertain")
                boundaries = timing.get("boundaries")
                if not isinstance(boundaries, list) or not boundaries:
                    errors.append(f"{fid}: timing.boundaries must be a non-empty array")
                else:
                    for b_idx, boundary in enumerate(boundaries):
                        bp = f"{fid}.timing.boundaries[{b_idx}]"
                        if not exact_keys(boundary, BOUNDARY_KEYS, bp, errors):
                            continue
                        assert isinstance(boundary, dict)
                        if boundary.get("source_provider") not in PROVIDERS:
                            errors.append(f"{bp}: invalid source_provider")
                        left = boundary.get("left_phrase")
                        right = boundary.get("right_phrase")
                        pause = boundary.get("pause_ms")
                        if not isinstance(left, str) or not left:
                            errors.append(f"{bp}: left_phrase must be non-empty string")
                        if not isinstance(right, str) or not right:
                            errors.append(f"{bp}: right_phrase must be non-empty string")
                        if not isinstance(pause, int) or isinstance(pause, bool) or pause < 0:
                            errors.append(f"{bp}: pause_ms must be a non-negative integer")
                        # source consistency: phrases must appear in available source text
                        prov = boundary.get("source_provider")
                        src_text = next(
                            (
                                s.get("text")
                                for s in sources
                                if isinstance(s, dict)
                                and s.get("provider") == prov
                                and s.get("available") is True
                            ),
                            None,
                        )
                        if not isinstance(src_text, str):
                            errors.append(f"{bp}: source_provider not available")
                        else:
                            if isinstance(left, str) and left not in src_text:
                                errors.append(f"{bp}: left_phrase not found in source text")
                            if isinstance(right, str) and right not in src_text:
                                errors.append(f"{bp}: right_phrase not found in source text")
                            if (
                                isinstance(left, str)
                                and isinstance(right, str)
                                and left in src_text
                                and right in src_text
                            ):
                                if src_text.find(left) > src_text.find(right):
                                    errors.append(
                                        f"{bp}: left_phrase must precede right_phrase in source"
                                    )

        # Allowed operations
        ops = fixture.get("allowed_operations")
        if exact_keys(ops, OPS_KEYS, f"{fid}.allowed_operations", errors):
            assert isinstance(ops, dict)
            for k in OPS_KEYS:
                if not is_str_list(ops.get(k)):
                    errors.append(f"{fid}: allowed_operations.{k} must be a string array")
            for lab in ops.get("labels") or []:
                if not isinstance(lab, str) or lab not in labels:
                    errors.append(f"{fid}: allowed_operations.labels has non-closed label {lab!r}")
            provenance_text = provenance_source_text(fixture).casefold()
            for conversion in ops.get("conversions") or []:
                if not isinstance(conversion, str) or conversion not in SAFE_CONVERSION_CUES:
                    errors.append(
                        f"{fid}: unknown safe conversion {conversion!r}; arbitrary conversions forbidden"
                    )
                    continue
                missing_cues = [
                    cue
                    for cue in SAFE_CONVERSION_CUES[conversion]
                    if not re.search(
                        rf"(?<!\w){re.escape(cue.casefold())}(?!\w)", provenance_text
                    )
                ]
                if missing_cues:
                    errors.append(
                        f"{fid}: safe conversion source cue missing for {conversion!r}: "
                        f"{missing_cues}"
                    )
            selected_text = selected_source_text(fixture) or ""
            if (
                re.search(r"\bsteps\b", selected_text, re.IGNORECASE)
                and isinstance(baseline, str)
                and not re.search(r"\bsteps\b", baseline, re.IGNORECASE)
                and not any("steps" in conversion.casefold() for conversion in ops.get("conversions", []))
            ):
                errors.append(
                    f"{fid}: spoken steps cue removed without a declared formatting-cue conversion"
                )

        # Label evidence + output labels
        label_ev = fixture.get("label_evidence")
        if not isinstance(label_ev, list):
            errors.append(f"{fid}: label_evidence must be an array")
            label_ev = []
        declared_labels: list[str] = []
        if isinstance(ops, dict):
            declared_labels = [
                lab for lab in (ops.get("labels") or []) if isinstance(lab, str)
            ]
        evidenced: set[str] = set()
        final_sections = labeled_sections(final, labels) if isinstance(final, str) else {}
        for e_idx, ev in enumerate(label_ev):
            ep = f"{fid}.label_evidence[{e_idx}]"
            if not exact_keys(ev, LABEL_EV_KEYS, ep, errors):
                continue
            assert isinstance(ev, dict)
            lab = ev.get("label")
            if lab not in labels:
                errors.append(f"{ep}: label {lab!r} not in closed set")
            else:
                evidenced.add(lab)
                labels_with_positive.add(lab)
            if lab not in declared_labels:
                errors.append(f"{ep}: label {lab!r} not declared in allowed_operations.labels")
            if ev.get("source_provider") not in PROVIDERS:
                errors.append(f"{ep}: invalid source_provider")
            span = ev.get("source_span_text")
            if not isinstance(span, str) or not span:
                errors.append(f"{ep}: source_span_text must be non-empty string")
            else:
                src_text = next(
                    (
                        s.get("text")
                        for s in sources
                        if isinstance(s, dict)
                        and s.get("provider") == ev.get("source_provider")
                        and s.get("available") is True
                    ),
                    None,
                )
                if not isinstance(src_text, str) or span not in src_text:
                    errors.append(f"{ep}: source_span_text not found in available source")
                elif isinstance(lab, str) and lab in LABEL_SOURCE_CUES:
                    expected_spans = cue_bearing_spans(src_text, declared_labels).get(lab, [])
                    if span not in expected_spans:
                        errors.append(
                            f"{ep}: source_span_text must equal the cue-bearing span for {lab!r}"
                        )
                    section = final_sections.get(lab)
                    if section is None:
                        errors.append(f"{ep}: expected_final lacks labeled section {lab!r}")
                    else:
                        span_atoms = lexical_atoms(span)
                        section_atoms = lexical_atoms(section)
                        consumed_atoms = lexical_atoms(LABEL_SOURCE_CUES[lab])
                        licensed_atoms: set[str] = set()
                        if isinstance(ops, dict):
                            for removal in ops.get("removals", []):
                                if isinstance(removal, str) and removal.casefold() in span.casefold():
                                    consumed_atoms.update(lexical_atoms(removal))
                            for conversion in ops.get("conversions", []):
                                if not isinstance(conversion, str) or conversion not in SAFE_CONVERSION_CUES:
                                    continue
                                cues = SAFE_CONVERSION_CUES[conversion]
                                if all(cue.casefold() in span.casefold() for cue in cues):
                                    for cue in cues:
                                        consumed_atoms.update(lexical_atoms(cue))
                                    rhs = conversion.split("→", 1)[1]
                                    licensed_atoms.update(lexical_atoms(rhs))
                        invented = sorted(section_atoms - span_atoms - licensed_atoms)
                        if invented:
                            errors.append(
                                f"{ep}: labeled section {lab!r} is not derived from its "
                                f"source_span_text; atoms {invented} lack section evidence"
                            )
                        removed = sorted(span_atoms - section_atoms - consumed_atoms)
                        if removed:
                            errors.append(
                                f"{ep}: labeled section {lab!r} removes undeclared source "
                                f"material from source_span_text: {removed}"
                            )

        # Every declared label needs evidence and an explicit cue-to-label conversion.
        conversions = ops.get("conversions", []) if isinstance(ops, dict) else []
        for lab in declared_labels:
            if lab not in evidenced:
                errors.append(f"{fid}: declared label {lab!r} lacks label_evidence")
            has_label_conversion = any(
                isinstance(conversion, str)
                and lab.casefold() in conversion.casefold()
                and "label" in conversion.casefold()
                for conversion in conversions
            )
            if lab == "Steps" and any(
                isinstance(conversion, str)
                and "spoken steps cue" in conversion.casefold()
                for conversion in conversions
            ):
                has_label_conversion = True
            if not has_label_conversion:
                errors.append(
                    f"{fid}: declared label {lab!r} lacks explicit cue-to-label conversion"
                )

        # Detect structural headers case-insensitively, then require canonical rendering.
        if isinstance(final, str):
            headers = structural_headers(final, labels if isinstance(labels, list) else [])
            found_labels = [canonical for _, canonical in headers if canonical]
            if policy == "natural" and found_labels:
                errors.append(
                    f"{fid}: Natural expected_final contains structural labels {found_labels}"
                )
            for raw, canonical in headers:
                if canonical is None:
                    errors.append(
                        f"{fid}: expected_final has non-closed structural header {raw!r}"
                    )
                    continue
                if raw != canonical:
                    errors.append(
                        f"{fid}: non-canonical structural label header {raw!r}; "
                        f"expected {canonical!r}"
                    )
                if canonical not in declared_labels:
                    errors.append(
                        f"{fid}: output label {canonical!r} not declared in "
                        "allowed_operations.labels"
                    )
                if canonical not in evidenced:
                    errors.append(f"{fid}: output label {canonical!r} lacks label_evidence")

        found_label_set = set(closed_label_prefixes(str(final), labels))
        for label, label_tag in LABEL_TAGS.items():
            actual = (
                label in declared_labels
                and label in evidenced
                and label in found_label_set
            )
            if (label_tag in tags) != actual:
                errors.append(
                    f"{fid}: {label_tag} tag must exactly match declared, evidenced, "
                    "canonical output label"
                )

        # Corpus-package provenance: ordinary output wording must be source-derived.
        if isinstance(ops, dict):
            provenance_text = provenance_source_text(fixture)
            source_atoms = lexical_atoms(provenance_text)
            licensed_atoms = source_atoms | declared_output_atoms(ops, labels)
            for field in ("local_baseline", "expected_final"):
                output = fixture.get(field)
                if not isinstance(output, str):
                    continue
                invented = sorted(lexical_atoms(output) - licensed_atoms)
                if invented:
                    errors.append(
                        f"{fid}: invented ordinary wording in {field}; lexical atoms "
                        f"not source-derived or declared: {invented}"
                    )

            source_order = ordered_lexical_tokens(provenance_text)
            for field in ("local_baseline", "expected_final"):
                output_order = ordinary_output_tokens(
                    fixture.get(field), labels, ops.get("conversions", [])
                )
                if not is_subsequence(output_order, source_order):
                    errors.append(
                        f"{fid}: {field} ordinary wording does not preserve source token order"
                    )

        # Protected tokens — case-sensitive
        protected = fixture.get("protected_tokens")
        if not isinstance(protected, list):
            errors.append(f"{fid}: protected_tokens must be an array")
            protected = []
        for token in protected:
            if not isinstance(token, str) or not token:
                errors.append(f"{fid}: protected_tokens items must be non-empty strings")
                continue
            for field in ("local_baseline", "expected_final"):
                text = fixture.get(field, "")
                if not isinstance(text, str) or token not in text:
                    errors.append(f"{fid}: protected token {token!r} absent from {field}")

        # Forbidden outcomes — empty string allowed
        forbidden = fixture.get("forbidden_outcomes")
        if not isinstance(forbidden, list):
            errors.append(f"{fid}: forbidden_outcomes must be an array")
            forbidden = []
        else:
            for outcome in forbidden:
                if not isinstance(outcome, str):
                    errors.append(
                        f"{fid}: forbidden_outcomes items must be strings "
                        f"(got {type(outcome).__name__})"
                    )
                elif outcome == final:
                    errors.append(
                        f"{fid}: forbidden outcome accidentally equals expected_final"
                    )

        # Delivery
        delivery = fixture.get("delivery")
        if exact_keys(delivery, DELIVERY_KEYS, f"{fid}.delivery", errors):
            assert isinstance(delivery, dict)
            delivery_state = delivery.get("state")
            if not isinstance(delivery_state, str) or delivery_state not in DELIVERY_STATES:
                errors.append(f"{fid}: delivery.state must be unsent")
            for flag in ("auto_send", "live_type", "replace_delivered"):
                if delivery.get(flag) is not False:
                    errors.append(f"{fid}: delivery.{flag} must be false")
            if outcome == "deadline_exceeded" and not (
                delivery_state == "unsent"
                and delivery.get("auto_send") is False
                and delivery.get("live_type") is False
                and delivery.get("replace_delivered") is False
            ):
                errors.append(
                    f"{fid}: deadline fallback requires unsent final-only Delivery with no replacement"
                )

        # Fallback
        fallback = fixture.get("fallback")
        if fallback is not None:
            if exact_keys(fallback, FALLBACK_OBJ_KEYS, f"{fid}.fallback", errors):
                assert isinstance(fallback, dict)
                trigger = fallback.get("trigger")
                result = fallback.get("result")
                if not isinstance(trigger, str) or trigger not in FALLBACK_TRIGGERS:
                    errors.append(f"{fid}: unknown fallback.trigger {trigger!r}")
                if not isinstance(result, str) or not result:
                    errors.append(f"{fid}: fallback.result must be a non-empty string")
                elif result != final:
                    errors.append(
                        f"{fid}: expected_final must equal fallback.result for fallback fixtures"
                    )
                fallback_n += 1

        # Attempted cloud failures require their exact fallback cause; success cannot fallback.
        cloud_obj = fixture.get("cloud") if isinstance(fixture.get("cloud"), dict) else {}
        cloud_outcome = cloud_obj.get("outcome")
        expected_trigger = (
            ATTEMPTED_FAILURE_FALLBACK.get(cloud_outcome)
            if isinstance(cloud_outcome, str)
            else None
        )
        if expected_trigger is not None:
            if not isinstance(fallback, dict):
                errors.append(
                    f"{fid}: cloud outcome {cloud_outcome!r} requires a failure fallback"
                )
            elif fallback.get("trigger") != expected_trigger:
                errors.append(
                    f"{fid}: cloud outcome {cloud_outcome!r} requires "
                    f"fallback.trigger {expected_trigger!r}, got {fallback.get('trigger')!r}"
                )
        elif cloud_outcome == "succeeded" and fallback is not None:
            errors.append(f"{fid}: succeeded cloud outcome must not have a fallback")

        if cloud_outcome == "deadline_exceeded":
            if route != "local_with_optional_cloud" or cloud_obj.get("actual_requests") != 1:
                errors.append(
                    f"{fid}: deadline_exceeded requires one attempted optional/required cloud branch"
                )
            if baseline != final:
                errors.append(
                    f"{fid}: deadline_exceeded must deliver the deterministic local baseline"
                )
            if not isinstance(fallback, dict) or fallback.get("trigger") != "deadline_exceeded":
                errors.append(
                    f"{fid}: deadline_exceeded requires deadline_exceeded fallback"
                )

        # Cloud required still needs baseline (already required non-empty)
        if cloud_obj.get("request_policy") == "required":
            if not isinstance(baseline, str) or not baseline:
                errors.append(f"{fid}: cloud-required fixture missing local_baseline")

    if len(ids) != len(set(ids)):
        try:
            dupes = sorted({i for i in ids if ids.count(i) > 1})
            errors.append(f"duplicate fixture IDs: {dupes}")
        except TypeError:
            errors.append("duplicate fixture ID check failed due to unhashable ids")

    # Every closed label needs positive corpus coverage (appears in some expected_final)
    for lab in labels:
        if not isinstance(lab, str):
            continue
        if lab not in labels_with_positive:
            # also accept detection from finals even without evidence (should be rare)
            any_final = any(
                isinstance(f.get("expected_final"), str)
                and closed_label_prefixes(f["expected_final"], [lab])
                for f in fixtures
                if isinstance(f, dict)
            )
            if not any_final:
                errors.append(f"closed label {lab!r} has no positive fixture coverage")

    # Coverage matrix: bind requirement names to fixtures that exhibit the tag
    if isinstance(matrix, dict) and coverage_reqs:
        for req in coverage_reqs:
            if not isinstance(req, str):
                errors.append(f"coverage_requirements item must be string, got {type(req).__name__}")
                continue
            if req not in matrix:
                errors.append(f"coverage_matrix missing requirement {req!r}")
                continue
            listed = matrix[req]
            if not isinstance(listed, list) or not listed:
                errors.append(f"coverage_matrix[{req!r}] must be a non-empty array")
                continue
            listed_ids = {item for item in listed if isinstance(item, str)}
            if len(listed_ids) != len(listed):
                errors.append(f"coverage_matrix[{req!r}] fixture IDs must be unique strings")
            for item in listed:
                if not isinstance(item, str):
                    errors.append(
                        f"coverage_matrix[{req!r}] items must be strings "
                        f"(got {type(item).__name__})"
                    )
                    continue
                if item not in fixture_by_id:
                    errors.append(
                        f"coverage_matrix[{req!r}] references unknown fixture {item!r}"
                    )
                else:
                    ftags = fixture_by_id[item].get("tags") or []
                    if req not in ftags:
                        errors.append(
                            f"coverage_matrix[{req!r}] lists {item} which lacks that tag"
                        )
            tagged_ids = {
                fid
                for fid, fixture in fixture_by_id.items()
                if req in (fixture.get("tags") or [])
            }
            if listed_ids != tagged_ids:
                errors.append(
                    f"coverage_matrix[{req!r}] must exactly equal fixtures carrying that tag; "
                    f"missing={sorted(tagged_ids - listed_ids)} "
                    f"extra={sorted(listed_ids - tagged_ids)}"
                )
        extra = set(matrix) - set(r for r in coverage_reqs if isinstance(r, str))
        if extra:
            errors.append(f"coverage_matrix has undeclared keys: {sorted(extra)}")

    counts = corpus.get("dataset_counts")
    if exact_keys(counts, COUNTS_KEYS, "dataset_counts", errors) and isinstance(counts, dict):
        if counts.get("fixtures_total") != len(fixtures):
            errors.append(
                f"dataset_counts.fixtures_total={counts.get('fixtures_total')} != actual {len(fixtures)}"
            )
        if counts.get("coverage_requirement_count") != len(coverage_reqs):
            errors.append("dataset_counts.coverage_requirement_count mismatch")
        if counts.get("closed_tag_count") != len(closed_tags):
            errors.append("dataset_counts.closed_tag_count mismatch")
        if counts.get("fallback_fixtures") != fallback_n:
            errors.append(
                f"dataset_counts.fallback_fixtures={counts.get('fallback_fixtures')} != actual {fallback_n}"
            )
        for name, actual in (
            ("by_kind", by_kind),
            ("by_policy", by_policy),
            ("by_route", by_route),
            ("by_cloud_request", by_cloud),
        ):
            declared = counts.get(name)
            if not isinstance(declared, dict):
                errors.append(f"dataset_counts.{name} must be an object")
            elif declared != actual:
                errors.append(
                    f"dataset_counts.{name} mismatch: declared={declared} actual={actual}"
                )

    invariants = corpus.get("invariants")
    if not isinstance(invariants, list) or not invariants:
        errors.append("invariants must be a non-empty array")
    else:
        for i_idx, inv in enumerate(invariants):
            if exact_keys(inv, INVARIANT_KEYS, f"invariants[{i_idx}]", errors):
                assert isinstance(inv, dict)
                for field in ("id", "summary"):
                    if not isinstance(inv.get(field), str) or not inv.get(field):
                        errors.append(
                            f"invariants[{i_idx}].{field} must be a non-empty string"
                        )

    # Bind the two required Adaptive/Structured policy contrasts to exact paired speech.
    for adaptive_id, structured_id, expected_kind in (
        ("DPR-27", "DPR-28", "developer_prompt"),
        ("DPR-30", "DPR-29", "everyday_message"),
    ):
        adaptive = fixture_by_id.get(adaptive_id)
        structured = fixture_by_id.get(structured_id)
        pair_name = f"{adaptive_id}/{structured_id}"
        if not adaptive or not structured:
            errors.append(f"policy pair {pair_name}: fixtures missing")
            continue
        if adaptive.get("kind") != expected_kind or structured.get("kind") != expected_kind:
            errors.append(f"policy pair {pair_name}: kind mismatch")
        adaptive_source = selected_source_text(adaptive)
        structured_source = selected_source_text(structured)
        if not adaptive_source or adaptive_source != structured_source:
            errors.append(f"policy pair {pair_name}: selected source speech must match exactly")
        if adaptive.get("policy") != "adaptive" or structured.get("policy") != "structured":
            errors.append(f"policy pair {pair_name}: policy assignment mismatch")
        adaptive_final = adaptive.get("expected_final")
        structured_final = structured.get("expected_final")
        if adaptive_final == structured_final:
            errors.append(f"policy pair {pair_name}: exact finals must differ")
        if closed_label_prefixes(str(adaptive_final), labels):
            errors.append(f"policy pair {pair_name}: Adaptive final must be Natural-shaped")
        if not closed_label_prefixes(str(structured_final), labels):
            errors.append(f"policy pair {pair_name}: Structured final must use a closed label")

    # Schema vocabulary alignment: Draft 2020-12 and this checker close the same sets.
    if isinstance(schema, dict):
        props = schema.get("properties") if isinstance(schema.get("properties"), dict) else {}
        cid = props.get("corpus_id", {})
        if isinstance(cid, dict) and cid.get("const") not in (
            None,
            "voisu-developer-prompt-rendering-behavior-2026-08-11",
        ):
            errors.append("schema corpus_id const mismatch")
        fixed_catalogs = {
            "closed_structured_labels": labels,
            "kinds": KINDS,
            "policies": POLICIES,
            "routes": ROUTES,
            "cloud_request_states": CLOUD_POLICIES,
            "cloud_outcomes": CLOUD_OUTCOMES,
            "provider_states": PROVIDER_STATES,
            "fallback_triggers": ["none"] + FALLBACK_TRIGGERS,
            "closed_tags": closed_tags,
            "coverage_requirements": coverage_reqs,
        }
        for name, expected in fixed_catalogs.items():
            declaration = props.get(name)
            if not isinstance(declaration, dict) or declaration.get("const") != expected:
                errors.append(f"schema {name} const must equal checker/corpus catalog")
        def schema_at(*path: str | int) -> Any:
            node: Any = schema
            try:
                for part in path:
                    node = node[part]
            except (KeyError, IndexError, TypeError):
                return None
            return node

        schema_shapes = [
            ("corpus", (), TOP_KEYS, TOP_KEYS),
            ("issue", ("$defs", "issue"), ISSUE_KEYS, ISSUE_KEYS),
            ("governing", ("$defs", "governing"), GOVERNING_KEYS, GOVERNING_KEYS),
            ("counts", ("$defs", "counts"), COUNTS_KEYS, COUNTS_KEYS),
            ("source", ("$defs", "source"), SOURCE_KEYS, SOURCE_KEYS),
            (
                "timing_boundary",
                ("$defs", "timing_boundary"),
                BOUNDARY_KEYS,
                BOUNDARY_KEYS,
            ),
            ("timing", ("$defs", "timing"), TIMING_KEYS, TIMING_KEYS),
            (
                "deadline_evidence",
                ("$defs", "deadline_evidence"),
                DEADLINE_EVIDENCE_KEYS,
                DEADLINE_EVIDENCE_KEYS,
            ),
            (
                "cloud_execution",
                ("$defs", "cloud_execution"),
                CLOUD_REQUIRED_KEYS,
                CLOUD_KEYS,
            ),
            (
                "label_evidence",
                ("$defs", "label_evidence"),
                LABEL_EV_KEYS,
                LABEL_EV_KEYS,
            ),
            (
                "source_selection",
                ("$defs", "source_selection"),
                SELECTION_KEYS,
                SELECTION_KEYS,
            ),
            (
                "allowed_operations",
                ("$defs", "allowed_operations"),
                OPS_KEYS,
                OPS_KEYS,
            ),
            ("delivery", ("$defs", "delivery"), DELIVERY_KEYS, DELIVERY_KEYS),
            (
                "fallback",
                ("$defs", "fallback"),
                FALLBACK_OBJ_KEYS,
                FALLBACK_OBJ_KEYS,
            ),
            ("fixture", ("$defs", "fixture"), FIXTURE_KEYS, FIXTURE_KEYS),
        ]
        for name, path, required_keys, property_keys in schema_shapes:
            definition = schema_at(*path) if path else schema
            if not isinstance(definition, dict):
                errors.append(f"schema shape drift: {name} definition must be an object")
                continue
            if definition.get("additionalProperties") is not False:
                errors.append(
                    f"schema shape drift: {name}.additionalProperties must be false"
                )
            schema_required = definition.get("required")
            if not isinstance(schema_required, list) or set(schema_required) != required_keys:
                errors.append(
                    f"schema shape drift: {name}.required must equal checker-required fields"
                )
            schema_properties = definition.get("properties")
            if not isinstance(schema_properties, dict) or set(schema_properties) != property_keys:
                errors.append(
                    f"schema shape drift: {name}.properties must equal checker-allowed fields"
                )

        nested_enums = [
            ("fixture.kind", ("$defs", "fixture", "properties", "kind", "enum"), KINDS),
            ("fixture.policy", ("$defs", "fixture", "properties", "policy", "enum"), POLICIES),
            ("fixture.route", ("$defs", "fixture", "properties", "route", "enum"), ROUTES),
            (
                "fixture.provider_state",
                ("$defs", "fixture", "properties", "provider_state", "enum"),
                PROVIDER_STATES,
            ),
            (
                "fixture.surface_hint",
                ("$defs", "fixture", "properties", "surface_hint", "anyOf", 1, "enum"),
                SURFACE_HINTS[1:],
            ),
            ("source.provider", ("$defs", "source", "properties", "provider", "enum"), PROVIDERS),
            (
                "timing_boundary.source_provider",
                ("$defs", "timing_boundary", "properties", "source_provider", "enum"),
                PROVIDERS,
            ),
            (
                "label_evidence.source_provider",
                ("$defs", "label_evidence", "properties", "source_provider", "enum"),
                PROVIDERS,
            ),
            (
                "source_selection.selected_provider",
                ("$defs", "source_selection", "properties", "selected_provider", "enum"),
                PROVIDERS,
            ),
            (
                "source_selection.reason",
                ("$defs", "source_selection", "properties", "reason", "enum"),
                SOURCE_SELECT_REASONS,
            ),
            (
                "cloud.request_policy",
                ("$defs", "cloud_execution", "properties", "request_policy", "enum"),
                CLOUD_POLICIES,
            ),
            (
                "cloud.outcome",
                ("$defs", "cloud_execution", "properties", "outcome", "enum"),
                CLOUD_OUTCOMES,
            ),
            (
                "fallback.trigger",
                ("$defs", "fallback", "properties", "trigger", "enum"),
                FALLBACK_TRIGGERS,
            ),
            (
                "timing.certainty",
                ("$defs", "timing", "properties", "certainty", "enum"),
                TIMING_CERTAINTIES,
            ),
            (
                "fixture.tags",
                ("$defs", "fixture", "properties", "tags", "items", "enum"),
                closed_tags,
            ),
            (
                "coverage_matrix.propertyNames",
                ("properties", "coverage_matrix", "propertyNames", "enum"),
                coverage_reqs,
            ),
            (
                "allowed_operations.conversions",
                ("$defs", "allowed_operations", "properties", "conversions", "items", "enum"),
                SAFE_CONVERSIONS,
            ),
            (
                "allowed_operations.labels",
                ("$defs", "allowed_operations", "properties", "labels", "items", "enum"),
                labels,
            ),
            (
                "label_evidence.label",
                ("$defs", "label_evidence", "properties", "label", "enum"),
                labels,
            ),
        ]
        for name, path, expected in nested_enums:
            if schema_at(*path) != expected:
                errors.append(f"schema nested enum drift: {name} must equal checker catalog")
        if schema_at("$defs", "delivery", "properties", "state", "const") != "unsent":
            errors.append("schema nested enum drift: delivery.state must equal 'unsent'")
        deadline_required = schema_at("$defs", "cloud_execution", "required")
        if not isinstance(deadline_required, list) or "deadline_evidence" not in deadline_required:
            errors.append("schema cloud_execution must require deadline_evidence")
        if schema_at("$defs", "deadline_evidence", "properties", "deadline_ms", "const") != 1500:
            errors.append("schema deadline_evidence.deadline_ms must equal 1500")
        if schema_at(
            "$defs", "deadline_evidence", "properties", "observed_elapsed_ms", "exclusiveMinimum"
        ) != 1500:
            errors.append("schema deadline_evidence.observed_elapsed_ms must exceed 1500")
        if schema_at(
            "$defs",
            "deadline_evidence",
            "properties",
            "local_baseline_delivery_start_by_ms",
            "maximum",
        ) != 1500:
            errors.append(
                "schema deadline_evidence.local_baseline_delivery_start_by_ms maximum "
                "must equal 1500"
            )

    return errors


def _recompute_counts(corpus: dict[str, Any]) -> None:
    """Adjust dataset_counts to match fixtures after a targeted mutation."""
    fixtures = corpus.get("fixtures") or []
    by_kind = {k: 0 for k in KINDS}
    by_policy = {p: 0 for p in POLICIES}
    by_route = {r: 0 for r in ROUTES}
    by_cloud = {c: 0 for c in CLOUD_POLICIES}
    fallback_n = 0
    for fixture in fixtures:
        if not isinstance(fixture, dict):
            continue
        k = fixture.get("kind")
        if isinstance(k, str) and k in by_kind:
            by_kind[k] += 1
        p = fixture.get("policy")
        if isinstance(p, str) and p in by_policy:
            by_policy[p] += 1
        r = fixture.get("route")
        if isinstance(r, str) and r in by_route:
            by_route[r] += 1
        cloud = fixture.get("cloud") if isinstance(fixture.get("cloud"), dict) else {}
        pol = cloud.get("request_policy")
        if isinstance(pol, str) and pol in by_cloud:
            by_cloud[pol] += 1
        if fixture.get("fallback") is not None:
            fallback_n += 1
    counts = corpus.setdefault("dataset_counts", {})
    counts["fixtures_total"] = len(fixtures)
    counts["by_kind"] = by_kind
    counts["by_policy"] = by_policy
    counts["by_route"] = by_route
    counts["by_cloud_request"] = by_cloud
    counts["fallback_fixtures"] = fallback_n


def _expect_diagnostic(
    failures: list[str],
    name: str,
    corpus: dict[str, Any],
    schema: dict[str, Any],
    needles: list[str],
) -> None:
    try:
        errs = validate_package(corpus, schema)
    except Exception as exc:  # noqa: BLE001
        failures.append(f"mutation {name}: validate_package crashed: {exc}")
        return
    if not errs:
        failures.append(f"mutation {name}: expected failure but package passed")
        return
    blob = "\n".join(errs)
    if not any(n in blob for n in needles):
        failures.append(
            f"mutation {name}: missing intended diagnostic {needles!r}; got first={errs[0]!r}"
        )


def run_mutations(
    corpus: dict[str, Any], schema: dict[str, Any]
) -> tuple[list[str], list[str]]:
    failures: list[str] = []
    mutation_names: list[str] = []

    def mut(name: str, needles: list[str], fn) -> None:
        mutation_names.append(name)
        clone = copy.deepcopy(corpus)
        try:
            fn(clone)
        except Exception as exc:  # noqa: BLE001
            failures.append(f"mutation {name} crashed before validate: {exc}")
            return
        _expect_diagnostic(failures, name, clone, schema, needles)

    def first_fixture(c: dict[str, Any]) -> dict[str, Any]:
        return c["fixtures"][0]

    def find(c: dict[str, Any], pred) -> dict[str, Any]:
        for fixture in c["fixtures"]:
            if pred(fixture):
                return fixture
        raise AssertionError("fixture not found for mutation")

    # 1 duplicate id
    mut(
        "duplicate_id",
        ["duplicate fixture IDs"],
        lambda c: (
            c["fixtures"].__setitem__(
                1, {**c["fixtures"][1], "id": c["fixtures"][0]["id"]}
            )
            or _recompute_counts(c)
        ),
    )

    # 2 count mismatch
    mut(
        "count_mismatch",
        ["dataset_counts.fixtures_total"],
        lambda c: c["dataset_counts"].__setitem__("fixtures_total", 0),
    )

    # 3 protected absent
    def drop_protected(c: dict[str, Any]) -> None:
        f = find(c, lambda x: x.get("protected_tokens"))
        f["expected_final"] = "x"
        f["local_baseline"] = "x"
        # keep route consistent if deterministic_local
        if f.get("route") == "deterministic_local":
            pass
        _recompute_counts(c)

    mut("protected_absent", ["protected token"], drop_protected)

    # 4 forbidden equals expected
    def forbid_eq(c: dict[str, Any]) -> None:
        f = find(c, lambda x: isinstance(x.get("forbidden_outcomes"), list))
        f["forbidden_outcomes"] = [f["expected_final"]]

    mut("forbidden_equals_expected", ["forbidden outcome accidentally equals"], forbid_eq)

    # 5 fallback mismatch
    def fb_mismatch(c: dict[str, Any]) -> None:
        f = find(c, lambda x: x.get("fallback") is not None)
        f["expected_final"] = f["expected_final"] + " CHANGED"
        if f.get("route") == "deterministic_local":
            f["local_baseline"] = f["expected_final"]
        f["fallback"]["result"] = f["fallback"]["result"]  # leave result old

    mut("fallback_mismatch", ["expected_final must equal fallback.result"], fb_mismatch)

    # 6 natural structural label (inline form)
    def natural_label(c: dict[str, Any]) -> None:
        f = find(c, lambda x: x.get("policy") == "natural")
        f["expected_final"] = "Goal: Do a thing."
        f["local_baseline"] = "Goal: Do a thing."

    mut("natural_structural_label", ["Natural expected_final contains structural labels"], natural_label)

    def lowercase_natural_label(c: dict[str, Any]) -> None:
        f = find(c, lambda x: x.get("policy") == "natural")
        f["expected_final"] = "goal: Please let me know if that timeline works."
        f["local_baseline"] = f["expected_final"]

    mut(
        "lowercase_natural_structural_label",
        ["non-canonical structural label header 'goal'"],
        lowercase_natural_label,
    )

    def mixed_case_natural_label(c: dict[str, Any]) -> None:
        f = find(c, lambda x: x.get("policy") == "natural")
        f["expected_final"] = "gOaL: Please let me know if that timeline works."
        f["local_baseline"] = f["expected_final"]

    mut(
        "mixed_case_natural_structural_label",
        ["non-canonical structural label header 'gOaL'"],
        mixed_case_natural_label,
    )

    # 7 unknown policy
    def unknown_policy(c: dict[str, Any]) -> None:
        first_fixture(c)["policy"] = "smart"
        _recompute_counts(c)

    mut("unknown_policy", ["unknown policy"], unknown_policy)

    # 8 cloud required empty baseline
    def cloud_req_empty(c: dict[str, Any]) -> None:
        f = find(
            c,
            lambda x: isinstance(x.get("cloud"), dict)
            and x["cloud"].get("request_policy") == "required",
        )
        f["local_baseline"] = ""
        # also break non-empty check
        if f.get("cloud", {}).get("outcome") != "succeeded":
            f["expected_final"] = f.get("expected_final") or "x"

    mut(
        "cloud_required_empty_baseline",
        ["cloud-required fixture missing local_baseline"],
        cloud_req_empty,
    )

    # 9 coverage empty
    def coverage_empty(c: dict[str, Any]) -> None:
        req = c["coverage_requirements"][0]
        c["coverage_matrix"][req] = []

    mut("coverage_empty", ["must be a non-empty array"], coverage_empty)

    def coverage_missing_reverse_id(c: dict[str, Any]) -> None:
        c["coverage_matrix"]["symbol-quoted-meta"].pop()

    mut(
        "coverage_matrix_missing_reverse_id",
        ["must exactly equal fixtures carrying that tag"],
        coverage_missing_reverse_id,
    )

    mut(
        "version_wrong_primitive_type",
        ["version must be a non-empty string"],
        lambda c: c.__setitem__("version", {}),
    )

    mut(
        "invariant_summary_wrong_primitive_type",
        ["invariants[0].summary must be a non-empty string"],
        lambda c: c["invariants"][0].__setitem__("summary", {}),
    )

    # 10 non-closed label header
    def non_closed_label(c: dict[str, Any]) -> None:
        f = find(c, lambda x: x.get("policy") == "structured" and x.get("expected_final"))
        f["expected_final"] = "Edge Cases:\nInvented.\n\n" + f["expected_final"]
        # keep baseline equal if non-success
        if f.get("cloud", {}).get("outcome") != "succeeded":
            f["local_baseline"] = f["expected_final"]

    mut("non_closed_label_header", ["non-closed structural header"], non_closed_label)

    # 11 schema-forbidden top-level property
    mut(
        "forbidden_top_level_property",
        ["schema-forbidden property"],
        lambda c: c.__setitem__("unexpected_top", True),
    )

    # 12 schema-forbidden fixture property
    def forbidden_fixture_prop(c: dict[str, Any]) -> None:
        first_fixture(c)["unexpected_fixture_field"] = 1

    mut("forbidden_fixture_property", ["schema-forbidden property"], forbidden_fixture_prop)

    # 13 unknown fixture tag
    def unknown_tag(c: dict[str, Any]) -> None:
        first_fixture(c)["tags"] = list(first_fixture(c)["tags"]) + ["not-a-real-tag"]

    mut("unknown_fixture_tag", ["unknown fixture tag"], unknown_tag)

    # 14 deterministic_local with cloud allowed (counts adjusted)
    def local_with_cloud_allowed(c: dict[str, Any]) -> None:
        f = find(c, lambda x: x.get("route") == "deterministic_local")
        f["cloud"]["request_policy"] = "allowed"
        f["cloud"]["actual_requests"] = 0
        f["cloud"]["outcome"] = "skipped"
        _recompute_counts(c)

    mut(
        "deterministic_local_cloud_allowed",
        ["deterministic_local requires cloud.request_policy not_allowed"],
        local_with_cloud_allowed,
    )

    # 15 deterministic_local final != baseline
    def local_final_diff(c: dict[str, Any]) -> None:
        f = find(c, lambda x: x.get("route") == "deterministic_local")
        f["expected_final"] = f["local_baseline"] + " EXTRA"

    mut(
        "deterministic_local_final_differs",
        ["deterministic_local final Transcript must equal local_baseline"],
        local_final_diff,
    )

    # 16 exact agreement with only one source
    def exact_one_source(c: dict[str, Any]) -> None:
        f = find(c, lambda x: x.get("provider_state") == "exact_agreement")
        # drop second source
        f["sources"] = [f["sources"][0]]
        f["sources"][0]["primary"] = True
        _recompute_counts(c)

    mut(
        "exact_agreement_one_source",
        ["exact_agreement requires two available Source Transcripts"],
        exact_one_source,
    )

    def duplicate_provider_identity(c: dict[str, Any]) -> None:
        f = find(c, lambda x: len(x.get("sources", [])) == 2)
        f["sources"][1]["provider"] = f["sources"][0]["provider"]

    mut(
        "duplicate_provider_identity",
        ["two-source fixtures require distinct provider identities provider_a and provider_b"],
        duplicate_provider_identity,
    )

    # 17 undeclared output label (has Goal: but labels list empty)
    def undeclared_output_label(c: dict[str, Any]) -> None:
        f = find(
            c,
            lambda x: x.get("policy") == "structured"
            and x.get("cloud", {}).get("outcome") == "succeeded",
        )
        f["allowed_operations"]["labels"] = []
        f["label_evidence"] = []

    mut(
        "undeclared_output_label",
        ["output label", "not declared"],
        undeclared_output_label,
    )

    # 18 optional-cloud non-success final differs from baseline
    def optional_cloud_branch_ambiguous(c: dict[str, Any]) -> None:
        f = find(
            c,
            lambda x: x.get("route") == "local_with_optional_cloud"
            and x.get("cloud", {}).get("outcome") == "skipped",
        )
        f["expected_final"] = f["local_baseline"] + " EXTRA_BRANCH_TEXT"

    mut(
        "optional_cloud_nonsuccess_differs",
        ["non-success cloud branch requires expected_final == local_baseline"],
        optional_cloud_branch_ambiguous,
    )

    # 19 punctuation_only with identical raw text
    def punct_identical(c: dict[str, Any]) -> None:
        f = find(c, lambda x: x.get("provider_state") == "punctuation_only_agreement")
        f["sources"][1]["text"] = f["sources"][0]["text"]

    mut(
        "punctuation_identical_raw",
        ["punctuation_only_agreement must not have identical raw text"],
        punct_identical,
    )

    # 20 primary rank without primary flag
    def primary_rank_bad(c: dict[str, Any]) -> None:
        f = find(
            c,
            lambda x: x.get("source_selection", {}).get("reason") == "configured_primary_rank",
        )
        for source in f["sources"]:
            source["primary"] = False
        f["sources"][0]["primary"] = False
        # keep exactly one primary false — both false triggers exactly-one-primary too
        f["sources"][0]["primary"] = True
        f["source_selection"]["selected_provider"] = f["sources"][1]["provider"]
        # selected is not primary
        f["sources"][0]["primary"] = True
        f["sources"][1]["primary"] = False

    mut(
        "primary_rank_not_primary",
        ["configured_primary_rank requires selected source primary=true"],
        primary_rank_bad,
    )

    # 21 unhashable tag / malformed list where strings expected
    def unhashable_tag(c: dict[str, Any]) -> None:
        first_fixture(c)["tags"] = [["not", "a", "string"]]

    mut("unhashable_tag", ["tag items must be strings", "unhashable item rejected", "tags must be a string array"], unhashable_tag)

    # 22 timing phrase not in source
    def timing_bad_phrase(c: dict[str, Any]) -> None:
        f = find(c, lambda x: x.get("timing") is not None)
        f["timing"]["boundaries"][0]["left_phrase"] = "THIS_PHRASE_IS_NOT_IN_SOURCE"

    mut("timing_phrase_absent", ["left_phrase not found in source text"], timing_bad_phrase)

    # 24 literal identity drift
    def literal_drift(c: dict[str, Any]) -> None:
        f = find(c, lambda x: x.get("route") == "literal_identity")
        f["expected_final"] = f["expected_final"] + "."
        f["local_baseline"] = f["expected_final"]

    mut(
        "literal_identity_drift",
        ["literal_identity requires selected source == local_baseline == expected_final"],
        literal_drift,
    )

    # 25 empty forbidden string still must not equal expected (separate: allow empty in schema)
    # closed tags missing from coverage equality
    def tags_coverage_desync(c: dict[str, Any]) -> None:
        c["closed_tags"] = list(c["closed_tags"]) + ["orphan-tag-not-in-coverage"]
        c["dataset_counts"]["closed_tag_count"] = len(c["closed_tags"])

    mut(
        "closed_tags_coverage_desync",
        ["coverage_requirements must equal closed_tags exactly"],
        tags_coverage_desync,
    )

    def wrong_failure_trigger(c: dict[str, Any]) -> None:
        f = find(c, lambda x: x.get("cloud", {}).get("outcome") == "rejected_unsafe")
        f["fallback"]["trigger"] = "deadline_exceeded"
        _recompute_counts(c)

    mut(
        "cloud_failure_wrong_fallback_trigger",
        ["requires fallback.trigger 'unsafe_semantics'"],
        wrong_failure_trigger,
    )

    def missing_failure_fallback(c: dict[str, Any]) -> None:
        f = find(c, lambda x: x.get("cloud", {}).get("outcome") == "rejected_unsafe")
        f["fallback"] = None
        _recompute_counts(c)

    mut(
        "cloud_failure_missing_fallback",
        ["requires a failure fallback"],
        missing_failure_fallback,
    )

    def success_with_fallback(c: dict[str, Any]) -> None:
        f = find(c, lambda x: x.get("cloud", {}).get("outcome") == "succeeded")
        f["fallback"] = {
            "trigger": "provider_failure",
            "result": f["expected_final"],
        }
        _recompute_counts(c)

    mut(
        "cloud_success_with_fallback",
        ["succeeded cloud outcome must not have a fallback"],
        success_with_fallback,
    )

    def unhashable_coverage_requirement(c: dict[str, Any]) -> None:
        c["coverage_requirements"][0] = ["not", "a", "string"]
        _recompute_counts(c)

    mut(
        "unhashable_coverage_requirement",
        ["coverage_requirements items must be non-empty strings"],
        unhashable_coverage_requirement,
    )

    def invented_ordinary_wording(c: dict[str, Any]) -> None:
        f = find(c, lambda x: x.get("id") == "DPR-01")
        f["local_baseline"] = "Restart production now."
        f["expected_final"] = "Restart production now."

    mut(
        "invented_ordinary_wording",
        ["invented ordinary wording"],
        invented_ordinary_wording,
    )

    def dishonest_timing_tag(c: dict[str, Any]) -> None:
        f = find(c, lambda x: x.get("id") == "DPR-06")
        f["timing"]["certainty"] = "uncertain"

    mut(
        "dishonest_timing_clear_tag",
        ["timing-certainty coverage tag mismatch"],
        dishonest_timing_tag,
    )

    def dishonest_policy_tag(c: dict[str, Any]) -> None:
        f = find(c, lambda x: x.get("id") == "DPR-01")
        f["tags"].remove("policy-adaptive")
        f["tags"].append("policy-natural")

    mut(
        "dishonest_policy_tag",
        ["policy coverage tag mismatch"],
        dishonest_policy_tag,
    )

    def dishonest_route_tag(c: dict[str, Any]) -> None:
        f = find(c, lambda x: x.get("id") == "DPR-01")
        f["tags"].remove("route-local")
        f["tags"].append("route-literal")

    mut(
        "dishonest_route_tag",
        ["route coverage tag mismatch"],
        dishonest_route_tag,
    )

    def dishonest_numbered_tag(c: dict[str, Any]) -> None:
        f = find(c, lambda x: x.get("id") == "DPR-05")
        f["local_baseline"] = "Build, test, ship."
        f["expected_final"] = f["local_baseline"]

    mut(
        "dishonest_numbered_sequence_tag",
        ["numbered-sequence tag requires at least two actual numbered lines"],
        dishonest_numbered_tag,
    )

    def missing_label_conversion(c: dict[str, Any]) -> None:
        f = find(c, lambda x: x.get("id") == "DPR-43")
        f["allowed_operations"]["conversions"] = [
            conversion
            for conversion in f["allowed_operations"]["conversions"]
            if "goal" not in conversion.casefold()
        ]

    mut(
        "missing_label_conversion",
        ["declared label 'Goal' lacks explicit cue-to-label conversion"],
        missing_label_conversion,
    )

    def unrelated_goal_label_evidence(c: dict[str, Any]) -> None:
        f = find(c, lambda x: x.get("id") == "DPR-43")
        f["label_evidence"][0]["source_span_text"] = "notes keep the change small"

    mut(
        "unrelated_goal_label_evidence",
        ["source_span_text must equal the cue-bearing span for 'Goal'"],
        unrelated_goal_label_evidence,
    )

    def punctuation_semantic_negation(c: dict[str, Any]) -> None:
        f = find(c, lambda x: x.get("id") == "DPR-32")
        f["sources"][1]["text"] = "Hey, do not send the notes when you get a chance?"

    mut(
        "punctuation_only_semantic_negation",
        ["punctuation_only_agreement lexical/semantic mismatch"],
        punctuation_semantic_negation,
    )

    def punctuation_lexical_order(c: dict[str, Any]) -> None:
        f = find(c, lambda x: x.get("id") == "DPR-32")
        f["sources"][1]["text"] = "Hey, can you send the notes when a chance you get?"

    mut(
        "punctuation_only_lexical_order",
        ["punctuation_only_agreement lexical/semantic mismatch"],
        punctuation_lexical_order,
    )

    def arbitrary_conversion(c: dict[str, Any]) -> None:
        f = find(c, lambda x: x.get("id") == "DPR-01")
        f["local_baseline"] = "Restart production now."
        f["expected_final"] = f["local_baseline"]
        f["allowed_operations"]["conversions"].append("hey→Restart production now")

    mut(
        "arbitrary_conversion",
        ["unknown safe conversion", "invented ordinary wording"],
        arbitrary_conversion,
    )

    def safe_conversion_without_source_cue(c: dict[str, Any]) -> None:
        f = find(c, lambda x: x.get("id") == "DPR-01")
        f["allowed_operations"]["conversions"].append("period→.")

    mut(
        "safe_conversion_without_source_cue",
        ["safe conversion source cue missing"],
        safe_conversion_without_source_cue,
    )

    def transfer_protected_code(c: dict[str, Any]) -> None:
        source = find(c, lambda x: x.get("id") == "DPR-21")
        target = find(c, lambda x: x.get("id") == "DPR-01")
        source["tags"].remove("protected-code")
        target["tags"].append("protected-code")
        c["coverage_matrix"]["protected-code"] = ["DPR-01"]

    mut(
        "transfer_protected_code",
        ["protected-code tag lacks required typed/source/final witness"],
        transfer_protected_code,
    )

    def transfer_symbol_conversion(c: dict[str, Any]) -> None:
        source = find(c, lambda x: x.get("id") == "DPR-09")
        target = find(c, lambda x: x.get("id") == "DPR-01")
        source["tags"].remove("symbol-convert")
        target["tags"].append("symbol-convert")
        c["coverage_matrix"]["symbol-convert"] = ["DPR-01"]

    mut(
        "transfer_symbol_conversion",
        ["symbol-convert tag lacks required typed/source/final witness"],
        transfer_symbol_conversion,
    )

    mutation_names.append("schema_nested_enum_drift")
    schema_clone = copy.deepcopy(schema)
    try:
        schema_clone["$defs"]["fixture"]["properties"]["policy"]["enum"].append("smart")
    except (KeyError, TypeError, AttributeError) as exc:
        failures.append(f"mutation schema_nested_enum_drift crashed before validate: {exc}")
    else:
        _expect_diagnostic(
            failures,
            "schema_nested_enum_drift",
            copy.deepcopy(corpus),
            schema_clone,
            ["schema nested enum drift: fixture.policy"],
        )

    def reorder_local_wording(c: dict[str, Any]) -> None:
        fixture = find(c, lambda x: x.get("id") == "DPR-01")
        reordered = "Hey, can you send notes the when you get a chance?"
        fixture["local_baseline"] = reordered
        fixture["expected_final"] = reordered

    mut(
        "local_word_order_reversed",
        ["ordinary wording does not preserve source token order"],
        reorder_local_wording,
    )

    def reorder_successful_cloud_final(c: dict[str, Any]) -> None:
        fixture = find(c, lambda x: x.get("id") == "DPR-43")
        fixture["expected_final"] = fixture["expected_final"].replace(
            "Fix the flaky auth test.", "The flaky auth test fix."
        )

    mut(
        "successful_cloud_final_word_order_reversed",
        ["expected_final ordinary wording does not preserve source token order"],
        reorder_successful_cloud_final,
    )

    def missing_rejected_invalid_label_evidence(c: dict[str, Any]) -> None:
        fixture = find(c, lambda x: x.get("id") == "DPR-39")
        fixture["cloud"].pop("rejected_labels")

    mut(
        "missing_rejected_invalid_label_evidence",
        ["requires typed rejected candidate evidence", "requires cloud.rejected_labels"],
        missing_rejected_invalid_label_evidence,
    )

    mutation_names.append("schema_fixture_additional_properties")
    schema_additional_properties = copy.deepcopy(schema)
    try:
        schema_additional_properties["$defs"]["fixture"]["additionalProperties"] = True
    except (KeyError, TypeError) as exc:
        failures.append(
            f"mutation schema_fixture_additional_properties crashed before validate: {exc}"
        )
    else:
        _expect_diagnostic(
            failures,
            "schema_fixture_additional_properties",
            copy.deepcopy(corpus),
            schema_additional_properties,
            ["schema shape drift: fixture.additionalProperties must be false"],
        )

    mutation_names.append("schema_fixture_missing_required")
    schema_missing_required = copy.deepcopy(schema)
    try:
        schema_missing_required["$defs"]["fixture"]["required"].remove("rationale")
    except (KeyError, TypeError, ValueError) as exc:
        failures.append(
            f"mutation schema_fixture_missing_required crashed before validate: {exc}"
        )
    else:
        _expect_diagnostic(
            failures,
            "schema_fixture_missing_required",
            copy.deepcopy(corpus),
            schema_missing_required,
            ["schema shape drift: fixture.required must equal checker-required fields"],
        )

    mut(
        "unhashable_policy",
        ["unknown policy"],
        lambda c: find(c, lambda x: x.get("id") == "DPR-01").__setitem__("policy", {}),
    )

    mut(
        "unhashable_cloud_outcome",
        ["unknown cloud.outcome"],
        lambda c: find(c, lambda x: x.get("id") == "DPR-01")["cloud"].__setitem__(
            "outcome", {}
        ),
    )

    def invalid_deadline_evidence(c: dict[str, Any]) -> None:
        f = find(c, lambda x: x.get("id") == "DPR-42")
        f["cloud"]["deadline_evidence"]["observed_elapsed_ms"] = 1499

    mut(
        "deadline_evidence_below_threshold",
        ["observed_elapsed_ms must be an integer greater than 1500"],
        invalid_deadline_evidence,
    )

    def late_delivery_start(c: dict[str, Any]) -> None:
        f = find(c, lambda x: x.get("id") == "DPR-42")
        evidence = f["cloud"]["deadline_evidence"]
        evidence["local_baseline_delivery_start_by_ms"] = evidence[
            "observed_elapsed_ms"
        ]

    mut(
        "deadline_waits_for_late_cloud",
        [
            "local_baseline_delivery_start_by_ms must be within 0..1500",
            "Delivery must start with the local baseline by the deadline while cloud is still pending",
        ],
        late_delivery_start,
    )

    return failures, mutation_names


def main(argv: list[str]) -> int:
    del argv  # unused
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

    mutation_failures, mutation_names = run_mutations(corpus, schema)
    errors.extend(mutation_failures)

    if errors:
        print(f"FAIL: {len(errors)} error(s)", file=sys.stderr)
        for err in errors:
            print(f"  - {err}", file=sys.stderr)
        return 1

    counts = corpus["dataset_counts"]
    print("OK: developer-prompt-rendering-behavior corpus package")
    print(f"  version: {corpus.get('version')}")
    print(f"  fixtures: {counts['fixtures_total']}")
    print(f"  by_kind: {counts['by_kind']}")
    print(f"  by_policy: {counts['by_policy']}")
    print(f"  by_route: {counts['by_route']}")
    print(f"  by_cloud_request: {counts['by_cloud_request']}")
    print(f"  fallback_fixtures: {counts['fallback_fixtures']}")
    print(f"  coverage_requirements: {counts['coverage_requirement_count']}")
    print(f"  closed_tags: {counts['closed_tag_count']}")
    print(f"  closed_structured_labels: {len(corpus['closed_structured_labels'])}")
    print(f"  mutations: {len(mutation_names)} property-bound")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
