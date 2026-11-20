//! When a freshly mapped Overlay pill may be seen, and whether it must be
//! mapped again first. Pure state, so the order of compositor events is
//! tested without one.
//!
//! Where the compositor says which output is focused (Hyprland), the Overlay
//! binds the surface there at its final margins before mapping it, and the
//! pill shows at once ([`RevealHold::show_bound`]). Elsewhere the surface is
//! mapped unbound and learns its output only after its first buffer (the
//! compositor's `wl_surface.enter`), so that map is a held probe: the pill
//! draws transparent frames at margins safe on every output until the output
//! is known. If that output wants the margins already applied, the pill shows
//! after one more painted frame. Otherwise the probe is unmapped, bound to
//! that output at its margins and mapped again, still held, and shows after
//! that map's first painted frame; a compositor that animates a mapped
//! layer's margins (Hyprland) never sees them change. If the output never
//! says, the pill shows anyway at [`REVEAL_DEADLINE`], unbound, where it is,
//! and an enter that comes later re-places it.
//!
//! The binding lasts one shown pill: a hide, or the next fresh map, drops it.
//!
//! Every callback the Overlay arms carries a token. Unmapping, a new map, or
//! a later change (a new output, a move) supersedes it, so a stale timer,
//! paint or remap never acts on the wrong map.

use std::time::Duration;

/// The longest a fresh probe stays transparent while it waits for its output.
pub const REVEAL_DEADLINE: Duration = Duration::from_millis(150);

/// Names one map, for its [`REVEAL_DEADLINE`] timer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MapToken(u64);

/// Names one transparent paint the reveal waits for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PaintToken(u64);

/// Names one remap the probe asked for, run later off the enter signal.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RemapToken(u64);

/// What the Overlay does when the surface enters an output.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Entered {
    /// The applied margins are right for it: paint one more transparent
    /// frame and show the pill once [`RevealHold::painted`] says so.
    Paint(PaintToken),
    /// That output wants other margins: unmap, bind to it at its margins and
    /// map again. Start with [`RevealHold::rebind`].
    Remap(RemapToken),
    /// An unbound pill already shown by the deadline: place it by this
    /// output where it is.
    Place,
    /// A bound or unmapped surface: the enter changes nothing.
    Ignore,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Phase {
    /// Not on screen.
    Unmapped,
    /// An unbound probe, held, output not yet known this map.
    Probing,
    /// Held, at the right margins: waiting for a frame there to be painted.
    Painting,
    /// A held probe that must be mapped again bound to its output.
    Rebinding,
    /// The Overlay's own unmap and map for a rebind are under way.
    Remapping,
    /// Seen.
    Shown,
}

/// The reveal of one Overlay surface across its maps.
#[derive(Debug)]
pub struct RevealHold {
    map: u64,
    paint: u64,
    phase: Phase,
    bound: bool,
}

impl Default for RevealHold {
    fn default() -> Self {
        Self {
            map: 0,
            paint: 0,
            phase: Phase::Unmapped,
            bound: false,
        }
    }
}

impl RevealHold {
    /// A fresh map bound to the focused output before it is mapped, at its
    /// final margins: nothing to wait for, so it is seen at once.
    pub fn show_bound(&mut self) {
        self.supersede();
        self.phase = Phase::Shown;
        self.bound = true;
    }

    /// A fresh unbound map starts as a held probe. Arm the deadline with the
    /// token.
    pub fn begin_map(&mut self) -> MapToken {
        self.supersede();
        self.phase = Phase::Probing;
        self.bound = false;
        MapToken(self.map)
    }

    /// The surface was unmapped: every armed callback is dead. Returns
    /// `true` when this is the Overlay's own unmap for a rebind, so the pill
    /// stays held for the map that follows; otherwise the pill is off screen
    /// and its binding is dropped.
    pub fn unmap(&mut self) -> bool {
        self.supersede();
        if self.phase == Phase::Remapping {
            return true;
        }
        self.phase = Phase::Unmapped;
        self.bound = false;
        false
    }

    /// The surface entered an output. `moves` is whether that output wants
    /// margins other than those applied. A bound surface ignores it, so a
    /// late enter from a probe never touches its rebound map.
    pub fn output_known(&mut self, moves: bool) -> Entered {
        match self.phase {
            _ if self.bound => Entered::Ignore,
            Phase::Probing | Phase::Painting | Phase::Rebinding => {
                if moves {
                    self.phase = Phase::Rebinding;
                    Entered::Remap(RemapToken(self.next_paint().0))
                } else {
                    self.phase = Phase::Painting;
                    Entered::Paint(self.next_paint())
                }
            }
            Phase::Shown => Entered::Place,
            Phase::Unmapped | Phase::Remapping => Entered::Ignore,
        }
    }

