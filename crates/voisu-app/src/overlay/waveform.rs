//! Recording-meter motion: one eased mic level that scrolls outward from the
//! centre of the bar row, newest at the middle and oldest at the edges. It is
//! the motion of the Windows overlay (`apps/windows/ui/overlay/overlay.js`):
//! the same 30 Hz sample cadence, the same attack/release coefficients, and
//! the same mirrored centre-out history.
//!
//! The Windows page steps once per 30 Hz level event. Here the GTK frame clock
//! advances the state by the real frame interval instead, so the eased level
//! moves continuously between samples and the history glides outward by a
//! fraction of a bar each frame rather than jumping a whole bar per sample.
//! At every 30 Hz sample boundary the eased level equals what the Windows
//! filter would hold, so the two platforms share one curve.
//!
//! Pure and deterministic: the output depends only on the targets and frame
//! intervals fed in, never on a clock.

use voisu_core::LevelFrame;

/// History slots per half of the row. The row is mirrored, so a 44-bar meter
/// shows 22 slots of history on each side of the centre.
pub const HISTORY: usize = super::VISUAL_BAR_COUNT / 2;
/// One history sample every 1/30 s, matching the Windows `audio-level` rate.
pub const SAMPLE_PERIOD_SECS: f64 = 1.0 / 30.0;
/// Per-sample easing toward the target: fast rise, slow fall (Windows values).
const ATTACK: f64 = 0.65;
const RELEASE: f64 = 0.16;
/// The longest frame interval honoured in one advance. A frame clock that
/// stalled (suspend, compositor hiccup) resumes from here instead of replaying
/// seconds of motion in one frame; it also bounds the per-call work.
pub const MAX_FRAME_SECS: f64 = 0.25;
/// Band levels are dB-scaled over this range (see `audio_level::Biquad::level`),
/// and the meter reads the summed level on the same -60..0 dB range Windows
/// meters its RMS on.
const FLOOR_DB: f64 = -60.0;

#[derive(Clone, Debug, PartialEq)]
pub struct Waveform {
    /// The newest level the daemon reported, 0.0..=1.0.
    target: f64,
    /// The eased live level drawn at the centre.
    head: f64,
    /// Committed samples, `history[0]` newest.
    history: [f64; HISTORY],
    /// Seconds since `history[0]` was committed, in `0.0..SAMPLE_PERIOD_SECS`.
    since_sample: f64,
}

impl Default for Waveform {
    fn default() -> Self {
        Self {
            target: 0.0,
            head: 0.0,
            history: [0.0; HISTORY],
            since_sample: 0.0,
        }
    }
}

impl Waveform {
    /// Silence: every bar back on the floor, as at the start of a Recording.
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// The level the head eases toward from now on. Non-finite input reads as
    /// silence; anything else is clamped to the meter range.
    pub fn set_target(&mut self, level: f64) {
        self.target = if level.is_finite() {
            level.clamp(0.0, 1.0)
        } else {
            0.0
        };
    }

    /// Advance by one frame interval. The interval is split at every 30 Hz
    /// boundary so the result does not depend on how the time was sliced.
    pub fn advance(&mut self, frame_secs: f64) {
        if !frame_secs.is_finite() || frame_secs <= 0.0 {
            return;
        }
        let mut remaining = frame_secs.min(MAX_FRAME_SECS);
        while remaining > 0.0 {
            let until_sample = SAMPLE_PERIOD_SECS - self.since_sample;
            if remaining < until_sample {
                self.ease(remaining);
                self.since_sample += remaining;
                return;
            }
            self.ease(until_sample);
            remaining -= until_sample;
            self.history.copy_within(..HISTORY - 1, 1);
            self.history[0] = self.head;
            self.since_sample = 0.0;
        }
    }

    /// Exponential easing scaled to `secs`: over one whole sample period it is
    /// exactly `head += (target - head) * coefficient`, the Windows step, and
    /// any split of that period composes to the same value.
    fn ease(&mut self, secs: f64) {
        let coefficient = if self.target > self.head {
            ATTACK
        } else {
            RELEASE
        };
        let kept = (1.0 - coefficient).powf(secs / SAMPLE_PERIOD_SECS);
        self.head = self.target + (self.head - self.target) * kept;
    }

    /// The level at `slot` steps out from the centre (0 = centre). The head
    /// sits at 0 and each committed sample `k` sits at `k + fraction`, where
    /// `fraction` is how far the current sample period has run, so the curve
    /// slides outward continuously and a commit never moves a bar.
    pub fn slot_level(&self, slot: usize) -> f64 {
        if slot == 0 {
            return self.head;
        }
        let slot = slot.min(HISTORY - 1);
        let fraction = self.since_sample / SAMPLE_PERIOD_SECS;
        // Slot `s` lies between sample s-1 (at s-1+fraction) and sample s (at
        // s+fraction): `fraction` of the way back toward the newer one.
        self.history[slot - 1] * fraction + self.history[slot] * (1.0 - fraction)
    }

