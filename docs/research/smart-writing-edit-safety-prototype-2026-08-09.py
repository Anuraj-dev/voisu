#!/usr/bin/env python3
"""Hermetic #100 architecture proof.

Formatting is a sealed local capability. Provider JSON contains localized grammar
patches only. Exit 0 iff the authoritative corpus schema, fixtures, #99 links,
and direct adversarial tests pass.
"""

from __future__ import annotations

import copy
import hashlib
import json
import re
import sys
from dataclasses import dataclass
from pathlib import Path
from types import MappingProxyType
from typing import Any, Callable, Mapping


HERE = Path(__file__).resolve().parent
SCHEMA_PATH = HERE / "smart-writing-edit-safety-schema-2026-08-09.json"
CORPUS_PATH = HERE / "smart-writing-edit-safety-corpus-2026-08-09.json"
BEHAVIOR_PATH = HERE / "smart-writing-behavior-corpus-2026-08-09.json"

MAX_FILE_BYTES = 524_288
MAX_JSON_DEPTH = 12
MAX_BASE_BYTES = 100_000
MAX_EDITS = 64
MAX_FIELD_BYTES = 512
MAX_DIAG_BYTES = 128
MAX_JSON_NODES = 20_000

RULE_IDS = (
    "G_THERE_IS_PLURAL_QUANTITY",
    "G_LETS_MEET_CONTRACTION",
    "G_DIDNT_APOSTROPHE",
)
ERROR_CODES = (
    "E_FORMATTING_TYPE",
    "E_FORMATTING_IDENTITY",
    "E_FORMATTING_DERIVATION",
    "E_MALFORMED",
    "E_OVERSIZE",
    "E_STALE_GRAMMAR",
    "E_UNSORTED",
    "E_SPAN_OUT_OF_BOUNDS",
    "E_SPAN_NOT_CHAR_BOUNDARY",
    "E_NOT_TOKEN_BOUNDARY",
    "E_ANCHOR_MISMATCH",
    "E_PROTECTED_SPAN",
    "E_UNKNOWN_RULE",
    "E_RULE_CONTEXT",
    "E_UNMAPPABLE",
    "E_OVERLAP",
)
APPROVED_99 = {
    "D_cmd": "D_cmd-A",
    "D1": "D1-B",
    "D3": "D3-B",
    "D4": "D4-A",
    "D5": "D5-A",
    "D10": "D10-B",
    "D14": "D14-A",
}

TOKEN_RE = re.compile(r"[^\W_]+(?:'[^\W_]+)*", re.UNICODE)
FINGERPRINT_RE = re.compile(r"^sha256:[0-9a-f]{64}$")
SECRET_RE = re.compile(
    r"(?i)(?:gsk_[A-Za-z0-9_-]{6,}|bearer\s+\S+|api[_-]?key\s*[:=]\s*\S+)"
)
PROMPT_MARKERS = (
    "ignore previous instructions",
    "system prompt",
    "developer message",
    "you are chatgpt",
)
COMMAND_PHRASES = (
    "period",
    "comma",
    "question mark",
    "exclamation point",
    "new line",
    "new paragraph",
    "quote",
    "unquote",
    "number one",
    "number two",
    "number three",
)
PLURAL_QUANTITIES = {
    "two",
    "three",
    "four",
    "five",
    "six",
    "seven",
    "eight",
    "nine",
    "ten",
    "eleven",
    "twelve",
}
SAFE_COUNT_NOUNS = {"issues"}
SAFE_NUMERIC_QUANTITIES = {str(value) for value in range(2, 13)}
FORMATTER_CONTRACT_ID = "voisu-local-formatting-proof-v1:#99-approved"

# These are deterministic formatting-oracle examples locked by #99. They are
# outside the grammar gate. No provider field selects or supplies their output.
FORMAT_OVERRIDES = {
    "hey can you send the notes when you get a chance":
        "Hey, can you send the notes when you get a chance?",
    "ship it command exclamation point": "Ship it!",
    "stop command period command new line next item": "Stop.\nNext item",
    "first thought command new line second thought": "First thought\nSecond thought",
    "intro command new paragraph body text": "Intro.\n\nBody text.",
    "command number one apples command number two oranges command number three pears":
        "1. Apples\n2. Oranges\n3. Pears",
    "buy milk eggs bread": "Buy:\n- milk\n- eggs\n- bread",
    "she said command quote we ship friday command unquote and hung up":
        'She said, "we ship friday," and hung up.',
    "use the exact error command quote connection refused command unquote in the ticket":
        'Use the exact error "connection refused" in the ticket.',
}


def fingerprint(text: str) -> str:
    return "sha256:" + hashlib.sha256(text.encode("utf-8")).hexdigest()


def utf8_len(text: str) -> int:
    return len(text.encode("utf-8"))


def json_depth(value: Any, depth: int = 0) -> int:
    if isinstance(value, dict):
        return max([depth] + [json_depth(v, depth + 1) for v in value.values()])
    if isinstance(value, list):
        return max([depth] + [json_depth(v, depth + 1) for v in value])
    return depth


def scalar_strings_valid(value: Any) -> bool:
    """Reject lone surrogates, cycles, and unreasonable in-memory JSON shapes."""
    stack = [value]
    containers: set[int] = set()
    nodes = 0
    while stack:
        current = stack.pop()
        nodes += 1
        if nodes > MAX_JSON_NODES:
            return False
        if isinstance(current, str):
            try:
                current.encode("utf-8", errors="strict")
            except UnicodeEncodeError:
                return False
        elif isinstance(current, dict):
            identity = id(current)
            if identity in containers:
                return False
            containers.add(identity)
            stack.extend(current.keys())
            stack.extend(current.values())
        elif isinstance(current, list):
            identity = id(current)
            if identity in containers:
                return False
            containers.add(identity)
            stack.extend(current)
    return True


def decode_json_bounded(text: str, label: str) -> Any:
    size = utf8_len(text)
    if size > MAX_FILE_BYTES:
        raise ValueError(f"{label}: {size} bytes exceeds {MAX_FILE_BYTES}")
    try:
        value = json.loads(text)
    except (RecursionError, MemoryError) as exc:
        raise ValueError(f"{label}: JSON decoder resource bound exceeded") from exc
    try:
        depth = json_depth(value)
    except (RecursionError, MemoryError) as exc:
        raise ValueError(f"{label}: JSON depth resource bound exceeded") from exc
    if depth > MAX_JSON_DEPTH:
        raise ValueError(f"{label}: depth {depth} exceeds {MAX_JSON_DEPTH}")
    if not scalar_strings_valid(value):
        raise ValueError(f"{label}: non-scalar string or invalid JSON object graph")
    return value


def load_json_bounded(path: Path) -> Any:
    size = path.stat().st_size
    if size > MAX_FILE_BYTES:
        raise ValueError(f"{path.name}: {size} bytes exceeds {MAX_FILE_BYTES}")
    return decode_json_bounded(path.read_text(encoding="utf-8"), path.name)


def char_to_utf8(text: str, char_offset: int) -> int:
    return utf8_len(text[:char_offset])


def utf8_to_char(text: str, byte_offset: int) -> int | None:
    if byte_offset < 0 or byte_offset > utf8_len(text):
        return None
    used = 0
    for index, char in enumerate(text):
        if used == byte_offset:
            return index
        used += utf8_len(char)
        if used > byte_offset:
            return None
    return len(text) if used == byte_offset else None


def slice_utf8(text: str, start: int, end: int) -> str | None:
    start_char = utf8_to_char(text, start)
    end_char = utf8_to_char(text, end)
    if start_char is None or end_char is None:
        return None
    return text[start_char:end_char]