    /// The remap armed by `token` is due. `true` when it still stands: unmap
    /// the probe now, bind it, apply its margins and call
    /// [`RevealHold::remapped`] before mapping it again.
    pub fn rebind(&mut self, token: RemapToken) -> bool {
        let due = self.phase == Phase::Rebinding && token.0 == self.paint;
        if due {
            self.supersede();
            self.phase = Phase::Remapping;
        }
        due
    }

    /// The rebound surface is about to be mapped, still held. Arm the
    /// deadline with the map token and reveal on the paint token. `None`
    /// unless a rebind is under way.
    pub fn remapped(&mut self) -> Option<(MapToken, PaintToken)> {
        (self.phase == Phase::Remapping).then(|| {
            self.supersede();
            self.phase = Phase::Painting;
            self.bound = true;
            (MapToken(self.map), PaintToken(self.paint))
        })
    }

    /// The margins changed while at the right spot: a pending paint is of
    /// the old one, so wait for the returned one instead.
    pub fn moved(&mut self) -> Option<PaintToken> {
        (self.phase == Phase::Painting).then(|| self.next_paint())
    }

    /// A frame was painted. `true` when it is the one the reveal waited for:
    /// show the pill now.
    pub fn painted(&mut self, token: PaintToken) -> bool {
        self.reveal_if(self.phase == Phase::Painting && token.0 == self.paint)
    }

    /// The deadline armed by `token` fired. `true` when that map is still
    /// held: show the pill now, where it is. A probe waiting on its remap
    /// shows unbound.
    pub fn deadline(&mut self, token: MapToken) -> bool {
        self.reveal_if(self.held() && token.0 == self.map)
    }

    /// Mapped (or between the two maps of a rebind) and not yet shown: the
    /// pill must stay transparent.
    #[must_use]
    pub fn held(&self) -> bool {
        matches!(
            self.phase,
            Phase::Probing | Phase::Painting | Phase::Rebinding | Phase::Remapping
        )
    }

    /// Whether the surface is bound to an output.
    #[must_use]
    pub fn bound(&self) -> bool {
        self.bound
    }

