//! The one-time GitHub star prompt.
//!
//! The daemon counts successful dictations (a Recording whose Transcript was
//! delivered: pasted, or left on the clipboard) and the distinct local dates
//! they happened on, in `$XDG_STATE_HOME/voisu/star-prompt.json`. Nothing here
//! is ever sent anywhere. Counting starts at the upgrade, with no backfill,
//! and stops once the threshold is met.
//!
//! Once 20 dictations on 3 or more dates are counted, with `star_prompt = on`
//! and no answer yet, the daemon sends one desktop notification after that
//! Delivery has finished, and the TUI Home screen shows a line until the user
//! answers. `Star` opens the repository page and marks the prompt answered;
//! `Don't ask again` turns `star_prompt` off. The notification is sent once,
//! ever: an ignored or closed one is never repeated.

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
pub const HOME_LINE: &str = "Enjoying Voisu? Star it on GitHub";

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
    /// The user starred, or said they would. The prompt never shows again.
    #[serde(default)]
    pub answered: bool,
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

    /// Whether the prompt shows: threshold met, setting on, not answered.
    pub fn eligible(&self, setting_on: bool) -> bool {
        setting_on && !self.answered && self.threshold_met()
    }
}

/// The user's answer to the prompt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Answer {
    Star,
    DontAsk,
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
/// notification now. It is sent only when the prompt is eligible, has never
/// been sent, and `idle` confirms no Recording runs; otherwise a later
/// Delivery tries again. The file records the notification before it goes
/// out, so a failure to save never sends it twice.
pub fn after_delivery(
    path: &Path,
    today: &str,
    setting_on: bool,
    idle: impl FnOnce() -> bool,
) -> bool {
    let before = read(path);
    let mut state = before.clone();
    state.record(today);
    let notify = state.eligible(setting_on) && !state.notified && idle();
    state.notified |= notify;
    if state == before {
        return false;
    }
    write(path, &state).is_ok() && notify
}

/// Whether the TUI Home line shows.
pub fn line_shows(path: &Path, setting_on: bool) -> bool {
    read(path).eligible(setting_on)
}

/// Records `Star`: the prompt is answered and never shows again.
pub fn mark_answered(path: &Path) -> std::io::Result<()> {
    let mut state = read(path);
    if state.answered {
        return Ok(());
    }
    state.answered = true;
    write(path, &state)
}

/// Applies an answer. `Star` opens the repository through `open` and marks
/// the prompt answered even when the browser does not open, so it never
/// returns. `Don't ask again` turns the setting off.
pub fn apply(
    answer: Answer,
    path: Option<&Path>,
    open: impl FnOnce(&str) -> Result<(), String>,
) -> Result<(), String> {
    match answer {
        Answer::Star => {
            let opened = open(REPO_URL);
            if let Some(path) = path {
                mark_answered(path).map_err(|error| error.to_string())?;
            }
            opened
        }
        Answer::DontAsk => {
            crate::config::set_switch(crate::config::Switch::StarPrompt, false).map(|_| ())
        }
    }
}

/// Opens `url` in the default browser through `xdg-open`.
pub fn open_in_browser(url: &str) -> Result<(), String> {
    let status = std::process::Command::new("xdg-open")
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map_err(|_| "xdg-open is unavailable".to_owned())?;
    if status.success() {
        Ok(())
    } else {
        Err("xdg-open could not open the browser".to_owned())
    }
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

/// How long the daemon waits for an answer on a notification with actions.
/// After that it stops listening; the TUI line still offers both answers.
const ANSWER_WAIT: Duration = Duration::from_secs(60 * 60);

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
            if !after_delivery(&path, &today, setting_on, daemon_idle) {
                return;
            }
            if let Some(answer) = notify_and_wait() {
                let _ = apply(answer, Some(&path), open_in_browser);
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

/// Sends the notification and returns the action the user picked, if any.
fn notify_and_wait() -> Option<Answer> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .ok()?;
    runtime.block_on(async {
        tokio::time::timeout(ANSWER_WAIT, desktop_prompt())
            .await
            .ok()
            .flatten()
    })
}

async fn desktop_prompt() -> Option<Answer> {
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
    let actions = vec![
        "star".to_owned(),
        "Star on GitHub".to_owned(),
        "never".to_owned(),
        "Don't ask again".to_owned(),
    ];
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
                return match key.as_str() {
                    "star" => Some(Answer::Star),
                    "never" => Some(Answer::DontAsk),
                    _ => None,
                };
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

    #[test]
    fn answered_and_switched_off_prompts_show_nothing() {
        let met = state(20, &["a", "b", "c"]);
        assert!(met.eligible(true));
        assert!(!met.eligible(false));
        assert!(
            !State {
                answered: true,
                ..met.clone()
            }
            .eligible(true)
        );
        assert!(!state(19, &["a", "b", "c"]).eligible(true));
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
        assert!(line_shows(&path, true));
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
        assert!(!line_shows(&path, false));

        let (_dir, answered) = file();
        mark_answered(&answered).unwrap();
        for day in ["2026-11-01", "2026-11-02", "2026-11-03"] {
            for _ in 0..7 {
                assert!(!after_delivery(&answered, day, true, || true));
            }
        }
        assert!(!line_shows(&answered, true));
    }

    #[test]
    fn star_opens_the_repository_and_answers_even_without_a_browser() {
        let (_dir, path) = file();
        let mut opened = None;
        let result = apply(Answer::Star, Some(&path), |url| {
            opened = Some(url.to_owned());
            Err("no browser".to_owned())
        });
        assert_eq!(opened.as_deref(), Some(REPO_URL));
        assert!(result.is_err());
        assert!(read(&path).answered);
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
