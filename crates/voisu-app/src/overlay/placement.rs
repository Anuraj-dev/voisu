//! Where the Overlay pill lands on an output. Pure arithmetic in logical
//! pixels, so placement is tested without a compositor.
//!
//! The saved [`OverlayPosition`] is the pill's centre as fractions of the
//! output. The Layer Shell surface is anchored to the top and left edges and
//! pushed into place with margins, recomputed whenever the output, the pill's
//! size or the position changes. Every function clamps so the whole pill stays
//! on the output, whatever the output's size.
//!
//! The Recording pill is never bound to an output: the compositor puts each
//! fresh map on the output it chooses (the focused one), and the Overlay only
//! learns which once the surface enters it. Until then [`safe_margins`] keep
//! the pill on screen on every connected output; [`output_margins`] picks
//! between the two.

use crate::config::OverlayPosition;

/// A width and height in logical pixels.
pub type Size = (i32, i32);

/// Within this many logical pixels of the output's vertical centre line, a
/// dragged pill snaps to it, so the centred default is easy to get back to.
pub const CENTRE_SNAP: f64 = 12.0;

/// The move-mode pill: ✗, the idle bars and ✓ in one capsule, wider than
/// the plain pill as on Windows. Move mode places and saves by this size, so
/// the centre a ✓ saves is the centre the user saw, and the plain pill (which
/// is narrower) lands centred on the same spot.
pub const MOVE_PILL: Size = (158, 32);

/// The left and top Layer Shell margins that put the centre of a pill of
/// `pill` size at `position` on an output of `output` size. A pill larger than
/// the output sits at its top-left corner.
#[must_use]
pub fn pill_margins(position: OverlayPosition, output: Size, pill: Size) -> (i32, i32) {
    let centre = (
        position.x() * f64::from(output.0),
        position.y() * f64::from(output.1),
    );
    let (x, y) = clamp_centre(centre, output, pill);
    (
        (x - f64::from(pill.0) / 2.0).round() as i32,
        (y - f64::from(pill.1) / 2.0).round() as i32,
    )
}

/// The largest size that fits inside every output: the narrowest width and
/// the shortest height, each taken on its own. Outputs with no area are
/// ignored; with none left there is no size.
#[must_use]
pub fn smallest_output(outputs: &[Size]) -> Option<Size> {
    let usable = || {
        outputs
            .iter()
            .copied()
            .filter(|&(width, height)| width > 0 && height > 0)
    };
    Some((
        usable().map(|(width, _)| width).min()?,
        usable().map(|(_, height)| height).min()?,
    ))
}

/// Margins for a surface whose output isn't known yet (a fresh map, or an
/// output that went away): the position placed on [`smallest_output`], so the
/// whole pill is on screen whichever connected output the compositor picks.
#[must_use]
pub fn safe_margins(position: OverlayPosition, connected: &[Size], pill: Size) -> (i32, i32) {
    pill_margins(position, smallest_output(connected).unwrap_or((0, 0)), pill)
}

/// Margins for the pill: from `entered`, the output the surface is on, once it
/// is known and has an area; otherwise [`safe_margins`] over `connected`.
#[must_use]
pub fn output_margins(
    position: OverlayPosition,
    pill: Size,
    entered: Option<Size>,
    connected: &[Size],
) -> (i32, i32) {
    match entered.filter(|&(width, height)| width > 0 && height > 0) {
        Some(output) => pill_margins(position, output, pill),
        None => safe_margins(position, connected, pill),
    }
}

/// `centre` (in output pixels) moved just enough that the pill around it
/// stays wholly on the output.
#[must_use]
pub fn clamp_centre(centre: (f64, f64), output: Size, pill: Size) -> (f64, f64) {
    let axis = |value: f64, output: i32, pill: i32| {
        let half = f64::from(pill) / 2.0;
        let far = (f64::from(output) - half).max(half);
        value.clamp(half, far)
    };
    (
        axis(centre.0, output.0, pill.0),
        axis(centre.1, output.1, pill.1),
    )
}