def token_spans(text: str) -> list[tuple[int, int, str]]:
    return [
        (char_to_utf8(text, match.start()), char_to_utf8(text, match.end()), match.group())
        for match in TOKEN_RE.finditer(text)
    ]


def exactly_one_token(text: str, start: int, end: int) -> bool:
    return any(start == left and end == right for left, right, _ in token_spans(text))


def overlaps(left: tuple[int, int], right: tuple[int, int]) -> bool:
    return left[0] < right[1] and right[0] < left[1]


def clamp_diagnostic(value: Any) -> str:
    text = SECRET_RE.sub("[REDACTED]", str(value))
    raw = text.encode("utf-8")
    if len(raw) <= MAX_DIAG_BYTES:
        return text
    budget = MAX_DIAG_BYTES - len("…".encode("utf-8"))
    return raw[:budget].decode("utf-8", "ignore") + "…"


@dataclass(frozen=True)
class ValidatedTranscript:
    text: str
    version: str
    fingerprint: str

    @classmethod
    def from_json(cls, value: Mapping[str, Any]) -> "ValidatedTranscript":
        text = value["text"]
        transcript = cls(text=text, version=value["version"], fingerprint=value["fingerprint"])
        if transcript.fingerprint != fingerprint(text):
            raise ValueError("base fingerprint mismatch")
        if utf8_len(text) > MAX_BASE_BYTES:
            raise ValueError("base exceeds research bound")
        return transcript


@dataclass(frozen=True)
class SourceAnchor:
    rendered_start: int
    rendered_end: int


_FORMATTER_SEAL = object()


@dataclass(frozen=True, slots=True, init=False)
class FormattingBaseline:
    """Typed formatting capability; provider JSON cannot construct this value."""

    base_version: str
    base_fingerprint: str
    rendered: str
    anchors: Mapping[tuple[int, int], SourceAnchor]
    protected_source_ranges: tuple[tuple[int, int], ...]
    formatter_contract: str
    derivation_fingerprint: str

    def __init__(
        self,
        seal: object,
        *,
        base_version: str,
        base_fingerprint: str,
        rendered: str,
        anchors: Mapping[tuple[int, int], SourceAnchor],
        protected_source_ranges: tuple[tuple[int, int], ...],
    ) -> None:
        if seal is not _FORMATTER_SEAL:
            raise TypeError("FormattingBaseline is formatter-owned")
        object.__setattr__(self, "base_version", base_version)
        object.__setattr__(self, "base_fingerprint", base_fingerprint)
        object.__setattr__(self, "rendered", rendered)
        object.__setattr__(self, "anchors", MappingProxyType(dict(anchors)))
        object.__setattr__(self, "protected_source_ranges", tuple(protected_source_ranges))
        object.__setattr__(self, "formatter_contract", FORMATTER_CONTRACT_ID)
        object.__setattr__(
            self,
            "derivation_fingerprint",
            _baseline_derivation(
                base_version,
                base_fingerprint,
                rendered,
                anchors,
                protected_source_ranges,
                FORMATTER_CONTRACT_ID,
            ),
        )


def _plain_format(text: str) -> str:
    if not text:
        return text
    chars = list(text)
    for index, char in enumerate(chars):
        if char.isalpha():
            chars[index] = char.upper()
            break
    rendered = "".join(chars)
    if rendered[-1] not in ".!?\n":
        rendered += "."
    return rendered


def _source_anchors(base: str, rendered: str) -> dict[tuple[int, int], SourceAnchor]:
    """Greedy anchors emitted by this proof formatter, never inferred by the gate."""
    base_tokens = token_spans(base)
    rendered_tokens = token_spans(rendered)
    anchors: dict[tuple[int, int], SourceAnchor] = {}
    rendered_index = 0
    for start, end, token in base_tokens:
        while rendered_index < len(rendered_tokens):
            r_start, r_end, r_token = rendered_tokens[rendered_index]
            rendered_index += 1
            if token.casefold() == r_token.casefold():
                anchors[(start, end)] = SourceAnchor(r_start, r_end)
                break
    return anchors


def _normalize_ranges(ranges: list[tuple[int, int]]) -> tuple[tuple[int, int], ...]:
    merged: list[tuple[int, int]] = []
    for start, end in sorted(set(ranges)):
        if start >= end:
            continue
        if merged and start <= merged[-1][1]:
            merged[-1] = (merged[-1][0], max(merged[-1][1], end))
        else:
            merged.append((start, end))
    return tuple(merged)


def _inside_word_apostrophe(text: str, char_index: int) -> bool:
    if char_index <= 0 or char_index + 1 >= len(text):
        return False
    left = text[char_index - 1]
    right = text[char_index + 1]
    return (left.isalnum() or left == "_") and (right.isalnum() or right == "_")


def _same_quote_ranges(
    text: str,
    delimiter: str,
    *,
    ignore_word_apostrophes: bool,
) -> tuple[list[tuple[int, int]], bool]:
    positions: list[int] = []
    search_from = 0
    while True:
        index = text.find(delimiter, search_from)
        if index < 0:
            break
        if not (ignore_word_apostrophes and _inside_word_apostrophe(text, index)):
            positions.append(index)
        search_from = index + len(delimiter)
    ranges = [
        (char_to_utf8(text, positions[index]), char_to_utf8(text, positions[index + 1] + len(delimiter)))
        for index in range(0, len(positions) - 1, 2)
    ]
    return ranges, len(positions) % 2 == 1


def _curly_quote_ranges(
    text: str,
    opening: str,
    closing: str,
    *,
    closing_can_be_apostrophe: bool,
) -> tuple[list[tuple[int, int]], bool]:
    ranges: list[tuple[int, int]] = []
    pending: int | None = None
    ambiguous = False
    for index, char in enumerate(text):
        if char == opening:
            if pending is not None:
                ambiguous = True
            else:
                pending = index
        elif char == closing:
            if closing_can_be_apostrophe and _inside_word_apostrophe(text, index):
                continue
            if pending is None:
                ambiguous = True
            else:
                ranges.append(
                    (char_to_utf8(text, pending), char_to_utf8(text, index + len(closing)))
                )
                pending = None
    return ranges, ambiguous or pending is not None


def _quotation_source_ranges(text: str) -> tuple[tuple[int, int], ...]:
    ranges: list[tuple[int, int]] = []
    ambiguous = False
    for delimiter, word_apostrophe in (("\"", False), ("'", True)):
        found, unmatched = _same_quote_ranges(
            text,
            delimiter,
            ignore_word_apostrophes=word_apostrophe,
        )
        ranges.extend(found)
        ambiguous = ambiguous or unmatched
    for opening, closing, apostrophe in (("“", "”", False), ("‘", "’", True)):
        found, unmatched = _curly_quote_ranges(
            text,
            opening,
            closing,
            closing_can_be_apostrophe=apostrophe,
        )
        ranges.extend(found)
        ambiguous = ambiguous or unmatched
    if ambiguous and text:
        ranges.append((0, utf8_len(text)))
    return _normalize_ranges(ranges)


def _formatting_protected_source_ranges(text: str) -> tuple[tuple[int, int], ...]:
    """Full quote/code ranges emitted as formatter-owned source metadata."""
    ranges = list(_quotation_source_ranges(text))
    patterns = (
        (r"```[\s\S]*?```", 0),
        (r"`[^`]*`", 0),
        (r"\bcommand\s+quote\b[\s\S]*?\bcommand\s+unquote\b", re.IGNORECASE),
    )
    for pattern, flags in patterns:
        for match in re.finditer(pattern, text, flags):
            ranges.append((char_to_utf8(text, match.start()), char_to_utf8(text, match.end())))
    return _normalize_ranges(ranges)


