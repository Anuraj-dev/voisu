use std::collections::VecDeque;
use std::f32::consts::PI;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use voisu_core::LevelFrame;

pub const BAND_COUNT: usize = 20;
pub const PCM_CHUNK_BYTES: usize = 3_200;
const LEVEL_RING_CAPACITY: usize = 8;
const SAMPLE_RATE: f32 = 16_000.0;

/// Recursive filter state below this magnitude flushes to exactly zero: it
/// sits ~14 orders of magnitude under the meter's -60 dB floor, so the flush
/// is invisible by construction, and it stops the IIR recursion from grinding
/// through denormals forever once silence follows a transient.
const STATE_FLUSH_EPSILON: f32 = 1e-12;

/// Contain one recursive state value: a non-finite intermediate (however it
/// arose) must never poison the filter permanently, and sub-epsilon residue
/// flushes to zero instead of decaying without end.
fn contain(value: f32) -> f32 {
    if !value.is_finite() || value.abs() < STATE_FLUSH_EPSILON {
        0.0
    } else {
        value
    }
}

/// Non-finite containment alone, for the resonator pair z1/z2 whose epsilon
/// flush must be joint (see `Biquad::observe`).
fn finite(value: f32) -> f32 {
    if value.is_finite() { value } else { 0.0 }
}

#[derive(Clone, Copy, Debug)]
struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    z1: f32,
    z2: f32,
    envelope: f32,
}

impl Biquad {
    fn bandpass(frequency: f32) -> Self {
        let omega = 2.0 * PI * frequency / SAMPLE_RATE;
        let alpha = omega.sin() / (2.0 * 3.0);
        let a0 = 1.0 + alpha;
        Self {
            b0: alpha / a0,
            b1: 0.0,
            b2: -alpha / a0,
            a1: (-2.0 * omega.cos()) / a0,
            a2: (1.0 - alpha) / a0,
            z1: 0.0,
            z2: 0.0,
            envelope: 0.0,
        }
    }

    fn observe(&mut self, sample: f32) {
        let filtered = self.b0 * sample + self.z1;
        let z1 = finite(self.b1 * sample - self.a1 * filtered + self.z2);
        let z2 = finite(self.b2 * sample - self.a2 * filtered);
        // The epsilon flush is joint: zeroing one state of the resonator
        // while the other still carries energy re-injects quantization error
        // each rotation and can sustain a limit cycle just above the
        // threshold forever instead of going quiet.
        if z1.abs() < STATE_FLUSH_EPSILON && z2.abs() < STATE_FLUSH_EPSILON {
            self.z1 = 0.0;
            self.z2 = 0.0;
        } else {
            self.z1 = z1;
            self.z2 = z2;
        }
        let power = filtered * filtered;
        let coefficient = if power > self.envelope { 0.18 } else { 0.025 };
        self.envelope = contain(self.envelope + coefficient * (power - self.envelope));
    }

    fn level(self) -> u8 {
        let amplitude = self.envelope.max(0.0).sqrt();
        if amplitude <= 0.001 {
            return 0;
        }
        let db = 20.0 * amplitude.log10();
        (((db + 60.0) / 60.0).clamp(0.0, 1.0) * u8::MAX as f32).round() as u8
    }
}

#[derive(Debug)]
pub struct BandState {
    filters: [Biquad; BAND_COUNT],
}

impl Default for BandState {
    fn default() -> Self {
        let ratio = (8_000.0_f32 / 80.0).powf(1.0 / (BAND_COUNT - 1) as f32);
        let mut frequency: f32 = 80.0;
        Self {
            filters: std::array::from_fn(|_| {
                // A bandpass centred on Nyquist degenerates: sin(omega) -> 0
                // leaves it nearly undamped, ringing for seconds after a
                // transient. Capping the top centre keeps the pole pair
                // complex and damped (ring-out ~30 ms to -60 dB) while the
                // band, ~2.6 kHz wide at Q=3, still covers up to Nyquist.
                let filter = Biquad::bandpass(frequency.min(7_800.0));
                frequency *= ratio;
                filter
            }),
        }
    }
}

/// Compute one log-spaced frequency-band frame from any number of mono
/// samples. Every sample — clipped ones included — runs through the filter
/// bank normally: a clipped transient is loud in ITS bands, never a
/// full-spectrum short circuit, and the recursive state always advances.
pub fn bands(pcm: &[i16], state: &mut BandState) -> [u8; BAND_COUNT] {
    for &sample in pcm {
        let sample = sample as f32 / i16::MAX as f32;
        for filter in &mut state.filters {
            filter.observe(sample);
        }
    }
    std::array::from_fn(|index| state.filters[index].level())
}

