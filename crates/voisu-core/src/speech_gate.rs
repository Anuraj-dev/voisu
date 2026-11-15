//! Speech-presence gate: decides for a whole recording whether it holds speech.
//!
//! The gate only answers yes or no. It never trims, resamples, or returns
//! modified audio: trimming risks cutting quiet speech on Cloud, the main
//! product, so the recording the provider sees is always the one captured.
//!
//! It is the adaptive energy rule of VoxType's energy VAD (see
//! `THIRD_PARTY_NOTICES.md`), with DC removal added in front:
//!
//! 1. **DC removal (Voisu's addition; VoxType does none).** Each 20 ms frame's
//!    own mean is subtracted before its RMS is taken, so a frame's RMS is its
//!    standard deviation. Some microphones sit on a DC offset (about 600 LSB on
//!    one of ours), so a raw peak or RMS reads "loud" on pure silence. The mean
//!    is taken per frame, not over the whole recording, because the offset is
//!    not constant: PipeWire can emit digital zeros before the offset appears
//!    and the offset can drift, and one recording-wide mean left a 600 LSB
//!    offset standing against those zeros as loud frames. A frame is far
//!    shorter than any drift, and it needs no filter state, start-up
//!    transient or cutoff, unlike a one-pole high-pass. The cost is that a
//!    frame's own very-low-frequency content is removed with its mean, which
//!    is below the speech band. A trailing partial frame is judged on its own
//!    samples, so a few trailing samples read near zero.
//! 2. **Frames.** 20 ms frames (the last may be partial), RMS of samples
//!    normalized to -1..1.
//! 3. **Adaptive threshold.** `effective = max(3 * p10, thr / 30)
//!    .clamp(0.0003, thr)`, where `p10` is the 10th-percentile RMS of the
//!    frames from the first non-zero frame on (the recording's own noise
//!    floor; a leading run of digital zeros is skipped) and `thr` is [`SPEECH_THRESHOLD`]. The
//!    `.min(thr)` cap is essential: without it a pause-free clip sets its own
//!    floor, because its 10th-percentile frame is already speech, and it is
//!    rejected as silence.
//! 4. **Speech** is at least [`MIN_SPEECH_FRAMES`] consecutive frames at or
//!    above the effective threshold; one or two loud frames are a click or pop.
//!
//! Adapted from VoxType (MIT, Copyright (c) 2025 Peter Jackson),
//! `peteonrails/voxtype` at commit `fbedd62`:
//! - `src/vad/energy.rs` lines 56-62 (`effective_threshold`) and 69-79
//!   (`NOISE_FLOOR_MULTIPLE`, `NOISE_FLOOR_PERCENTILE`, `MIN_ENERGY_THRESHOLD`,
//!   `MAX_ADAPTIVE_DROP`, `MIN_RUN_FRAMES`);
//! - `src/vad/energy.rs` lines 40-46 (`calculate_rms`) and 105-130 (20 ms
//!   framing, the `rms >= threshold` run counting);
//! - `src/config/vad.rs` `default_vad_threshold` (0.5), mapped by
//!   `map_threshold_to_energy` in `src/vad/energy.rs` to
//!   `0.001 * 100^0.5 = 0.01`, which is [`SPEECH_THRESHOLD`].
//!
//! Deliberate differences from VoxType:
//! - VoxType also requires `min_speech_duration_ms` (default 100 ms) of total
//!   speech. Voisu keeps only the three-frame run.
//! - VoxType's lowest threshold is 0.001 (`MIN_ENERGY_THRESHOLD`, and
//!   `thr / MAX_ADAPTIVE_DROP` with a drop of 10). Voisu lowers both: the
//!   floor to 0.0003 and the drop to 30 (`thr / 30`, about 0.00033). A false
//!   reject refuses a dictation, which costs more than a false accept, and a
//!   mic with a 2 LSB noise floor must still accept a short quiet "yes" (RMS
//!   about 0.0009). The `3 * p10` term still lifts the bar in a noisy room.