def _baseline_derivation(
    base_version: str,
    base_fingerprint: str,
    rendered: str,
    anchors: Mapping[tuple[int, int], SourceAnchor],
    protected_source_ranges: tuple[tuple[int, int], ...],
    formatter_contract: str,
) -> str:
    canonical = {
        "formatter_contract": formatter_contract,
        "base_version": base_version,
        "base_fingerprint": base_fingerprint,
        "rendered": rendered,
        "anchors": [
            [start, end, anchor.rendered_start, anchor.rendered_end]
            for (start, end), anchor in sorted(anchors.items())
        ],
        "protected_source_ranges": [list(span) for span in protected_source_ranges],
    }
    encoded = json.dumps(
        canonical,
        ensure_ascii=False,
        sort_keys=True,
        separators=(",", ":"),
    )
    return fingerprint(encoded)


def _baseline_derivation_valid(
    base: ValidatedTranscript,
    baseline: FormattingBaseline,
) -> bool:
    try:
        if baseline.formatter_contract != FORMATTER_CONTRACT_ID:
            return False
        expected_protected = _formatting_protected_source_ranges(base.text)
        if baseline.protected_source_ranges != expected_protected:
            return False
        base_bytes = utf8_len(base.text)
        rendered_bytes = utf8_len(baseline.rendered)
        if _normalize_ranges(list(baseline.protected_source_ranges)) != baseline.protected_source_ranges:
            return False
        for start, end in baseline.protected_source_ranges:
            if (
                start < 0
                or start >= end
                or end > base_bytes
                or utf8_to_char(base.text, start) is None
                or utf8_to_char(base.text, end) is None
            ):
                return False
        for (start, end), anchor in baseline.anchors.items():
            if (
                start < 0
                or start >= end
                or end > base_bytes
                or anchor.rendered_start < 0
                or anchor.rendered_start >= anchor.rendered_end
                or anchor.rendered_end > rendered_bytes
                or utf8_to_char(base.text, start) is None
                or utf8_to_char(base.text, end) is None
                or utf8_to_char(baseline.rendered, anchor.rendered_start) is None
                or utf8_to_char(baseline.rendered, anchor.rendered_end) is None
            ):
                return False
        expected = _baseline_derivation(
            baseline.base_version,
            baseline.base_fingerprint,
            baseline.rendered,
            baseline.anchors,
            baseline.protected_source_ranges,
            baseline.formatter_contract,
        )
        return baseline.derivation_fingerprint == expected
    except (AttributeError, TypeError, ValueError, UnicodeError):
        return False


def format_locally(base: ValidatedTranscript) -> FormattingBaseline:
    rendered = FORMAT_OVERRIDES.get(base.text, _plain_format(base.text))
    anchors = _source_anchors(base.text, rendered)
    protected_source_ranges = _formatting_protected_source_ranges(base.text)
    return FormattingBaseline(
        _FORMATTER_SEAL,
        base_version=base.version,
        base_fingerprint=base.fingerprint,
        rendered=rendered,
        anchors=anchors,
        protected_source_ranges=protected_source_ranges,
    )


@dataclass(frozen=True)
class Diagnostic:
    code: str
    message: str
    edit_id: str = ""

    def json(self) -> dict[str, str]:
        return {
            "code": self.code,
            "message": clamp_diagnostic(self.message),
            "edit_id": clamp_diagnostic(self.edit_id),
        }


def result_for(
    base: ValidatedTranscript,
    rendered: str,
    grammar_applied: bool,
    diagnostics: list[Diagnostic],
) -> dict[str, Any]:
    formatting_applied = rendered != base.text
    if grammar_applied and formatting_applied:
        decision = "both"
    elif grammar_applied:
        decision = "grammar_only"
    elif formatting_applied:
        decision = "formatting_only"
    else:
        decision = "unchanged"
    return {
        "decision": decision,
        "rendered": rendered,
        "error_codes": [diag.code for diag in diagnostics],
        "diagnostics": [diag.json() for diag in diagnostics],
    }


def _candidate_container(candidate: Any) -> tuple[list[Any] | None, list[Diagnostic]]:
    if not isinstance(candidate, dict):
        return None, [Diagnostic("E_MALFORMED", "grammar candidate must be an object")]
    if set(candidate) != {"base_version", "base_fingerprint", "edits"}:
        return None, [Diagnostic("E_MALFORMED", "grammar candidate keys are not exact")]
    if not isinstance(candidate["base_version"], str) or not candidate["base_version"]:
        return None, [Diagnostic("E_MALFORMED", "base_version must be a nonempty string")]
    if not isinstance(candidate["base_fingerprint"], str) or not FINGERPRINT_RE.fullmatch(
        candidate["base_fingerprint"]
    ):
        return None, [Diagnostic("E_MALFORMED", "base_fingerprint shape is invalid")]
    edits = candidate["edits"]
    if not isinstance(edits, list):
        return None, [Diagnostic("E_MALFORMED", "edits must be an array")]
    if len(edits) > MAX_EDITS:
        return None, [Diagnostic("E_OVERSIZE", f"edit count {len(edits)} exceeds {MAX_EDITS}")]
    return edits, []


def _edit_shapes(edits: list[Any]) -> tuple[list[dict[str, Any]] | None, list[Diagnostic]]:
    exact = {"id", "rule_id", "start_utf8", "end_utf8", "before", "after"}
    for index, edit in enumerate(edits):
        if not isinstance(edit, dict) or set(edit) != exact:
            return None, [Diagnostic("E_MALFORMED", f"edit {index} keys are not exact")]
        if not all(isinstance(edit[key], str) for key in ("id", "rule_id", "before", "after")):
            return None, [Diagnostic("E_MALFORMED", f"edit {index} string field invalid")]
        if not edit["id"] or not edit["rule_id"]:
            return None, [Diagnostic("E_MALFORMED", f"edit {index} id/rule_id empty")]
        for key in ("start_utf8", "end_utf8"):
            if isinstance(edit[key], bool) or not isinstance(edit[key], int) or edit[key] < 0:
                return None, [Diagnostic("E_MALFORMED", f"edit {index} {key} invalid")]
        if any(utf8_len(edit[key]) > MAX_FIELD_BYTES for key in ("id", "rule_id", "before", "after")):
            return None, [Diagnostic("E_OVERSIZE", f"edit {index} string exceeds bound")]
    return edits, []  # type: ignore[return-value]


def _phrase_spans(text: str, phrase: str) -> list[tuple[int, int]]:
    pattern = re.compile(r"(?<![^\W_])" + re.escape(phrase) + r"(?![^\W_])", re.IGNORECASE)
    return [
        (char_to_utf8(text, match.start()), char_to_utf8(text, match.end()))
        for match in pattern.finditer(text)
    ]


