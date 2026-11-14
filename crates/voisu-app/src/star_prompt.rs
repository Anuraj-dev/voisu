//! The GitHub star nudge.
//!
//! The daemon counts successful dictations (a Recording whose Transcript was
//! delivered: pasted, or left on the clipboard) and the distinct local dates
//! they happened on, in `$XDG_STATE_HOME/voisu/star-prompt.json`. Nothing here
//! is ever sent anywhere. Counting starts at the upgrade, with no backfill,
//! and stops once the threshold is met.
//!
//! Once 20 dictations on 3 or more dates are counted, with `star_prompt = on`,
//! the daemon sends one desktop notification after that Delivery has finished,
//! and the TUI Home screen shows a line. The notification is sent once, ever:
//! closing or ignoring it changes nothing. Closing the Home line hides it for
//! [`RETURN_AFTER_DAYS`] days; it then comes back once, with no second
//! notification, and a second close hides it for good. `Star`, from anywhere,
//! opens the repository page and ends the nudge for good.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// The page `Star` opens.
pub const REPO_URL: &str = "https://github.com/Anuraj-dev/voisu";
/// Successful dictations before the prompt appears.
pub const DICTATIONS: u32 = 20;
/// Distinct local calendar dates those dictations must span.
pub const DATES: usize = 3;

pub const TITLE: &str = "Enjoying Voisu?";
pub const BODY: &str = "A GitHub star helps other people find it.";
/// The body for a notification server without actions.
pub const PLAIN_BODY: &str =
    "A GitHub star helps other people find it. Run voisu star to open the page.";
/// The TUI Home line.
pub const HOME_LINE: &str = "Enjoying Voisu?";
/// The permanent entry, shown whatever the nudge and the setting say.
pub const ENTRY: &str = "Voisu is free and built by one student. A star helps others find it.";
/// Days the nudge stays hidden after its first close.
pub const RETURN_AFTER_DAYS: i64 = 30;
/// Closes after which the nudge never shows again.
const CLOSES: u32 = 2;

/// What the state file holds.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct State {
    /// Successful dictations, up to [`DICTATIONS`].
    #[serde(default)]
    pub count: u32,
    /// Distinct local dates (`YYYY-MM-DD`) with one, up to [`DATES`].
    #[serde(default)]
    pub dates: Vec<String>,
    /// The notification went out. It never goes out again.
    #[serde(default)]
    pub notified: bool,
    /// The user starred. The nudge never shows again.
    #[serde(default)]
    pub answered: bool,
    /// Times the Home line was closed.
    #[serde(default)]
    pub closes: u32,
    /// The local date (`YYYY-MM-DD`) the nudge may show again after a close.
    #[serde(default)]
    pub returns_on: Option<String>,
}

impl State {
    pub fn threshold_met(&self) -> bool {
        self.count >= DICTATIONS && self.dates.len() >= DATES
    }

    /// Counts one successful dictation on `date`. Once the threshold is met
    /// nothing changes, and the counters never grow past it.
    pub fn record(&mut self, date: &str) {
        if self.threshold_met() {
            return;
        }
        self.count = (self.count + 1).min(DICTATIONS);
        if self.dates.len() < DATES && !self.dates.iter().any(|seen| seen == date) {
            self.dates.push(date.to_owned());
        }
    }

    /// Whether the nudge shows on `today`: threshold met, setting on, not
    /// starred, not closed for good, and past any return date.
    pub fn shows(&self, setting_on: bool, today: &str) -> bool {
        setting_on
            && !self.answered
            && self.threshold_met()
            && self.closes < CLOSES
            && self
                .returns_on
                .as_deref()
                .is_none_or(|returns_on| today >= returns_on)
    }

    /// Whether the one notification goes out: it would show, it has never
    /// gone out, and the Home line was never closed.
    fn notifies(&self, setting_on: bool, today: &str) -> bool {
        self.shows(setting_on, today) && !self.notified && self.closes == 0
    }

    /// Closes the Home line on `today`: the first close hides it until
    /// [`RETURN_AFTER_DAYS`] later, the second for good.
    pub fn close(&mut self, today: &str) {
        self.closes = (self.closes + 1).min(CLOSES);
        if self.closes == 1 {
            // A date that can't be read can't be counted from; stay hidden.
            self.returns_on =
                Some(add_days(today, RETURN_AFTER_DAYS).unwrap_or_else(|| "9999-12-31".to_owned()));
        }
    }
}