/// The configured RMS threshold (normalized -1..1): VoxType's default setting
/// 0.5 mapped through `0.001 * 100^t`.
pub const SPEECH_THRESHOLD: f32 = 0.01;
/// Frame length in milliseconds.
pub const FRAME_MS: u32 = 20;
/// Consecutive loud frames (60 ms) that count as speech.
pub const MIN_SPEECH_FRAMES: usize = 3;
/// Speech must be this many times louder than the recording's noise floor.
const NOISE_FLOOR_MULTIPLE: f32 = 3.0;
/// Frame-RMS percentile taken as the noise floor.
const NOISE_FLOOR_PERCENTILE: f32 = 0.10;
/// The lowest effective threshold ever used (VoxType: 0.001).
const MIN_ENERGY_THRESHOLD: f32 = 0.0003;
/// The adaptive threshold goes at most this far below [`SPEECH_THRESHOLD`]
/// (VoxType: 10).
const MAX_ADAPTIVE_DROP: f32 = 30.0;

/// The gate's decision for one recording, with the numbers behind it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SpeechPresence {
    /// Whether the recording contains speech.
    pub has_speech: bool,
    /// The adaptive threshold used (normalized RMS).
    pub effective_threshold: f32,
    /// Longest run of consecutive frames at or above the threshold.
    pub longest_run_frames: usize,
    /// Number of 20 ms frames analysed.
    pub frames: usize,
}

/// Decides whether `samples` (16-bit PCM, mono) hold speech. The whole
/// recording is judged; the samples are never modified.
pub fn detect_speech(samples: &[i16], sample_rate: u32) -> SpeechPresence {
    if samples.is_empty() {
        return SpeechPresence {
            has_speech: false,
            effective_threshold: MIN_ENERGY_THRESHOLD,
            longest_run_frames: 0,
            frames: 0,
        };
    }
    let frame_len = ((u64::from(sample_rate) * u64::from(FRAME_MS) / 1000) as usize).max(1);
    let frame_rms: Vec<f32> = samples.chunks(frame_len).map(centered_rms).collect();

    // A leading run of digital-zero frames (the PipeWire start-up gap) says
    // nothing about the room's noise floor; counting it would drag p10 to 0
    // on a short clip. Later zero frames stay: behind a noise gate they are the
    // real floor, and dropping them would make p10 the speech level and refuse
    // quiet speech.
    let start = frame_rms
        .iter()
        .position(|rms| *rms > 0.0)
        .unwrap_or(frame_rms.len());
    let mut sorted = frame_rms[start..].to_vec();
    sorted.sort_by(f32::total_cmp);
    let floor = sorted
        .get(((sorted.len().saturating_sub(1)) as f32 * NOISE_FLOOR_PERCENTILE) as usize)
        .copied()
        .unwrap_or(0.0);
    let effective_threshold = (floor * NOISE_FLOOR_MULTIPLE)
        .max(SPEECH_THRESHOLD / MAX_ADAPTIVE_DROP)
        // max(MIN_ENERGY_THRESHOLD).min(SPEECH_THRESHOLD): the cap is essential.
        .clamp(MIN_ENERGY_THRESHOLD, SPEECH_THRESHOLD);

    let longest_run_frames = longest_run(&frame_rms, effective_threshold);
    SpeechPresence {
        has_speech: longest_run_frames >= MIN_SPEECH_FRAMES,
        effective_threshold,
        longest_run_frames,
        frames: frame_rms.len(),
    }
}

/// RMS of one frame after removing the frame's own mean, normalized to -1..1.
/// Accumulates in f64: a long frame's sum of squares loses precision in f32.
fn centered_rms(frame: &[i16]) -> f32 {
    let n = frame.len() as f64;
    let mean = frame.iter().map(|&s| f64::from(s)).sum::<f64>() / n;
    let sum_squares: f64 = frame
        .iter()
        .map(|&s| {
            let centered = (f64::from(s) - mean) / 32768.0;
            centered * centered
        })
        .sum();
    (sum_squares / n).sqrt() as f32
}