def protected_spans(
    text: str,
    protected_names: list[str],
    dictionary_terms: list[str],
) -> list[tuple[int, int]]:
    spans: list[tuple[int, int]] = []
    for phrase in protected_names + dictionary_terms:
        spans.extend(_phrase_spans(text, phrase))
    patterns = (
        r"\b[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}\b",
        r"https?://[^\s]+",
        r"(?<!\w)/(?:[A-Za-z0-9_.-]+/)*[A-Za-z0-9_.-]+",
        r"\b(?:\d{1,4}(?:[-/:.]\d{1,4})+|\d+)\b",
        r"\b(?:not|no|never)\b",
        r"\bdo\s+not\b",
        r"\bcommand\s+(?:" + "|".join(re.escape(p) for p in COMMAND_PHRASES) + r")\b",
    )
    for pattern in patterns:
        for match in re.finditer(pattern, text, re.IGNORECASE):
            spans.append((char_to_utf8(text, match.start()), char_to_utf8(text, match.end())))
    # Identifier casing is semantic. IGNORECASE here would make the camelCase
    # branch match every ordinary word.
    identifier_pattern = (
        r"\b(?:[A-Za-z]+_[A-Za-z0-9_]+|[A-Za-z]*[a-z][A-Z][A-Za-z0-9]*|"
        r"[A-Z]{2,}[A-Z0-9]*|[A-Za-z0-9]+(?:[-_.][A-Za-z0-9]+)+)\b"
    )
    for match in re.finditer(identifier_pattern, text):
        spans.append((char_to_utf8(text, match.start()), char_to_utf8(text, match.end())))
    if any(marker in text.casefold() for marker in PROMPT_MARKERS):
        spans.append((0, utf8_len(text)))
    return sorted(set(spans))


def _token_index(base: str, start: int, end: int) -> tuple[list[tuple[int, int, str]], int] | None:
    tokens = token_spans(base)
    for index, (left, right, _) in enumerate(tokens):
        if (left, right) == (start, end):
            return tokens, index
    return None


def _horizontal_gap(base: str, left_end: int, right_start: int) -> bool:
    gap = slice_utf8(base, left_end, right_start)
    return gap is not None and re.fullmatch(r"[ \t]+", gap) is not None


def rule_there_is(base: str, edit: Mapping[str, Any]) -> bool:
    if edit["before"] != "is" or edit["after"] != "are":
        return False
    located = _token_index(base, edit["start_utf8"], edit["end_utf8"])
    if located is None:
        return False
    tokens, index = located
    if index == 0 or index + 2 >= len(tokens):
        return False
    previous = tokens[index - 1][2].casefold()
    following = tokens[index + 1][2].casefold()
    noun = tokens[index + 2][2].casefold()
    exact_gaps = (
        _horizontal_gap(base, tokens[index - 1][1], tokens[index][0])
        and _horizontal_gap(base, tokens[index][1], tokens[index + 1][0])
        and _horizontal_gap(base, tokens[index + 1][1], tokens[index + 2][0])
    )
    safe_number = following in SAFE_NUMERIC_QUANTITIES
    return previous == "there" and exact_gaps and noun in SAFE_COUNT_NOUNS and (
        following in PLURAL_QUANTITIES or safe_number
    )


def rule_lets_meet(base: str, edit: Mapping[str, Any]) -> bool:
    if edit["before"] != "lets" or edit["after"] != "let's":
        return False
    located = _token_index(base, edit["start_utf8"], edit["end_utf8"])
    if located is None:
        return False
    tokens, index = located
    prefix = slice_utf8(base, 0, edit["start_utf8"])
    return (
        index == 0
        and prefix is not None
        and re.fullmatch(r"[ \t]*", prefix) is not None
        and len(tokens) > 1
        and tokens[1][2].casefold() == "meet"
        and _horizontal_gap(base, tokens[0][1], tokens[1][0])
    )


def rule_didnt(base: str, edit: Mapping[str, Any]) -> bool:
    return (
        edit["before"] == "didnt"
        and edit["after"] == "didn't"
        and _token_index(base, edit["start_utf8"], edit["end_utf8"]) is not None
    )


RULES: dict[str, Callable[[str, Mapping[str, Any]], bool]] = {
    "G_THERE_IS_PLURAL_QUANTITY": rule_there_is,
    "G_LETS_MEET_CONTRACTION": rule_lets_meet,
    "G_DIDNT_APOSTROPHE": rule_didnt,
}


def _preserve_formatter_case(existing: str, replacement: str) -> str:
    if existing and replacement and existing[0].isupper() and replacement[0].islower():
        return replacement[0].upper() + replacement[1:]
    return replacement


def validate_grammar(
    base: ValidatedTranscript,
    baseline: Any,
    candidate: Any,
    *,
    protected_names: list[str] | None = None,
    dictionary_terms: list[str] | None = None,
) -> dict[str, Any]:
    if not isinstance(baseline, FormattingBaseline):
        return result_for(
            base,
            base.text,
            False,
            [Diagnostic("E_FORMATTING_TYPE", "baseline is not a formatter capability")],
        )
    if (
        baseline.base_version != base.version
        or baseline.base_fingerprint != base.fingerprint
    ):
        return result_for(
            base,
            base.text,
            False,
            [Diagnostic("E_FORMATTING_IDENTITY", "baseline identity differs from base")],
        )
    if not _baseline_derivation_valid(base, baseline):
        return result_for(
            base,
            base.text,
            False,
            [Diagnostic("E_FORMATTING_DERIVATION", "baseline capability derivation invalid")],
        )

    if not scalar_strings_valid(candidate):
        return result_for(
            base,
            baseline.rendered,
            False,
            [Diagnostic("E_MALFORMED", "grammar candidate contains invalid text or shape")],
        )

    # Container/envelope first, then freshness, then individual edit shapes.
    raw_edits, diagnostics = _candidate_container(candidate)
    if diagnostics or raw_edits is None:
        return result_for(base, baseline.rendered, False, diagnostics)
    if (
        candidate["base_version"] != base.version
        or candidate["base_fingerprint"] != base.fingerprint
    ):
        return result_for(
            base,
            baseline.rendered,
            False,
            [Diagnostic("E_STALE_GRAMMAR", "candidate identity differs from base")],
        )
    edits, diagnostics = _edit_shapes(raw_edits)
    if diagnostics or edits is None:
        return result_for(base, baseline.rendered, False, diagnostics)
    order = [(edit["start_utf8"], edit["end_utf8"]) for edit in edits]
    if order != sorted(order):
        return result_for(
            base,
            baseline.rendered,
            False,
            [Diagnostic("E_UNSORTED", "grammar edits are not source ordered")],
        )

    protected = list(baseline.protected_source_ranges) + protected_spans(
        base.text,
        protected_names or [],
        dictionary_terms or [],
    )
    accepted: list[tuple[dict[str, Any], SourceAnchor]] = []
    diagnostics = []

    def add(code: str, message: str, edit_id: str) -> None:
        if code not in [diag.code for diag in diagnostics]:
            diagnostics.append(Diagnostic(code, message, edit_id))

    base_bytes = utf8_len(base.text)
    for edit in edits:
        edit_id = edit["id"]
        start = edit["start_utf8"]
        end = edit["end_utf8"]
        if start > end or end > base_bytes:
            add("E_SPAN_OUT_OF_BOUNDS", f"range {start}:{end} outside base", edit_id)
            continue
        if utf8_to_char(base.text, start) is None or utf8_to_char(base.text, end) is None:
            add("E_SPAN_NOT_CHAR_BOUNDARY", f"range {start}:{end} cuts UTF-8", edit_id)
            continue

        token_ok = start < end and exactly_one_token(base.text, start, end)
        if not token_ok:
            add("E_NOT_TOKEN_BOUNDARY", "grammar must replace exactly one token", edit_id)
        anchored = slice_utf8(base.text, start, end) == edit["before"]
        if not anchored:
            add("E_ANCHOR_MISMATCH", "before does not match base range", edit_id)
        if start < end and any(overlaps((start, end), span) for span in protected):
            add("E_PROTECTED_SPAN", "edit intersects protected text", edit_id)

        rule = RULES.get(edit["rule_id"])
        rule_ok = rule is not None and rule(base.text, edit)
        if rule is None:
            add("E_UNKNOWN_RULE", f"unknown rule {edit['rule_id']}", edit_id)
        elif not rule_ok:
            add("E_RULE_CONTEXT", f"rule context failed for {edit['rule_id']}", edit_id)

        anchor = baseline.anchors.get((start, end))
        if token_ok and anchored and rule_ok and anchor is None:
            add("E_UNMAPPABLE", "formatter emitted no source anchor", edit_id)
        if token_ok and anchored and rule_ok and anchor is not None:
            mapped = slice_utf8(baseline.rendered, anchor.rendered_start, anchor.rendered_end)
            if mapped is None or mapped.casefold() != edit["before"].casefold():
                add("E_UNMAPPABLE", "formatter anchor no longer names the base token", edit_id)
            else:
                accepted.append((edit, anchor))

    for index, left in enumerate(edits):
        for right in edits[index + 1:]:
            if overlaps(
                (left["start_utf8"], left["end_utf8"]),
                (right["start_utf8"], right["end_utf8"]),
            ):
                add("E_OVERLAP", "grammar ranges overlap or duplicate", right["id"])
    for index, (_, left_anchor) in enumerate(accepted):
        for _, right_anchor in accepted[index + 1:]:
            if overlaps(
                (left_anchor.rendered_start, left_anchor.rendered_end),
                (right_anchor.rendered_start, right_anchor.rendered_end),
            ):
                add("E_UNMAPPABLE", "formatter anchors overlap", "")

    if diagnostics:
        return result_for(base, baseline.rendered, False, diagnostics)
    if not accepted:
        return result_for(base, baseline.rendered, False, [])

    rendered = baseline.rendered
    replacements: list[tuple[int, int, str]] = []
    for edit, anchor in accepted:
        existing = slice_utf8(rendered, anchor.rendered_start, anchor.rendered_end)
        if existing is None:
            return result_for(
                base,
                baseline.rendered,
                False,
                [Diagnostic("E_UNMAPPABLE", "rendered anchor is not UTF-8 aligned", edit["id"])],
            )
        replacements.append(
            (
                anchor.rendered_start,
                anchor.rendered_end,
                _preserve_formatter_case(existing, edit["after"]),
            )
        )
    for start, end, replacement in sorted(replacements, reverse=True):
        start_char = utf8_to_char(rendered, start)
        end_char = utf8_to_char(rendered, end)
        assert start_char is not None and end_char is not None
        rendered = rendered[:start_char] + replacement + rendered[end_char:]
    return result_for(base, rendered, True, [])


