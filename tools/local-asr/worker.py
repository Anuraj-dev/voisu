#!/usr/bin/env python3
"""Private final-only ASR worker.

Usage is intentionally absolute-path based; the Rust supervisor owns the
manifest and launch restrictions.
"""

from __future__ import annotations

import argparse
import sys
from pathlib import Path


_ROOT = Path(__file__).resolve().parent
if str(_ROOT) not in sys.path:
    sys.path.insert(0, str(_ROOT))

from voisu_asr.service import run  # noqa: E402


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Voisu private local ASR worker")
    parser.add_argument("--manifest", required=True, type=Path)
    args = parser.parse_args(argv)
    return run(args.manifest)


if __name__ == "__main__":
    raise SystemExit(main())
