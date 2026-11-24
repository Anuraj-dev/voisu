//! Whether the Trigger Key's keyboard hook still receives keys. Pure and
//! clock-injected (Windows tick counts in, actions out), so every rule is
//! tested on Linux.
//!
//! Windows silently removes a low-level keyboard hook that answers too
//! slowly, so a dead hook shows only as silence. Silence alone proves
//! nothing: `GetLastInputInfo` counts mouse, touchpad and pen input too, so a
//! user who only moves the mouse looks exactly like a dead hook. When input
//! arrived well after the last callback, the hook thread therefore injects a
//! tagged probe key and reinstalls only if the callback never sees it.

/// Input newer than the last callback by this much makes the hook suspect.
pub const SILENT_HOOK_MS: u32 = 30_000;
/// How long a probe key has to reach the callback.
pub const PROBE_WAIT_MS: u32 = 1_000;
/// `dwExtraInfo` of the probe key, so the callback can swallow it.
pub const PROBE_TAG: usize = 0x564F_5052;

/// What the hook thread does after a check.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HealthAction {
    /// Nothing: the hook answered, or a probe is still in flight.
    None,
    /// Inject a probe key and check again after [`PROBE_WAIT_MS`].
    SendProbe,
    /// The probe never reached the callback: the hook is gone.
    Reinstall,
}

/// The hook's health check, fed Windows tick counts (milliseconds that wrap).
#[derive(Debug, Default)]
pub struct HookHealth {
    probe_sent_at: Option<u32>,
}

impl HookHealth {
    /// `now`, `last_input` (from `GetLastInputInfo`) and `last_callback`
    /// (the tick of the hook callback's most recent run) are all
    /// `GetTickCount` values.
    pub fn check(&mut self, now: u32, last_input: u32, last_callback: u32) -> HealthAction {
        if let Some(sent) = self.probe_sent_at {
            if at_or_after(last_callback, sent) {
                self.probe_sent_at = None;
                return HealthAction::None;
            }
            if now.wrapping_sub(sent) >= PROBE_WAIT_MS {
                self.probe_sent_at = None;
                return HealthAction::Reinstall;
            }
            return HealthAction::None;
        }
        let input_after = last_input.wrapping_sub(last_callback);
        if at_or_after(last_input, last_callback) && input_after > SILENT_HOOK_MS {
            self.probe_sent_at = Some(now);
            return HealthAction::SendProbe;
        }
        HealthAction::None
    }

    /// Forgets a probe in flight, after a reinstall for another reason.
    pub fn reset(&mut self) {
        self.probe_sent_at = None;
    }
}

/// Wrapping tick arithmetic: `a` is at or after `b` within half the range.
fn at_or_after(a: u32, b: u32) -> bool {
    a.wrapping_sub(b) < u32::MAX / 2
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: u32 = 1_000_000;

    #[test]
    fn recent_key_input_needs_no_probe() {
        let mut health = HookHealth::default();
        assert_eq!(health.check(KEY + 5_000, KEY, KEY), HealthAction::None);
        assert_eq!(
            health.check(KEY + 40_000, KEY + SILENT_HOOK_MS, KEY),
            HealthAction::None,
            "input exactly at the limit is not yet suspect"
        );
    }

    #[test]
    fn idle_user_needs_no_probe() {
        let mut health = HookHealth::default();
        // No input at all since the last key, hours later.
        assert_eq!(health.check(KEY + 3_600_000, KEY, KEY), HealthAction::None);
    }

    #[test]
    fn mouse_only_use_is_not_a_dead_hook() {
        let mut health = HookHealth::default();
        let mouse = KEY + 45_000;
        let now = mouse + 2_000;
        assert_eq!(health.check(now, mouse, KEY), HealthAction::SendProbe);
        // The callback saw the probe: the input was the mouse, not lost keys.
        let probe_seen = now + 10;
        assert_eq!(
            health.check(now + PROBE_WAIT_MS, probe_seen, probe_seen),
            HealthAction::None
        );
        // And the next timer tick starts clean.
        assert_eq!(
            health.check(now + 5_000, probe_seen, probe_seen),
            HealthAction::None
        );
    }

    #[test]
    fn unanswered_probe_reinstalls() {
        let mut health = HookHealth::default();
        let input = KEY + 45_000;
        let now = input + 2_000;
        assert_eq!(health.check(now, input, KEY), HealthAction::SendProbe);
        assert_eq!(
            health.check(now + PROBE_WAIT_MS - 1, now, KEY),
            HealthAction::None,
            "still waiting for the probe"
        );
        assert_eq!(
            health.check(now + PROBE_WAIT_MS, now, KEY),
            HealthAction::Reinstall
        );
        // A reinstall stamps a fresh callback tick, so nothing is suspect.
        let reinstalled = now + PROBE_WAIT_MS;
        assert_eq!(
            health.check(reinstalled + 5_000, now, reinstalled),
            HealthAction::None
        );
    }

    #[test]
    fn reset_forgets_a_probe_in_flight() {
        let mut health = HookHealth::default();
        let input = KEY + 45_000;
        assert_eq!(health.check(input, input, KEY), HealthAction::SendProbe);
        health.reset();
        let fresh = input + 100;
        assert_eq!(
            health.check(input + PROBE_WAIT_MS * 3, input, fresh),
            HealthAction::None
        );
    }

    #[test]
    fn tick_wraparound_is_handled() {
        let mut health = HookHealth::default();
        let key = u32::MAX - 10_000;
        let mouse = key.wrapping_add(45_000);
        let now = mouse.wrapping_add(1_000);
        assert_eq!(health.check(now, mouse, key), HealthAction::SendProbe);
        let seen = now.wrapping_add(5);
        assert_eq!(
            health.check(now.wrapping_add(PROBE_WAIT_MS), seen, seen),
            HealthAction::None
        );
    }
}