# ---- Authoritative stdlib corpus-schema validator -------------------------

SCHEMA_KEYWORDS = {
    "$schema",
    "$id",
    "$ref",
    "$defs",
    "title",
    "description",
    "type",
    "const",
    "enum",
    "required",
    "properties",
    "additionalProperties",
    "items",
    "minItems",
    "minLength",
    "minimum",
    "pattern",
}


def schema_definition_errors(schema: Any, path: str = "$") -> list[str]:
    if not isinstance(schema, dict):
        return [f"{path}: schema node must be object"]
    errors = [f"{path}: unsupported schema keyword {key}" for key in schema if key not in SCHEMA_KEYWORDS]
    if "properties" in schema:
        if not isinstance(schema["properties"], dict):
            errors.append(f"{path}.properties: must be object")
        else:
            for name, child in schema["properties"].items():
                errors.extend(schema_definition_errors(child, f"{path}.properties.{name}"))
    if "$defs" in schema:
        if not isinstance(schema["$defs"], dict):
            errors.append(f"{path}.$defs: must be object")
        else:
            for name, child in schema["$defs"].items():
                errors.extend(schema_definition_errors(child, f"{path}.$defs.{name}"))
    if isinstance(schema.get("items"), dict):
        errors.extend(schema_definition_errors(schema["items"], f"{path}.items"))
    return errors


def resolve_ref(root: Mapping[str, Any], ref: str) -> Mapping[str, Any]:
    if not ref.startswith("#/"):
        raise ValueError(f"non-local ref forbidden: {ref}")
    value: Any = root
    for part in ref[2:].split("/"):
        value = value[part.replace("~1", "/").replace("~0", "~")]
    if not isinstance(value, dict):
        raise ValueError(f"ref does not resolve to schema object: {ref}")
    return value


def _type_matches(value: Any, expected: str) -> bool:
    if expected == "object":
        return isinstance(value, dict)
    if expected == "array":
        return isinstance(value, list)
    if expected == "string":
        return isinstance(value, str)
    if expected == "integer":
        return isinstance(value, int) and not isinstance(value, bool)
    if expected == "boolean":
        return isinstance(value, bool)
    if expected == "null":
        return value is None
    return False


def validate_schema_instance(
    value: Any,
    schema: Mapping[str, Any],
    root: Mapping[str, Any],
    path: str = "$",
) -> list[str]:
    if "$ref" in schema:
        return validate_schema_instance(value, resolve_ref(root, schema["$ref"]), root, path)
    errors: list[str] = []
    if "const" in schema and value != schema["const"]:
        errors.append(f"{path}: expected const {schema['const']!r}")
    if "enum" in schema and value not in schema["enum"]:
        errors.append(f"{path}: value not in enum")
    expected_type = schema.get("type")
    if expected_type is not None and not _type_matches(value, expected_type):
        return errors + [f"{path}: expected {expected_type}"]
    if isinstance(value, dict):
        properties = schema.get("properties", {})
        for name in schema.get("required", []):
            if name not in value:
                errors.append(f"{path}: missing required {name}")
        if schema.get("additionalProperties") is False:
            for name in value:
                if name not in properties:
                    errors.append(f"{path}: additional property {name}")
        for name, child in properties.items():
            if name in value:
                errors.extend(validate_schema_instance(value[name], child, root, f"{path}.{name}"))
    if isinstance(value, list):
        if len(value) < schema.get("minItems", 0):
            errors.append(f"{path}: too few items")
        item_schema = schema.get("items")
        if isinstance(item_schema, dict):
            for index, item in enumerate(value):
                errors.extend(validate_schema_instance(item, item_schema, root, f"{path}[{index}]"))
    if isinstance(value, str):
        if len(value) < schema.get("minLength", 0):
            errors.append(f"{path}: string too short")
        if "pattern" in schema and re.search(schema["pattern"], value) is None:
            errors.append(f"{path}: pattern mismatch")
    if isinstance(value, int) and not isinstance(value, bool):
        if "minimum" in schema and value < schema["minimum"]:
            errors.append(f"{path}: below minimum")
    return errors


def corpus_meta_errors(corpus: Mapping[str, Any]) -> list[str]:
    errors: list[str] = []
    fixtures = corpus["fixtures"]
    ids = [fixture["id"] for fixture in fixtures]
    if len(ids) != len(set(ids)):
        errors.append("fixture IDs are not unique")
    if tuple(corpus["grammar_rule_catalog"]) != RULE_IDS:
        errors.append("grammar rule catalog differs from executable catalog")
    if tuple(item["code"] for item in corpus["error_codes"]) != ERROR_CODES:
        errors.append("error code catalog differs from executable order")
    for fixture in fixtures:
        base = fixture["base"]
        if base["fingerprint"] != fingerprint(base["text"]):
            errors.append(f"{fixture['id']}: base fingerprint mismatch")
    counts = corpus["dataset_counts"]
    accept = sum("grammar" in f["roles"] and "accept" in f["roles"] for f in fixtures)
    reject = sum("grammar" in f["roles"] and "reject" in f["roles"] for f in fixtures)
    formatting_only = sum("grammar" not in f["roles"] for f in fixtures)
    actual = (len(fixtures), accept, reject, formatting_only)
    declared = (
        counts["fixtures_total"],
        counts["fixtures_accept_grammar"],
        counts["fixtures_reject_grammar"],
        counts["fixtures_formatting_only"],
    )
    if actual != declared:
        errors.append(f"dataset counts {declared} != {actual}")
    return errors


