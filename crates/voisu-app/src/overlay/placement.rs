//! Where the Overlay pill lands on an output. Pure arithmetic in logical
//! pixels, so placement is tested without a compositor.
//!
//! The saved [`OverlayPosition`] is the pill's centre as fractions of the
//! output. The Layer Shell surface is anchored to the top and left edges and
//! pushed into place with margins. Every function clamps so the whole pill
//! stays on the output, whatever the output's size.
//!
//! The pill's surface is a fixed, transparent [`PILL_BOX`], big enough for
//! every pill state, and the drawn pill sits inside it ([`box_placement`]).
//! So a state change that resizes the pill only re-lays it out inside the
//! surface; the margins, which a compositor such as Hyprland animates when
//! they change on a mapped surface, depend on the position and the output
//! alone.
//!
//! When the Overlay can't ask which output is focused, it maps the surface
//! unbound and learns the output once the surface enters it. Until then
//! [`safe_margins`] keep the pill on screen on every connected output;
//! [`output_margins`] picks between the two.

use crate::config::OverlayPosition;
use crate::overlay::PILL_MAX_WIDTH;

/// A width and height in logical pixels.
pub type Size = (i32, i32);

/// Within this many logical pixels of the output's vertical centre line, a
/// dragged pill snaps to it, so the centred default is easy to get back to.
pub const CENTRE_SNAP: f64 = 12.0;

/// The move-mode pill: ✗, the idle bars and ✓ in one capsule, wider than
/// the plain pill as on Windows. Move mode places and saves by this size, so
/// the centre a ✓ saves is the centre the user saw, and the plain pill (which
/// is narrower) lands centred on the same spot.
pub const MOVE_PILL: Size = (158, 40);

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

/// The pill's surface: as wide as the widest text pill and tall enough for
/// two lines of text, so no pill state resizes it.
pub const PILL_BOX: Size = (PILL_MAX_WIDTH, 64);