    /// Whether the margins must be left as they are: a probe waiting on its
    /// remap would otherwise move while mapped. The remap applies the right
    /// ones.
    #[must_use]
    pub fn margins_frozen(&self) -> bool {
        matches!(self.phase, Phase::Rebinding | Phase::Remapping)
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

    fn paint_of(entered: Entered) -> PaintToken {
        match entered {
            Entered::Paint(paint) => paint,
            other => panic!("expected a paint, got {other:?}"),
        }
    }

    fn remap_of(entered: Entered) -> RemapToken {
        match entered {
            Entered::Remap(remap) => remap,
            other => panic!("expected a remap, got {other:?}"),
        }
    }

    #[test]
    fn a_map_bound_before_it_is_mapped_shows_at_once_and_ignores_enters() {
        let mut hold = RevealHold::default();
        hold.show_bound();
        assert!(!hold.held());
        assert!(hold.bound());
        assert_eq!(hold.output_known(true), Entered::Ignore);
        assert_eq!(hold.moved(), None);
        assert!(!hold.unmap());
        assert!(!hold.bound());
    }

    #[test]
    fn an_enter_at_the_applied_margins_shows_after_a_paint_without_a_remap() {
        let mut hold = RevealHold::default();
        assert!(!hold.held());
        let map = hold.begin_map();
        assert!(hold.held());
        let paint = paint_of(hold.output_known(false));
        assert!(!hold.margins_frozen());
        assert!(hold.held(), "still transparent until that frame is painted");
        assert!(hold.painted(paint));
        assert!(!hold.held());
        assert!(!hold.bound(), "no remap, so never bound");
        // The deadline that follows does nothing.
        assert!(!hold.deadline(map));
        assert!(!hold.painted(paint));
    }

    #[test]
    fn an_enter_wanting_other_margins_remaps_bound_and_shows_after_that_maps_paint() {
        let mut hold = RevealHold::default();
        let probe = hold.begin_map();
        let remap = remap_of(hold.output_known(true));
        assert!(hold.held());
        assert!(hold.margins_frozen(), "the mapped probe must not move");
        assert!(hold.rebind(remap));
        assert!(!hold.rebind(remap), "a remap runs once");
        // The Overlay's own unmap keeps the pill held.
        assert!(hold.unmap());
        assert!(hold.held());
        let (map, paint) = hold.remapped().expect("a rebind is under way");
        assert!(hold.bound());
        assert!(hold.held());
        assert!(!hold.margins_frozen());
        // The probe's deadline died with it.
        assert!(!hold.deadline(probe));
        assert!(hold.painted(paint));
        assert!(!hold.held());
        assert!(hold.bound(), "bound for the life of the shown pill");
        assert!(!hold.deadline(map));
    }

    #[test]
    fn a_hide_or_a_fresh_map_drops_the_binding() {
        let mut hold = RevealHold::default();
        hold.begin_map();
        let remap = remap_of(hold.output_known(true));
        assert!(hold.rebind(remap));
        assert!(hold.unmap());
        let (_, paint) = hold.remapped().unwrap();
        assert!(hold.painted(paint));
        assert!(hold.bound());
        // Hidden.
        assert!(!hold.unmap(), "not a rebind: the pill is off screen");
        assert!(!hold.bound());
        assert!(!hold.held());

        // A fresh map over a bound pill still up starts unbound too.
        hold.show_bound();
        hold.begin_map();
        assert!(!hold.bound());
        let remap = remap_of(hold.output_known(true));
        assert!(hold.rebind(remap));
        hold.unmap();
        hold.remapped().unwrap();
        assert!(hold.bound());
        hold.begin_map();
        assert!(!hold.bound());
        assert_eq!(hold.remapped(), None, "the rebind was superseded");
    }

    #[test]
    fn the_deadline_shows_the_probe_where_it_is_and_a_late_enter_places_it() {
        let mut hold = RevealHold::default();
        let map = hold.begin_map();
        let remap = remap_of(hold.output_known(true));
        assert!(hold.deadline(map));
        assert!(!hold.held());
        assert!(!hold.bound());
        assert!(!hold.margins_frozen());
        assert!(!hold.rebind(remap), "the remap died with the deadline");
        assert_eq!(hold.remapped(), None);

        // With no enter at all, the deadline shows it the same way, and an
        // enter that comes after only places it, with no hold.
        let map = hold.begin_map();
        assert!(hold.deadline(map));
        assert_eq!(hold.output_known(true), Entered::Place);
        assert_eq!(hold.moved(), None);
        assert!(!hold.held());
    }

    #[test]
    fn a_stale_enter_from_the_probe_is_ignored_after_the_remap() {
        let mut hold = RevealHold::default();
        hold.begin_map();
        let early = paint_of(hold.output_known(false));
        let remap = remap_of(hold.output_known(true));
        assert!(!hold.painted(early), "that frame was at the old margins");
        assert!(hold.rebind(remap));
        hold.unmap();
        // An enter the probe sends late, once unmapped.
        assert_eq!(hold.output_known(true), Entered::Ignore);
        let (_, paint) = hold.remapped().unwrap();
        // And once the bound map is up.
        assert_eq!(hold.output_known(true), Entered::Ignore);
        assert!(hold.bound());
        assert!(hold.held());
        assert!(hold.painted(paint));
        assert_eq!(hold.output_known(true), Entered::Ignore);
    }

    #[test]
    fn a_move_before_the_paint_waits_for_a_new_one() {
        let mut hold = RevealHold::default();
        hold.begin_map();
        // A move while probing arms nothing: no output to paint for yet.
        assert_eq!(hold.moved(), None);
        let first = paint_of(hold.output_known(false));
        let moved = hold.moved().unwrap();
        assert!(!hold.painted(first), "that frame was of the old margins");
        assert!(hold.painted(moved));
        assert!(!hold.held());
    }

    #[test]
    fn unmapping_or_mapping_again_kills_every_armed_callback() {
        let mut hold = RevealHold::default();
        let map = hold.begin_map();
        let paint = paint_of(hold.output_known(false));
        assert!(!hold.unmap());
        assert!(!hold.painted(paint));
        assert!(!hold.deadline(map));
        assert!(!hold.held());
        assert_eq!(hold.output_known(false), Entered::Ignore);

        // A hide while a remap is pending kills the remap.
        hold.begin_map();
        let remap = remap_of(hold.output_known(true));
        assert!(!hold.unmap());
        assert!(!hold.rebind(remap));

        // A remap's callbacks belong to it alone.
        let old = hold.begin_map();
        let stale = paint_of(hold.output_known(false));
        let new = hold.begin_map();
        assert!(!hold.painted(stale));
        assert!(!hold.deadline(old));
        assert!(hold.held());
        assert!(hold.deadline(new));
    }
}
