//! When a freshly mapped Overlay pill may be seen. Pure state, so the order
//! of compositor events is tested without one.
//!
//! An unbound Layer Shell surface learns its output only after its first
//! buffer is mapped (the compositor's `wl_surface.enter`). Until then the
//! pill sits at margins safe on every output, which on a bigger output are
//! the wrong spot. So each fresh map is held: the pill draws transparent
//! frames until the output is known and a frame at that output's margins has
//! been painted, and only the frame after that shows it. If the output never
//! says, the pill shows anyway at [`REVEAL_DEADLINE`], at the best margins
//! known.
//!
//! Every callback the Overlay arms carries a token. Unmapping, a new map, or
//! a later change (a new output, a move) supersedes it, so a stale timer or
//! paint never reveals the wrong frame.

use std::time::Duration;

/// The longest a fresh map stays transparent while it waits for its output.
pub const REVEAL_DEADLINE: Duration = Duration::from_millis(150);

/// Names one fresh map, for its [`REVEAL_DEADLINE`] timer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MapToken(u64);

/// Names one transparent paint the reveal waits for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PaintToken(u64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Phase {
    /// Not on screen.
    Unmapped,
    /// Mapped, held, output not yet known this map.
    Waiting,
    /// Held, output known: waiting for a frame at its margins to be painted.
    Painting,
    /// Seen.
    Shown,
}

/// The reveal of one Overlay surface across its maps.
#[derive(Debug)]
pub struct RevealHold {
    map: u64,
    paint: u64,
    phase: Phase,
}

impl Default for RevealHold {
    fn default() -> Self {
        Self {
            map: 0,
            paint: 0,
            phase: Phase::Unmapped,
        }
    }
}

impl RevealHold {
    /// A fresh map starts held. Arm the deadline with the token.
    pub fn begin_map(&mut self) -> MapToken {
        self.supersede();
        self.phase = Phase::Waiting;
        MapToken(self.map)
    }

    /// Off screen: every armed callback is dead.
    pub fn unmap(&mut self) {
        self.supersede();
        self.phase = Phase::Unmapped;
    }

    /// The surface entered an output. While held, place the pill by it,
    /// paint a transparent frame and reveal once the returned paint is done.
    pub fn output_known(&mut self) -> Option<PaintToken> {
        match self.phase {
            Phase::Waiting | Phase::Painting => {
                self.phase = Phase::Painting;
                Some(self.next_paint())
            }
            Phase::Unmapped | Phase::Shown => None,
        }
    }

    /// The surface left the output it was placed by. While held, wait for
    /// the next one again.
    pub fn output_lost(&mut self) {
        if self.phase == Phase::Painting {
            self.paint += 1;
            self.phase = Phase::Waiting;
        }
    }

    /// The margins changed. A pending paint is of the old spot, so wait for
    /// the returned one instead.
    pub fn moved(&mut self) -> Option<PaintToken> {
        (self.phase == Phase::Painting).then(|| self.next_paint())
    }

    /// A frame was painted. `true` when it is the one the reveal waited for:
    /// show the pill now.
    pub fn painted(&mut self, token: PaintToken) -> bool {
        self.reveal_if(self.phase == Phase::Painting && token.0 == self.paint)
    }

    /// The deadline armed by `token` fired. `true` when that map is still
    /// held: show the pill now, at the best margins known.
    pub fn deadline(&mut self, token: MapToken) -> bool {
        self.reveal_if(self.held() && token.0 == self.map)
    }

    /// Mapped and not yet shown: the pill must stay transparent.
    #[must_use]
    pub fn held(&self) -> bool {
        matches!(self.phase, Phase::Waiting | Phase::Painting)
    }

    /// Whether the output the surface was last known on may place the pill.
    /// Not while a fresh map waits for its output, since that output may be
    /// from the last map.
    #[must_use]
    pub fn trusts_output(&self) -> bool {
        self.phase != Phase::Waiting
    }

    fn reveal_if(&mut self, now: bool) -> bool {
        if now {
            self.supersede();
            self.phase = Phase::Shown;
        }
        now
    }

    fn next_paint(&mut self) -> PaintToken {
        self.paint += 1;
        PaintToken(self.paint)
    }

    fn supersede(&mut self) {
        self.map += 1;
        self.paint += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_map_shows_after_its_output_is_known_and_a_frame_is_painted() {
        let mut hold = RevealHold::default();
        assert!(!hold.held());
        let map = hold.begin_map();
        assert!(hold.held());
        // The last map's output can't place it yet.
        assert!(!hold.trusts_output());
        let paint = hold.output_known().expect("held, so a paint is armed");
        assert!(hold.trusts_output());
        assert!(hold.held(), "still transparent until that frame is painted");
        assert!(hold.painted(paint));
        assert!(!hold.held());
        // The deadline that follows does nothing.
        assert!(!hold.deadline(map));
        assert!(!hold.painted(paint));
    }

    #[test]
    fn without_an_output_the_deadline_shows_it_and_a_late_enter_only_moves_it() {
        let mut hold = RevealHold::default();
        let map = hold.begin_map();
        assert!(hold.deadline(map));
        assert!(!hold.held());
        // Shown: the best output known now places it.
        assert!(hold.trusts_output());
        // A late enter re-places it but arms no further hold.
        assert_eq!(hold.output_known(), None);
        assert_eq!(hold.moved(), None);
        assert!(!hold.held());
    }

    #[test]
    fn leaving_the_output_or_moving_before_the_paint_waits_for_a_new_one() {
        let mut hold = RevealHold::default();
        hold.begin_map();
        let first = hold.output_known().unwrap();
        hold.output_lost();
        assert!(!hold.painted(first), "that frame was for the output left");
        assert!(hold.held());
        assert!(!hold.trusts_output());
        // A move while waiting arms nothing: no output to paint for yet.
        assert_eq!(hold.moved(), None);
        let second = hold.output_known().unwrap();
        let moved = hold.moved().unwrap();
        assert!(!hold.painted(second), "that frame was of the old margins");
        assert!(hold.painted(moved));
        assert!(!hold.held());
    }

    #[test]
    fn unmapping_or_mapping_again_kills_every_armed_callback() {
        let mut hold = RevealHold::default();
        let map = hold.begin_map();
        let paint = hold.output_known().unwrap();
        hold.unmap();
        assert!(!hold.painted(paint));
        assert!(!hold.deadline(map));
        assert!(!hold.held());
        assert_eq!(hold.output_known(), None, "an unmapped pill arms nothing");

        // A remap's callbacks belong to it alone.
        let old = hold.begin_map();
        let stale = hold.output_known().unwrap();
        let new = hold.begin_map();
        assert!(!hold.trusts_output());
        assert!(!hold.painted(stale));
        assert!(!hold.deadline(old));
        assert!(hold.held());
        assert!(hold.deadline(new));
    }
}
