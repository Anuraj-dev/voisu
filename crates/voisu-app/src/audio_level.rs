use std::collections::VecDeque;
use std::f32::consts::PI;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use voisu_core::LevelFrame;

pub const BAND_COUNT: usize = 20;
pub const PCM_CHUNK_BYTES: usize = 3_200;
const LEVEL_RING_CAPACITY: usize = 8;
const SAMPLE_RATE: f32 = 16_000.0;

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
        self.z1 = self.b1 * sample - self.a1 * filtered + self.z2;
        self.z2 = self.b2 * sample - self.a2 * filtered;
        let power = filtered * filtered;
        let coefficient = if power > self.envelope { 0.18 } else { 0.025 };
        self.envelope += coefficient * (power - self.envelope);
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
                // A digital bandpass cannot be centred exactly on Nyquist;
                // one hertz below keeps the specified upper edge numerically stable.
                let filter = Biquad::bandpass(frequency.min(7_999.0));
                frequency *= ratio;
                filter
            }),
        }
    }
}

/// Compute one log-spaced frequency-band frame from any number of mono samples.
pub fn bands(pcm: &[i16], state: &mut BandState) -> [u8; BAND_COUNT] {
    if pcm.is_empty() {
        return [0; BAND_COUNT];
    }
    if pcm.iter().any(|sample| sample.unsigned_abs() >= i16::MAX as u16) {
        return [u8::MAX; BAND_COUNT];
    }
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
    fn red_silence_is_floor_and_clipping_is_ceiling() {
        assert_eq!(
            bands(&[0; 320], &mut BandState::default()),
            [0; BAND_COUNT]
        );
        assert_eq!(
            bands(&[i16::MAX; 320], &mut BandState::default()),
            [u8::MAX; BAND_COUNT]
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