def behavior_crosslink_errors(corpus: Mapping[str, Any], behavior: Mapping[str, Any]) -> list[str]:
    errors: list[str] = []
    if behavior.get("approval_state") != "approved":
        errors.append("#99 behavior corpus is not approved")
    decisions = {
        decision["id"]: decision.get("selected_option_id")
        for decision in behavior.get("decisions", [])
    }
    if decisions != APPROVED_99:
        errors.append(f"#99 decisions {decisions!r} != {APPROVED_99!r}")
    by_id = {fixture["id"]: fixture for fixture in behavior.get("fixtures", [])}
    approved_outputs = {
        fixture.get("input"): fixture.get("expected", {}).get("smart")
        for fixture in behavior.get("fixtures", [])
    }
    for source, rendered in FORMAT_OVERRIDES.items():
        if approved_outputs.get(source) != rendered:
            errors.append(f"formatter oracle differs from approved #99 input {source!r}")
    for fixture in corpus["fixtures"]:
        for related_id in fixture["related_behavior_fixture_ids"]:
            related = by_id.get(related_id)
            if related is None:
                errors.append(f"{fixture['id']}: missing #99 fixture {related_id}")
                continue
            if related.get("input") != fixture["base"]["text"]:
                errors.append(f"{fixture['id']}: base differs from #99 {related_id}")
            smart = related.get("expected", {}).get("smart")
            if fixture["expected"]["rendered"] != smart:
                errors.append(f"{fixture['id']}: rendered differs from #99 {related_id}")
    return errors


def run_fixture(fixture: Mapping[str, Any]) -> list[str]:
    try:
        base = ValidatedTranscript.from_json(fixture["base"])
        baseline = format_locally(base)
        actual = validate_grammar(
            base,
            baseline,
            fixture["grammar_candidate"],
            protected_names=fixture["protected_names"],
            dictionary_terms=fixture["dictionary_terms"],
        )
    except Exception as exc:  # proof runner must report the fixture, not hide it
        return [f"raised {type(exc).__name__}: {exc}"]
    expected = fixture["expected"]
    errors: list[str] = []
    for key in ("decision", "rendered", "error_codes"):
        if actual[key] != expected[key]:
            errors.append(f"{key} {actual[key]!r} != {expected[key]!r}")
    return errors


def _base(text: str, version: str = "validated-en-v1") -> ValidatedTranscript:
    return ValidatedTranscript(text, version, fingerprint(text))


def _candidate(base: ValidatedTranscript, edits: list[dict[str, Any]]) -> dict[str, Any]:
    return {
        "base_version": base.version,
        "base_fingerprint": base.fingerprint,
        "edits": edits,
    }


