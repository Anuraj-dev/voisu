//! Speech-presence gate: decides for a whole recording whether it holds speech.
//!
//! The gate only answers yes or no. It never trims, resamples, or returns
//! modified audio: trimming risks cutting quiet speech on Cloud, the main
//! product, so the recording the provider sees is always the one captured.
//!
//! It is the adaptive energy rule of VoxType's energy VAD (see
//! `THIRD_PARTY_NOTICES.md`), run after DC removal:
//!
//! 1. **DC removal.** The mean of the whole recording is subtracted. Some
//!    microphones sit on a DC offset (about 600 LSB on one of ours), so a raw
//!    peak or RMS reads "loud" on pure silence. Mean subtraction is chosen over
//!    a one-pole high-pass because the decision covers the whole recording at
//!    once, so the exact mean is available; a high-pass would add a start-up
//!    transient (the step from zero to the offset rings through the first
//!    frames and can read as speech), a cutoff to tune, and would attenuate
//!    low-frequency speech energy. A slowly drifting offset is far smaller than
//!    the 3 x noise-floor margin and is not worth that cost.
//! 2. **Frames.** 20 ms frames (the last may be partial), RMS of samples
//!    normalized to -1..1.
//! 3. **Adaptive threshold.** `effective = max(3 * p10, thr / 10, 0.001)
//!    .min(thr)`, where `p10` is the 10th-percentile frame RMS (the recording's
//!    own noise floor) and `thr` is [`SPEECH_THRESHOLD`]. The `.min(thr)` cap is
//!    essential: without it a pause-free clip sets its own floor, because its
//!    10th-percentile frame is already speech, and it is rejected as silence.
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
//! Deliberate difference: VoxType also requires `min_speech_duration_ms`
//! (default 100 ms) of total speech. Voisu keeps only the three-frame run.

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
/// The lowest effective threshold ever used.
const MIN_ENERGY_THRESHOLD: f32 = 0.001;
/// The adaptive threshold goes at most one decade below [`SPEECH_THRESHOLD`].
const MAX_ADAPTIVE_DROP: f32 = 10.0;

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
    // Accumulate in f64: a long recording's sum of squares loses precision in f32.
    let mean = samples.iter().map(|&s| f64::from(s)).sum::<f64>() / samples.len() as f64;
    let frame_len = ((sample_rate * FRAME_MS / 1000) as usize).max(1);
    let frame_rms: Vec<f32> = samples
        .chunks(frame_len)
        .map(|frame| {
            let sum_squares: f64 = frame
                .iter()
                .map(|&s| {
                    let centered = (f64::from(s) - mean) / 32768.0;
                    centered * centered
                })
                .sum();
            (sum_squares / frame.len() as f64).sqrt() as f32
        })
        .collect();

    let mut sorted = frame_rms.clone();
    sorted.sort_by(f32::total_cmp);
    let floor = sorted[((sorted.len() - 1) as f32 * NOISE_FLOOR_PERCENTILE) as usize];
    let effective_threshold = (floor * NOISE_FLOOR_MULTIPLE)
        .max(SPEECH_THRESHOLD / MAX_ADAPTIVE_DROP)
        // max(MIN_ENERGY_THRESHOLD).min(SPEECH_THRESHOLD): the cap is essential.
        .clamp(MIN_ENERGY_THRESHOLD, SPEECH_THRESHOLD);

    let (mut run, mut longest_run_frames) = (0usize, 0usize);
    for &rms in &frame_rms {
        if rms >= effective_threshold {
            run += 1;
            longest_run_frames = longest_run_frames.max(run);
        } else {
            run = 0;
        }
    }
    SpeechPresence {
        has_speech: longest_run_frames >= MIN_SPEECH_FRAMES,
        effective_threshold,
        longest_run_frames,
        frames: frame_rms.len(),
    }
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
        // Two 300 ms syllables at peak 250 LSB (RMS about 0.005) over a noise
        // floor of 20 LSB and a 600 LSB offset: quiet, but 10 dB over the floor.
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
}