/// Where the [`PILL_BOX`] surface goes and where a pill of `pill` size sits
/// inside it, for `position` on `output` (or, unknown, on every one of
/// `connected`): the box's margins, and the pill's offset from the box's
/// top-left corner. The box is centred on the saved point and clamped to the
/// output like a pill; the offset puts the pill exactly where
/// [`output_margins`] would put a surface of the pill's own size. The margins
/// never depend on `pill`.
#[must_use]
pub fn box_placement(
    position: OverlayPosition,
    pill: Size,
    output: Option<Size>,
    connected: &[Size],
) -> ((i32, i32), (i32, i32)) {
    let margins = output_margins(position, PILL_BOX, output, connected);
    let wanted = output_margins(position, pill, output, connected);
    // Only an output smaller than the box could put the pill outside it.
    let inside = |at: i32, from: i32, room: i32| (at - from).clamp(0, room.max(0));
    let offset = (
        inside(wanted.0, margins.0, PILL_BOX.0 - pill.0),
        inside(wanted.1, margins.1, PILL_BOX.1 - pill.1),
    );
    (margins, offset)
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

    const PILL: Size = (120, 40);
    const FHD: Size = (1920, 1080);

    fn at(x: f64, y: f64) -> OverlayPosition {
        OverlayPosition::from_fractions(x, y).unwrap()
    }

    #[test]
    fn margins_centre_the_pill_on_the_fractions_of_the_output() {
        assert_eq!(pill_margins(at(0.5, 0.5), FHD, PILL), (900, 520));
        assert_eq!(pill_margins(at(0.25, 0.1), FHD, PILL), (420, 88));
        // The same fractions follow the output's size.
        assert_eq!(pill_margins(at(0.25, 0.1), (2560, 1440), PILL), (580, 124));
        // The default's bottom edge sits 19 px above the bottom of a 1080 px
        // output.
        assert_eq!(
            pill_margins(OverlayPosition::DEFAULT, FHD, PILL),
            (900, 1080 - 19 - 40)
        );
    }

    #[test]
    fn margins_keep_the_whole_pill_on_the_output() {
        assert_eq!(pill_margins(at(0.0, 0.0), FHD, PILL), (0, 0));
        assert_eq!(pill_margins(at(1.0, 1.0), FHD, PILL), (1800, 1040));
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
            at(60.0 / 1920.0, 1060.0 / 1080.0)
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
        assert_eq!(pill_margins(saved, FHD, PILL), (420, 88));
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
            (708, 905)
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
            (708, 905)
        );
        assert_eq!(
            output_margins(position, PILL, Some(HDMI), &connected),
            (900, 1021)
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
        assert_eq!(pill_margins(saved, LAPTOP, PILL), (324, 76));
    }

    /// Every pill state the box must hold: the bars, the move pill's
    /// footprint, and one- and two-line text pills up to the widest.
    const STATES: [Size; 5] = [PILL, (180, 40), (320, 40), (320, 48), (256, 58)];

    #[test]
    fn the_pill_sits_inside_the_box_where_its_own_margins_would_put_it() {
        let connected = [HDMI, LAPTOP];
        for output in [Some(HDMI), Some(LAPTOP), Some((3840, 2160)), None] {
            for position in [
                OverlayPosition::DEFAULT,
                at(0.5, 0.5),
                at(0.0, 0.0),
                at(1.0, 1.0),
                at(0.02, 0.98),
                at(0.99, 0.01),
            ] {
                for pill in STATES {
                    let (margins, offset) = box_placement(position, pill, output, &connected);
                    let wanted = output_margins(position, pill, output, &connected);
                    assert_eq!(
                        (margins.0 + offset.0, margins.1 + offset.1),
                        wanted,
                        "{position} {pill:?} on {output:?}"
                    );
                    assert!(offset.0 >= 0 && offset.0 + pill.0 <= PILL_BOX.0);
                    assert!(offset.1 >= 0 && offset.1 + pill.1 <= PILL_BOX.1);
                    let on = output.unwrap_or(LAPTOP);
                    assert!(on_output(margins, on, PILL_BOX), "box off {on:?}");
                }
            }
        }
    }

    #[test]
    fn the_box_is_centred_on_the_point_and_clamped_at_the_edges() {
        // Centred: the pill in the middle of the box.
        assert_eq!(
            box_placement(at(0.5, 0.5), PILL, Some(HDMI), &[HDMI]),
            ((800, 508), (100, 12))
        );
        // The default on HDMI: the pill where it always was, (900,1021).
        assert_eq!(
            box_placement(OverlayPosition::DEFAULT, PILL, Some(HDMI), &[HDMI]),
            ((800, 1009), (100, 12))
        );
        // At the corners the box stops at the edge and the pill shifts
        // inside it to reach the edge too.
        assert_eq!(
            box_placement(at(0.0, 0.0), PILL, Some(HDMI), &[HDMI]),
            ((0, 0), (0, 0))
        );
        assert_eq!(
            box_placement(at(1.0, 1.0), PILL, Some(HDMI), &[HDMI]),
            ((1600, 1016), (200, 24))
        );
        // Unknown output on mixed outputs: placed on the smallest, the laptop.
        assert_eq!(
            box_placement(OverlayPosition::DEFAULT, PILL, None, &[HDMI, LAPTOP]),
            ((608, 893), (100, 12))
        );
        // An output smaller than the box: the box at its corner, the pill
        // still on the output.
        let tiny = (200, 50);
        let (margins, offset) = box_placement(at(0.5, 0.5), PILL, Some(tiny), &[tiny]);
        assert_eq!(margins, (0, 0));
        assert!(on_output(offset, tiny, PILL), "{offset:?}");
    }

    #[test]
    fn a_state_change_never_moves_the_box() {
        let connected = [HDMI, LAPTOP];
        for output in [Some(HDMI), Some(LAPTOP), None] {
            for position in [
                OverlayPosition::DEFAULT,
                at(0.0, 0.0),
                at(1.0, 1.0),
                at(0.3, 0.7),
            ] {
                let (first, _) = box_placement(position, PILL, output, &connected);
                for pill in STATES {
                    assert_eq!(
                        box_placement(position, pill, output, &connected).0,
                        first,
                        "{position} {pill:?} on {output:?}"
                    );
                }
            }
        }
    }
}