def direct_adversaries(schema: Mapping[str, Any], corpus: Mapping[str, Any]) -> list[tuple[str, bool, str]]:
    cases: list[tuple[str, bool, str]] = []

    def record(name: str, condition: bool, detail: Any) -> None:
        cases.append((name, bool(condition), str(detail)))

    # A provider cannot forge formatting with JSON or call the constructor without the seal.
    base = _base("do not transfer money")
    forged = validate_grammar(base, {"rendered": "transfer money now"}, _candidate(base, []))
    record(
        "ADV_TYPED_BASELINE_JSON_FORGERY",
        forged["rendered"] == base.text and forged["error_codes"] == ["E_FORMATTING_TYPE"],
        forged,
    )
    try:
        FormattingBaseline(
            object(),
            base_version=base.version,
            base_fingerprint=base.fingerprint,
            rendered="transfer money now",
            anchors={},
            protected_source_ranges=(),
        )
        constructor_rejected = False
    except TypeError:
        constructor_rejected = True
    record("ADV_BASELINE_CONSTRUCTOR_SEALED", constructor_rejected, constructor_rejected)

    immutable = format_locally(base)
    try:
        immutable.rendered = "transfer money now"  # type: ignore[misc]
        mutation_rejected = False
    except (AttributeError, TypeError):
        mutation_rejected = True
    record("ADV_BASELINE_IMMUTABLE", mutation_rejected, mutation_rejected)

    tampered = format_locally(base)
    object.__setattr__(tampered, "rendered", "transfer money now")
    tampered_result = validate_grammar(base, tampered, _candidate(base, []))
    record(
        "ADV_BASELINE_DERIVATION_TAMPER",
        tampered_result["rendered"] == base.text
        and tampered_result["error_codes"] == ["E_FORMATTING_DERIVATION"],
        tampered_result,
    )

    omission_base = _base('she said "i didnt know" today')
    omitted = format_locally(omission_base)
    object.__setattr__(omitted, "protected_source_ranges", ())
    object.__setattr__(
        omitted,
        "derivation_fingerprint",
        _baseline_derivation(
            omitted.base_version,
            omitted.base_fingerprint,
            omitted.rendered,
            omitted.anchors,
            omitted.protected_source_ranges,
            omitted.formatter_contract,
        ),
    )
    omission_result = validate_grammar(omission_base, omitted, _candidate(omission_base, []))
    record(
        "ADV_BASELINE_RANGE_OMISSION_WITH_REHASH",
        omission_result["rendered"] == omission_base.text
        and omission_result["error_codes"] == ["E_FORMATTING_DERIVATION"],
        omission_result,
    )

    other = _base("other transcript")
    mismatched = validate_grammar(base, format_locally(other), _candidate(base, []))
    record(
        "ADV_BASELINE_IDENTITY",
        mismatched["rendered"] == base.text
        and mismatched["error_codes"] == ["E_FORMATTING_IDENTITY"],
        mismatched,
    )

    baseline = format_locally(base)
    injected = _candidate(base, []) | {"rendered": "transfer money now"}
    injection_result = validate_grammar(base, baseline, injected)
    record(
        "ADV_CANDIDATE_WHOLE_RENDER_FIELD",
        injection_result["rendered"] == baseline.rendered
        and injection_result["error_codes"] == ["E_MALFORMED"],
        injection_result,
    )

    surrogate_edit = {
        "id": "surrogate",
        "rule_id": "G_DIDNT_APOSTROPHE",
        "start_utf8": 0,
        "end_utf8": 2,
        "before": "do",
        "after": "\ud800",
    }
    surrogate_result = validate_grammar(base, baseline, _candidate(base, [surrogate_edit]))
    record(
        "ADV_PROVIDER_LONE_SURROGATE",
        surrogate_result["rendered"] == baseline.rendered
        and surrogate_result["error_codes"] == ["E_MALFORMED"],
        surrogate_result,
    )

    try:
        decode_json_bounded(r'{"base":{"text":"\ud800"}}', "surrogate-corpus")
        surrogate_corpus_rejected = False
    except ValueError:
        surrogate_corpus_rejected = True
    record(
        "ADV_CORPUS_LONE_SURROGATE",
        surrogate_corpus_rejected,
        surrogate_corpus_rejected,
    )

    # Protected URL/path/command/name coverage uses otherwise valid apostrophe edits.
    for name, text, needle, protected_names, terms in (
        ("URL", "visit https://didnt.example", "didnt", [], []),
        ("PATH", "open /tmp/didnt now", "didnt", [], []),
        ("COMMAND", "say command period", "period", [], []),
        ("NAME", "ask Didnt tomorrow", "Didnt", ["Didnt"], []),
        ("EMAIL", "mail didnt@example.com", "didnt", [], []),
        ("NUMBER", "there is 2 issues", "2", [], []),
        ("DATE", "on 2026-08-09 ship", "2026", [], []),
        ("IDENTIFIER", "set API_KEY now", "API_KEY", [], []),
    ):
        protected_base = _base(text)
        char_start = text.index(needle)
        start = char_to_utf8(text, char_start)
        end = char_to_utf8(text, char_start + len(needle))
        edit = {
            "id": f"protect_{name}",
            "rule_id": "G_DIDNT_APOSTROPHE",
            "start_utf8": start,
            "end_utf8": end,
            "before": slice_utf8(text, start, end),
            "after": "didn't",
        }
        protected_result = validate_grammar(
            protected_base,
            format_locally(protected_base),
            _candidate(protected_base, [edit]),
            protected_names=protected_names,
            dictionary_terms=terms,
        )
        record(
            f"ADV_PROTECTED_{name}",
            "E_PROTECTED_SPAN" in protected_result["error_codes"]
            and protected_result["decision"] == "formatting_only",
            protected_result,
        )

    # The formatter capability carries complete quote/code source ranges,
    # including consumed command markers and every interior byte.
    for name, text, needle, open_marker, close_marker in (
        (
            "COMMAND_QUOTE",
            "she said command quote i didnt know command unquote today",
            "didnt",
            "command quote",
            "command unquote",
        ),
        ("ASCII_SINGLE", "she said 'i didnt know' today", "didnt", "'", "'"),
        (
            "ASCII_DOUBLE_MULTILINE",
            'she said "i didnt\nknow" today',
            "didnt",
            '"',
            '"',
        ),
        (
            "ASCII_SINGLE_MULTILINE",
            "she said 'i didnt\nknow' today",
            "didnt",
            "'",
            "'",
        ),
        ("CURLY_DOUBLE", "she said “i didnt know” today", "didnt", "“", "”"),
        ("CURLY_SINGLE", "she said ‘i didnt know’ today", "didnt", "‘", "’"),
        (
            "CURLY_DOUBLE_MULTILINE",
            "she said “i didnt\nknow” today",
            "didnt",
            "“",
            "”",
        ),
        (
            "CURLY_SINGLE_MULTILINE",
            "she said ‘i didnt\nknow’ today",
            "didnt",
            "‘",
            "’",
        ),
        ("INLINE_CODE", "use `didnt` exactly", "didnt", "`", "`"),
        ("FENCED_CODE", "use ```didnt``` exactly", "didnt", "```", "```"),
    ):
        quote_base = _base(text)
        quote_baseline = format_locally(quote_base)
        char_start = text.index(needle)
        start = char_to_utf8(text, char_start)
        end = char_to_utf8(text, char_start + len(needle))
        open_char = text.index(open_marker)
        close_char = text.index(close_marker, open_char + len(open_marker)) + len(close_marker)
        expected_range = (
            char_to_utf8(text, open_char),
            char_to_utf8(text, close_char),
        )
        edit = {
            "id": f"quote_{name}",
            "rule_id": "G_DIDNT_APOSTROPHE",
            "start_utf8": start,
            "end_utf8": end,
            "before": "didnt",
            "after": "didn't",
        }
        quote_result = validate_grammar(
            quote_base,
            quote_baseline,
            _candidate(quote_base, [edit]),
        )
        record(
            f"ADV_FORMATTER_RANGE_{name}",
            expected_range in quote_baseline.protected_source_ranges
            and _baseline_derivation_valid(quote_base, quote_baseline)
            and quote_result["error_codes"] == ["E_PROTECTED_SPAN"]
            and quote_result["decision"] == "formatting_only",
            (quote_baseline.protected_source_ranges, quote_result),
        )

    for name, text in (
        ("ASCII", "we didn't stop"),
        ("CURLY", "we didn’t stop"),
    ):
        apostrophe_ranges = _formatting_protected_source_ranges(text)
        record(
            f"ADV_APOSTROPHE_NOT_QUOTE_{name}",
            apostrophe_ranges == (),
            apostrophe_ranges,
        )

    unmatched_text = "she said 'i didnt know"
    unmatched_base = _base(unmatched_text)
    unmatched_start_char = unmatched_text.index("didnt")
    unmatched_start = char_to_utf8(unmatched_text, unmatched_start_char)
    unmatched_end = char_to_utf8(unmatched_text, unmatched_start_char + len("didnt"))
    unmatched_edit = {
        "id": "unmatched_quote",
        "rule_id": "G_DIDNT_APOSTROPHE",
        "start_utf8": unmatched_start,
        "end_utf8": unmatched_end,
        "before": "didnt",
        "after": "didn't",
    }
    unmatched_baseline = format_locally(unmatched_base)
    unmatched_result = validate_grammar(
        unmatched_base,
        unmatched_baseline,
        _candidate(unmatched_base, [unmatched_edit]),
    )
    record(
        "ADV_UNMATCHED_QUOTE_PROTECTS_BASE",
        unmatched_baseline.protected_source_ranges == ((0, utf8_len(unmatched_text)),)
        and unmatched_result["error_codes"] == ["E_PROTECTED_SPAN"],
        (unmatched_baseline.protected_source_ranges, unmatched_result),
    )

    # Token order alone is insufficient: punctuation/newlines between context
    # tokens must not satisfy either narrow grammar predicate.
    for name, text, needle, rule_id, before, after in (
        ("THERE_PERIOD", "there. is two issues", "is", "G_THERE_IS_PLURAL_QUANTITY", "is", "are"),
        ("THERE_COMMA", "there, is two issues", "is", "G_THERE_IS_PLURAL_QUANTITY", "is", "are"),
        ("THERE_DASH", "there — is two issues", "is", "G_THERE_IS_PLURAL_QUANTITY", "is", "are"),
        ("THERE_NEWLINE", "there\nis two issues", "is", "G_THERE_IS_PLURAL_QUANTITY", "is", "are"),
        ("LETS_PERIOD", "lets. meet tomorrow", "lets", "G_LETS_MEET_CONTRACTION", "lets", "let's"),
        ("LETS_COMMA", "lets, meet tomorrow", "lets", "G_LETS_MEET_CONTRACTION", "lets", "let's"),
        ("LETS_DASH", "lets — meet tomorrow", "lets", "G_LETS_MEET_CONTRACTION", "lets", "let's"),
        ("LETS_NEWLINE", "lets\nmeet tomorrow", "lets", "G_LETS_MEET_CONTRACTION", "lets", "let's"),
    ):
        punct_base = _base(text)
        char_start = text.index(needle)
        start = char_to_utf8(text, char_start)
        end = char_to_utf8(text, char_start + len(needle))
        punct_edit = {
            "id": f"punct_{name}",
            "rule_id": rule_id,
            "start_utf8": start,
            "end_utf8": end,
            "before": before,
            "after": after,
        }
        punct_result = validate_grammar(
            punct_base,
            format_locally(punct_base),
            _candidate(punct_base, [punct_edit]),
        )
        record(
            f"ADV_CONTEXT_{name}",
            punct_result["error_codes"] == ["E_RULE_CONTEXT"]
            and punct_result["decision"] == "formatting_only",
            punct_result,
        )

    for name, text in (
        ("ZERO_GAS", "there is 0 gas"),
        ("MASS_GAS", "there is two gas"),
        ("SINGULAR_S_NEWS", "there is two news"),
        ("UNLISTED_PLURAL_ERRORS", "there is two errors"),
        ("OUT_OF_DOMAIN_13", "there is 13 issues"),
        ("UNICODE_SUPERSCRIPT_TWO", "there is ² issues"),
        ("OVERSIZED_ASCII_DIGITS", "there is " + "2" * 5_000 + " issues"),
    ):
        noun_base = _base(text)
        char_start = text.index("is")
        start = char_to_utf8(text, char_start)
        end = char_to_utf8(text, char_start + 2)
        noun_edit = {
            "id": f"noun_{name}",
            "rule_id": "G_THERE_IS_PLURAL_QUANTITY",
            "start_utf8": start,
            "end_utf8": end,
            "before": "is",
            "after": "are",
        }
        noun_result = validate_grammar(
            noun_base,
            format_locally(noun_base),
            _candidate(noun_base, [noun_edit]),
        )
        record(
            f"ADV_COUNT_NOUN_{name}",
            noun_result["error_codes"] == ["E_RULE_CONTEXT"]
            and noun_result["decision"] == "formatting_only",
            noun_result,
        )

    # Freshness is decided after envelope shape but before edit shape.
    precedence_base = _base("lets meet tomorrow")
    stale_malformed = {
        "base_version": "validated-en-v0",
        "base_fingerprint": "sha256:" + "0" * 64,
        "edits": [{"malformed": True}],
    }
    precedence_result = validate_grammar(
        precedence_base,
        format_locally(precedence_base),
        stale_malformed,
    )
    record(
        "ADV_STALE_PRECEDES_MALFORMED_EDIT",
        precedence_result["error_codes"] == ["E_STALE_GRAMMAR"]
        and precedence_result["rendered"] == "Lets meet tomorrow.",
        precedence_result,
    )
    missing_required = {
        "base_version": precedence_base.version,
        "base_fingerprint": precedence_base.fingerprint,
    }
    missing_result = validate_grammar(
        precedence_base,
        format_locally(precedence_base),
        missing_required,
    )
    record(
        "ADV_CANDIDATE_MISSING_REQUIRED",
        missing_result["error_codes"] == ["E_MALFORMED"],
        missing_result,
    )

    for name, depth in (("DEEP_NESTING", 32), ("RECURSION_BOUND", 2000)):
        deeply_nested = "[" * depth + "0" + "]" * depth
        try:
            decode_json_bounded(deeply_nested, name)
            bounded = False
        except ValueError:
            bounded = True
        record(f"ADV_JSON_{name}", bounded, bounded)

    # Diagnostics scrub secrets and stay byte-bounded.
    secret_base = _base("lets meet tomorrow")
    secret_edit = {
        "id": "gsk_supersecretvalue123456789",
        "rule_id": "G_UNKNOWN",
        "start_utf8": 0,
        "end_utf8": 4,
        "before": "lets",
        "after": "let's",
    }
    secret_result = validate_grammar(
        secret_base,
        format_locally(secret_base),
        _candidate(secret_base, [secret_edit]),
    )
    encoded_diags = json.dumps(secret_result["diagnostics"], ensure_ascii=False)
    record(
        "ADV_DIAGNOSTIC_SCRUB",
        "supersecret" not in encoded_diags
        and all(
            utf8_len(value) <= MAX_DIAG_BYTES
            for diag in secret_result["diagnostics"]
            for value in diag.values()
        ),
        secret_result,
    )

    # The stdlib schema path is required and rejects declared-shape violations.
    negative_mutations: list[tuple[str, Any]] = []
    extra = copy.deepcopy(corpus)
    extra["provider_rendered"] = "unsafe"
    negative_mutations.append(("EXTRA_TOP_LEVEL", extra))
    missing = copy.deepcopy(corpus)
    del missing["fixtures"][0]["grammar_candidate"]["base_fingerprint"]
    negative_mutations.append(("MISSING_REQUIRED_FIELD", missing))
    wrong_type = copy.deepcopy(corpus)
    wrong_type["fixtures"][0]["base"]["text"] = 42
    negative_mutations.append(("WRONG_BASE_TYPE", wrong_type))
    candidate_extra = copy.deepcopy(corpus)
    candidate_extra["fixtures"][0]["grammar_candidate"]["rendered"] = "unsafe"
    negative_mutations.append(("GRAMMAR_EXTRA_FIELD", candidate_extra))
    for name, mutated in negative_mutations:
        schema_errors = validate_schema_instance(mutated, schema, schema)
        record(f"SCHEMA_REJECT_{name}", bool(schema_errors), schema_errors[:3])

    unsupported = copy.deepcopy(schema)
    unsupported["oneOf"] = []
    unsupported_errors = schema_definition_errors(unsupported)
    record(
        "SCHEMA_REJECT_UNSUPPORTED_KEYWORD",
        any("unsupported schema keyword oneOf" in error for error in unsupported_errors),
        unsupported_errors[:3],
    )

    def reviewable_json(path: Path) -> bool:
        text = path.read_text(encoding="utf-8")
        lines = text.splitlines()
        return (
            text.endswith("\n")
            and len(lines) > 50
            and max((utf8_len(line) for line in lines), default=0) <= 160
            and json.loads(text) is not None
        )

    pretty_schema = reviewable_json(SCHEMA_PATH)
    pretty_corpus = reviewable_json(CORPUS_PATH)
    record("JSON_PRETTY_PRINTED", pretty_schema and pretty_corpus, (pretty_schema, pretty_corpus))
    return cases


