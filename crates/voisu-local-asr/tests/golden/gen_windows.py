"""Regenerate windows.json from the Python reference splitter.

Run from the repository root:

    python3 -I crates/voisu-local-asr/tests/golden/gen_windows.py \
        tools/local-asr crates/voisu-local-asr/tests/golden/windows.json

Every fixture is synthetic PCM built from integer-only segments, so the Rust
test (`tests/window_golden.rs`) rebuilds the same samples bit for bit. No real
audio is involved.
"""

from __future__ import annotations

import array
import json
import sys

RATE = 16_000


def segment_samples(kind: str, count: int, amp: int, seed: int) -> list[int]:
    if kind == "silence":
        return [0] * count
    if kind == "noise":
        # 31-bit LCG; modulo of a non-negative value is the same in both languages.
        state = seed
        out = []
        for _ in range(count):
            state = (state * 1103515245 + 12345) % 2147483648
            out.append(((state >> 16) % (2 * amp + 1)) - amp)
        return out
    if kind == "square":
        half = seed  # half period in samples
        return [amp if (i // half) % 2 == 0 else -amp for i in range(count)]
    raise ValueError(kind)


def build(segments: list[list]) -> bytes:
    samples: list[int] = []
    for index, (kind, count, amp) in enumerate(segments):
        samples.extend(segment_samples(kind, count, amp, 40 if kind == "square" else index + 1))
    pcm = array.array("h", samples)
    if sys.byteorder != "little":
        pcm.byteswap()
    return pcm.tobytes()


def s(seconds: float) -> int:
    return int(round(seconds * RATE))


FIXTURES = {
    "29s_speech": [["noise", s(29), 8000]],
    "31s_with_pause": [["noise", s(27.5), 8000], ["silence", s(0.3), 0], ["noise", s(3.2), 8000]],
    "61s_two_pauses": [
        ["noise", s(26), 6000],
        ["noise", s(1), 300],
        ["noise", s(29), 6000],
        ["silence", s(0.5), 0],
        ["noise", s(4.5), 6000],
    ],
    "600s_at_the_cap": [
        item
        for _ in range(24)
        for item in (["noise", s(24.5), 7000], ["silence", s(0.5), 0])
    ],
    "tail_under_1s_folds": [["noise", s(30.5), 8000]],
    "tone_without_dip": [["square", s(45), 9000]],
    "silence_then_speech": [["silence", s(10), 0], ["noise", s(25), 5000]],
    "all_zero_40s": [["silence", s(40), 0]],
    "uneven_pauses_95s": [
        ["noise", s(21), 7000],
        ["silence", s(0.25), 0],
        ["noise", s(8), 7000],
        ["noise", s(0.4), 900],
        ["noise", s(32), 7000],
        ["silence", s(0.6), 0],
        ["noise", s(33), 7000],
    ],
}


def main() -> int:
    sys.path.insert(0, sys.argv[1])
    from voisu_asr.windowing import WindowingError, plan_windows

    cases = []
    for name, segments in FIXTURES.items():
        windows = plan_windows(build(segments))
        cases.append(
            {
                "name": name,
                "segments": segments,
                "windows": [[w.start, w.end, w.cut] for w in windows],
            }
        )
    over = [["silence", 600 * RATE + 1, 0]]
    try:
        plan_windows(build(over))
    except WindowingError as exc:
        cases.append({"name": "over_the_cap", "segments": over, "error": exc.code})
    else:
        raise SystemExit("over_the_cap did not fail")
    with open(sys.argv[2], "w", encoding="utf-8") as out:
        json.dump({"cases": cases}, out, indent=1)
        out.write("\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