/// `date` (`YYYY-MM-DD`) plus `days`, in the proleptic Gregorian calendar.
fn add_days(date: &str, days: i64) -> Option<String> {
    let mut parts = date.splitn(3, '-').map(str::parse::<i64>);
    let (year, month, day) = (
        parts.next()?.ok()?,
        parts.next()?.ok()?,
        parts.next()?.ok()?,
    );
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    // Days from civil and back (Howard Hinnant's algorithms).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let z = era * 146_097 + doe - 719_468 + days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    Some(format!("{year:04}-{month:02}-{day:02}"))
}

pub fn state_path() -> Option<PathBuf> {
    voisu_core::state_dir()
        .ok()
        .map(|dir| dir.join("star-prompt.json"))
}

/// A missing or malformed file is a fresh state.
pub fn read(path: &Path) -> State {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// Writes through a temporary file and a rename, so a reader never sees a
/// torn file.
pub fn write(path: &Path, state: &State) -> std::io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| std::io::Error::other("state path has no folder"))?;
    std::fs::create_dir_all(dir)?;
    let mut file = tempfile::NamedTempFile::new_in(dir)?;
    serde_json::to_writer(&mut file, state)?;
    file.write_all(b"\n")?;
    file.persist(path).map_err(|error| error.error)?;
    Ok(())
}

/// Counts one successful dictation on `today` and says whether to send the
/// notification now. It is sent only when the nudge shows, the notification
/// has never been sent, the Home line was never closed, and `idle` confirms
/// no Recording runs; otherwise a later Delivery tries again. The file
/// records the notification before it goes out, so a failure to save never
/// sends it twice. Callers in one process serialise through [`DAEMON_LOCK`].
pub fn after_delivery(
    path: &Path,
    today: &str,
    setting_on: bool,
    idle: impl FnOnce() -> bool,
) -> bool {
    let before = read(path);
    let mut state = before.clone();
    state.record(today);
    let notify = state.notifies(setting_on, today) && idle();
    state.notified |= notify;
    if state == before {
        return false;
    }
    write(path, &state).is_ok() && notify
}

/// Whether the TUI Home line shows on `today`.
pub fn line_shows(path: &Path, setting_on: bool, today: &str) -> bool {
    read(path).shows(setting_on, today)
}

/// Records a close of the Home line on `today` and returns the new state.
pub fn close(path: &Path, today: &str) -> std::io::Result<State> {
    let mut state = read(path);
    state.close(today);
    write(path, &state)?;
    Ok(state)
}

/// Records `Star`: the nudge never shows again.
pub fn mark_answered(path: &Path) -> std::io::Result<()> {
    let mut state = read(path);
    if state.answered {
        return Ok(());
    }
    state.answered = true;
    write(path, &state)
}

/// `Star`: opens the repository through `open` and ends the nudge for good,
/// even when the browser does not open.
pub fn star(path: Option<&Path>, open: impl FnOnce(&str) -> Result<(), String>) -> Starred {
    let opened = open(REPO_URL).is_ok();
    let saved = match path {
        Some(path) => mark_answered(path).map_err(|error| error.to_string()),
        None => Err("no state folder".to_owned()),
    };
    Starred { opened, saved }
}

/// What [`star`] managed: opening the page and saving the answer are
/// reported apart, so a failed save never reads as a failed open.
#[derive(Debug)]
pub struct Starred {
    /// The browser launcher started.
    pub opened: bool,
    /// The answer was saved; on `Err` the nudge may show again.
    pub saved: Result<(), String>,
}

/// How long [`open_in_browser`] waits to learn whether the launcher failed.
const OPEN_GRACE: std::time::Duration = std::time::Duration::from_millis(500);

/// Opens `url` in the default browser through `xdg-open`. It waits only
/// half a second: on some desktops `xdg-open` stays alive as long as the
/// browser it started. `systemd-run --scope` moves the browser out of the
/// caller's cgroup, so restarting the daemon does not close it; where that
/// is unavailable or fails, `xdg-open` runs directly.
pub fn open_in_browser(url: &str) -> Result<(), String> {
    let scoped = detached("systemd-run")
        .args(["--user", "--scope", "--quiet", "--collect", "xdg-open", url])
        .spawn();
    if scoped.is_ok_and(launched) {
        return Ok(());
    }
    let direct = detached("xdg-open")
        .arg(url)
        .spawn()
        .map_err(|_| "xdg-open is unavailable".to_owned())?;
    if launched(direct) {
        Ok(())
    } else {
        Err("xdg-open could not open the browser".to_owned())
    }
}

/// A command with no terminal attached, in its own process group.
fn detached(program: &str) -> std::process::Command {
    use std::os::unix::process::CommandExt as _;
    let mut command = std::process::Command::new(program);
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .process_group(0);
    command
}

