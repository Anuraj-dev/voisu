//! Pure audio conversion for capture: downmix to mono, resample to 16 kHz,
//! quantize to s16le and meter the level. No device or OS code, so it runs
//! and is tested on any platform.

use std::f64::consts::PI;

/// The providers' input rate.
pub const TARGET_RATE: u32 = 16_000;
/// One provider chunk: 100 ms of s16le mono 16 kHz, the Linux chunk size.
pub const CHUNK_BYTES: usize = 3_200;
/// Samples per level reading: one reading per 1/30 s keeps `audio-level` at
/// or under 30 Hz.
pub const LEVEL_WINDOW: usize = (TARGET_RATE / 30) as usize + 1;
/// Quieter than this reads as silence on the meter.
const LEVEL_FLOOR_DB: f32 = -60.0;

/// Averages interleaved frames into one mono channel, appending to `out`.
pub fn downmix(interleaved: &[f32], channels: usize, out: &mut Vec<f32>) {
    let channels = channels.max(1);
    if channels == 1 {
        out.extend_from_slice(interleaved);
        return;
    }
    let scale = 1.0 / channels as f32;
    out.extend(
        interleaved
            .chunks_exact(channels)
            .map(|frame| frame.iter().sum::<f32>() * scale),
    );
}

/// Quantizes to little-endian signed 16-bit PCM, clamping overs.
pub fn push_s16le(samples: &[f32], out: &mut Vec<u8>) {
    out.reserve(samples.len() * 2);
    for &sample in samples {
        let value = (sample.clamp(-1.0, 1.0) * f32::from(i16::MAX)).round() as i16;
        out.extend_from_slice(&value.to_le_bytes());
    }
}

/// A streaming windowed-sinc resampler from any device rate to 16 kHz.
///
/// The kernel is a Blackman-windowed sinc whose cutoff sits at 90% of the
/// lower Nyquist, so downsampling from 44.1/48 kHz does not alias speech
/// harmonics into the band the providers hear. Input arrives in arbitrary
/// slices; output is identical however the input is split.
pub struct Resampler {
    input_rate: u64,
    /// Cutoff in cycles per input sample.
    cutoff: f64,
    /// Kernel half-width in input samples.
    half_width: usize,
    /// Buffered input still reachable by a future kernel.
    history: Vec<f32>,
    /// Stream index of `history[0]`; negative while the leading zero padding
    /// is still buffered.
    base: i64,
    /// Output samples produced so far. Output `n` sits at input position
    /// `n * input_rate / TARGET_RATE`, computed exactly in integers so the
    /// result does not depend on how the input is split.
    emitted: u64,
    passthrough: bool,
}

impl Resampler {
    pub fn new(input_rate: u32) -> Self {
        let input_rate = input_rate.max(1);
        let cutoff = 0.45 * f64::from(input_rate.min(TARGET_RATE)) / f64::from(input_rate);
        // About eight zero crossings each side of the kernel centre.
        let half_width = (8.0 / (2.0 * cutoff)).ceil() as usize;
        Self {
            input_rate: u64::from(input_rate),
            cutoff,
            half_width,
            // Leading zeros let the first output centre on the first sample.
            history: vec![0.0; half_width],
            base: -(half_width as i64),
            emitted: 0,
            passthrough: input_rate == TARGET_RATE,
        }
    }

    /// Feeds input and appends every output sample it now fully determines.
    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if self.passthrough {
            out.extend_from_slice(input);
            return;
        }
        self.history.extend_from_slice(input);
        loop {
            let (centre, fraction) = self.next_position();
            let relative = (centre - self.base) as usize;
            if relative + self.half_width >= self.history.len() {
                break;
            }
            out.push(self.sample_at(relative, fraction));
            self.emitted += 1;
        }
        // Keep only what the next output's kernel can still reach.
        let (centre, _) = self.next_position();
        let keep_from = (centre - self.base + 1 - self.half_width as i64).max(0) as usize;
        if keep_from > 0 {
            self.history.drain(..keep_from);
            self.base += keep_from as i64;
        }
    }

    /// Emits the tail held back for look-ahead, as if silence followed.
    pub fn flush(&mut self, out: &mut Vec<f32>) {
        if self.passthrough {
            return;
        }
        let end = self.base + self.history.len() as i64;
        self.history
            .extend(std::iter::repeat_n(0.0, self.half_width + 1));
        loop {
            let (centre, fraction) = self.next_position();
            if centre >= end {
                break;
            }
            out.push(self.sample_at((centre - self.base) as usize, fraction));
            self.emitted += 1;
        }
        self.history.clear();
        self.base = end;
    }

    /// The next output's input position as a whole sample and a fraction.
    fn next_position(&self) -> (i64, f64) {
        let target = u64::from(TARGET_RATE);
        let numerator = self.emitted * self.input_rate;
        (
            (numerator / target) as i64,
            (numerator % target) as f64 / target as f64,
        )
    }

    fn sample_at(&self, centre: usize, fraction: f64) -> f32 {
        let first = (centre + 1).saturating_sub(self.half_width);
        let last = (centre + self.half_width).min(self.history.len() - 1);
        let mut acc = 0.0;
        for index in first..=last {
            let distance = centre as f64 - index as f64 + fraction;
            acc += f64::from(self.history[index]) * self.kernel(distance);
        }
        acc as f32
    }

    fn kernel(&self, distance: f64) -> f64 {
        let half = self.half_width as f64;
        if distance.abs() >= half {
            return 0.0;
        }
        let x = 2.0 * self.cutoff * distance;
        let sinc = if x.abs() < 1e-12 {
            1.0
        } else {
            (PI * x).sin() / (PI * x)
        };
        // Blackman window over [-half, half].
        let w = 0.5 + 0.5 * distance / half;
        let window = 0.42 - 0.5 * (2.0 * PI * w).cos() + 0.08 * (4.0 * PI * w).cos();
        2.0 * self.cutoff * sinc * window
    }
}

