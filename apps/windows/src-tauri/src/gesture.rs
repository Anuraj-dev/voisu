//! The gesture half of the Trigger Key: turns chord signals into Recording
//! commands. Pure and clock-injected so every timing rule is tested on Linux.
//!
//! - Hold the chord: a Recording runs until release. A hold shorter than
//!   [`MIN_HOLD`] is a tap, not a Recording.
//! - Tap, then press again within [`DOUBLE_TAP`]: hands-free. The Recording
//!   keeps running after release; the next chord press stops it.
//! - Esc, or another key pressed during the chord, cancels with no Transcript.
//!
//! A Recording starts on the first press so no speech is clipped; a single tap
//! is cancelled once the double-tap window lapses.

use std::time::{Duration, Instant};

use crate::chord::ChordSignal;

/// Shorter holds are taps.
pub const MIN_HOLD: Duration = Duration::from_millis(300);
/// How long after a tap's release a second press still counts as a double tap.
pub const DOUBLE_TAP: Duration = Duration::from_millis(400);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Action {
    Start(u64),
    /// The Recording continues hands-free.
    HandsFree(u64),
    Stop(u64),
    Cancel(u64),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum State {
    Idle,
    Holding { id: u64, since: Instant },
    TapPending { id: u64, until: Instant },
    HandsFree { id: u64 },
}

#[derive(Debug)]
pub struct GestureMachine {
    state: State,
    next_id: u64,
}

impl Default for GestureMachine {
    fn default() -> Self {
        Self {
            state: State::Idle,
            next_id: 1,
        }
    }
}

impl GestureMachine {
    /// True while a Recording or a pending tap is in flight: Esc then means
    /// Cancel and is swallowed by the hook.
    pub fn armed(&self) -> bool {
        self.state != State::Idle
    }

    /// When [`Self::on_timeout`] must next run, if ever.
    pub fn next_deadline(&self) -> Option<Instant> {
        match self.state {
            State::TapPending { until, .. } => Some(until),
            _ => None,
        }
    }

    /// Applies one chord signal. Returns up to two actions: a lapsed tap
    /// window is settled first, so a late second press cancels the tap and
    /// starts a fresh Recording instead of entering hands-free.
    pub fn on_signal(&mut self, signal: ChordSignal, now: Instant) -> Vec<Action> {
        let mut actions = Vec::with_capacity(2);
        actions.extend(self.on_timeout(now));
        actions.extend(self.step(signal, now));
        actions
    }

    fn step(&mut self, signal: ChordSignal, now: Instant) -> Option<Action> {
        match (self.state, signal) {
            (State::Idle, ChordSignal::Pressed) => {
                let id = self.next_id;
                self.next_id += 1;
                self.state = State::Holding { id, since: now };
                Some(Action::Start(id))
            }
            (State::Holding { id, since }, ChordSignal::Released) => {
                if now.saturating_duration_since(since) >= MIN_HOLD {
                    self.state = State::Idle;
                    Some(Action::Stop(id))
                } else {
                    self.state = State::TapPending {
                        id,
                        until: now + DOUBLE_TAP,
                    };
                    None
                }
            }
            (State::TapPending { id, .. }, ChordSignal::Pressed) => {
                self.state = State::HandsFree { id };
                Some(Action::HandsFree(id))
            }
            (State::HandsFree { id }, ChordSignal::Pressed) => {
                self.state = State::Idle;
                Some(Action::Stop(id))
            }
            (
                State::Holding { id, .. } | State::TapPending { id, .. } | State::HandsFree { id },
                ChordSignal::Escape | ChordSignal::Interrupted,
            ) => {
                self.state = State::Idle;
                Some(Action::Cancel(id))
            }
            _ => None,
        }
    }

    /// Settles a lapsed double-tap window: the tap was not a Recording.
    pub fn on_timeout(&mut self, now: Instant) -> Option<Action> {
        match self.state {
            State::TapPending { id, until } if now >= until => {
                self.state = State::Idle;
                Some(Action::Cancel(id))
            }
            _ => None,
        }
    }

    /// The engine ended Recording `id` on its own (an error, the Recording
    /// Deadline, or a refused start). The gesture forgets it so the next press
    /// starts fresh.
    pub fn on_engine_ended(&mut self, ended: u64) {
        let current = match self.state {
            State::Holding { id, .. } | State::TapPending { id, .. } | State::HandsFree { id } => {
                id
            }
            State::Idle => return,
        };
        if current == ended {
            self.state = State::Idle;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ChordSignal::*;

    struct Clock(Instant);

    impl Clock {
        fn at(&self, ms: u64) -> Instant {
            self.0 + Duration::from_millis(ms)
        }
    }

    fn setup() -> (GestureMachine, Clock) {
        (GestureMachine::default(), Clock(Instant::now()))
    }

    #[test]
    fn hold_records_until_release() {
        let (mut g, c) = setup();
        assert_eq!(g.on_signal(Pressed, c.at(0)), [Action::Start(1)]);
        assert!(g.armed());
        assert_eq!(g.on_signal(Released, c.at(1_500)), [Action::Stop(1)]);
        assert!(!g.armed());
    }

    #[test]
    fn hold_of_exactly_min_hold_is_a_recording() {
        let (mut g, c) = setup();
        g.on_signal(Pressed, c.at(0));
        assert_eq!(
            g.on_signal(Released, c.at(MIN_HOLD.as_millis() as u64)),
            [Action::Stop(1)]
        );
    }

    #[test]
    fn single_tap_is_cancelled_when_the_window_lapses() {
        let (mut g, c) = setup();
        g.on_signal(Pressed, c.at(0));
        assert_eq!(g.on_signal(Released, c.at(100)), []);
        assert!(g.armed());
        assert_eq!(g.next_deadline(), Some(c.at(500)));
        assert_eq!(g.on_timeout(c.at(499)), None);
        assert_eq!(g.on_timeout(c.at(500)), Some(Action::Cancel(1)));
        assert!(!g.armed());
        assert_eq!(g.next_deadline(), None);
    }

    #[test]
    fn double_tap_enters_hands_free_and_next_press_stops() {
        let (mut g, c) = setup();
        g.on_signal(Pressed, c.at(0));
        g.on_signal(Released, c.at(100));
        assert_eq!(g.on_signal(Pressed, c.at(300)), [Action::HandsFree(1)]);
        // Releasing the second tap keeps recording.
        assert_eq!(g.on_signal(Released, c.at(400)), []);
        assert_eq!(g.next_deadline(), None);
        assert!(g.armed());
        assert_eq!(g.on_signal(Pressed, c.at(9_000)), [Action::Stop(1)]);
        // The stopping press's release does nothing.
        assert_eq!(g.on_signal(Released, c.at(9_100)), []);
        assert!(!g.armed());
    }

    #[test]
    fn late_second_press_cancels_the_tap_and_starts_fresh() {
        let (mut g, c) = setup();
        g.on_signal(Pressed, c.at(0));
        g.on_signal(Released, c.at(100));
        assert_eq!(
            g.on_signal(Pressed, c.at(600)),
            [Action::Cancel(1), Action::Start(2)]
        );
    }

    #[test]
    fn escape_cancels_in_every_active_state() {
        let (mut g, c) = setup();
        g.on_signal(Pressed, c.at(0));
        assert_eq!(g.on_signal(Escape, c.at(50)), [Action::Cancel(1)]);

        g.on_signal(Pressed, c.at(1_000));
        g.on_signal(Released, c.at(1_100));
        assert_eq!(g.on_signal(Escape, c.at(1_200)), [Action::Cancel(2)]);

        g.on_signal(Pressed, c.at(2_000));
        g.on_signal(Released, c.at(2_100));
        g.on_signal(Pressed, c.at(2_200));
        assert_eq!(g.on_signal(Escape, c.at(5_000)), [Action::Cancel(3)]);
        assert!(!g.armed());
    }

    #[test]
    fn escape_when_idle_does_nothing() {
        let (mut g, c) = setup();
        assert_eq!(g.on_signal(Escape, c.at(0)), []);
    }

    #[test]
    fn interrupted_chord_cancels_and_its_release_is_ignored() {
        let (mut g, c) = setup();
        g.on_signal(Pressed, c.at(0));
        assert_eq!(g.on_signal(Interrupted, c.at(800)), [Action::Cancel(1)]);
        assert_eq!(g.on_signal(Released, c.at(900)), []);
    }

    #[test]
    fn engine_end_resets_only_the_matching_recording() {
        let (mut g, c) = setup();
        g.on_signal(Pressed, c.at(0));
        g.on_engine_ended(7);
        assert!(g.armed());
        g.on_engine_ended(1);
        assert!(!g.armed());
        // The release of the press the engine refused is ignored.
        assert_eq!(g.on_signal(Released, c.at(2_000)), []);
        assert_eq!(g.on_signal(Pressed, c.at(3_000)), [Action::Start(2)]);
    }
}