/// Whether `child` succeeded or is still running after [`OPEN_GRACE`]. A
/// child that is still running is waited for on its own thread, so it never
/// becomes a zombie and never holds the caller.
fn launched(mut child: std::process::Child) -> bool {
    let deadline = std::time::Instant::now() + OPEN_GRACE;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Err(_) => return false,
            Ok(None) if std::time::Instant::now() >= deadline => break,
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(10)),
        }
    }
    let _ = std::thread::Builder::new()
        .name("voisu-open-url".to_owned())
        .spawn(move || {
            let _ = child.wait();
        });
    true
}

/// Today's local calendar date, `YYYY-MM-DD`.
pub fn local_date(unix_secs: u64) -> String {
    let time = libc::time_t::try_from(unix_secs).unwrap_or(libc::time_t::MAX);
    // SAFETY: localtime_r writes only into the `tm` it is given.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let filled = unsafe { !libc::localtime_r(&time, &mut tm).is_null() };
    if !filled {
        return format!("day-{}", unix_secs / 86_400);
    }
    format!(
        "{:04}-{:02}-{:02}",
        i64::from(tm.tm_year) + 1900,
        tm.tm_mon + 1,
        tm.tm_mday
    )
}

/// How long the daemon waits for a click on a notification with actions.
/// After that it stops listening; the TUI still offers `Star`.
const ANSWER_WAIT: Duration = Duration::from_secs(60 * 60);

/// Serialises the daemon's read-change-write of the state file, so two
/// Deliveries finishing together cannot lose a count. Other processes (TUI,
/// CLI) write through an atomic rename and are not covered by it.
pub static DAEMON_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// The notification's actions, as `Notify` takes them: key, then label.
pub fn notification_actions() -> Vec<String> {
    vec!["star".to_owned(), "Star on GitHub".to_owned()]
}

/// Called by the daemon after a successful Delivery. Counts it, and when this
/// is the Delivery that crosses the threshold, sends the notification and
/// waits for its answer, all on a thread of its own so the next Recording is
/// never delayed. Test daemons never prompt.
pub fn spawn_after_delivery() {
    if std::env::var_os("VOISU_TEST_MODE").is_some() {
        return;
    }
    let _ = std::thread::Builder::new()
        .name("voisu-star-prompt".to_owned())
        .spawn(|| {
            let Some(path) = state_path() else {
                return;
            };
            let today = local_date(crate::update_notice::unix_now());
            let setting_on = crate::config::switch(crate::config::Switch::StarPrompt);
            let notify = {
                let _held = DAEMON_LOCK
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner());
                after_delivery(&path, &today, setting_on, daemon_idle)
            };
            if notify && notify_and_wait() {
                let _held = DAEMON_LOCK
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner());
                let _ = star(Some(&path), open_in_browser);
            }
        });
}

/// Whether the daemon answers that no Recording runs.
fn daemon_idle() -> bool {
    matches!(
        crate::daemon_client::send_command(voisu_core::Command::Status),
        Ok(response) if response.state == Some(voisu_core::DaemonState::Idle)
    )
}

/// Sends the notification and says whether the user clicked `Star on GitHub`.
fn notify_and_wait() -> bool {
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return false;
    };
    runtime.block_on(async {
        tokio::time::timeout(ANSWER_WAIT, desktop_prompt())
            .await
            .ok()
            .flatten()
            .unwrap_or(false)
    })
}