/// RMS level of 16 kHz mono audio, mapped from dB onto 0.0–1.0 for the
/// overlay meter: −60 dBFS and below read as 0, full scale as 1.
pub fn level(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let mean_square = samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32;
    let rms = mean_square.sqrt();
    if rms <= 0.0 {
        return 0.0;
    }
    let db = 20.0 * rms.log10();
    ((db - LEVEL_FLOOR_DB) / -LEVEL_FLOOR_DB).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(rate: u32, freq: f64, seconds: f64) -> Vec<f32> {
        let n = (f64::from(rate) * seconds) as usize;
        (0..n)
            .map(|i| (0.5 * (2.0 * PI * freq * i as f64 / f64::from(rate)).sin()) as f32)
            .collect()
    }

    fn rms(samples: &[f32]) -> f32 {
        (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt()
    }

    fn resample_all(rate: u32, input: &[f32], split: usize) -> Vec<f32> {
        let mut r = Resampler::new(rate);
        let mut out = Vec::new();
        for piece in input.chunks(split) {
            r.process(piece, &mut out);
        }
        r.flush(&mut out);
        out
    }

    #[test]
    fn output_length_tracks_the_rate_ratio() {
        for rate in [8_000, 44_100, 48_000] {
            let input = sine(rate, 440.0, 1.0);
            let out = resample_all(rate, &input, 480);
            let expected = TARGET_RATE as usize;
            assert!(
                out.len().abs_diff(expected) <= 2,
                "{rate}: {} samples",
                out.len()
            );
        }
    }

    #[test]
    fn sixteen_khz_passes_through_untouched() {
        let input = sine(TARGET_RATE, 440.0, 0.1);
        assert_eq!(resample_all(TARGET_RATE, &input, 37), input);
    }

    #[test]
    fn speech_band_tone_keeps_its_level() {
        let out = resample_all(48_000, &sine(48_000, 1_000.0, 1.0), 512);
        // Skip edges, where the zero padding ramps in and out.
        let middle = &out[1_000..out.len() - 1_000];
        let expected = 0.5 / 2f32.sqrt();
        assert!((rms(middle) - expected).abs() < 0.01, "rms {}", rms(middle));
    }

    #[test]
    fn tone_above_the_new_nyquist_is_rejected() {
        // 12 kHz would alias to 4 kHz without the anti-alias filter.
        let out = resample_all(48_000, &sine(48_000, 12_000.0, 1.0), 512);
        let middle = &out[1_000..out.len() - 1_000];
        assert!(rms(middle) < 0.005, "rms {}", rms(middle));
    }

    #[test]
    fn output_does_not_depend_on_how_input_is_split() {
        let input = sine(44_100, 300.0, 0.5);
        let whole = resample_all(44_100, &input, input.len());
        let pieces = resample_all(44_100, &input, 7);
        assert_eq!(whole.len(), pieces.len());
        assert!(whole.iter().zip(&pieces).all(|(a, b)| (a - b).abs() < 1e-6));
    }

    #[test]
    fn downmix_averages_frames() {
        let mut out = Vec::new();
        downmix(&[1.0, 0.0, 0.5, 0.5, -1.0, -0.5], 2, &mut out);
        assert_eq!(out, [0.5, 0.5, -0.75]);
    }

    #[test]
    fn s16le_clamps_and_orders_bytes() {
        let mut out = Vec::new();
        push_s16le(&[0.0, 1.0, -2.0], &mut out);
        assert_eq!(out, [0, 0, 0xff, 0x7f, 0x01, 0x80]);
    }

    #[test]
    fn level_maps_db_onto_the_meter() {
        assert_eq!(level(&[]), 0.0);
        assert_eq!(level(&[0.0; 100]), 0.0);
        assert!((level(&[1.0; 100]) - 1.0).abs() < 1e-6);
        // -30 dBFS sits halfway.
        let half = 10f32.powf(-30.0 / 20.0);
        assert!((level(&[half; 100]) - 0.5).abs() < 1e-3);
        assert_eq!(level(&[1e-5; 100]), 0.0);
    }

    #[test]
    fn level_window_caps_the_rate_at_thirty_hertz() {
        assert!(TARGET_RATE as usize / LEVEL_WINDOW <= 30);
    }
}
