#!/usr/bin/env python3
"""Private final-only ASR worker.

Usage is intentionally absolute-path based; the Rust supervisor owns the
manifest and launch restrictions.
"""

from __future__ import annotations

import argparse
import os
import sys
from pathlib import Path
from typing import BinaryIO


_ROOT = Path(__file__).resolve().parent
if str(_ROOT) not in sys.path:
    sys.path.insert(0, str(_ROOT))

from voisu_asr.service import run  # noqa: E402


def claim_protocol_stdout() -> BinaryIO:
    """Reserve the original stdout for protocol frames; send fd 1 to stderr.

    Native runtimes (whisper.cpp, CTranslate2, CUDA, torch) print through C
    stdio at load, inference, and exit, sometimes buffered past any scoped
    redirect. With fd 1 pointing at stderr for the whole worker lifetime, no
    native output can interleave with a frame.
    """

    sys.stdout.flush()
    protocol = os.fdopen(os.dup(1), "wb")
    os.dup2(2, 1)
    return protocol


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Voisu private local ASR worker")
    parser.add_argument("--manifest", required=True, type=Path)
    args = parser.parse_args(argv)
    return run(args.manifest, stdout=claim_protocol_stdout())


if __name__ == "__main__":
    raise SystemExit(main())
