"""Windowing contract: bounded per-call input, isolation, and final-only output.

These are fake-adapter tests. They prove the worker's windowing contract, not
CUDA memory, model quality, or boundary word accuracy.
"""

import array
import contextlib
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


def transcribe_request(pcm_bytes: int, prompt=None, request_id: str = "t", **extra) -> dict:
    request = {"v": 1, "op": "transcribe", "id": request_id, "pcm_bytes": pcm_bytes, **extra}
    if prompt is not None:
        request["prompt"] = prompt
    return request


def serve_requests(adapter: BaseAdapter, requests: list[tuple[dict, bytes]]) -> list[dict]:
    """Prepare, then send each (request, pcm) in order; return ready + every reply."""

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
        wire = frame({"v": 1, "op": "prepare", "id": "p"})
        for request, pcm in requests:
            wire += frame(request) + pcm
        writer = io.BytesIO()
        asr_service.serve_stream(manifest, adapter, io.BytesIO(wire), writer)
    stream = io.BytesIO(writer.getvalue())
    frames = []
    while (reply := read_json_frame(stream)) is not None:
        frames.append(reply)
    assert frames and frames[0]["status"] == "ready", frames
    return frames


def serve(adapter: BaseAdapter, pcm: bytes, prompt=None) -> dict:
    frames = serve_requests(adapter, [(transcribe_request(len(pcm), prompt), pcm)])
    assert len(frames) == 2, frames
    return frames[1]


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


class PromptAdapter(ScriptedAdapter):
    """A prompt-capable adapter (like the faster-whisper one) that records prompts."""

    accepts_initial_prompt = True

    def __init__(self, replies):
        super().__init__(replies)
        self.prompts: list = []

    def transcribe(self, pcm: bytes, initial_prompt=None) -> str:
        self.prompts.append(initial_prompt)
        return super().transcribe(pcm)


class InitialPromptTests(unittest.TestCase):
    def test_a_prompt_reaches_a_prompt_capable_adapter_for_every_window(self):
        adapter = PromptAdapter(["one", "two"])
        reply = serve(adapter, pcm_of(loud(WINDOW + RATE)), prompt="Voisu, Tokio")
        self.assertEqual(reply["text"], "one two")
        # The health window carries no prompt; both request windows do.
        self.assertEqual(adapter.prompts, [None, "Voisu, Tokio", "Voisu, Tokio"])

    def test_a_request_without_a_prompt_passes_none(self):
        adapter = PromptAdapter(["one"])
        serve(adapter, pcm_of(loud(RATE)))
        self.assertEqual(adapter.prompts, [None, None])

    def test_an_adapter_that_ignores_prompts_is_never_called_with_one(self):
        # ScriptedAdapter.transcribe(pcm) would raise TypeError on a prompt kwarg.
        adapter = ScriptedAdapter(["one"])
        reply = serve(adapter, pcm_of(loud(RATE)), prompt="Voisu")
        self.assertEqual((reply["status"], reply["text"]), ("transcript", "one"))

    def assert_rejected_then_stream_stays_in_sync(self, bad_request: dict, pcm: bytes):
        good_pcm = pcm_of(loud(RATE))
        frames = serve_requests(
            PromptAdapter(["after"]),
            [(bad_request, pcm), (transcribe_request(len(good_pcm), request_id="good"), good_pcm)],
        )
        self.assertEqual(len(frames), 3, frames)
        self.assertEqual((frames[1]["status"], frames[1]["code"]), ("error", "invalid_request"))
        # The PCM of the rejected request was consumed, so the next one parses.
        self.assertEqual((frames[2]["status"], frames[2]["text"]), ("transcript", "after"))

    def test_a_malformed_prompt_is_rejected_and_the_stream_stays_in_sync(self):
        pcm = pcm_of(loud(RATE))
        for bad in ["", "  ", 7, ["a"], "x" * 1025, "a\nb", "a\x00b", "a\u0085b"]:
            with self.subTest(bad=bad):
                request = transcribe_request(len(pcm), request_id="bad")
                request["prompt"] = bad
                self.assert_rejected_then_stream_stays_in_sync(request, pcm)

    def test_an_unknown_key_is_rejected_and_the_stream_stays_in_sync(self):
        pcm = pcm_of(loud(RATE))
        request = transcribe_request(len(pcm), request_id="bad", bogus=1)
        self.assert_rejected_then_stream_stays_in_sync(request, pcm)


class PromptCapabilityTests(unittest.TestCase):
    def test_ready_advertises_prompt_support_only_for_a_prompt_capable_adapter(self):
        pcm = pcm_of(loud(RATE))
        capable = serve_requests(PromptAdapter(["x"]), [(transcribe_request(len(pcm)), pcm)])
        self.assertIs(capable[0]["accepts_prompt"], True)
        plain = serve_requests(ScriptedAdapter(["x"]), [(transcribe_request(len(pcm)), pcm)])
        self.assertNotIn("accepts_prompt", plain[0])

    def test_only_faster_whisper_declares_prompt_support(self):
        from voisu_asr import adapters, candidate_adapters

        def subclasses(cls):
            for sub in cls.__subclasses__():
                yield sub
                yield from subclasses(sub)

        shipped = [cls for cls in subclasses(BaseAdapter) if cls.__module__.startswith("voisu_asr")]
        capable = {cls.__name__ for cls in shipped if cls.accepts_initial_prompt}
        self.assertEqual(capable, {"FasterWhisperAdapter"})
        # Parakeet is served by the NeMo adapter; it must never advertise prompts.
        self.assertFalse(adapters.NemoSpeechCAdapter.accepts_initial_prompt)
        self.assertNotIn("NemoSpeechCAdapter", capable)


