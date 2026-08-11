#!/usr/bin/env python3
"""Pure-local weighted intent routing prototype for Developer Prompt Rendering (#141).

Selects among literal_identity, deterministic_local, and local_with_optional_cloud
using ordered rules + explicit integer weights. Zero network I/O on the decision
path. Standard-library only. Exit 0 iff the sibling package is healthy and all
mutations fail closed as expected.
"""

from __future__ import annotations

import copy
import json
import re
import sys
import time
from pathlib import Path
from typing import Any

HERE = Path(__file__).resolve().parent
CORPUS_PATH = HERE / "developer-prompt-rendering-intent-routing-corpus-2026-08-11.json"
SCHEMA_PATH = HERE / "developer-prompt-rendering-intent-routing-schema-2026-08-11.json"

CORPUS_ID = "voisu-developer-prompt-rendering-intent-routing-2026-08-11"
FIXTURE_ID_RE = re.compile(r"^IRI-[0-9]{2}$")
DPR_ID_RE = re.compile(r"^DPR-[0-9]{2}$")

ROUTES = (
    "literal_identity",
    "deterministic_local",
    "local_with_optional_cloud",
)
CLOUD_REQUESTS = ("not_allowed", "allowed", "required")
POLICIES = ("natural", "adaptive", "structured")
PROVIDER_STATES = (
    "exact_agreement",
    "punctuation_only_agreement",
    "safe_complementary",
    "protected_token_disagreement",
    "semantic_disagreement",
    "single_provider",
)
SURFACE_HINTS = (
    "shell",
    "terminal",
    "coding_agent",
    "gui_agent",
    "messaging",
    "browser",
    "unknown",
    None,
)
PROCESS_CLASSES = (
    "shell",
    "terminal",
    "coding_agent",
    "gui_agent",
    "messaging",
    "browser",
    "unknown",
)
RULE_IDS = (
    "R_DISPUTE_CLOUD",
    "R_DISPUTE_POLICY_FORBID",
    "R_LITERAL_PREFORMATTED",
    "R_LITERAL_COMMAND",
    "R_NATURAL_LOCAL",
    "R_COMPLEX_CLOUD",
    "R_DEFAULT_LOCAL",
)
DISPUTE_STATES = frozenset(
    {"protected_token_disagreement", "semantic_disagreement"}
)
AGREEMENT_FAST_PATH = frozenset(
    {
        "exact_agreement",
        "punctuation_only_agreement",
        "safe_complementary",
        "single_provider",
    }
)

# Fixed catalogs — must match corpus weights/thresholds constants.
DEFAULT_WEIGHTS: dict[str, int] = {
    "section_goal": 12,
    "section_context": 12,
    "section_requirements": 12,
    "section_constraints": 12,
    "section_steps": 12,
    "section_acceptance_criteria": 14,
    "section_files": 10,
    "section_notes": 10,
    "words_ge_40": 4,
    "words_ge_80": 6,
    "surface_coding_agent_sections": 4,
    "surface_gui_agent_sections": 3,
    "surface_messaging_short": -6,
    "surface_browser_short": -4,
    "process_coding_boost": 2,
    "timing_clear_pause": 0,
    "timing_uncertain_pause": 0,
}
DEFAULT_THRESHOLDS: dict[str, int] = {
    "complexity_cloud": 24,
    "messaging_short_words": 30,
    "browser_short_words": 25,
    "section_cues_for_length_assist": 2,
    "delivery_deadline_ms": 1500,
}

# Section cues: header / introducer patterns only — not every mid-sentence noun.
# Strong cues may fire alone. Weak cues (common nouns: steps/files/notes) fire
# only when at least one strong cue is already present — avoids "release notes",
# "next steps", "open files" inflating everyday complexity.
#
# A cue counts when it looks like a section header:
#   - start of utterance or after sentence pause (. ! ? newline)
#   - colon form ("goal:" / "context:")
#   - structural "the goal is" / "goal is"
# Mid-sentence nouns ("the project goal depends on business context") do NOT count.
# Multi-section spoken dictation that *starts* with a section label also collects
# subsequent bare catalog labels as free-standing introducers (stream mode).
_BOUNDARY = r"(?:^|[\n.!?]\s*)"
_STRUCT_IS = r"(?:^|[\n.!?]\s*)(?:the\s+)?"

def _section_header_re(phrase: str) -> re.Pattern[str]:
    """Header-shaped match for a cue phrase (single- or multi-word)."""
    esc = re.sub(r"\s+", r"\\s+", phrase.strip())
    return re.compile(
        rf"(?:"
        rf"{_BOUNDARY}{esc}\b"  # start / after pause
        rf"|"
        rf"(?:^|[\n.!?]\s*|,\s*){esc}\s*:"  # "goal:" / ", context:"
        rf"|"
        rf"{_STRUCT_IS}{esc}\s+is\b"  # "the goal is" / "goal is"
        rf")",
        re.I,
    )


# (signal_id, phrase, pattern, strength) strength is "strong" | "weak"
SECTION_CUE_PATTERNS: tuple[tuple[str, str, re.Pattern[str], str], ...] = (
    (
        "section_acceptance_criteria",
        "acceptance criteria",
        _section_header_re("acceptance criteria"),
        "strong",
    ),
    (
        "section_goal",
        "goal",
        _section_header_re("goal"),
        "strong",
    ),
    (
        "section_context",
        "context",
        _section_header_re("context"),
        "strong",
    ),
    (
        "section_requirements",
        "requirements",
        _section_header_re("requirements"),
        "strong",
    ),
    (
        "section_constraints",
        "constraints",
        _section_header_re("constraints"),
        "strong",
    ),
    (
        "section_steps",
        "steps",
        _section_header_re("steps"),
        "weak",
    ),
    (
        "section_files",
        "files",
        _section_header_re("files"),
        "weak",
    ),
    (
        "section_notes",
        "notes",
        _section_header_re("notes"),
        "weak",
    ),
)

# Token-level catalog for multi-section stream collection (spoken dictation).
# Multi-word cues use a tuple of tokens.
SECTION_CUE_TOKEN_CATALOG: tuple[tuple[tuple[str, ...], str, str], ...] = (
    (("acceptance", "criteria"), "section_acceptance_criteria", "strong"),
    (("goal",), "section_goal", "strong"),
    (("context",), "section_context", "strong"),
    (("requirements",), "section_requirements", "strong"),
    (("constraints",), "section_constraints", "strong"),
    (("steps",), "section_steps", "weak"),
    (("files",), "section_files", "weak"),
    (("notes",), "section_notes", "weak"),
)
STRONG_SECTION_LEAD_TOKENS = frozenset(
    {"goal", "context", "requirements", "constraints", "acceptance"}
)
_DETERMINER_TOKENS = frozenset(
    {
        "the",
        "a",
        "an",
        "my",
        "your",
        "our",
        "some",
        "any",
        "these",
        "those",
        "this",
        "that",
    }
)
_COMPOUND_LEFT_TOKENS = frozenset(
    {
        "release",
        "next",
        "open",
        "send",
        "share",
        "project",
        "business",
        "user",
        "team",
        "main",
        "overall",
        "primary",
        "broader",
        "historical",
        "social",
        "local",
        "global",
        "market",
        "product",
    }
)