/// The position to save for a pill whose centre a drag left at `centre`:
/// clamped onto the output and snapped to the vertical centre line within
/// [`CENTRE_SNAP`]. An output with no area keeps the default.
#[must_use]
pub fn position_at(centre: (f64, f64), output: Size, pill: Size) -> OverlayPosition {
    if output.0 <= 0 || output.1 <= 0 {
        return OverlayPosition::DEFAULT;
    }
    let (mut x, y) = clamp_centre(centre, output, pill);
    let middle = f64::from(output.0) / 2.0;
    if (x - middle).abs() <= CENTRE_SNAP {
        x = middle;
    }
    OverlayPosition::from_fractions(x / f64::from(output.0), y / f64::from(output.1))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    const PILL: Size = (120, 32);
    const FHD: Size = (1920, 1080);

    fn at(x: f64, y: f64) -> OverlayPosition {
        OverlayPosition::from_fractions(x, y).unwrap()
    }

    #[test]
    fn margins_centre_the_pill_on_the_fractions_of_the_output() {
        assert_eq!(pill_margins(at(0.5, 0.5), FHD, PILL), (900, 524));
        assert_eq!(pill_margins(at(0.25, 0.1), FHD, PILL), (420, 92));
        // The same fractions follow the output's size.
        assert_eq!(pill_margins(at(0.25, 0.1), (2560, 1440), PILL), (580, 128));
        // The default sits 12 px above the bottom of a 1080 px output.
        assert_eq!(
            pill_margins(OverlayPosition::DEFAULT, FHD, PILL),
            (900, 1080 - 12 - 32)
        );
    }

    #[test]
    fn margins_keep_the_whole_pill_on_the_output() {
        assert_eq!(pill_margins(at(0.0, 0.0), FHD, PILL), (0, 0));
        assert_eq!(pill_margins(at(1.0, 1.0), FHD, PILL), (1800, 1048));
        assert_eq!(pill_margins(at(1.0, 0.0), FHD, PILL), (1800, 0));
        // A text pill widens about its centre and is clamped the same way.
        assert_eq!(pill_margins(at(0.5, 0.5), FHD, (320, 48)), (800, 516));
        assert_eq!(pill_margins(at(0.99, 0.5), FHD, (320, 48)), (1600, 516));
        // A pill bigger than the output sits at its corner, never negative.
        assert_eq!(pill_margins(at(0.5, 0.5), (100, 20), PILL), (0, 0));
        assert_eq!(pill_margins(at(0.5, 0.5), (0, 0), PILL), (0, 0));
    }

    #[test]
    fn a_dragged_centre_becomes_clamped_fractions_snapped_to_the_middle() {
        assert_eq!(position_at((480.0, 108.0), FHD, PILL), at(0.25, 0.1));
        // Past the edges, the pill stops at them.
        assert_eq!(
            position_at((-50.0, 5000.0), FHD, PILL),
            at(60.0 / 1920.0, 1064.0 / 1080.0)
        );
        // Near the vertical centre line, x snaps to it; farther, it does not.
        assert_eq!(position_at((965.0, 540.0), FHD, PILL).x(), 0.5);
        assert_eq!(position_at((948.0, 540.0), FHD, PILL).x(), 0.5);
        assert_eq!(
            position_at((990.0, 540.0), FHD, PILL),
            at(990.0 / 1920.0, 0.5)
        );
        assert_eq!(
            position_at((10.0, 10.0), (0, 0), PILL),
            OverlayPosition::DEFAULT
        );
        // Saving and placing again round-trips to the pixel.
        let saved = position_at((480.0, 108.0), FHD, PILL);
        assert_eq!(pill_margins(saved, FHD, PILL), (420, 92));
    }

    #[test]
    fn the_plain_pill_lands_centred_where_the_move_pill_was_dropped() {
        for centre in [
            (480.0, 108.0),
            (1900.0, 1075.0),
            (3.0, 2.0),
            (960.0, 1052.0),
        ] {
            let saved = position_at(centre, FHD, MOVE_PILL);
            let moving = pill_margins(saved, FHD, MOVE_PILL);
            let plain = pill_margins(saved, FHD, PILL);
            assert_eq!(
                (plain.0 + PILL.0 / 2, plain.1 + PILL.1 / 2),
                (moving.0 + MOVE_PILL.0 / 2, moving.1 + MOVE_PILL.1 / 2),
                "{centre:?}"
            );
        }
    }
    /// Raja's desk: eDP-1 1920x1200 at 1.25 and HDMI-A-1 1920x1080 at 1, as
    /// GDK reports them, in logical pixels.
    const LAPTOP: Size = (1536, 960);
    const HDMI: Size = FHD;

    fn on_output(margins: (i32, i32), output: Size, pill: Size) -> bool {
        margins.0 >= 0
            && margins.1 >= 0
            && margins.0 + pill.0 <= output.0
            && margins.1 + pill.1 <= output.1
    }

    #[test]
    fn the_smallest_output_is_the_narrowest_width_and_the_shortest_height() {
        assert_eq!(smallest_output(&[LAPTOP, HDMI]), Some(LAPTOP));
        // Each axis on its own: a tall narrow output and a wide short one.
        assert_eq!(
            smallest_output(&[(1080, 1920), (2560, 1080)]),
            Some((1080, 1080))
        );
        // An output with no area (unplugged, not yet configured) is ignored.
        assert_eq!(smallest_output(&[(0, 0), HDMI]), Some(HDMI));
        assert_eq!(smallest_output(&[]), None);
        assert_eq!(smallest_output(&[(0, 1080)]), None);
    }

    #[test]
    fn before_the_output_is_known_the_pill_fits_every_connected_output() {
        let connected = [HDMI, LAPTOP, (1080, 1920), (3840, 2160)];
        for position in [
            OverlayPosition::DEFAULT,
            at(1.0, 1.0),
            at(0.0, 0.0),
            at(0.99, 0.02),
        ] {
            for pill in [PILL, MOVE_PILL, (320, 48)] {
                let margins = safe_margins(position, &connected, pill);
                for output in connected {
                    assert!(
                        on_output(margins, output, pill),
                        "{position} {pill:?} off {output:?}: {margins:?}"
                    );
                }
            }
        }
        // The default on the smaller laptop panel: centred, near the bottom.
        assert_eq!(
            safe_margins(OverlayPosition::DEFAULT, &[HDMI, LAPTOP], PILL),
            (708, 919)
        );
        // With no output at all it still never goes negative.
        assert_eq!(safe_margins(OverlayPosition::DEFAULT, &[], PILL), (0, 0));
    }

    #[test]
    fn once_the_surface_enters_an_output_its_margins_come_from_that_output() {
        let connected = [HDMI, LAPTOP];
        let position = OverlayPosition::DEFAULT;
        // The same saved fractions land at the same relative spot on each.
        assert_eq!(
            output_margins(position, PILL, Some(LAPTOP), &connected),
            (708, 919)
        );
        assert_eq!(
            output_margins(position, PILL, Some(HDMI), &connected),
            (900, 1036)
        );
        // On the laptop, HDMI's margins would put the pill below the panel:
        // the very bug a margin from the wrong output caused.
        assert!(!on_output(
            output_margins(position, PILL, Some(HDMI), &connected),
            LAPTOP,
            PILL
        ));
        // Unknown (left, or not yet entered) or arealess output: safe margins.
        for entered in [None, Some((0, 0))] {
            assert_eq!(
                output_margins(position, PILL, entered, &connected),
                safe_margins(position, &connected, PILL)
            );
        }
        // A position saved on one output is the same fractions on another.
        let saved = position_at((480.0, 108.0), HDMI, MOVE_PILL);
        assert_eq!(
            output_margins(saved, PILL, Some(LAPTOP), &connected),
            pill_margins(saved, LAPTOP, PILL)
        );
        assert_eq!(pill_margins(saved, LAPTOP, PILL), (324, 80));
    }
}