class PromptEchoTests(unittest.TestCase):
    """A quiet window can make Whisper recite its prompt; that is not speech."""

    PROMPT = "Voisu, Tokio, wayfinder"

    def run_window(self, text: str) -> dict:
        return serve(PromptAdapter([text]), pcm_of(loud(RATE)), prompt=self.PROMPT)

    def test_a_window_that_only_recites_the_glossary_is_no_text(self):
        for echo in [
            "Voisu, Tokio, wayfinder",
            "Voisu, Tokio, wayfinder.",
            "voisu tokio wayfinder",
            "VOISU,  tokio ,wayfinder .",
        ]:
            with self.subTest(echo=echo):
                self.assertEqual(self.run_window(echo)["status"], "no_text")

    def test_a_single_dictionary_term_is_real_speech(self):
        reply = self.run_window("Voisu")
        self.assertEqual((reply["status"], reply["text"]), ("transcript", "Voisu"))

    def test_real_speech_that_mentions_terms_is_kept(self):
        for text in ["Voisu is great", "Voisu, Tokio and more", "Voisu, Tokio, and Rust"]:
            with self.subTest(text=text):
                self.assertEqual(self.run_window(text)["text"], text)

    def test_two_terms_or_terms_out_of_prompt_order_are_real_speech(self):
        # Traded away: a recital of only two terms, or of terms in another
        # order, is no longer dropped. People dictate short lists of names.
        for text in ["Voisu, Tokio", "voisu tokio", "wayfinder, Tokio, Voisu."]:
            with self.subTest(text=text):
                self.assertEqual(self.run_window(text)["text"], text)

    def test_the_same_term_twice_is_not_a_glossary_echo(self):
        self.assertEqual(self.run_window("Voisu Voisu")["text"], "Voisu Voisu")

    def test_only_the_echo_window_is_dropped_from_a_long_recording(self):
        adapter = PromptAdapter(["real words", "Voisu, Tokio, wayfinder."])
        reply = serve(adapter, pcm_of(loud(WINDOW + RATE)), prompt=self.PROMPT)
        self.assertEqual((reply["status"], reply["text"]), ("transcript", "real words"))

    def test_a_dropped_echo_is_logged_locally_without_the_dictionary(self):
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            self.run_window("Voisu, Tokio, wayfinder")
        log = stderr.getvalue()
        self.assertIn("dropped a window that only listed Dictionary terms", log)
        for term in ("Voisu", "Tokio", "wayfinder"):
            self.assertNotIn(term, log)

    def test_a_glossary_sentence_prompt_is_recognised_when_recited(self):
        prompt = "Glossary: Voisu, Tokio, wayfinder, Rust."
        for echo in [
            "Voisu, Tokio, wayfinder",
            "Tokio, wayfinder, Rust.",
            "Glossary: Voisu, Tokio, wayfinder, Rust.",
            "glossary voisu tokio wayfinder",
        ]:
            with self.subTest(echo=echo):
                reply = serve(PromptAdapter([echo]), pcm_of(loud(RATE)), prompt=prompt)
                self.assertEqual(reply["status"], "no_text")

    def test_a_glossary_sentence_prompt_keeps_real_speech(self):
        prompt = "Glossary: Voisu, Tokio, wayfinder."
        for text in ["Voisu", "Glossary: Voisu", "the glossary has Voisu, Tokio", "Voisu, Tokio and more"]:
            with self.subTest(text=text):
                reply = serve(PromptAdapter([text]), pcm_of(loud(RATE)), prompt=prompt)
                self.assertEqual(reply["text"], text)

    def test_the_built_in_glossary_prompt_keeps_short_dictated_name_lists(self):
        # The daemon's English prompt with an empty Dictionary starts so.
        prompt = "Glossary: Claude, Claude Code, Codex, OpenAI, Anthropic, GPT, LLM, Groq, Deepgram, Whisper, systemctl."
        for text in ["OpenAI, Anthropic.", "GPT LLM", "Whisper Deepgram", "Claude Code, Codex."]:
            with self.subTest(text=text):
                reply = serve(PromptAdapter([text]), pcm_of(loud(RATE)), prompt=prompt)
                self.assertEqual((reply["status"], reply["text"]), ("transcript", text))
        for echo in ["Claude, Claude Code, Codex, OpenAI.", "Glossary: Claude, Claude Code, Codex."]:
            with self.subTest(echo=echo):
                reply = serve(PromptAdapter([echo]), pcm_of(loud(RATE)), prompt=prompt)
                self.assertEqual(reply["status"], "no_text")

    def test_without_a_prompt_nothing_is_filtered(self):
        reply = serve(PromptAdapter(["Voisu, Tokio"]), pcm_of(loud(RATE)))
        self.assertEqual(reply["text"], "Voisu, Tokio")


if __name__ == "__main__":
    unittest.main()