/// Longest run of consecutive frames with `rms >= threshold` (VoxType's `>=`).
fn longest_run(frame_rms: &[f32], threshold: f32) -> usize {
    let (mut run, mut longest) = (0usize, 0usize);
    for &rms in frame_rms {
        if rms >= threshold {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    longest
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 16_000;

    /// Deterministic noise in [-amplitude, amplitude] LSB.
    fn noise(len: usize, amplitude: f32, seed: u32) -> Vec<f32> {
        let mut state = seed;
        (0..len)
            .map(|_| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                ((state >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0) * amplitude
            })
            .collect()
    }

    /// A vowel-like tone (fundamental plus two harmonics) of peak `amplitude`.
    fn voiced(len: usize, amplitude: f32) -> Vec<f32> {
        (0..len)
            .map(|i| {
                let t = i as f32 / RATE as f32;
                let phase = 2.0 * std::f32::consts::PI * 140.0 * t;
                amplitude
                    * (0.6 * phase.sin() + 0.3 * (2.0 * phase).sin() + 0.1 * (3.0 * phase).sin())
            })
            .collect()
    }

    fn add(audio: &mut [f32], start: usize, burst: &[f32]) {
        for (sample, extra) in audio[start..].iter_mut().zip(burst) {
            *sample += extra;
        }
    }

    fn pcm(audio: &[f32], dc: f32) -> Vec<i16> {
        audio
            .iter()
            .map(|sample| (sample + dc).round().clamp(-32768.0, 32767.0) as i16)
            .collect()
    }

    #[test]
    fn silence_on_a_dc_offset_mic_is_rejected() {
        let audio = noise(2 * RATE as usize, 20.0, 1);
        let decision = detect_speech(&pcm(&audio, 600.0), RATE);
        assert!(!decision.has_speech, "{decision:?}");
    }

    #[test]
    fn digital_silence_is_rejected() {
        let decision = detect_speech(&vec![0i16; 2 * RATE as usize], RATE);
        assert!(!decision.has_speech, "{decision:?}");
        assert!(!detect_speech(&[], RATE).has_speech);
    }

    #[test]
    fn quiet_speech_bursts_on_a_dc_offset_are_accepted() {
        // Two 300 ms syllables at peak 250 LSB (RMS about 120 LSB, 0.0037) over
        // a +-20 LSB noise floor (RMS about 12 LSB) and a 600 LSB offset: quiet,
        // but well over the noise floor.
        let mut audio = noise(2 * RATE as usize, 20.0, 2);
        add(&mut audio, 6_400, &voiced(4_800, 250.0));
        add(&mut audio, 17_600, &voiced(4_800, 250.0));
        let decision = detect_speech(&pcm(&audio, 600.0), RATE);
        assert!(decision.has_speech, "{decision:?}");
    }

    #[test]
    fn a_pause_free_signal_is_accepted() {
        // No pauses: the 10th-percentile frame is already loud, so without the
        // `.min(thr)` cap the clip would set its own floor and fail.
        let audio = voiced(2 * RATE as usize, 6_000.0);
        let decision = detect_speech(&pcm(&audio, 0.0), RATE);
        assert!(decision.has_speech, "{decision:?}");
        assert_eq!(decision.effective_threshold, SPEECH_THRESHOLD);
        let on_dc = detect_speech(&pcm(&audio, 600.0), RATE);
        assert!(on_dc.has_speech, "{on_dc:?}");
    }

    #[test]
    fn a_short_click_is_rejected() {
        let mut audio = noise(2 * RATE as usize, 20.0, 3);
        for start in [8_000, 19_200] {
            add(&mut audio, start, &voiced(320, 8_000.0));
        }
        let decision = detect_speech(&pcm(&audio, 600.0), RATE);
        assert!(!decision.has_speech, "{decision:?}");
        assert_eq!(decision.longest_run_frames, 1);
    }

    #[test]
    fn three_loud_frames_are_the_shortest_speech() {
        let mut audio = noise(RATE as usize, 20.0, 4);
        add(&mut audio, 3_840, &voiced(3 * 320, 3_000.0));
        assert!(detect_speech(&pcm(&audio, 0.0), RATE).has_speech);
        let mut audio = noise(RATE as usize, 20.0, 4);
        add(&mut audio, 3_840, &voiced(2 * 320, 3_000.0));
        assert!(!detect_speech(&pcm(&audio, 0.0), RATE).has_speech);
    }

    #[test]
    fn leading_digital_zeros_on_a_dc_offset_mic_do_not_read_as_speech() {
        // PipeWire start-up can emit zeros before the offset appears; the step
        // from 0 to 600 must not read as loud frames, and the zero frames must
        // not drag p10 below the noise. 200 ms of zeros is 10% of this 2 s
        // clip, enough to set p10 to 0 if zero frames counted.
        let audio = noise(2 * RATE as usize, 20.0, 5);
        let mut samples = pcm(&audio, 600.0);
        samples[..3_200].fill(0);
        let decision = detect_speech(&samples, RATE);
        assert!(!decision.has_speech, "{decision:?}");
    }

    #[test]
    fn a_slowly_drifting_dc_offset_is_rejected() {
        // 600 -> 700 LSB over 10 s with +-20 noise: the drift inside one 20 ms
        // frame is 0.2 LSB, so per-frame DC removal sees only noise.
        let len = 10 * RATE as usize;
        let audio = noise(len, 20.0, 6);
        let drifting: Vec<i16> = audio
            .iter()
            .enumerate()
            .map(|(i, sample)| (sample + 600.0 + 100.0 * i as f32 / len as f32).round() as i16)
            .collect();
        let decision = detect_speech(&drifting, RATE);
        assert!(!decision.has_speech, "{decision:?}");
    }

    #[test]
    fn a_noise_gated_mic_still_hears_a_short_yes() {
        // A noise gate outputs exact zeros around words; the zeros after the
        // first sound are the floor, so the quiet "yes" must still pass.
        let mut audio = vec![0.0; 2 * RATE as usize];
        add(&mut audio, 6_400, &voiced(4_800, 60.0));
        let decision = detect_speech(&pcm(&audio, 0.0), RATE);
        assert!(decision.has_speech, "{decision:?}");
    }

    #[test]
    fn a_quiet_mic_still_hears_a_short_yes() {
        // 2 LSB noise floor, no DC, a 300 ms voiced burst at peak 60 LSB (RMS
        // about 29 LSB, 0.0009): a false reject refuses a dictation.
        let mut audio = noise(2 * RATE as usize, 2.0, 7);
        add(&mut audio, 6_400, &voiced(4_800, 60.0));
        let decision = detect_speech(&pcm(&audio, 0.0), RATE);
        assert!(decision.has_speech, "{decision:?}");
    }

    #[test]
    fn a_noise_only_room_is_rejected_but_speech_over_it_is_accepted() {
        // Noise at about 0.003 RMS (98 LSB): the adaptive 3 x p10 term (about
        // 0.009, under the 0.01 cap) puts the bar above the noise itself.
        let amplitude = 0.003 * 32768.0 * 3f32.sqrt();
        let room = noise(2 * RATE as usize, amplitude, 8);
        let decision = detect_speech(&pcm(&room, 0.0), RATE);
        assert!(!decision.has_speech, "{decision:?}");

        // Speech about 10 dB over that floor (RMS about 0.01).
        let mut audio = room;
        add(&mut audio, 6_400, &voiced(4_800, 700.0));
        add(&mut audio, 17_600, &voiced(4_800, 700.0));
        let decision = detect_speech(&pcm(&audio, 0.0), RATE);
        assert!(decision.has_speech, "{decision:?}");
    }

    #[test]
    fn a_run_exactly_at_the_threshold_counts() {
        assert_eq!(longest_run(&[0.01, 0.01, 0.01], 0.01), 3);
        assert_eq!(longest_run(&[0.01, 0.0099, 0.01], 0.01), 1);
    }

    #[test]
    fn a_tiny_trailing_partial_frame_does_not_extend_a_two_frame_burst() {
        // Two loud frames, then 5 trailing samples: the partial frame's own
        // mean is removed, so it reads about 0 and cannot make a third frame.
        let mut audio = noise(3 * RATE as usize + 640, 20.0, 9);
        add(&mut audio, 3 * RATE as usize, &voiced(640, 3_000.0));
        let mut samples = pcm(&audio, 600.0);
        samples.extend([600_i16; 5]);
        let decision = detect_speech(&samples, RATE);
        assert!(!decision.has_speech, "{decision:?}");
        assert_eq!(decision.longest_run_frames, 2);
    }
}
