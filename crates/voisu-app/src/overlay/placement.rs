//! Where the Overlay pill lands on an output. Pure arithmetic in logical
//! pixels, so placement is tested without a compositor.
//!
//! The saved [`OverlayPosition`] is the pill's centre as fractions of the
//! output. The Layer Shell surface is anchored to the top and left edges and
//! pushed into place with margins, recomputed whenever the output, the pill's
//! size or the position changes. Every function clamps so the whole pill stays
//! on the output, whatever the output's size.

use crate::config::OverlayPosition;

/// A width and height in logical pixels.
pub type Size = (i32, i32);

/// Within this many logical pixels of the output's vertical centre line, a
/// dragged pill snaps to it, so the centred default is easy to get back to.
pub const CENTRE_SNAP: f64 = 12.0;

/// Space between the pill and the move-mode buttons, in logical pixels.
pub const BUTTONS_GAP: i32 = 8;

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

/// The top-left corner of the move-mode buttons beside a pill whose top-left
/// corner is `pill_origin`: to its right, or to its left when the right side
/// has no room, vertically centred on the pill and kept on the output.
#[must_use]
pub fn buttons_origin(
    pill_origin: (i32, i32),
    pill: Size,
    buttons: Size,
    output: Size,
) -> (i32, i32) {
    let right = pill_origin.0 + pill.0 + BUTTONS_GAP;
    let x = if right + buttons.0 <= output.0 {
        right
    } else {
        pill_origin.0 - BUTTONS_GAP - buttons.0
    };
    let y = pill_origin.1 + (pill.1 - buttons.1) / 2;
    (
        x.clamp(0, (output.0 - buttons.0).max(0)),
        y.clamp(0, (output.1 - buttons.1).max(0)),
    )
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
    fn buttons_sit_right_of_the_pill_or_left_at_the_right_edge() {
        let buttons = (64, 28);
        assert_eq!(buttons_origin((900, 524), PILL, buttons, FHD), (1028, 526));
        assert_eq!(buttons_origin((1800, 524), PILL, buttons, FHD), (1728, 526));
        // Kept on the output even at the very top.
        assert_eq!(buttons_origin((0, 0), (120, 20), buttons, FHD), (128, 0));
    }
}