    /// The level for bar `index` of a `count`-bar row mirrored about its
    /// centre: the two middle bars show the head, the outer bars the oldest
    /// history.
    pub fn bar_level(&self, index: usize, count: usize) -> f64 {
        let half = count / 2;
        let slot = if index < half {
            half - 1 - index
        } else {
            index - half
        };
        self.slot_level(slot)
    }
}

/// One mic level, 0.0..=1.0, from a 20-band frame. Each band carries its own
/// dB level over the -60..0 dBFS meter range; summing the band powers gives
/// the frame's overall loudness, which maps back onto the same dB range.
/// The range matches the Windows RMS meter, but the value is not identical:
/// the bandpasses overlap, so any signal, a pure tone included, also counts
/// in neighbouring bands and reads a few dB above its true RMS.
pub fn level_from_bands(bands: &[u8; 20]) -> f64 {
    let power: f64 = bands
        .iter()
        .filter(|&&band| band > 0)
        .map(|&band| {
            let db = FLOOR_DB + f64::from(band) / f64::from(u8::MAX) * -FLOOR_DB;
            10.0_f64.powf(db / 10.0)
        })
        .sum();
    if power <= 0.0 {
        return 0.0;
    }
    ((10.0 * power.log10() - FLOOR_DB) / -FLOOR_DB).clamp(0.0, 1.0)
}

