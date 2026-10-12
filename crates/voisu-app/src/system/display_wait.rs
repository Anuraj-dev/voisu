// Startup wait for the session display environment under `voisu-daemon --systemd`.
//
// The packaged unit has no ConditionEnvironment: systemd evaluates conditions
// once and never retries a skip, and GNOME imports WAYLAND_DISPLAY/DISPLAY into
// the user manager on its own schedule, so ordering after
// graphical-session.target cannot prove the variables are there. The daemon
// waits for them itself instead.

use std::time::Duration;

use super::readiness::manager_env_has;
use super::run_restricted_stdout;

/// How long a systemd-launched daemon waits for a display variable to appear in
/// the user manager environment.
pub const DISPLAY_WAIT_TIMEOUT: Duration = Duration::from_secs(20);
/// Interval between user manager environment polls.
pub const DISPLAY_WAIT_POLL: Duration = Duration::from_millis(250);
/// EX_TEMPFAIL: the display environment arrived after start. `Restart=on-failure`
/// respawns the daemon, which then inherits the manager's current environment.
pub const DISPLAY_ARRIVED_EXIT: i32 = 75;
/// EX_CONFIG: the display environment never arrived. The unit's
/// `RestartPreventExitStatus=78` keeps this from becoming a restart loop.
pub const DISPLAY_TIMEOUT_EXIT: i32 = 78;

const DISPLAY_VARIABLES: [&str; 2] = ["WAYLAND_DISPLAY", "DISPLAY"];

#[derive(Debug, PartialEq, Eq)]
pub enum DisplayWaitOutcome {
    /// The daemon's own environment already has a display variable.
    Present,
    /// The user manager gained a display variable while waiting.
    Arrived,
    TimedOut,
}

/// Poll `manager_environment` (the `systemctl --user show-environment` text, or
/// `None` when it cannot be read) until a display variable appears or `timeout`
/// elapses. `now` is the time elapsed since the wait began, so a slow query
/// counts against the budget like a sleep does. The last sleep is capped to the
/// time remaining and no query starts after the deadline, so an unresponsive
/// user manager costs at most one query beyond it.
pub fn wait_for_display_environment(
    process_has_display: bool,
    timeout: Duration,
    poll: Duration,
    mut manager_environment: impl FnMut() -> Option<String>,
    mut sleep: impl FnMut(Duration),
    mut now: impl FnMut() -> Duration,
) -> DisplayWaitOutcome {
    if process_has_display {
        return DisplayWaitOutcome::Present;
    }
    loop {
        if manager_environment().is_some_and(|environment| {
            DISPLAY_VARIABLES
                .iter()
                .any(|key| manager_env_has(&environment, key))
        }) {
            return DisplayWaitOutcome::Arrived;
        }
        let Some(remaining) = timeout.checked_sub(now()).filter(|left| !left.is_zero()) else {
            return DisplayWaitOutcome::TimedOut;
        };
        sleep(poll.min(remaining));
        if now() >= timeout {
            return DisplayWaitOutcome::TimedOut;
        }
    }
}

fn process_has_display() -> bool {
    DISPLAY_VARIABLES
        .iter()
        .any(|key| std::env::var_os(key).is_some_and(|value| !value.is_empty()))
}

/// Wait bounds; the `VOISU_TEST_*` overrides (never set by the packaged unit)
/// let tests shrink the 20 s production wait to milliseconds.
fn display_wait_bounds() -> (Duration, Duration) {
    let millis = |name: &str| {
        std::env::var(name)
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .map(Duration::from_millis)
    };
    (
        millis("VOISU_TEST_DISPLAY_WAIT_MS").unwrap_or(DISPLAY_WAIT_TIMEOUT),
        millis("VOISU_TEST_DISPLAY_POLL_MS")
            .filter(|poll| !poll.is_zero())
            .unwrap_or(DISPLAY_WAIT_POLL),
    )
}

fn live_manager_environment() -> Option<String> {
    run_restricted_stdout("systemctl", &["--user", "show-environment"])
        .map(|stdout| String::from_utf8_lossy(&stdout).into_owned())
}