#[derive(Debug)]
pub struct LevelRing {
    frames: Mutex<VecDeque<LevelFrame>>,
    next_seq: Arc<AtomicU64>,
    active: AtomicBool,
}

impl Default for LevelRing {
    fn default() -> Self {
        Self::with_counter(Arc::new(AtomicU64::new(1)))
    }
}

impl LevelRing {
    fn with_counter(next_seq: Arc<AtomicU64>) -> Self {
        Self {
            frames: Mutex::new(VecDeque::with_capacity(LEVEL_RING_CAPACITY)),
            next_seq,
            active: AtomicBool::new(true),
        }
    }

    pub fn push(&self, bands: [u8; BAND_COUNT]) {
        if !self.active.load(Ordering::Acquire) {
            return;
        }
        let seq = self.next_seq.fetch_add(1, Ordering::Relaxed);
        let mut frames = self.frames.lock().unwrap();
        if frames.len() == LEVEL_RING_CAPACITY {
            frames.pop_front();
        }
        frames.push_back(LevelFrame { seq, bands });
    }

    pub fn after(&self, after_seq: u64) -> Vec<LevelFrame> {
        if !self.active.load(Ordering::Acquire) {
            return Vec::new();
        }
        self.frames
            .lock()
            .unwrap()
            .iter()
            .filter(|frame| frame.seq > after_seq)
            .copied()
            .collect()
    }

    pub fn deactivate(&self) {
        self.active.store(false, Ordering::Release);
    }
}

#[derive(Clone, Debug)]
pub struct LevelRegistry {
    current: Arc<Mutex<Option<Arc<LevelRing>>>>,
    /// One frame-sequence counter shared by every Recording of this daemon.
    /// A stop/start pair can slip between two 200 ms Overlay status polls, so
    /// a cursor from the previous Recording may survive into the next one;
    /// sequences that restarted per ring would leave that cursor "ahead" of
    /// every new frame and freeze the bars.
    next_seq: Arc<AtomicU64>,
}

impl Default for LevelRegistry {
    fn default() -> Self {
        Self {
            current: Arc::new(Mutex::new(None)),
            next_seq: Arc::new(AtomicU64::new(1)),
        }
    }
}

impl LevelRegistry {
    pub fn begin_recording(&self) -> Arc<LevelRing> {
        let ring = Arc::new(LevelRing::with_counter(Arc::clone(&self.next_seq)));
        *self.current.lock().unwrap() = Some(Arc::clone(&ring));
        ring
    }

    pub fn current(&self) -> Option<Arc<LevelRing>> {
        self.current.lock().unwrap().clone()
    }

    pub fn after(&self, after_seq: u64) -> Vec<LevelFrame> {
        self.current()
            .map(|ring| ring.after(after_seq))
            .unwrap_or_default()
    }
}

/// Decodes little-endian s16 mono bytes into complete samples across
/// arbitrarily split reads, carrying a trailing half-sample byte into the
/// next call. A call that completes no sample returns an empty vector — the
/// caller then pushes no level frame, so tiny or odd-sized reads can never
/// mint all-zero frames that advance the ring sequence and evict real peaks.
#[derive(Debug, Default)]
pub struct SampleDecoder {
    partial: Option<u8>,
}

impl SampleDecoder {
    pub fn decode(&mut self, bytes: &[u8]) -> Vec<i16> {
        let mut buffered = Vec::with_capacity(bytes.len() + 1);
        if let Some(byte) = self.partial.take() {
            buffered.push(byte);
        }
        buffered.extend_from_slice(bytes);
        if buffered.len() % 2 != 0 {
            self.partial = buffered.pop();
        }
        buffered
            .chunks_exact(2)
            .map(|pair| i16::from_le_bytes([pair[0], pair[1]]))
            .collect()
    }
}

#[derive(Debug, Default)]
pub struct PcmChunkAssembler {
    pending: Vec<u8>,
}

impl PcmChunkAssembler {
    pub fn push(&mut self, bytes: &[u8]) -> Vec<Vec<u8>> {
        self.pending.extend_from_slice(bytes);
        let mut chunks = Vec::new();
        while self.pending.len() >= PCM_CHUNK_BYTES {
            let tail = self.pending.split_off(PCM_CHUNK_BYTES);
            chunks.push(std::mem::replace(&mut self.pending, tail));
        }
        chunks
    }