def main() -> int:
    try:
        schema = load_json_bounded(SCHEMA_PATH)
        corpus = load_json_bounded(CORPUS_PATH)
        behavior = load_json_bounded(BEHAVIOR_PATH)
    except (OSError, ValueError, json.JSONDecodeError) as exc:
        print(f"ERROR load: {exc}", file=sys.stderr)
        return 1

    failures = 0
    definition_errors = schema_definition_errors(schema)
    if definition_errors:
        for error in definition_errors:
            print(f"FAIL SCHEMA_DEFINITION: {error}")
        print(
            f"summary: fixtures=0 adversaries=0 failures={len(definition_errors)} "
            "semantic_consumers=stopped"
        )
        return 1
    print("PASS SCHEMA_DEFINITION_SUPPORTED_SUBSET")

    shape_errors = validate_schema_instance(corpus, schema, schema)
    if shape_errors:
        for error in shape_errors:
            print(f"FAIL CORPUS_SCHEMA: {error}")
        print(
            f"summary: fixtures=0 adversaries=0 failures={len(shape_errors)} "
            "semantic_consumers=stopped"
        )
        return 1
    print("PASS CORPUS_SCHEMA_STDLIB_AUTHORITATIVE")

    for label, errors in (
        ("CORPUS_META", corpus_meta_errors(corpus)),
        ("BEHAVIOR_99_CROSSLINK", behavior_crosslink_errors(corpus, behavior)),
    ):
        if errors:
            for error in errors:
                print(f"FAIL {label}: {error}")
            failures += len(errors)
        else:
            print(f"PASS {label}")

    fixture_count = 0
    for fixture in corpus.get("fixtures", []):
        fixture_count += 1
        errors = run_fixture(fixture)
        if errors:
            failures += len(errors)
            for error in errors:
                print(f"FAIL {fixture['id']}: {error}")
        else:
            print(f"PASS {fixture['id']}")

    adversary_count = 0
    for name, passed, detail in direct_adversaries(schema, corpus):
        adversary_count += 1
        if passed:
            print(f"PASS {name}")
        else:
            failures += 1
            print(f"FAIL {name}: {detail}")

    print(
        f"summary: fixtures={fixture_count} adversaries={adversary_count} "
        f"failures={failures} approval={corpus.get('approval_state')}"
    )
    return 0 if failures == 0 else 1


if __name__ == "__main__":
    raise SystemExit(main())