/// The startup gate for `voisu-daemon`. Returns the exit code the daemon must
/// terminate with, or `None` to continue starting. Only a systemd-owned start
/// waits; a manual daemon starts as before. Logs go to stderr (the journal).
pub fn startup_display_gate(systemd_owned: bool) -> Option<i32> {
    let (timeout, poll) = display_wait_bounds();
    let started = std::time::Instant::now();
    display_gate(
        systemd_owned,
        process_has_display(),
        (timeout, poll),
        live_manager_environment,
        std::thread::sleep,
        || started.elapsed(),
        |line| eprintln!("{line}"),
    )
}

fn display_gate(
    systemd_owned: bool,
    process_has_display: bool,
    (timeout, poll): (Duration, Duration),
    manager_environment: impl FnMut() -> Option<String>,
    sleep: impl FnMut(Duration),
    now: impl FnMut() -> Duration,
    mut log: impl FnMut(&str),
) -> Option<i32> {
    if !systemd_owned {
        return None;
    }
    match wait_for_display_environment(
        process_has_display,
        timeout,
        poll,
        manager_environment,
        sleep,
        now,
    ) {
        DisplayWaitOutcome::Present => None,
        DisplayWaitOutcome::Arrived => {
            log(
                "session display environment arrived after start; restarting to pick up WAYLAND_DISPLAY/DISPLAY from the user manager",
            );
            Some(DISPLAY_ARRIVED_EXIT)
        }
        DisplayWaitOutcome::TimedOut => {
            log(&format!(
                "no display environment after {}s: WAYLAND_DISPLAY and DISPLAY are both missing from the systemd user manager; start a graphical session, then run: voisu service restart",
                timeout.as_secs()
            ));
            Some(DISPLAY_TIMEOUT_EXIT)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    const NO_DISPLAY: &str = "LANG=C\nDISPLAY=\n";
    const WITH_DISPLAY: &str = "LANG=C\nWAYLAND_DISPLAY=wayland-0\nDISPLAY=:0\n";

    /// A fake clock shared by the injected `sleep`, `now` and manager query, so
    /// time spent in a query is visible to the deadline like a real one.
    #[derive(Default)]
    struct Clock {
        elapsed: Cell<Duration>,
        sleeps: RefCell<Vec<Duration>>,
    }

    impl Clock {
        fn advance(&self, by: Duration) {
            self.elapsed.set(self.elapsed.get() + by);
        }
    }

    #[test]
    fn a_display_already_in_the_process_environment_never_waits() {
        let outcome = wait_for_display_environment(
            true,
            Duration::from_secs(20),
            Duration::from_millis(250),
            || panic!("the manager must not be queried"),
            |_| panic!("the daemon must not sleep"),
            || panic!("the clock must not be read"),
        );
        assert_eq!(outcome, DisplayWaitOutcome::Present);
    }

    #[test]
    fn a_display_arriving_in_the_manager_ends_the_wait() {
        let clock = Clock::default();
        let polls = Cell::new(0);
        let outcome = wait_for_display_environment(
            false,
            Duration::from_secs(20),
            Duration::from_millis(250),
            || {
                polls.set(polls.get() + 1);
                Some(
                    if polls.get() < 3 {
                        NO_DISPLAY
                    } else {
                        WITH_DISPLAY
                    }
                    .to_owned(),
                )
            },
            |interval| clock.advance(interval),
            || clock.elapsed.get(),
        );
        assert_eq!(outcome, DisplayWaitOutcome::Arrived);
        assert_eq!(polls.get(), 3);
        assert_eq!(clock.elapsed.get(), Duration::from_millis(500));
    }

    #[test]
    fn an_unreadable_manager_keeps_waiting_until_the_timeout() {
        let clock = Clock::default();
        let polls = Cell::new(0);
        let outcome = wait_for_display_environment(
            false,
            Duration::from_secs(1),
            Duration::from_millis(250),
            || {
                polls.set(polls.get() + 1);
                None
            },
            |interval| clock.advance(interval),
            || clock.elapsed.get(),
        );
        assert_eq!(outcome, DisplayWaitOutcome::TimedOut);
        // One poll at t=0 plus one after each of the first three intervals; the
        // wait ends at the deadline without a further query.
        assert_eq!(polls.get(), 4);
        assert_eq!(clock.elapsed.get(), Duration::from_secs(1));
    }

    #[test]
    fn slow_manager_queries_count_against_the_deadline() {
        // Every query "takes" 2 s (the production PROCESS_DEADLINE order of
        // magnitude). The wait must end at the 20 s budget, not after 20 s of
        // sleeps plus the time spent in queries.
        let clock = Clock::default();
        let polls = Cell::new(0);
        let outcome = wait_for_display_environment(
            false,
            Duration::from_secs(20),
            Duration::from_millis(250),
            || {
                polls.set(polls.get() + 1);
                clock.advance(Duration::from_secs(2));
                None
            },
            |interval| clock.advance(interval),
            || clock.elapsed.get(),
        );
        assert_eq!(outcome, DisplayWaitOutcome::TimedOut);
        assert_eq!(clock.elapsed.get(), Duration::from_secs(20));
        // Each cycle is a 2 s query plus a 250 ms sleep; 8 full cycles reach 18 s
        // and the ninth query lands exactly on the budget.
        assert_eq!(polls.get(), 9);
    }

    #[test]
    fn the_final_sleep_is_capped_to_the_time_remaining() {
        let clock = Clock::default();
        let outcome = wait_for_display_environment(
            false,
            Duration::from_secs(1),
            Duration::from_millis(400),
            || None,
            |interval| {
                clock.sleeps.borrow_mut().push(interval);
                clock.advance(interval);
            },
            || clock.elapsed.get(),
        );
        assert_eq!(outcome, DisplayWaitOutcome::TimedOut);
        assert_eq!(
            *clock.sleeps.borrow(),
            [
                Duration::from_millis(400),
                Duration::from_millis(400),
                Duration::from_millis(200)
            ]
        );
        assert_eq!(clock.elapsed.get(), Duration::from_secs(1));
    }

    fn gate(
        systemd_owned: bool,
        process_has_display: bool,
        environment: Option<&'static str>,
        log: &RefCell<Vec<String>>,
    ) -> Option<i32> {
        let clock = Clock::default();
        display_gate(
            systemd_owned,
            process_has_display,
            (Duration::from_secs(1), Duration::from_millis(250)),
            || environment.map(str::to_owned),
            |interval| clock.advance(interval),
            || clock.elapsed.get(),
            |line| log.borrow_mut().push(line.to_owned()),
        )
    }

    #[test]
    fn a_manual_daemon_starts_without_consulting_the_manager() {
        let log = RefCell::new(Vec::new());
        let exit = display_gate(
            false,
            false,
            (Duration::from_secs(1), Duration::from_millis(250)),
            || panic!("a manual daemon must not query the manager"),
            |_| panic!("a manual daemon must not sleep"),
            || panic!("a manual daemon must not read the clock"),
            |line| log.borrow_mut().push(line.to_owned()),
        );
        assert_eq!(exit, None);
        assert!(log.borrow().is_empty());
    }

    #[test]
    fn a_systemd_daemon_with_a_display_starts_normally() {
        let log = RefCell::new(Vec::new());
        assert_eq!(gate(true, true, None, &log), None);
        assert!(log.borrow().is_empty());
    }

    #[test]
    fn late_arrival_exits_75_and_says_it_is_restarting() {
        let log = RefCell::new(Vec::new());
        assert_eq!(
            gate(true, false, Some(WITH_DISPLAY), &log),
            Some(DISPLAY_ARRIVED_EXIT)
        );
        assert_eq!(DISPLAY_ARRIVED_EXIT, 75);
        assert!(log.borrow()[0].contains("arrived after start; restarting"));
    }

    #[test]
    fn timeout_exits_78_naming_the_variables_and_the_recovery() {
        let log = RefCell::new(Vec::new());
        assert_eq!(
            gate(true, false, Some(NO_DISPLAY), &log),
            Some(DISPLAY_TIMEOUT_EXIT)
        );
        assert_eq!(DISPLAY_TIMEOUT_EXIT, 78);
        let line = log.borrow()[0].clone();
        assert!(line.contains("WAYLAND_DISPLAY") && line.contains("DISPLAY"));
        assert!(line.contains("voisu service restart"));
    }
}
