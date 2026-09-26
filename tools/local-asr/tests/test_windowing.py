"""Windowing contract: bounded per-call input, isolation, and final-only output.

These are fake-adapter tests. They prove the worker's windowing contract, not
CUDA memory, model quality, or boundary word accuracy.
"""

import array
import io
import json
from pathlib import Path
import sys
import tempfile
from types import SimpleNamespace
import unittest
import wave

ASR_ROOT = Path(__file__).parents[1]
if str(ASR_ROOT) not in sys.path:
    sys.path.insert(0, str(ASR_ROOT))
from voisu_asr import MAX_PCM_BYTES
from voisu_asr import service as asr_service
from voisu_asr import windowing
from voisu_asr.adapters import AdapterError, AdapterMetadata, BaseAdapter
from voisu_asr.protocol import read_json_frame

RATE = 16_000
WINDOW = windowing.WINDOW_SAMPLES


def loud(samples: int, amplitude: int = 8000) -> array.array:
    """Steady sound with no dip: every 100 ms frame has the same RMS."""

    return array.array("h", (amplitude if index % 2 else -amplitude for index in range(samples)))


def silence(samples: int) -> array.array:
    return array.array("h", bytes(samples * 2))


def pcm_of(*parts: array.array) -> bytes:
    return b"".join(part.tobytes() for part in parts)


def assert_contiguous(test: unittest.TestCase, windows, total: int) -> None:
    test.assertEqual(windows[0].start, 0)
    test.assertEqual(windows[-1].end, total)
    for before, after in zip(windows, windows[1:]):
        test.assertEqual(before.end, after.start)
    for window in windows:
        test.assertGreater(window.end, window.start)
        test.assertLess(window.end - window.start, WINDOW + windowing.MIN_FINAL_WINDOW_SAMPLES)