RUNNER_TOKENS = frozenset(
    {
        "run",
        "cargo",
        "npm",
        "pnpm",
        "yarn",
        "git",
        "docker",
        "kubectl",
        "make",
        "python",
        "python3",
        "pip",
        "curl",
        "ssh",
        "scp",
        "go",
        "bazel",
        "ninja",
    }
)
FLAG_RE = re.compile(r"^--?[A-Za-z0-9][\w-]*$")
DOUBLE_DASH_RE = re.compile(r"^--[\w-]+(?:=.*)?$")
NUMBERED_LINE_RE = re.compile(r"^\s*\d+[\.)]\s+\S")
BULLET_LINE_RE = re.compile(r"^\s*[-*]\s+\S")
WORD_RE = re.compile(r"[A-Za-z0-9_./:-]+")
# Absolute, home, relative, bazel //target, or path-with-slash tokens.
PATH_LIKE_RE = re.compile(
    r"^(?:~(?:/.*)?|/(?!/).*|\.{1,2}/.*|//[A-Za-z0-9_./:@+-]+|.*/.+)$"
)
# File-ish tokens (script.py, main.rs) that are not pure prose words.
FILE_EXT_RE = re.compile(
    r"^[A-Za-z0-9_.-]+\.(?:rs|py|ts|tsx|js|jsx|sh|go|toml|json|ya?ml|md|txt|lock|so|a|o)$",
    re.I,
)

# Everyday English second tokens after a leading runner — not CLI.
# Covers "make sure…", "go ahead…", "run this/by…" and similar prose.
PROSE_RUNNER_SECONDS = frozenset(
    {
        "sure",
        "certain",
        "sense",
        "clear",
        "it",
        "this",
        "that",
        "me",
        "us",
        "him",
        "her",
        "them",
        "my",
        "your",
        "our",
        "a",
        "an",
        "the",
        "ahead",
        "for",
        "back",
        "on",
        "through",
        "away",
        "home",
        "there",
        "here",
        "to",
        "and",
        "with",
        "get",
        "by",
        "into",
        "out",
        "over",
        "some",
        "any",
        "when",
        "if",
        "while",
        "before",
        "after",
    }
)

# Common CLI subcommands/targets that follow a runner (path/subcommand arm).
KNOWN_CLI_SUBCOMMANDS = frozenset(
    {
        "test",
        "build",
        "run",
        "check",
        "clippy",
        "fmt",
        "bench",
        "doc",
        "install",
        "clean",
        "status",
        "commit",
        "push",
        "pull",
        "clone",
        "diff",
        "log",
        "add",
        "checkout",
        "branch",
        "merge",
        "rebase",
        "fetch",
        "exec",
        "ps",
        "images",
        "compose",
        "apply",
        "get",
        "describe",
        "logs",
        "delete",
        "create",
        "scale",
        "rollout",
        "config",
        "init",
        "start",
        "stop",
        "restart",
        "up",
        "down",
        "serve",
        "dev",
        "publish",
        "pack",
        "login",
        "logout",
        "whoami",
        "version",
        "help",
        "mod",
        "env",
        "list",
        "info",
        "search",
        "uninstall",
        "update",
        "upgrade",
        "remove",
        "sync",
        "lock",
        "audit",
        "outdated",
        "workspace",
        "package",
        "target",
        "release",
        "debug",
        "all",
        "dist",
        "deploy",
        "vet",
        "generate",
        "tool",
        "work",
        "tidy",
        "vendor",
        "query",
        "coverage",
        "nextest",
    }
)

NETWORK_MODULE_NAMES = frozenset(
    {
        "socket",
        "ssl",
        "http",
        "http.client",
        "urllib",
        "urllib.request",
        "urllib.error",
        "urllib.parse",
        "requests",
        "aiohttp",
        "httpx",
        "ftplib",
        "smtplib",
    }
)