/// The target one Level poll publishes. A poll that returned frames targets
/// the loudest of them, so a peak that arrived between two polls still reaches
/// the meter. A poll that returned no new frames publishes nothing, and the
/// current target holds. A failed poll (`None`) targets silence: the bars ease
/// down rather than freeze, and daemon liveness stays with the status poll.
pub fn poll_target(poll: Option<&[LevelFrame]>) -> Option<f64> {
    match poll {
        None => Some(0.0),
        Some(frames) => frames
            .iter()
            .map(|frame| level_from_bands(&frame.bands))
            .reduce(f64::max),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EPS: f64 = 1e-9;

    fn frame(bands: [u8; 20]) -> LevelFrame {
        LevelFrame { seq: 1, bands }
    }

    #[test]
    fn one_sample_period_is_exactly_the_windows_easing_step() {
        let mut rising = Waveform::default();
        rising.set_target(1.0);
        rising.advance(SAMPLE_PERIOD_SECS);
        assert!((rising.slot_level(0) - 0.65).abs() < EPS);

        let mut falling = Waveform::default();
        falling.set_target(1.0);
        falling.advance(MAX_FRAME_SECS);
        let start = falling.slot_level(0);
        falling.set_target(0.0);
        falling.advance(SAMPLE_PERIOD_SECS);
        assert!((falling.slot_level(0) - start * 0.84).abs() < EPS);
    }

    #[test]
    fn rises_fast_and_falls_slowly() {
        let mut waveform = Waveform::default();
        waveform.set_target(1.0);
        waveform.advance(0.05);
        let risen = waveform.slot_level(0);
        waveform.set_target(0.0);
        waveform.advance(0.05);
        let fallen = risen - waveform.slot_level(0);
        assert!(risen > 0.75, "attack reached {risen}");
        assert!(fallen < risen / 2.0, "release dropped {fallen} of {risen}");
    }

    #[test]
    fn result_does_not_depend_on_how_frames_slice_time() {
        let targets = [0.2, 0.9, 0.0, 0.6, 0.6, 0.1];
        let mut coarse = Waveform::default();
        let mut fine = Waveform::default();
        for &target in &targets {
            coarse.set_target(target);
            coarse.advance(0.07);
            fine.set_target(target);
            for _ in 0..7 {
                fine.advance(0.01);
            }
        }
        for slot in 0..HISTORY {
            assert!(
                (coarse.slot_level(slot) - fine.slot_level(slot)).abs() < 1e-9,
                "slot {slot}: {} vs {}",
                coarse.slot_level(slot),
                fine.slot_level(slot)
            );
        }
    }

    #[test]
    fn a_burst_travels_outward_one_slot_per_sample() {
        let mut waveform = Waveform::default();
        waveform.set_target(1.0);
        waveform.advance(SAMPLE_PERIOD_SECS);
        waveform.set_target(0.0);
        for age in 1..10 {
            waveform.advance(SAMPLE_PERIOD_SECS);
            let peak = (0..HISTORY)
                .max_by(|&a, &b| waveform.slot_level(a).total_cmp(&waveform.slot_level(b)))
                .unwrap();
            assert_eq!(peak, age, "after {age} samples the burst is at slot {peak}");
        }
    }

    #[test]
    fn bars_glide_through_a_sample_commit_without_jumping() {
        let mut waveform = Waveform::default();
        for (step, target) in [0.9, 0.1, 0.7, 0.3, 1.0, 0.0].into_iter().enumerate() {
            waveform.set_target(target);
            waveform.advance(SAMPLE_PERIOD_SECS * if step == 0 { 0.5 } else { 1.0 });
        }
        // Straddle the next commit with two tiny frames.
        let until = SAMPLE_PERIOD_SECS - waveform.since_sample;
        waveform.advance(until - 1e-6);
        let before: Vec<f64> = (0..HISTORY).map(|s| waveform.slot_level(s)).collect();
        waveform.advance(2e-6);
        for (slot, &previous) in before.iter().enumerate() {
            let delta = (waveform.slot_level(slot) - previous).abs();
            assert!(delta < 1e-3, "slot {slot} jumped by {delta} at a commit");
        }
    }

    #[test]
    fn the_row_is_mirrored_with_the_head_in_the_middle() {
        let mut waveform = Waveform::default();
        for target in [0.3, 0.8, 0.5, 1.0] {
            waveform.set_target(target);
            waveform.advance(0.045);
        }
        let count = super::super::VISUAL_BAR_COUNT;
        for index in 0..count {
            let mirrored = waveform.bar_level(count - 1 - index, count);
            assert!((waveform.bar_level(index, count) - mirrored).abs() < EPS);
        }
        assert!((waveform.bar_level(count / 2, count) - waveform.slot_level(0)).abs() < EPS);
        assert!(
            (waveform.bar_level(0, count) - waveform.slot_level(HISTORY - 1)).abs() < EPS,
            "the outermost bar shows the oldest history"
        );
    }

    #[test]
    fn a_stalled_frame_clock_cannot_replay_seconds_of_motion() {
        let mut stalled = Waveform::default();
        stalled.set_target(1.0);
        stalled.advance(30.0);
        let mut capped = Waveform::default();
        capped.set_target(1.0);
        capped.advance(MAX_FRAME_SECS);
        assert_eq!(stalled, capped);
    }

    #[test]
    fn bad_input_never_moves_or_poisons_the_meter() {
        let mut waveform = Waveform::default();
        waveform.set_target(0.5);
        waveform.advance(0.1);
        let settled = waveform.clone();
        for frame_secs in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            waveform.advance(frame_secs);
        }
        assert_eq!(waveform, settled);
        waveform.set_target(f64::NAN);
        waveform.advance(0.1);
        assert!(waveform.slot_level(0).is_finite());
        waveform.set_target(7.0);
        waveform.advance(MAX_FRAME_SECS);
        assert!(waveform.slot_level(0) <= 1.0);
    }

    #[test]
    fn reset_returns_every_bar_to_the_floor() {
        let mut waveform = Waveform::default();
        waveform.set_target(1.0);
        waveform.advance(MAX_FRAME_SECS);
        waveform.reset();
        assert!((0..HISTORY).all(|slot| waveform.slot_level(slot) == 0.0));
    }

    #[test]
    fn band_frames_map_onto_the_windows_db_meter() {
        assert_eq!(level_from_bands(&[0; 20]), 0.0);
        // One band at full scale is 0 dBFS: the top of the meter.
        let mut single = [0; 20];
        single[7] = 255;
        assert!((level_from_bands(&single) - 1.0).abs() < EPS);
        assert_eq!(level_from_bands(&[255; 20]), 1.0);
        // Twenty bands at -40 dB sum to -40 + 10*log10(20) ≈ -27 dB.
        let minus_40 = (20.0 / 60.0 * 255.0_f64).round() as u8;
        let db = FLOOR_DB + f64::from(minus_40) / 255.0 * 60.0;
        let want = (db + 10.0 * 20.0_f64.log10() + 60.0) / 60.0;
        assert!((level_from_bands(&[minus_40; 20]) - want).abs() < EPS);
        // Louder bands never read quieter.
        assert!(level_from_bands(&[90; 20]) < level_from_bands(&[140; 20]));
    }

    #[test]
    fn a_poll_keeps_the_loudest_frame_an_empty_poll_holds_and_a_failure_is_silence() {
        let quiet = frame([40; 20]);
        let loud = frame([220; 20]);
        let level = poll_target(Some(&[quiet, loud, quiet])).unwrap();
        assert!((level - level_from_bands(&loud.bands)).abs() < EPS);
        assert_eq!(poll_target(Some(&[])), None);
        assert_eq!(poll_target(None), Some(0.0));
    }
}