/// `Some(true)` when `Star on GitHub` was clicked. Closing or ignoring the
/// notification is not an answer.
async fn desktop_prompt() -> Option<bool> {
    use zbus::export::ordered_stream::OrderedStreamExt;

    let connection = zbus::Connection::session().await.ok()?;
    let proxy = zbus::Proxy::new(
        &connection,
        "org.freedesktop.Notifications",
        "/org/freedesktop/Notifications",
        "org.freedesktop.Notifications",
    )
    .await
    .ok()?;
    let hints: std::collections::HashMap<String, zbus::zvariant::OwnedValue> =
        std::collections::HashMap::new();
    let capabilities: Vec<String> = proxy.call("GetCapabilities", &()).await.unwrap_or_default();
    if !capabilities
        .iter()
        .any(|capability| capability == "actions")
    {
        let none: Vec<String> = Vec::new();
        let _ = proxy
            .call::<_, _, u32>(
                "Notify",
                &("Voisu", 0_u32, "", TITLE, PLAIN_BODY, none, hints, -1_i32),
            )
            .await;
        return None;
    }
    // Listen before sending, so an instant click is not missed.
    let mut invoked = proxy.receive_signal("ActionInvoked").await.ok()?;
    let mut closed = proxy.receive_signal("NotificationClosed").await.ok()?;
    let actions = notification_actions();
    let id: u32 = proxy
        .call(
            "Notify",
            &("Voisu", 0_u32, "", TITLE, BODY, actions, hints, -1_i32),
        )
        .await
        .ok()?;
    loop {
        tokio::select! {
            message = invoked.next() => {
                let message = message?;
                let Ok((from, key)) = message.body().deserialize::<(u32, String)>() else {
                    continue;
                };
                if from != id {
                    continue;
                }
                return Some(key == "star");
            }
            message = closed.next() => {
                let message = message?;
                if matches!(message.body().deserialize::<(u32, u32)>(), Ok((from, _)) if from == id) {
                    return None;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("voisu").join("star-prompt.json");
        (dir, path)
    }

    fn state(count: u32, dates: &[&str]) -> State {
        State {
            count,
            dates: dates.iter().map(|date| (*date).to_owned()).collect(),
            ..State::default()
        }
    }

    #[test]
    fn the_threshold_is_twenty_dictations_on_three_dates() {
        let three = ["2026-11-01", "2026-11-02", "2026-11-03"];
        assert!(!state(19, &three).threshold_met());
        assert!(state(20, &three).threshold_met());
        assert!(!state(20, &three[..2]).threshold_met());

        // Twenty on two days waits for a third day, however many follow.
        let mut counted = State::default();
        for _ in 0..10 {
            counted.record("2026-11-01");
        }
        for _ in 0..30 {
            counted.record("2026-11-02");
        }
        assert!(!counted.threshold_met());
        assert_eq!(counted.count, DICTATIONS, "counting is capped");
        counted.record("2026-11-03");
        assert!(counted.threshold_met());

        // Once met, nothing more is counted.
        let met = counted.clone();
        counted.record("2026-11-04");
        assert_eq!(counted, met);
    }

    const TODAY: &str = "2026-11-11";

    #[test]
    fn answered_and_switched_off_prompts_show_nothing() {
        let met = state(20, &["a", "b", "c"]);
        assert!(met.shows(true, TODAY));
        assert!(!met.shows(false, TODAY));
        assert!(
            !State {
                answered: true,
                ..met.clone()
            }
            .shows(true, TODAY)
        );
        assert!(!state(19, &["a", "b", "c"]).shows(true, TODAY));
    }

    #[test]
    fn a_first_close_hides_the_line_for_thirty_days_then_it_returns() {
        let mut met = state(20, &["a", "b", "c"]);
        met.close("2026-11-11");
        assert!(!met.shows(true, "2026-11-11"), "first close hides");
        assert!(!met.shows(true, "2026-12-10"), "day 29 still hidden");
        assert!(met.shows(true, "2026-12-11"), "day 30 shows again");
        assert!(met.shows(true, "2027-03-01"), "and stays until answered");
        assert!(!met.shows(false, "2026-12-11"), "the setting still wins");
    }

    #[test]
    fn a_second_close_hides_the_line_for_good() {
        let mut met = state(20, &["a", "b", "c"]);
        met.close("2026-11-11");
        met.close("2026-12-11");
        assert!(!met.shows(true, "2026-12-11"));
        assert!(!met.shows(true, "2099-01-01"));
    }

    #[test]
    fn starring_after_a_close_ends_the_nudge() {
        let (_dir, path) = file();
        write(&path, &state(20, &["a", "b", "c"])).unwrap();
        close(&path, "2026-11-11").unwrap();
        star(Some(&path), |_| Ok(())).saved.unwrap();
        assert!(!line_shows(&path, true, "2026-12-11"));
        assert!(!line_shows(&path, true, "2099-01-01"));
    }

    #[test]
    fn the_return_brings_no_second_notification() {
        let (_dir, path) = file();
        for day in ["2026-11-01", "2026-11-02", "2026-11-03"] {
            for _ in 0..7 {
                let _ = after_delivery(&path, day, true, || true);
            }
        }
        assert!(read(&path).notified);
        close(&path, "2026-11-03").unwrap();
        assert!(line_shows(&path, true, "2026-12-03"));
        assert!(!after_delivery(&path, "2026-12-03", true, || true));

        // Closed before the notification could go out: it never does.
        let (_dir, early) = file();
        for day in ["2026-11-01", "2026-11-02", "2026-11-03"] {
            for _ in 0..7 {
                let _ = after_delivery(&early, day, true, || false);
            }
        }
        close(&early, "2026-11-03").unwrap();
        for day in ["2026-11-04", "2026-12-03", "2026-12-04"] {
            assert!(!after_delivery(&early, day, true, || true));
        }
        assert!(!read(&early).notified);
    }

    #[test]
    fn a_state_file_from_before_the_second_chance_loads_with_its_count() {
        let (_dir, path) = file();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            r#"{"count":20,"dates":["2026-11-01","2026-11-02","2026-11-03"],"notified":true,"answered":false}"#,
        )
        .unwrap();
        let loaded = read(&path);
        assert_eq!(loaded.count, 20);
        assert_eq!(loaded.closes, 0);
        assert_eq!(loaded.returns_on, None);
        assert!(line_shows(&path, true, TODAY));
    }

    #[test]
    fn the_notification_offers_only_the_star_action() {
        assert_eq!(notification_actions(), ["star", "Star on GitHub"]);
    }

    #[test]
    fn return_dates_cross_months_years_and_leap_days() {
        assert_eq!(add_days("2026-11-11", 30).as_deref(), Some("2026-12-11"));
        assert_eq!(add_days("2026-12-15", 30).as_deref(), Some("2027-01-14"));
        assert_eq!(add_days("2028-02-10", 30).as_deref(), Some("2028-03-11"));
        assert_eq!(add_days("day-5", 30), None);
    }

    #[test]
    fn the_notification_goes_out_exactly_once() {
        let (_dir, path) = file();
        let mut sent = 0;
        for day in ["2026-11-01", "2026-11-02", "2026-11-03", "2026-11-04"] {
            for _ in 0..10 {
                sent += usize::from(after_delivery(&path, day, true, || true));
            }
        }
        assert_eq!(sent, 1);
        let saved = read(&path);
        assert!(saved.notified);
        assert!(saved.threshold_met());
        // The line stays until the user answers.
        assert!(line_shows(&path, true, "2026-11-04"));
    }

    #[test]
    fn the_notification_crosses_on_the_twentieth_dictation_once_idle() {
        let (_dir, path) = file();
        for date in ["2026-11-01", "2026-11-02"] {
            assert!(!after_delivery(&path, date, true, || panic!("asked early")));
        }
        for _ in 0..17 {
            assert!(!after_delivery(&path, "2026-11-03", true, || panic!(
                "asked early"
            )));
        }
        assert_eq!(read(&path).count, 19);
        // A Recording already running holds it back to a later Delivery.
        assert!(!after_delivery(&path, "2026-11-03", true, || false));
        assert!(!read(&path).notified);
        assert!(after_delivery(&path, "2026-11-04", true, || true));
    }

    #[test]
    fn a_switched_off_or_answered_prompt_never_notifies() {
        let (_dir, path) = file();
        for day in ["2026-11-01", "2026-11-02", "2026-11-03"] {
            for _ in 0..7 {
                assert!(!after_delivery(&path, day, false, || true));
            }
        }
        assert!(!line_shows(&path, false, TODAY));

        let (_dir, answered) = file();
        mark_answered(&answered).unwrap();
        for day in ["2026-11-01", "2026-11-02", "2026-11-03"] {
            for _ in 0..7 {
                assert!(!after_delivery(&answered, day, true, || true));
            }
        }
        assert!(!line_shows(&answered, true, TODAY));
    }

    #[test]
    fn star_opens_the_repository_and_answers_even_without_a_browser() {
        let (_dir, path) = file();
        let mut opened = None;
        let result = star(Some(&path), |url| {
            opened = Some(url.to_owned());
            Err("no browser".to_owned())
        });
        assert_eq!(opened.as_deref(), Some(REPO_URL));
        assert!(!result.opened);
        assert!(result.saved.is_ok());
        assert!(read(&path).answered);
    }

    #[test]
    fn a_failed_save_does_not_read_as_a_failed_open() {
        let dir = tempfile::tempdir().unwrap();
        // A directory where the state file should be cannot be written.
        let path = dir.path().join("star-prompt.json");
        std::fs::create_dir_all(path.join("x")).unwrap();
        let result = star(Some(&path), |_| Ok(()));
        assert!(result.opened);
        assert!(result.saved.is_err());
    }

    #[test]
    fn a_malformed_state_file_starts_fresh() {
        let (_dir, path) = file();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(read(&path), State::default());
    }

    #[test]
    fn local_dates_read_as_year_month_day() {
        let date = local_date(1_762_819_200); // 2025-11-11 around midnight UTC
        assert_eq!(date.len(), 10, "{date}");
        assert!(date.starts_with("2025-11-1"), "{date}");
    }
}