TOP_REQUIRED = [
    "corpus_id",
    "version",
    "issue",
    "language",
    "governing",
    "routes",
    "cloud_request_states",
    "policies",
    "provider_states",
    "surface_hints",
    "process_classes",
    "rule_ids",
    "weights",
    "thresholds",
    "closed_tags",
    "coverage_requirements",
    "coverage_matrix",
    "fixtures",
    "dataset_counts",
    "invariants",
    "negative_cases",
]
FIXTURE_KEYS = {
    "id",
    "title",
    "tags",
    "policy",
    "primary_text",
    "provider_state",
    "surface_hint",
    "process_hint",
    "timing",
    "kind_hint",
    "expected",
    "negative_case_ids",
    "dpr_links",
    "rationale",
}
EXPECTED_KEYS = {
    "route",
    "cloud_request",
    "rule_id",
    "min_complexity_score",
    "max_complexity_score",
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
        raise CheckError(f"invalid JSON in {path.name}: {exc}") from exc


def tokenize(text: str) -> list[str]:
    return WORD_RE.findall(text)


def word_count(text: str) -> int:
    return len(tokenize(text))


def is_preformatted(text: str) -> bool:
    if "\n" not in text:
        return False
    lines = text.splitlines()
    numbered = sum(1 for line in lines if NUMBERED_LINE_RE.search(line))
    bullets = sum(1 for line in lines if BULLET_LINE_RE.search(line))
    return numbered >= 2 or bullets >= 2


def is_path_like(token: str) -> bool:
    """True for absolute/relative/home/bazel paths or file-with-extension tokens."""
    if PATH_LIKE_RE.match(token) or FILE_EXT_RE.match(token):
        return True
    return False


def _has_cli_flag(tokens: list[str]) -> bool:
    return any(FLAG_RE.match(t) or DOUBLE_DASH_RE.match(t) for t in tokens)


def _runner_follow_is_cli(runner: str, follow: str, follow_raw: str) -> bool:
    """True when the token after a runner has **positive** CLI evidence.

    Bare everyday words after a runner (``run errands``, ``make dinner``,
    ``go shopping``, ``python is great``) are NOT CLI. Require one of:
    - flag-shaped token (``--package`` / strong single-dash)
    - path-like token
    - known CLI subcommand (``test``, ``build``, ``install``, ``status``, …)
    - nested runner (``run cargo …``)
    """
    del runner  # runner identity is not needed once follow evidence is checked
    if follow in PROSE_RUNNER_SECONDS:
        return False
    if FLAG_RE.match(follow_raw) or DOUBLE_DASH_RE.match(follow_raw):
        return True
    if is_path_like(follow_raw):
        return True
    if follow in KNOWN_CLI_SUBCOMMANDS:
        return True
    if follow in RUNNER_TOKENS:
        # e.g. `run cargo …` — nested runner is CLI, not prose
        return True
    return False


def is_command_shaped(text: str) -> bool:
    """True when speech looks like a real CLI invocation, not everyday English.

    Runner tokens only count with **positive** CLI evidence:
    - any CLI flag token (``-x`` / ``--flag``), or
    - leading runner with positive follow-on (subcommand / nested runner /
      flag / path), or bare single-token runner (``cargo``), or
    - runner in the first three tokens with path / subcommand / nested runner
    Reject bare prose after runners: ``run errands``, ``make dinner``,
    ``go shopping``, ``python is great``, ``make sure…``, ``go ahead…``.
    """
    tokens = tokenize(text)
    if not tokens:
        return False
    lower = [t.casefold() for t in tokens]

    # Flag evidence alone is command-shaped (rare in everyday speech).
    if _has_cli_flag(tokens):
        return True

    has_path = any(is_path_like(t) for t in tokens)

    # Arm A: leading runner with positive CLI continuation (or bare runner).
    if lower[0] in RUNNER_TOKENS:
        if len(lower) == 1:
            return True
        if lower[1] in PROSE_RUNNER_SECONDS and not has_path:
            # "make sure…", "go ahead…", "run this by…" — not CLI.
            return False
        # Require positive evidence on the follow token, or a path anywhere.
        if _runner_follow_is_cli(lower[0], lower[1], tokens[1]):
            return True
        if has_path:
            return True
        return False

    # Arm B: runner in first three tokens with path / subcommand / nested-CLI shape.
    for i, tok in enumerate(lower[:3]):
        if tok not in RUNNER_TOKENS:
            continue
        if has_path:
            return True
        if i + 1 < len(lower) and _runner_follow_is_cli(
            tok, lower[i + 1], tokens[i + 1]
        ):
            return True
    return False


def has_strong_cli_evidence(text: str) -> bool:
    return any(DOUBLE_DASH_RE.match(t) for t in tokenize(text))


def command_anchor_ok(
    surface_hint: str | None,
    process_hint: dict[str, Any] | None,
    text: str,
) -> bool:
    if surface_hint in {"shell", "terminal"}:
        return True
    if isinstance(process_hint, dict) and process_hint.get("class") in {
        "shell",
        "terminal",
    }:
        return True
    return has_strong_cli_evidence(text)


def _collect_section_cue_hits(
    primary_text: str,
) -> tuple[list[tuple[str, str, int, str]], list[tuple[str, str, int, str]]]:
    """Return (strong_hits, weak_hits) as (signal_id, phrase, weight_key_unused, detail_kind).

    Weight is filled by the caller from the weights table. detail_kind is
    "header" or "stream".
    """
    # signal_id -> (phrase, strength, detail_kind)
    found: dict[str, tuple[str, str, str]] = {}

    for signal_id, phrase, pattern, strength in SECTION_CUE_PATTERNS:
        if pattern.search(primary_text):
            found[signal_id] = (phrase, strength, "header")

    # Multi-section stream: spoken dictation that opens with a section label
    # also collects later bare catalog labels as introducers (not mid-NP nouns).
    tokens = tokenize(primary_text)
    lower = [t.casefold() for t in tokens]
    stream_mode = bool(lower) and lower[0] in STRONG_SECTION_LEAD_TOKENS
    if stream_mode:
        i = 0
        while i < len(lower):
            matched = False
            for cue_toks, signal_id, strength in SECTION_CUE_TOKEN_CATALOG:
                n = len(cue_toks)
                if i + n > len(lower):
                    continue
                if tuple(lower[i : i + n]) != cue_toks:
                    continue
                # Mid-stream introducers must introduce following content.
                # Token-0 (or multi-word starting at 0) may stand alone.
                if i > 0 and i + n >= len(lower):
                    matched = True  # consumed, but do not count trailing NP head
                    i += n
                    break
                prev = lower[i - 1] if i > 0 else ""
                if prev in _DETERMINER_TOKENS or prev in _COMPOUND_LEFT_TOKENS:
                    matched = True
                    i += n
                    break
                # Compound weak forms: release notes / next steps / open files
                if signal_id not in found:
                    phrase = " ".join(cue_toks)
                    found[signal_id] = (phrase, strength, "stream")
                matched = True
                i += n
                break
            if not matched:
                i += 1

    strong_hits: list[tuple[str, str, int, str]] = []
    weak_hits: list[tuple[str, str, int, str]] = []
    for signal_id, (phrase, strength, kind) in found.items():
        # weight placeholder 0 — caller applies weights table
        item = (signal_id, phrase, 0, kind)
        if strength == "strong":
            strong_hits.append(item)
        else:
            weak_hits.append(item)
    return strong_hits, weak_hits


def score_complexity(
    primary_text: str,
    surface_hint: str | None,
    process_hint: dict[str, Any] | None,
    timing: dict[str, Any] | None,
    weights: dict[str, int],
    thresholds: dict[str, int],
) -> tuple[int, list[dict[str, Any]], int]:
    """Return (score, contributions, section_cue_count)."""
    contributions: list[dict[str, Any]] = []
    score = 0
    section_hits = 0

    strong_hits, weak_hits = _collect_section_cue_hits(primary_text)

    for signal_id, phrase, _zero, kind in strong_hits:
        weight = int(weights[signal_id])
        score += weight
        section_hits += 1
        contributions.append(
            {
                "signal": signal_id,
                "weight": weight,
                "detail": f"matched strong section cue '{phrase}' ({kind})",
            }
        )

    # Weak common-noun cues require multi-section evidence (any strong cue).
    if strong_hits:
        for signal_id, phrase, _zero, kind in weak_hits:
            weight = int(weights[signal_id])
            score += weight
            section_hits += 1
            contributions.append(
                {
                    "signal": signal_id,
                    "weight": weight,
                    "detail": (
                        f"matched weak section cue '{phrase}' with strong "
                        f"multi-section evidence ({kind})"
                    ),
                }
            )

    wc = word_count(primary_text)
    min_sections = int(thresholds["section_cues_for_length_assist"])
    if section_hits >= min_sections:
        if wc >= 80:
            weight = int(weights["words_ge_80"])
            score += weight
            contributions.append(
                {
                    "signal": "words_ge_80",
                    "weight": weight,
                    "detail": f"word_count={wc}",
                }
            )
        if wc >= 40:
            weight = int(weights["words_ge_40"])
            score += weight
            contributions.append(
                {
                    "signal": "words_ge_40",
                    "weight": weight,
                    "detail": f"word_count={wc}",
                }
            )

    if surface_hint == "coding_agent" and section_hits >= 1:
        weight = int(weights["surface_coding_agent_sections"])
        score += weight
        contributions.append(
            {
                "signal": "surface_coding_agent_sections",
                "weight": weight,
                "detail": "coding_agent with section cues",
            }
        )
    if surface_hint == "gui_agent" and section_hits >= 1:
        weight = int(weights["surface_gui_agent_sections"])
        score += weight
        contributions.append(
            {
                "signal": "surface_gui_agent_sections",
                "weight": weight,
                "detail": "gui_agent with section cues",
            }
        )
    if (
        surface_hint == "messaging"
        and wc < int(thresholds["messaging_short_words"])
        and section_hits == 0
    ):
        weight = int(weights["surface_messaging_short"])
        score += weight
        contributions.append(
            {
                "signal": "surface_messaging_short",
                "weight": weight,
                "detail": f"messaging short word_count={wc}",
            }
        )
    if (
        surface_hint == "browser"
        and wc < int(thresholds["browser_short_words"])
        and section_hits == 0
    ):
        weight = int(weights["surface_browser_short"])
        score += weight
        contributions.append(
            {
                "signal": "surface_browser_short",
                "weight": weight,
                "detail": f"browser short word_count={wc}",
            }
        )

    process_class = None
    if isinstance(process_hint, dict):
        process_class = process_hint.get("class")
    if process_class in {"coding_agent", "gui_agent"} and section_hits >= 1:
        weight = int(weights["process_coding_boost"])
        score += weight
        contributions.append(
            {
                "signal": "process_coding_boost",
                "weight": weight,
                "detail": f"process.class={process_class}",
            }
        )

    if isinstance(timing, dict):
        certainty = timing.get("certainty")
        if certainty == "clear":
            weight = int(weights["timing_clear_pause"])
            contributions.append(
                {
                    "signal": "timing_clear_pause",
                    "weight": weight,
                    "detail": f"max_pause_ms={timing.get('max_pause_ms')}",
                }
            )
            score += weight
        elif certainty == "uncertain":
            weight = int(weights["timing_uncertain_pause"])
            contributions.append(
                {
                    "signal": "timing_uncertain_pause",
                    "weight": weight,
                    "detail": f"max_pause_ms={timing.get('max_pause_ms')}",
                }
            )
            score += weight

    if score < 0:
        contributions.append(
            {
                "signal": "score_floor",
                "weight": -score,
                "detail": "clamped complexity score to 0",
            }
        )
        score = 0

    return score, contributions, section_hits


def route_intent(
    observation: dict[str, Any],
    weights: dict[str, int] | None = None,
    thresholds: dict[str, int] | None = None,
) -> dict[str, Any]:
    """Pure-local routing decision. No network, no sleep, no randomness."""
    w = dict(DEFAULT_WEIGHTS if weights is None else weights)
    t = dict(DEFAULT_THRESHOLDS if thresholds is None else thresholds)

    policy = observation["policy"]
    primary_text = observation["primary_text"]
    provider_state = observation["provider_state"]
    surface_hint = observation.get("surface_hint", None)
    process_hint = observation.get("process_hint", None)
    timing = observation.get("timing", None)

    surface_degraded = surface_hint is None and process_hint is None

    score, contributions, section_hits = score_complexity(
        primary_text, surface_hint, process_hint, timing, w, t
    )

    # Ordered rules — first match wins.
    # Dispute cloud eligibility is evaluated BEFORE literal preformatted/command
    # so protected_token / semantic disagreement on a list or CLI still opens
    # cloud under adaptive/structured. Natural still forbids cloud.
    if provider_state in DISPUTE_STATES:
        if policy == "natural":
            return _decision(
                "deterministic_local",
                "not_allowed",
                "R_DISPUTE_POLICY_FORBID",
                score,
                contributions,
                surface_degraded,
                section_hits,
            )
        return _decision(
            "local_with_optional_cloud",
            "allowed",
            "R_DISPUTE_CLOUD",
            score,
            contributions,
            surface_degraded,
            section_hits,
        )

    if is_preformatted(primary_text):
        return _decision(
            "literal_identity",
            "not_allowed",
            "R_LITERAL_PREFORMATTED",
            score,
            contributions,
            surface_degraded,
            section_hits,
        )

    if is_command_shaped(primary_text) and command_anchor_ok(
        surface_hint, process_hint, primary_text
    ):
        return _decision(
            "literal_identity",
            "not_allowed",
            "R_LITERAL_COMMAND",
            score,
            contributions,
            surface_degraded,
            section_hits,
        )

    if policy == "natural":
        return _decision(
            "deterministic_local",
            "not_allowed",
            "R_NATURAL_LOCAL",
            score,
            contributions,
            surface_degraded,
            section_hits,
        )

    if score >= int(t["complexity_cloud"]):
        if policy == "structured":
            cloud = "required"
        else:
            cloud = "allowed"
        return _decision(
            "local_with_optional_cloud",
            cloud,
            "R_COMPLEX_CLOUD",
            score,
            contributions,
            surface_degraded,
            section_hits,
        )

    return _decision(
        "deterministic_local",
        "not_allowed",
        "R_DEFAULT_LOCAL",
        score,
        contributions,
        surface_degraded,
        section_hits,
    )


def _decision(
    route: str,
    cloud_request: str,
    rule_id: str,
    score: int,
    contributions: list[dict[str, Any]],
    surface_degraded: bool,
    section_hits: int,
) -> dict[str, Any]:
    return {
        "route": route,
        "cloud_request": cloud_request,
        "rule_id": rule_id,
        "complexity_score": score,
        "contributions": list(contributions),
        "surface_degraded": surface_degraded,
        "section_cue_count": section_hits,
    }


def observation_from_fixture(fixture: dict[str, Any]) -> dict[str, Any]:
    return {
        "policy": fixture["policy"],
        "primary_text": fixture["primary_text"],
        "provider_state": fixture["provider_state"],
        "surface_hint": fixture.get("surface_hint"),
        "process_hint": fixture.get("process_hint"),
        "timing": fixture.get("timing"),
        "kind_hint": fixture.get("kind_hint"),
    }


# ---------------------------------------------------------------------------
# Package validation
# ---------------------------------------------------------------------------


def validate_catalog_constants(corpus: dict[str, Any], errors: list[str]) -> None:
    if corpus.get("corpus_id") != CORPUS_ID:
        errors.append(f"corpus_id must be {CORPUS_ID}")
    if corpus.get("language") != "en":
        errors.append("language must be en")
    if list(corpus.get("routes") or []) != list(ROUTES):
        errors.append("routes catalog drift")
    if list(corpus.get("cloud_request_states") or []) != list(CLOUD_REQUESTS):
        errors.append("cloud_request_states catalog drift")
    if list(corpus.get("policies") or []) != list(POLICIES):
        errors.append("policies catalog drift")
    if list(corpus.get("provider_states") or []) != list(PROVIDER_STATES):
        errors.append("provider_states catalog drift")
    if list(corpus.get("rule_ids") or []) != list(RULE_IDS):
        errors.append("rule_ids catalog drift")
    if list(corpus.get("process_classes") or []) != list(PROCESS_CLASSES):
        errors.append("process_classes catalog drift")
    surfaces = corpus.get("surface_hints")
    if surfaces != list(SURFACE_HINTS):
        errors.append("surface_hints catalog drift")

    weights = corpus.get("weights")
    if not isinstance(weights, dict):
        errors.append("weights must be object")
    else:
        for key, expected in DEFAULT_WEIGHTS.items():
            if weights.get(key) != expected:
                errors.append(f"weight {key} must be {expected}")
        if set(weights) != set(DEFAULT_WEIGHTS):
            errors.append("weights key set drift")

    thresholds = corpus.get("thresholds")
    if not isinstance(thresholds, dict):
        errors.append("thresholds must be object")
    else:
        for key, expected in DEFAULT_THRESHOLDS.items():
            if thresholds.get(key) != expected:
                errors.append(f"threshold {key} must be {expected}")
        if set(thresholds) != set(DEFAULT_THRESHOLDS):
            errors.append("thresholds key set drift")


def validate_structure(corpus: dict[str, Any], schema: dict[str, Any], errors: list[str]) -> None:
    if set(corpus.keys()) != set(TOP_REQUIRED):
        missing = set(TOP_REQUIRED) - set(corpus.keys())
        extra = set(corpus.keys()) - set(TOP_REQUIRED)
        if missing:
            errors.append(f"corpus missing keys: {sorted(missing)}")
        if extra:
            errors.append(f"corpus extra keys: {sorted(extra)}")

    if not isinstance(schema, dict) or schema.get("$id") is None:
        errors.append("schema missing $id")

    closed_tags = corpus.get("closed_tags")
    coverage_req = corpus.get("coverage_requirements")
    if not isinstance(closed_tags, list) or not closed_tags:
        errors.append("closed_tags must be non-empty list")
        closed_tags = []
    if not isinstance(coverage_req, list) or not coverage_req:
        errors.append("coverage_requirements must be non-empty list")
        coverage_req = []
    if list(closed_tags) != list(coverage_req):
        errors.append("closed_tags must equal coverage_requirements")

    matrix = corpus.get("coverage_matrix")
    if not isinstance(matrix, dict):
        errors.append("coverage_matrix must be object")
        matrix = {}
    for req in coverage_req:
        if req not in matrix:
            errors.append(f"coverage_matrix missing requirement {req}")
        elif not matrix[req]:
            errors.append(f"coverage_matrix empty for {req}")

    negatives = corpus.get("negative_cases")
    if not isinstance(negatives, list) or len(negatives) < 1:
        errors.append("negative_cases must be non-empty")
    else:
        neg_ids = [n.get("id") for n in negatives if isinstance(n, dict)]
        if len(neg_ids) != len(set(neg_ids)):
            errors.append("duplicate negative_case ids")
        for expected in ("N1", "N2", "N3", "N4", "N5", "N6"):
            if expected not in neg_ids:
                errors.append(f"missing negative case {expected}")

    governing = corpus.get("governing")
    if not isinstance(governing, dict):
        errors.append("governing must be object")
    else:
        if governing.get("network_on_decision_path") is not False:
            errors.append("governing.network_on_decision_path must be false")
        if governing.get("delivery_deadline_ms") != 1500:
            errors.append("governing.delivery_deadline_ms must be 1500")
        if governing.get("behavior_corpus_issue") != 138:
            errors.append("governing.behavior_corpus_issue must be 138")


def validate_fixtures(corpus: dict[str, Any], errors: list[str]) -> list[dict[str, Any]]:
    fixtures = corpus.get("fixtures")
    if not isinstance(fixtures, list) or not fixtures:
        errors.append("fixtures must be non-empty list")
        return []

    closed_tags = set(corpus.get("closed_tags") or [])
    neg_ids = {
        n["id"]
        for n in (corpus.get("negative_cases") or [])
        if isinstance(n, dict) and "id" in n
    }
    seen_ids: set[str] = set()
    matrix = corpus.get("coverage_matrix") or {}
    tag_to_fixtures: dict[str, list[str]] = {k: [] for k in closed_tags}

    for fixture in fixtures:
        if not isinstance(fixture, dict):
            errors.append("fixture is not an object")
            continue
        if set(fixture.keys()) != FIXTURE_KEYS:
            missing = FIXTURE_KEYS - set(fixture.keys())
            extra = set(fixture.keys()) - FIXTURE_KEYS
            fid = fixture.get("id", "?")
            if missing:
                errors.append(f"{fid}: missing keys {sorted(missing)}")
            if extra:
                errors.append(f"{fid}: extra keys {sorted(extra)}")

        fid = fixture.get("id")
        if not isinstance(fid, str) or not FIXTURE_ID_RE.match(fid):
            errors.append(f"invalid fixture id: {fid!r}")
            continue
        if fid in seen_ids:
            errors.append(f"duplicate fixture id {fid}")
        seen_ids.add(fid)

        if fixture.get("policy") not in POLICIES:
            errors.append(f"{fid}: unknown policy")
        if fixture.get("provider_state") not in PROVIDER_STATES:
            errors.append(f"{fid}: unknown provider_state")
        if fixture.get("surface_hint") not in SURFACE_HINTS:
            errors.append(f"{fid}: unknown surface_hint")
        if not isinstance(fixture.get("primary_text"), str):
            errors.append(f"{fid}: primary_text must be string")

        ph = fixture.get("process_hint")
        if ph is not None:
            if not isinstance(ph, dict) or set(ph.keys()) != {"class", "name"}:
                errors.append(f"{fid}: process_hint shape")
            elif ph.get("class") not in PROCESS_CLASSES:
                errors.append(f"{fid}: process_hint.class invalid")

        timing = fixture.get("timing")
        if timing is not None:
            if not isinstance(timing, dict):
                errors.append(f"{fid}: timing must be object or null")
            else:
                if timing.get("certainty") not in {"clear", "uncertain"}:
                    errors.append(f"{fid}: timing.certainty invalid")
                if not isinstance(timing.get("max_pause_ms"), int) or timing["max_pause_ms"] < 0:
                    errors.append(f"{fid}: timing.max_pause_ms invalid")

        kind = fixture.get("kind_hint")
        if kind not in {"everyday_message", "developer_prompt", None}:
            errors.append(f"{fid}: kind_hint invalid")

        expected = fixture.get("expected")
        if not isinstance(expected, dict) or set(expected.keys()) != EXPECTED_KEYS:
            errors.append(f"{fid}: expected key set")
        else:
            if expected.get("route") not in ROUTES:
                errors.append(f"{fid}: expected.route invalid")
            if expected.get("cloud_request") not in CLOUD_REQUESTS:
                errors.append(f"{fid}: expected.cloud_request invalid")
            if expected.get("rule_id") not in RULE_IDS:
                errors.append(f"{fid}: expected.rule_id invalid")
            # Pairing invariants
            route = expected.get("route")
            cloud = expected.get("cloud_request")
            if route == "literal_identity" and cloud != "not_allowed":
                errors.append(f"{fid}: literal must not_allowed cloud")
            if route == "deterministic_local" and cloud != "not_allowed":
                errors.append(f"{fid}: local must not_allowed cloud")
            if route == "local_with_optional_cloud" and cloud not in {
                "allowed",
                "required",
            }:
                errors.append(f"{fid}: optional-cloud route needs allowed/required")
            if expected.get("min_complexity_score", 0) > expected.get(
                "max_complexity_score", 0
            ):
                errors.append(f"{fid}: min_complexity_score > max")

        tags = fixture.get("tags")
        if not isinstance(tags, list) or not tags:
            errors.append(f"{fid}: tags required")
        else:
            for tag in tags:
                if tag not in closed_tags:
                    errors.append(f"{fid}: tag {tag!r} not in closed_tags")
                else:
                    tag_to_fixtures.setdefault(tag, []).append(fid)

        for nid in fixture.get("negative_case_ids") or []:
            if nid not in neg_ids:
                errors.append(f"{fid}: unknown negative_case_id {nid}")

        for dpr in fixture.get("dpr_links") or []:
            if not isinstance(dpr, str) or not DPR_ID_RE.match(dpr):
                errors.append(f"{fid}: bad dpr_link {dpr!r}")

    # Coverage matrix consistency
    for tag, listed in matrix.items():
        if tag not in closed_tags:
            errors.append(f"coverage_matrix unknown tag {tag}")
            continue
        if not isinstance(listed, list):
            errors.append(f"coverage_matrix[{tag}] not list")
            continue
        for fid in listed:
            if fid not in seen_ids:
                errors.append(f"coverage_matrix[{tag}] unknown fixture {fid}")
            else:
                # fixture must carry the tag
                fix = next(f for f in fixtures if f.get("id") == fid)
                if tag not in (fix.get("tags") or []):
                    errors.append(f"coverage_matrix[{tag}] lists {fid} without tag")

    counts = corpus.get("dataset_counts")
    if isinstance(counts, dict):
        if counts.get("fixtures_total") != len(fixtures):
            errors.append("dataset_counts.fixtures_total mismatch")
        if counts.get("closed_tag_count") != len(closed_tags):
            errors.append("dataset_counts.closed_tag_count mismatch")
        if counts.get("coverage_requirement_count") != len(
            corpus.get("coverage_requirements") or []
        ):
            errors.append("dataset_counts.coverage_requirement_count mismatch")
        if counts.get("negative_case_count") != len(corpus.get("negative_cases") or []):
            errors.append("dataset_counts.negative_case_count mismatch")

        by_route: dict[str, int] = {r: 0 for r in ROUTES}
        by_cloud: dict[str, int] = {c: 0 for c in CLOUD_REQUESTS}
        by_policy: dict[str, int] = {p: 0 for p in POLICIES}
        by_surface: dict[str, int] = {
            "shell": 0,
            "terminal": 0,
            "coding_agent": 0,
            "gui_agent": 0,
            "messaging": 0,
            "browser": 0,
            "unknown": 0,
            "null": 0,
        }
        for f in fixtures:
            exp = f.get("expected") or {}
            by_route[exp.get("route", "")] = by_route.get(exp.get("route", ""), 0) + 1
            by_cloud[exp.get("cloud_request", "")] = (
                by_cloud.get(exp.get("cloud_request", ""), 0) + 1
            )
            by_policy[f.get("policy", "")] = by_policy.get(f.get("policy", ""), 0) + 1
            surf = f.get("surface_hint")
            key = "null" if surf is None else str(surf)
            by_surface[key] = by_surface.get(key, 0) + 1

        if counts.get("by_route") != by_route:
            errors.append(f"dataset_counts.by_route mismatch: {counts.get('by_route')} != {by_route}")
        if counts.get("by_cloud_request") != by_cloud:
            errors.append(
                f"dataset_counts.by_cloud_request mismatch: {counts.get('by_cloud_request')} != {by_cloud}"
            )
        if counts.get("by_policy") != by_policy:
            errors.append(
                f"dataset_counts.by_policy mismatch: {counts.get('by_policy')} != {by_policy}"
            )
        if counts.get("by_surface") != by_surface:
            errors.append(
                f"dataset_counts.by_surface mismatch: {counts.get('by_surface')} != {by_surface}"
            )

    return fixtures


def validate_router_decisions(corpus: dict[str, Any], errors: list[str]) -> None:
    weights = corpus["weights"]
    thresholds = corpus["thresholds"]
    for fixture in corpus["fixtures"]:
        fid = fixture["id"]
        decision = route_intent(
            observation_from_fixture(fixture), weights=weights, thresholds=thresholds
        )
        expected = fixture["expected"]
        if decision["route"] != expected["route"]:
            errors.append(
                f"{fid}: route {decision['route']} != expected {expected['route']} "
                f"(rule={decision['rule_id']}, score={decision['complexity_score']})"
            )
        if decision["cloud_request"] != expected["cloud_request"]:
            errors.append(
                f"{fid}: cloud_request {decision['cloud_request']} != "
                f"expected {expected['cloud_request']}"
            )
        if decision["rule_id"] != expected["rule_id"]:
            errors.append(
                f"{fid}: rule_id {decision['rule_id']} != expected {expected['rule_id']}"
            )
        score = decision["complexity_score"]
        if score < expected["min_complexity_score"] or score > expected["max_complexity_score"]:
            errors.append(
                f"{fid}: complexity_score {score} outside "
                f"[{expected['min_complexity_score']}, {expected['max_complexity_score']}] "
                f"contrib={decision['contributions']}"
            )

        # Negative-case specific checks
        negs = set(fixture.get("negative_case_ids") or [])
        if "N1" in negs and decision["cloud_request"] != "not_allowed":
            errors.append(f"{fid}: N1 violated (clouded simple everyday)")
        if "N2" in negs:
            if fixture["provider_state"] in AGREEMENT_FAST_PATH and decision[
                "cloud_request"
            ] != "not_allowed":
                errors.append(f"{fid}: N2 violated (agreement forced cloud)")
        if "N3" in negs:
            if fixture["provider_state"] == "protected_token_disagreement":
                if fixture["policy"] == "natural":
                    if decision["cloud_request"] != "not_allowed":
                        errors.append(f"{fid}: N3 natural must forbid cloud")
                else:
                    if decision["route"] != "local_with_optional_cloud":
                        errors.append(f"{fid}: N3 must keep cloud eligibility")
        if "N4" in negs and fixture["policy"] == "natural":
            if decision["cloud_request"] != "not_allowed":
                errors.append(f"{fid}: N4 natural clouded")
        if "N5" in negs and decision["cloud_request"] != "not_allowed":
            errors.append(f"{fid}: N5 timing opened cloud")
        if "N6" in negs:
            if fixture.get("surface_hint") is not None or fixture.get("process_hint") is not None:
                errors.append(f"{fid}: N6 fixture should be speech-only")
            if decision.get("surface_degraded") is not True:
                errors.append(f"{fid}: N6 surface_degraded should be true")


def validate_no_network_modules(errors: list[str]) -> None:
    """Decision path must not import or newly load network stacks."""
    # Source-level: this file must not import network modules.
    source = Path(__file__).read_text(encoding="utf-8")
    forbidden_import = re.compile(
        r"^\s*(?:import|from)\s+"
        r"(socket|ssl|http|http\.client|urllib|urllib\.request|urllib\.error|"
        r"urllib\.parse|requests|aiohttp|httpx|ftplib|smtplib)\b",
        re.M,
    )
    if forbidden_import.search(source):
        errors.append("prototype source imports network modules")

    # Runtime: a pure route_intent call must not newly load network modules.
    # Modules already present from interpreter bootstrap (e.g. socket on some
    # builds) are ignored; only *new* loads by the decision path fail the check.
    before = set(sys.modules)
    route_intent(
        {
            "policy": "adaptive",
            "primary_text": "cargo test --package voisu-core",
            "provider_state": "single_provider",
            "surface_hint": "shell",
            "process_hint": None,
            "timing": None,
        }
    )
    newly = set(sys.modules) - before
    for mod in sorted(newly):
        top = mod.split(".", 1)[0]
        if mod in NETWORK_MODULE_NAMES or top in NETWORK_MODULE_NAMES:
            errors.append(f"decision path newly imported network module {mod!r}")
            break


def validate_latency_budget(corpus: dict[str, Any], errors: list[str]) -> None:
    """Routing must be far under 1.5s with no sleeps — wall-clock sample."""
    # Ensure no time.sleep in this module source.
    source = Path(__file__).read_text(encoding="utf-8")
    if re.search(r"\btime\.sleep\s*\(", source):
        errors.append("prototype must not call time.sleep")

    weights = corpus["weights"]
    thresholds = corpus["thresholds"]
    fixtures = corpus["fixtures"]
    started = time.perf_counter()
    iterations = 200
    for _ in range(iterations):
        for fixture in fixtures:
            route_intent(
                observation_from_fixture(fixture),
                weights=weights,
                thresholds=thresholds,
            )
    elapsed = time.perf_counter() - started
    # 200 * corpus fixtures should still be well under a second on any reasonable host.
    if elapsed > 1.5:
        errors.append(
            f"routing latency sample {elapsed:.3f}s for {iterations} full corpus passes "
            f"exceeds 1.5s Delivery budget (decision path too slow)"
        )


def validate_package(corpus: dict[str, Any], schema: dict[str, Any]) -> list[str]:
    errors: list[str] = []
    if not isinstance(corpus, dict):
        return ["corpus root must be object"]
    validate_catalog_constants(corpus, errors)
    validate_structure(corpus, schema, errors)
    fixtures = validate_fixtures(corpus, errors)
    if fixtures and not any(e.startswith("weight") for e in errors):
        validate_router_decisions(corpus, errors)
    validate_no_network_modules(errors)
    if fixtures:
        validate_latency_budget(corpus, errors)
    return errors


# ---------------------------------------------------------------------------
# Mutations — each must produce at least one diagnostic
# ---------------------------------------------------------------------------


def run_mutations(corpus: dict[str, Any], schema: dict[str, Any]) -> list[str]:
    failures: list[str] = []

    def find(c: dict[str, Any], pred: Any) -> dict[str, Any]:
        for f in c["fixtures"]:
            if pred(f):
                return f
        raise KeyError("fixture not found")

    def expect_fail(name: str, mutator: Any, needles: list[str]) -> None:
        clone = copy.deepcopy(corpus)
        try:
            mutator(clone)
        except Exception as exc:  # noqa: BLE001 — mutation harness
            failures.append(f"mutation {name} crashed before validate: {exc}")
            return
        errs = validate_package(clone, schema)
        if not errs:
            failures.append(f"mutation {name} expected failure, got clean package")
            return
        blob = " | ".join(errs)
        if not any(n.lower() in blob.lower() for n in needles):
            failures.append(
                f"mutation {name} failed without expected needle {needles!r}; got: {errs[:3]}"
            )

    expect_fail(
        "cloud_simple_everyday",
        lambda c: (
            find(c, lambda x: x["id"] == "IRI-01")["expected"].__setitem__(
                "route", "local_with_optional_cloud"
            )
            or find(c, lambda x: x["id"] == "IRI-01")["expected"].__setitem__(
                "cloud_request", "allowed"
            )
            or find(c, lambda x: x["id"] == "IRI-01")["expected"].__setitem__(
                "rule_id", "R_COMPLEX_CLOUD"
            )
        ),
        ["IRI-01", "route"],
    )

    expect_fail(
        "skip_protected_disagreement_cloud",
        lambda c: (
            find(c, lambda x: x["id"] == "IRI-10")["expected"].__setitem__(
                "route", "deterministic_local"
            )
            or find(c, lambda x: x["id"] == "IRI-10")["expected"].__setitem__(
                "cloud_request", "not_allowed"
            )
            or find(c, lambda x: x["id"] == "IRI-10")["expected"].__setitem__(
                "rule_id", "R_DEFAULT_LOCAL"
            )
        ),
        ["IRI-10"],
    )

    expect_fail(
        "natural_complex_clouds",
        lambda c: (
            find(c, lambda x: x["id"] == "IRI-15")["expected"].__setitem__(
                "route", "local_with_optional_cloud"
            )
            or find(c, lambda x: x["id"] == "IRI-15")["expected"].__setitem__(
                "cloud_request", "allowed"
            )
            or find(c, lambda x: x["id"] == "IRI-15")["expected"].__setitem__(
                "rule_id", "R_COMPLEX_CLOUD"
            )
        ),
        ["IRI-15"],
    )

    expect_fail(
        "threshold_drift",
        lambda c: c["thresholds"].__setitem__("complexity_cloud", 99),
        ["threshold complexity_cloud"],
    )

    expect_fail(
        "weight_drift",
        lambda c: c["weights"].__setitem__("section_goal", 1),
        ["weight section_goal"],
    )

    expect_fail(
        "literal_with_cloud",
        lambda c: find(c, lambda x: x["id"] == "IRI-03")["expected"].__setitem__(
            "cloud_request", "allowed"
        ),
        ["literal must not_allowed"],
    )

    expect_fail(
        "unknown_tag",
        lambda c: find(c, lambda x: x["id"] == "IRI-01")["tags"].append("not-a-real-tag"),
        ["not-a-real-tag"],
    )

    expect_fail(
        "coverage_matrix_orphan",
        lambda c: c["coverage_matrix"]["simple-everyday"].append("IRI-99"),
        ["IRI-99"],
    )

    expect_fail(
        "drop_negative_n3",
        lambda c: c.__setitem__(
            "negative_cases", [n for n in c["negative_cases"] if n["id"] != "N3"]
        ),
        ["N3"],
    )

    expect_fail(
        "counts_fixture_total",
        lambda c: c["dataset_counts"].__setitem__("fixtures_total", 1),
        ["fixtures_total"],
    )

    expect_fail(
        "extra_top_key",
        lambda c: c.__setitem__("unexpected_top", True),
        ["extra keys"],
    )

    expect_fail(
        "network_flag_true",
        lambda c: c["governing"].__setitem__("network_on_decision_path", True),
        ["network_on_decision_path"],
    )

    expect_fail(
        "force_agreement_cloud_expectation",
        lambda c: (
            find(c, lambda x: x["id"] == "IRI-06")["expected"].__setitem__(
                "route", "local_with_optional_cloud"
            )
            or find(c, lambda x: x["id"] == "IRI-06")["expected"].__setitem__(
                "cloud_request", "allowed"
            )
            or find(c, lambda x: x["id"] == "IRI-06")["expected"].__setitem__(
                "rule_id", "R_COMPLEX_CLOUD"
            )
        ),
        ["IRI-06"],
    )

    expect_fail(
        "structured_simple_required",
        lambda c: (
            find(c, lambda x: x["id"] == "IRI-17")["expected"].__setitem__(
                "route", "local_with_optional_cloud"
            )
            or find(c, lambda x: x["id"] == "IRI-17")["expected"].__setitem__(
                "cloud_request", "required"
            )
            or find(c, lambda x: x["id"] == "IRI-17")["expected"].__setitem__(
                "rule_id", "R_COMPLEX_CLOUD"
            )
        ),
        ["IRI-17"],
    )

    expect_fail(
        "shell_prose_literal",
        lambda c: (
            find(c, lambda x: x["id"] == "IRI-26")["expected"].__setitem__(
                "route", "literal_identity"
            )
            or find(c, lambda x: x["id"] == "IRI-26")["expected"].__setitem__(
                "rule_id", "R_LITERAL_COMMAND"
            )
        ),
        ["IRI-26"],
    )

    expect_fail(
        "timing_opens_cloud",
        lambda c: (
            find(c, lambda x: x["id"] == "IRI-18")["expected"].__setitem__(
                "route", "local_with_optional_cloud"
            )
            or find(c, lambda x: x["id"] == "IRI-18")["expected"].__setitem__(
                "cloud_request", "allowed"
            )
            or find(c, lambda x: x["id"] == "IRI-18")["expected"].__setitem__(
                "rule_id", "R_COMPLEX_CLOUD"
            )
        ),
        ["IRI-18"],
    )

    expect_fail(
        "duplicate_fixture_id",
        lambda c: c["fixtures"].append(copy.deepcopy(c["fixtures"][0])),
        ["duplicate fixture id"],
    )

    expect_fail(
        "bad_process_class",
        lambda c: find(c, lambda x: x["id"] == "IRI-30")["process_hint"].__setitem__(
            "class", "spreadsheet"
        ),
        ["process_hint.class"],
    )

    # Direct adversarial decision checks (not corpus mutation)
    def adversarial_decisions() -> None:
        # Simple everyday must not cloud
        d = route_intent(
            {
                "policy": "adaptive",
                "primary_text": "hey can you send the notes when you get a chance",
                "provider_state": "single_provider",
                "surface_hint": "messaging",
                "process_hint": None,
                "timing": None,
            }
        )
        if d["cloud_request"] != "not_allowed" or d["route"] != "deterministic_local":
            failures.append(
                f"adversarial simple everyday clouded: {d['route']}/{d['cloud_request']}"
            )

        # Protected disagreement must allow cloud under adaptive
        d = route_intent(
            {
                "policy": "adaptive",
                "primary_text": "file the bug in voisu",
                "provider_state": "protected_token_disagreement",
                "surface_hint": None,
                "process_hint": None,
                "timing": None,
            }
        )
        if d["route"] != "local_with_optional_cloud" or d["cloud_request"] != "allowed":
            failures.append(
                f"adversarial protected disagreement missed cloud: {d['route']}/{d['cloud_request']}"
            )

        # Dispute wins over preformatted / cargo literal under adaptive
        for text, surf in (
            ("1. Build\n2. Test\n3. Ship", None),
            ("cargo test --package voisu-core", "shell"),
        ):
            d = route_intent(
                {
                    "policy": "adaptive",
                    "primary_text": text,
                    "provider_state": "protected_token_disagreement",
                    "surface_hint": surf,
                    "process_hint": None,
                    "timing": None,
                }
            )
            if d["rule_id"] != "R_DISPUTE_CLOUD" or d["cloud_request"] != "allowed":
                failures.append(
                    f"adversarial disputed literal missed cloud: {text!r} -> "
                    f"{d['rule_id']}/{d['cloud_request']}"
                )

        # Natural forbids cloud on disagreement
        d = route_intent(
            {
                "policy": "natural",
                "primary_text": "file the bug in voisu",
                "provider_state": "protected_token_disagreement",
                "surface_hint": None,
                "process_hint": None,
                "timing": None,
            }
        )
        if d["cloud_request"] != "not_allowed":
            failures.append("adversarial natural allowed cloud on disagreement")

        # Shell prose false literals must stay local
        for text in (
            "run errands tomorrow",
            "make dinner later",
            "go shopping now",
            "python is great",
        ):
            d = route_intent(
                {
                    "policy": "adaptive",
                    "primary_text": text,
                    "provider_state": "single_provider",
                    "surface_hint": "shell",
                    "process_hint": None,
                    "timing": None,
                }
            )
            if d["route"] != "deterministic_local" or d["cloud_request"] != "not_allowed":
                failures.append(
                    f"adversarial shell prose literalized: {text!r} -> "
                    f"{d['route']}/{d['rule_id']}"
                )

        # Ordinary compound prose must not trip section cues to threshold
        d = route_intent(
            {
                "policy": "adaptive",
                "primary_text": "the project goal depends on business context",
                "provider_state": "single_provider",
                "surface_hint": None,
                "process_hint": None,
                "timing": None,
            }
        )
        if d["route"] != "deterministic_local" or d["complexity_score"] >= 24:
            failures.append(
                f"adversarial compound prose clouded: score={d['complexity_score']} "
                f"route={d['route']}"
            )

        # Missing surface does not hard-fail
        d = route_intent(
            {
                "policy": "adaptive",
                "primary_text": "goal one context two requirements three",
                "provider_state": "single_provider",
                "surface_hint": None,
                "process_hint": None,
                "timing": None,
            }
        )
        if d["surface_degraded"] is not True:
            failures.append("adversarial missing surface did not degrade")
        if d["route"] != "local_with_optional_cloud":
            failures.append(
                f"adversarial speech-only multi-section missed cloud: {d}"
            )

        # Exact agreement simple stays local
        d = route_intent(
            {
                "policy": "adaptive",
                "primary_text": "please let me know if that timeline works",
                "provider_state": "exact_agreement",
                "surface_hint": None,
                "process_hint": None,
                "timing": None,
            }
        )
        if d["cloud_request"] != "not_allowed":
            failures.append("adversarial exact agreement forced cloud")

        # Reproducibility: same inputs → same decision twice
        obs = {
            "policy": "structured",
            "primary_text": "goal fix the flaky auth test context it fails on CI only",
            "provider_state": "single_provider",
            "surface_hint": "coding_agent",
            "process_hint": None,
            "timing": None,
        }
        a = route_intent(obs)
        b = route_intent(obs)
        if a != b:
            failures.append("adversarial non-reproducible decision")

    adversarial_decisions()
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
    print("OK: developer-prompt-rendering-intent-routing package")
    print(f"  version: {corpus.get('version')}")
    print(f"  fixtures: {counts['fixtures_total']}")
    print(f"  by_route: {counts['by_route']}")
    print(f"  by_cloud_request: {counts['by_cloud_request']}")
    print(f"  by_policy: {counts['by_policy']}")
    print(f"  by_surface: {counts['by_surface']}")
    print(f"  negative_cases: {counts['negative_case_count']}")
    print(f"  coverage_requirements: {counts['coverage_requirement_count']}")
    print(f"  closed_tags: {counts['closed_tag_count']}")
    print(f"  complexity_cloud_threshold: {corpus['thresholds']['complexity_cloud']}")
    print(f"  delivery_deadline_ms: {corpus['thresholds']['delivery_deadline_ms']}")
    print("  network_on_decision_path: false")
    print("  mutations: 18 property-bound + 9 adversarial")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