class PlanWindowsTests(unittest.TestCase):
    def test_recording_within_one_window_is_not_split(self):
        for samples in (1, RATE, WINDOW):
            windows = windowing.plan_windows(pcm_of(loud(samples)))
            self.assertEqual([(w.start, w.end, w.cut) for w in windows], [(0, samples, "end")])

    def test_steady_sound_takes_a_hard_cut_at_thirty_seconds(self):
        windows = windowing.plan_windows(pcm_of(loud(WINDOW + RATE)))
        self.assertEqual([(w.start, w.end, w.cut) for w in windows], [(0, WINDOW, "hard"), (WINDOW, WINDOW + RATE, "end")])

    def test_a_final_remainder_under_one_second_joins_the_previous_window(self):
        hard = windowing.plan_windows(pcm_of(loud(WINDOW + RATE - 1)))
        self.assertEqual([(w.start, w.end, w.cut) for w in hard], [(0, WINDOW + RATE - 1, "end")])
        # A quiet cut at ~29.9 s would leave a ~0.6 s tail; it is folded too.
        quiet = windowing.plan_windows(pcm_of(loud(WINDOW - RATE // 5), silence(RATE // 5), loud(RATE // 2)))
        self.assertEqual([(w.start, w.end) for w in quiet], [(0, WINDOW + RATE // 2)])
        after_quiet = windowing.plan_windows(pcm_of(loud(27 * RATE), silence(RATE // 5), loud(40 * RATE + RATE // 2)))
        self.assertEqual(after_quiet[-1].end, 67 * RATE + RATE // 5 + RATE // 2)
        self.assertGreaterEqual(after_quiet[-1].end - after_quiet[-1].start, RATE)

    def test_cut_lands_inside_a_pause_in_the_last_five_seconds(self):
        pause_start, pause_samples = 27 * RATE, RATE // 5
        pcm = pcm_of(loud(pause_start), silence(pause_samples), loud(20 * RATE))
        first = windowing.plan_windows(pcm)[0]
        self.assertEqual(first.cut, "quiet")
        self.assertGreaterEqual(first.end, pause_start)
        self.assertLessEqual(first.end, pause_start + pause_samples)

    def test_a_pause_before_the_search_region_is_ignored(self):
        pcm = pcm_of(loud(20 * RATE), silence(RATE // 5), loud(20 * RATE))
        first = windowing.plan_windows(pcm)[0]
        self.assertEqual((first.end, first.cut), (WINDOW, "hard"))

    def test_a_shallow_dip_is_not_treated_as_a_pause(self):
        pcm = pcm_of(loud(27 * RATE), loud(RATE // 5, amplitude=7000), loud(20 * RATE))
        first = windowing.plan_windows(pcm)[0]
        self.assertEqual((first.end, first.cut), (WINDOW, "hard"))

    def test_the_largest_accepted_recording_fits_twenty_windows(self):
        total = MAX_PCM_BYTES // 2
        windows = windowing.plan_windows(pcm_of(loud(total)))
        self.assertEqual(len(windows), windowing.MAX_WINDOWS)
        assert_contiguous(self, windows, total)

    def test_early_quiet_cuts_never_push_a_long_recording_past_the_window_cap(self):
        # Pauses every 25.5 s would give 22 windows for 540 s if every pause
        # were taken; the planner takes early ones, then must hard-cut.
        total = 18 * WINDOW
        samples = loud(total)
        for pause in range(25 * RATE + RATE // 2, total, 25 * RATE + RATE // 2):
            samples[pause : pause + RATE // 5] = silence(min(RATE // 5, total - pause))
        windows = windowing.plan_windows(samples.tobytes())
        self.assertLessEqual(len(windows), windowing.MAX_WINDOWS)
        assert_contiguous(self, windows, total)
        cuts = [window.cut for window in windows]
        self.assertEqual(cuts[0], "quiet")
        self.assertIn("hard", cuts)

    def test_recording_beyond_twenty_windows_is_rejected(self):
        with self.assertRaises(windowing.WindowingError) as caught:
            windowing.plan_windows(pcm_of(loud(windowing.MAX_WINDOWS * WINDOW + 1)))
        self.assertEqual(caught.exception.code, "too_many_windows")


class TranscribeWindowsTests(unittest.TestCase):
    def test_each_call_receives_only_its_own_window_and_texts_join_with_one_space(self):
        pcm = pcm_of(loud(2 * WINDOW + RATE))
        windows = windowing.plan_windows(pcm)
        received: list[bytes] = []

        def fake(window: bytes) -> str:
            received.append(window)
            return f"  part{len(received)} \n"

        text = windowing.transcribe_windows(pcm, fake)
        self.assertEqual(text, "part1 part2 part3")
        self.assertEqual(received, [window.pcm(pcm) for window in windows])
        self.assertTrue(all(len(window) <= WINDOW * 2 for window in received))

    def test_a_model_that_rejects_sub_second_input_still_transcribes_a_barely_long_recording(self):
        def rejects_short(window: bytes) -> str:
            if len(window) < 2 * RATE:
                raise AdapterError("inference_failed", "input shorter than 1 s")
            return "words"

        for extra in (1, 16, RATE - 1):
            self.assertEqual(windowing.transcribe_windows(pcm_of(loud(WINDOW + extra)), rejects_short), "words")

    def test_failed_window_stops_the_request_with_no_text(self):
        pcm = pcm_of(loud(3 * WINDOW))
        calls: list[int] = []

        def fake(window: bytes) -> str:
            calls.append(len(window))
            if len(calls) == 2:
                raise AdapterError("oom", "CUDA out of memory")
            return "words"

        with self.assertRaises(AdapterError):
            windowing.transcribe_windows(pcm, fake)
        self.assertEqual(len(calls), 2)

    def test_empty_windows_are_dropped_and_all_zero_windows_are_not_transcribed(self):
        pcm = pcm_of(loud(WINDOW), silence(WINDOW), loud(10 * RATE))
        replies = iter(["", "third"])
        seen: list[bytes] = []

        def fake(window: bytes) -> str:
            seen.append(window)
            return next(replies)

        # The silent middle window is skipped; the empty reply is dropped.
        self.assertEqual(windowing.transcribe_windows(pcm, fake), "third")
        self.assertEqual(len(windowing.plan_windows(pcm)), 3)
        self.assertEqual(len(seen), 2)

    def test_joined_transcript_over_24_kib_is_an_error(self):
        pcm = pcm_of(loud(2 * WINDOW))
        with self.assertRaises(windowing.WindowingError) as caught:
            windowing.transcribe_windows(pcm, lambda window: "x" * (windowing.MAX_TRANSCRIPT_BYTES // 2 + 1))
        self.assertEqual(caught.exception.code, "transcript_too_large")


class ScriptedAdapter(BaseAdapter):
    """Answers the health window, then one scripted reply per request window."""

    def __init__(self, replies, gpu_peaks=None):
        self.replies = list(replies)
        self.gpu_peaks = list(gpu_peaks or [])
        self.calls: list[int] = []
        self.metadata = AdapterMetadata("fake", "cuda:0", False, "fake", 100, 0)

    def transcribe(self, pcm: bytes) -> str:
        self.calls.append(len(pcm))
        if len(self.calls) == 1:
            return "health phrase"
        peak = self.gpu_peaks.pop(0) if self.gpu_peaks else 0
        self.metadata = AdapterMetadata("fake", "cuda:0", True, "fake", 100 + peak, peak)
        reply = self.replies.pop(0)
        if isinstance(reply, Exception):
            raise reply
        return reply


def frame(value: dict) -> bytes:
    payload = json.dumps(value).encode("utf-8")
    return len(payload).to_bytes(4, "little") + payload


def serve(adapter: BaseAdapter, pcm: bytes) -> dict:
    with tempfile.TemporaryDirectory() as tmp:
        health = Path(tmp) / "health.wav"
        with wave.open(str(health), "wb") as handle:
            handle.setnchannels(1)
            handle.setsampwidth(2)
            handle.setframerate(RATE)
            handle.writeframes(pcm_of(loud(RATE)))
        manifest = SimpleNamespace(
            model_id="fake",
            options={"health_audio": str(health), "health_expected": "health phrase"},
        )
        reader = io.BytesIO(
            frame({"v": 1, "op": "prepare", "id": "p"})
            + frame({"v": 1, "op": "transcribe", "id": "t", "pcm_bytes": len(pcm)})
            + pcm
        )
        writer = io.BytesIO()
        asr_service.serve_stream(manifest, adapter, reader, writer)
    replies = io.BytesIO(writer.getvalue())
    ready = read_json_frame(replies)
    assert ready is not None and ready["status"] == "ready", ready
    reply = read_json_frame(replies)
    assert reply is not None
    return reply


class ServiceWindowingTests(unittest.TestCase):
    def test_long_recording_returns_one_joined_transcript_with_the_max_window_peak(self):
        adapter = ScriptedAdapter(["one", "two", "three"], gpu_peaks=[300, 900, 500])
        reply = serve(adapter, pcm_of(loud(2 * WINDOW + RATE)))
        self.assertEqual(reply["status"], "transcript")
        self.assertEqual(reply["text"], "one two three")
        self.assertEqual(reply["peak_gpu_memory_bytes"], 900)
        self.assertEqual(reply["peak_host_ram_bytes"], 1000)
        self.assertEqual(adapter.calls[1:], [WINDOW * 2, WINDOW * 2, RATE * 2])

    def test_one_window_with_an_unknown_gpu_peak_makes_the_request_peak_unknown(self):
        adapter = ScriptedAdapter(["one", "two", "three"], gpu_peaks=[300, 0, 500])
        reply = serve(adapter, pcm_of(loud(2 * WINDOW + RATE)))
        self.assertEqual(reply["status"], "transcript")
        self.assertEqual(reply["peak_gpu_memory_bytes"], 0)

    def test_a_failed_window_is_an_error_with_no_partial_text(self):
        adapter = ScriptedAdapter(["one", AdapterError("oom", "CUDA out of memory"), "three"])
        reply = serve(adapter, pcm_of(loud(3 * WINDOW)))
        self.assertEqual((reply["status"], reply["code"]), ("error", "oom"))
        self.assertNotIn("text", reply)
        self.assertEqual(len(adapter.calls), 3)  # health + two windows; the third never ran

    def test_all_empty_windows_are_no_text(self):
        reply = serve(ScriptedAdapter(["", " "]), pcm_of(loud(2 * WINDOW)))
        self.assertEqual(reply["status"], "no_text")

    def test_all_zero_recording_is_no_text_without_calling_the_model(self):
        adapter = ScriptedAdapter([])
        reply = serve(adapter, pcm_of(silence(2 * WINDOW)))
        self.assertEqual(reply["status"], "no_text")
        self.assertEqual(reply["peak_gpu_memory_bytes"], 0)
        self.assertEqual(reply["peak_host_ram_bytes"], 100)
        self.assertEqual(len(adapter.calls), 1)  # health only

    def test_oversized_joined_transcript_is_an_error(self):
        big = "x" * (windowing.MAX_TRANSCRIPT_BYTES // 2 + 1)
        reply = serve(ScriptedAdapter([big, big]), pcm_of(loud(2 * WINDOW)))
        self.assertEqual((reply["status"], reply["code"]), ("error", "transcript_too_large"))


if __name__ == "__main__":
    unittest.main()
