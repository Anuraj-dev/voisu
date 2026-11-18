//! Bounded-memory windowing for one finalized Recording, ported from
//! `tools/local-asr/voisu_asr/windowing.py` (goldens in `tests/window_golden.rs`).
//!
//! A Recording is split into sequential, non-overlapping windows of about 30
//! seconds. Each window is transcribed on its own with no text or state from
//! any other window. Cut points prefer the quietest 200 ms frame inside the
//! last 5 seconds before the limit so a word is not split; a region with no dip
//! falls back to a hard cut at the limit. A final remainder shorter than 1
//! second is folded into the window before it (so one window may reach 31
//! seconds): a near-empty window could be rejected by a model and fail the
//! whole request.

use voisu_core::local_wire::PCM_SAMPLE_RATE_HZ;

const RATE: usize = PCM_SAMPLE_RATE_HZ as usize;
pub const WINDOW_SAMPLES: usize = 30 * RATE;
pub const SEARCH_SAMPLES: usize = 5 * RATE;
/// 200 ms frames are longer than a stop-consonant closure, so the quietest one
/// is normally a pause between words rather than a gap inside a word.
pub const FRAME_SAMPLES: usize = RATE / 5;
pub const HOP_SAMPLES: usize = RATE / 100;
pub const MAX_WINDOWS: usize = 20;
/// The final window is never shorter than this unless it is the only window.
pub const MIN_FINAL_WINDOW_SAMPLES: usize = RATE;
/// A cut is "quiet" when the quietest frame dips to at most this share of the
/// RMS of the loud (90th percentile) frames in the search region. It only
/// sends steady sound with no dip (tone, music, constant noise) to a hard cut.
pub const QUIET_RATIO: f64 = 0.8;

/// How a window ended.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Cut {
    Quiet,
    Hard,
    /// The final window.
    End,
}

/// One half-open sample range `[start, end)`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Window {
    pub start: usize,
    pub end: usize,
    pub cut: Cut,
}

/// The Recording needs more than [`MAX_WINDOWS`] windows.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TooManyWindows;

/// Split mono PCM16 samples into ordered, contiguous, non-overlapping windows.
///
/// # Errors
///
/// [`TooManyWindows`] when the Recording is longer than
/// `MAX_WINDOWS × WINDOW_SAMPLES`.
pub fn plan_windows(samples: &[i16]) -> Result<Vec<Window>, TooManyWindows> {
    let total = samples.len();
    if total > WINDOW_SAMPLES * MAX_WINDOWS {
        return Err(TooManyWindows);
    }
    let mut windows: Vec<Window> = Vec::new();
    let mut start = 0usize;
    while total - start > WINDOW_SAMPLES {
        let limit = start + WINDOW_SAMPLES;
        let remaining_windows = MAX_WINDOWS - windows.len() - 1;
        // Never cut so early that the rest would need more than the cap.
        let earliest = (limit - SEARCH_SAMPLES)
            .max(total.saturating_sub(remaining_windows * WINDOW_SAMPLES))
            .max(start + 1);
        let window = match quiet_cut(samples, earliest, limit) {
            Some(cut) => Window {
                start,
                end: cut,
                cut: Cut::Quiet,
            },
            None => Window {
                start,
                end: limit,
                cut: Cut::Hard,
            },
        };
        start = window.end;
        windows.push(window);
    }
    if total - start < MIN_FINAL_WINDOW_SAMPLES
        && let Some(previous) = windows.pop()
    {
        start = previous.start;
    }
    windows.push(Window {
        start,
        end: total,
        cut: Cut::End,
    });
    Ok(windows)
}

/// The centre of the quietest frame in `[lo, hi)`, or `None` when the region
/// is shorter than one frame or has no clear dip. Among equally quiet frames
/// the latest wins, keeping windows long.
fn quiet_cut(samples: &[i16], lo: usize, hi: usize) -> Option<usize> {
    if hi < lo + FRAME_SAMPLES {
        return None;
    }
    let region = &samples[lo..hi];
    let mut prefix = Vec::with_capacity(region.len() + 1);
    let mut energy = 0u64;
    prefix.push(0);
    for &sample in region {
        energy += u64::from(sample.unsigned_abs()).pow(2);
        prefix.push(energy);
    }
    // Exact integer sums, then one division and sqrt: bit-identical to the
    // Python reference, which the goldens rely on for ties.
    let frames: Vec<(f64, usize)> = (0..=region.len() - FRAME_SAMPLES)
        .step_by(HOP_SAMPLES)
        .map(|offset| {
            let sum = prefix[offset + FRAME_SAMPLES] - prefix[offset];
            ((sum as f64 / FRAME_SAMPLES as f64).sqrt(), offset)
        })
        .collect();
    let (quietest_rms, quietest_offset) =
        frames
            .iter()
            .copied()
            .fold((f64::INFINITY, 0), |best, frame| {
                if frame.0 <= best.0 { frame } else { best }
            });
    let mut sorted: Vec<f64> = frames.iter().map(|frame| frame.0).collect();
    sorted.sort_by(f64::total_cmp);
    let loud = sorted[(sorted.len() * 9) / 10];
    if quietest_rms > QUIET_RATIO * loud {
        return None;
    }
    Some(lo + quietest_offset + FRAME_SAMPLES / 2)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_recording_is_one_window() {
        let samples = vec![100i16; 5 * RATE];
        assert_eq!(
            plan_windows(&samples).unwrap(),
            vec![Window {
                start: 0,
                end: 5 * RATE,
                cut: Cut::End
            }]
        );
    }

    #[test]
    fn longer_than_the_cap_is_refused() {
        let samples = vec![0i16; WINDOW_SAMPLES * MAX_WINDOWS + 1];
        assert_eq!(plan_windows(&samples), Err(TooManyWindows));
    }

    #[test]
    fn empty_recording_is_one_empty_window() {
        assert_eq!(
            plan_windows(&[]).unwrap(),
            vec![Window {
                start: 0,
                end: 0,
                cut: Cut::End
            }]
        );
    }
}