    pub fn finish(&mut self) -> Option<Vec<u8>> {
        (!self.pending.is_empty()).then(|| std::mem::take(&mut self.pending))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(frequency: f32, samples: usize) -> Vec<i16> {
        (0..samples)
            .map(|sample| {
                let phase = 2.0 * std::f32::consts::PI * frequency * sample as f32 / 16_000.0;
                (phase.sin() * i16::MAX as f32 * 0.8) as i16
            })
            .collect()
    }

    #[test]
    fn red_a_one_kilohertz_tone_peaks_in_its_log_spaced_band() {
        let mut state = BandState::default();
        let frame = bands(&sine(1_000.0, 320), &mut state);
        let peak = frame
            .iter()
            .enumerate()
            .max_by_key(|(_, level)| *level)
            .unwrap()
            .0;
        assert!((9..=12).contains(&peak), "unexpected peak band {peak}: {frame:?}");
    }

    #[test]
    fn red_silence_is_floor_and_sustained_saturation_reads_near_ceiling() {
        assert_eq!(
            bands(&[0; 320], &mut BandState::default()),
            [0; BAND_COUNT]
        );
        // Band-limited saturation: a sustained full-scale tone drives ITS
        // band close to the ceiling while distant bands stay far below it.
        let mut state = BandState::default();
        let full_scale_tone = (0..640)
            .map(|sample| {
                let phase = 2.0 * std::f32::consts::PI * 1_000.0 * sample as f32 / 16_000.0;
                (phase.sin() * i16::MAX as f32) as i16
            })
            .collect::<Vec<_>>();
        let frame = bands(&full_scale_tone, &mut state);
        let peak = *frame.iter().max().unwrap();
        assert!(peak >= 220, "full-scale tone should read near ceiling: {frame:?}");
        // The u8 scale is dB-mapped (60 dB over 255 steps), so "band-limited"
        // means the far skirt sits at least ~19 dB (80 steps) below the
        // driven band, not half its raw value.
        assert!(
            peak - frame[0] >= 80,
            "saturation stays band-limited, not full-spectrum: {frame:?}"
        );
        // Broadband saturation: deterministic full-scale noise puts real
        // energy in every band of the meter.
        let mut lcg = 0x2545_F491_u32;
        let clipped_noise = (0..3_200)
            .map(|_| {
                lcg = lcg.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                // High LCG bits are the well-mixed ones; the low bit merely
                // alternates, which would be a pure Nyquist tone, not noise.
                if (lcg >> 16) & 1 == 0 { i16::MAX } else { i16::MIN }
            })
            .collect::<Vec<_>>();
        let broadband = bands(&clipped_noise, &mut BandState::default());
        assert!(
            broadband.iter().all(|level| *level > 60),
            "broadband saturation must light the whole meter: {broadband:?}"
        );
    }

    #[test]
    fn red_short_and_partial_sample_buffers_are_safe() {
        let mut state = BandState::default();
        assert_eq!(bands(&[], &mut state), [0; BAND_COUNT]);
        assert_eq!(bands(&[42], &mut state).len(), BAND_COUNT);
    }

    #[test]
    fn red_level_cursor_has_no_duplicates_or_drops_and_wraps_at_capacity() {
        let ring = LevelRing::default();
        for value in 1..=10 {
            ring.push([value; BAND_COUNT]);
        }
        let retained = ring.after(0);
        assert_eq!(
            retained.iter().map(|frame| frame.seq).collect::<Vec<_>>(),
            vec![3, 4, 5, 6, 7, 8, 9, 10]
        );
        let cursor = retained[4].seq;
        assert_eq!(
            ring.after(cursor).iter().map(|frame| frame.seq).collect::<Vec<_>>(),
            vec![8, 9, 10]
        );
        assert!(ring.after(10).is_empty());
    }

    #[test]
    fn red_one_clipped_sample_amid_silence_does_not_light_every_band() {
        // A legitimate clipped transient is a single full-scale sample, not
        // full-spectrum energy: it must pass through the filter bank like any
        // other sample instead of short-circuiting the frame to all-ceiling,
        // and the bank's state must keep advancing so the next frame decays
        // smoothly rather than jumping.
        let mut state = BandState::default();
        let mut pcm = vec![0_i16; 320];
        pcm[160] = i16::MAX;
        let frame = bands(&pcm, &mut state);
        assert_ne!(frame, [u8::MAX; BAND_COUNT], "clipping short-circuited the bank");
        assert!(
            frame.iter().all(|level| *level < 200),
            "an impulse carries almost no energy per band: {frame:?}"
        );
        let decayed = bands(&vec![0_i16; 320], &mut state);
        assert!(
            decayed.iter().zip(&frame).all(|(after, before)| after <= before),
            "the bank state did not advance through the clipped frame: {frame:?} -> {decayed:?}"
        );
    }

    #[test]
    fn red_long_silence_after_an_impulse_flushes_the_recursive_state_to_zero() {
        // Without a flush, the IIR recursion rings on shrinking residue (and
        // eventually denormals) forever after a transient. Sustained silence
        // must land every band at the floor AND drain the recursive state to
        // exactly zero so the filters go fully quiet.
        let mut state = BandState::default();
        let mut impulse = vec![0_i16; 320];
        impulse[0] = i16::MAX;
        bands(&impulse, &mut state);
        let settled = bands(&vec![0_i16; 16_000], &mut state);
        assert_eq!(settled, [0; BAND_COUNT]);
        for filter in &state.filters {
            assert_eq!(
                (filter.z1, filter.z2, filter.envelope),
                (0.0, 0.0, 0.0),
                "residual recursive state survived a full second of silence"
            );
        }
    }

    #[test]
    fn red_a_non_finite_intermediate_cannot_poison_the_filter_permanently() {
        // Belt-and-braces containment: should any recursive value ever go
        // non-finite, the filter must recover on the next samples instead of
        // propagating NaN into every future frame.
        let mut state = BandState::default();
        state.filters[7].z1 = f32::NAN;
        state.filters[7].envelope = f32::INFINITY;
        bands(&[1_000; 320], &mut state);
        let frame = bands(&sine(1_000.0, 640), &mut state);
        for filter in &state.filters {
            assert!(filter.z1.is_finite() && filter.z2.is_finite() && filter.envelope.is_finite());
        }
        assert!(frame[7] > 0, "the poisoned filter never recovered: {frame:?}");
    }

    #[test]
    fn red_a_second_recording_continues_the_sequence_for_a_stale_cursor() {
        // A stop/start pair can slip between two 200 ms status polls, so the
        // Overlay's cursor from the previous Recording may survive into the
        // next one. Frame sequences must be monotonic across Recordings or
        // that stale cursor would sit "ahead" of the fresh ring forever and
        // freeze the bars.
        let registry = LevelRegistry::default();
        let first = registry.begin_recording();
        for _ in 0..3 {
            first.push([1; BAND_COUNT]);
        }
        let stale_cursor = registry.after(0).last().unwrap().seq;
        first.deactivate();
        let second = registry.begin_recording();
        second.push([2; BAND_COUNT]);
        let fresh = registry.after(stale_cursor);
        assert_eq!(
            fresh.iter().map(|frame| frame.bands[0]).collect::<Vec<_>>(),
            vec![2],
            "a stale cursor from the previous Recording must still observe new frames"
        );
        assert!(fresh[0].seq > stale_cursor);
    }

    #[test]
    fn red_one_byte_reads_decode_the_same_samples_and_never_a_partial_one() {
        let bytes: Vec<u8> = (0..25).map(|value| value as u8).collect();
        let mut whole = SampleDecoder::default();
        let expected = whole.decode(&bytes);
        assert_eq!(expected.len(), 12, "12 complete samples in 25 bytes");
        let mut trickled = SampleDecoder::default();
        let mut samples = Vec::new();
        let mut empty_returns = 0;
        for byte in &bytes {
            let decoded = trickled.decode(std::slice::from_ref(byte));
            assert!(decoded.len() <= 1, "one byte can complete at most one sample");
            if decoded.is_empty() {
                empty_returns += 1;
            }
            samples.extend(decoded);
        }
        assert_eq!(samples, expected, "byte-at-a-time decode must match whole-buffer decode");
        // Half a sample decodes nothing: the caller must therefore have
        // nothing to push, so no all-zero frame can advance the ring's
        // sequence and evict real peaks under repeated tiny reads.
        assert_eq!(empty_returns, 13);
        assert!(SampleDecoder::default().decode(&[0x42]).is_empty());
    }

    #[test]
    fn red_short_reads_reassemble_without_changing_bytes_and_flush_the_tail() {
        let source = (0..6_731)
            .map(|value| (value % 251) as u8)
            .collect::<Vec<_>>();
        let mut assembler = PcmChunkAssembler::default();
        let mut chunks = Vec::new();
        for piece in source.chunks(137) {
            chunks.extend(assembler.push(piece));
        }
        if let Some(tail) = assembler.finish() {
            chunks.push(tail);
        }
        assert_eq!(
            chunks
                .iter()
                .take(chunks.len() - 1)
                .map(Vec::len)
                .collect::<Vec<_>>(),
            vec![PCM_CHUNK_BYTES; 2]
        );
        assert_eq!(chunks.concat(), source);
    }
}
