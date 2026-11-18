"""Regenerate echo.json from the Python reference `_is_prompt_echo`.

Run from the repository root:

    python3 -I crates/voisu-local-asr/tests/golden/gen_echo.py \
        tools/local-asr crates/voisu-local-asr/tests/golden/echo.json

The cases are the worker's PromptEchoTests plus a few edge cases; the
expected verdicts come from running the Python function, not by hand.
"""

from __future__ import annotations

import json
import sys

PLAIN = "Voisu, Tokio, wayfinder"
GLOSSARY4 = "Glossary: Voisu, Tokio, wayfinder, Rust."
GLOSSARY3 = "Glossary: Voisu, Tokio, wayfinder."
BUILTIN = "Glossary: Claude, Claude Code, Codex, OpenAI, Anthropic, GPT, LLM, Groq, Deepgram, Whisper, systemctl."

CASES = [
    (PLAIN, text)
    for text in [
        "Voisu, Tokio, wayfinder",
        "Voisu, Tokio, wayfinder.",
        "voisu tokio wayfinder",
        "VOISU,  tokio ,wayfinder .",
        "Voisu",
        "Voisu is great",
        "Voisu, Tokio and more",
        "Voisu, Tokio, and Rust",
        "Voisu, Tokio",
        "voisu tokio",
        "wayfinder, Tokio, Voisu.",
        "Voisu Voisu",
        "Voisu Voisu Voisu",
        "Voisu, wayfinder, Tokio.",
        "I use Voisu, Tokio, wayfinder every day.",
        "real words",
        "",
    ]
] + [
    (GLOSSARY4, text)
    for text in [
        "Voisu, Tokio, wayfinder",
        "Tokio, wayfinder, Rust.",
        "Glossary: Voisu, Tokio, wayfinder, Rust.",
        "glossary voisu tokio wayfinder",
        "Glossary Voisu Tokio",
    ]
] + [
    (GLOSSARY3, text)
    for text in [
        "Voisu",
        "Glossary: Voisu",
        "the glossary has Voisu, Tokio",
        "Voisu, Tokio and more",
        "Voisu, Tokio, wayfinder.",
    ]
] + [
    (BUILTIN, text)
    for text in [
        "OpenAI, Anthropic.",
        "GPT LLM",
        "Whisper Deepgram",
        "Claude Code, Codex.",
        "Claude, Claude Code, Codex, OpenAI.",
        "Glossary: Claude, Claude Code, Codex.",
        "Claude Code, Codex, OpenAI",
        "We asked Claude, Claude Code, Codex, OpenAI about it.",
    ]
] + [
    ("Voisu, Tokio", "Voisu, Tokio"),
    ("a.b, c+d, e(f)", "a.b c+d e(f)"),
    ("a.b, c+d, e(f)", "axb c+d e(f)"),
]


def main() -> int:
    sys.path.insert(0, sys.argv[1])
    from voisu_asr.service import _is_prompt_echo

    rows = [{"prompt": p, "text": t, "echo": _is_prompt_echo(t, p)} for p, t in CASES]
    with open(sys.argv[2], "w", encoding="utf-8") as out:
        json.dump({"cases": rows}, out, indent=1, ensure_ascii=False)
        out.write("\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
