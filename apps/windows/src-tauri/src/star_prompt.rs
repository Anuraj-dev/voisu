//! The GitHub star nudge. After 20 successful dictations on at least 3
//! different local days the window shows an "Enjoying Voisu?" banner and one
//! notification is sent. The first close hides the banner for 30 days; it
//! then returns once, without a second notification, and a second close
//! hides it for good. Starring at any time ends it. The count lives in a
//! small local file and is never sent anywhere (ADR 0006). Counting starts
//! at the upgrade that added it and stops once the threshold is met.

use std::fs;
use std::io;
use std::path::Path;

use chrono::{Days, NaiveDate};
use serde::{Deserialize, Serialize};

/// Successful dictations before the nudge shows.
pub const DICTATIONS_NEEDED: u32 = 20;
/// Distinct local calendar dates those dictations must span.
pub const DAYS_NEEDED: usize = 3;
/// How long the first close hides the banner.
pub const SECOND_CHANCE_AFTER: Days = Days::new(30);
/// Closes after which the banner never returns.
const CLOSES_FOR_GOOD: u32 = 2;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct StarPrompt {
    /// Successful dictations counted, capped at [`DICTATIONS_NEEDED`].
    dictations: u32,
    /// Distinct local dates with a successful dictation, capped at
    /// [`DAYS_NEEDED`].
    days: Vec<NaiveDate>,
    /// The user chose Star on GitHub; the nudge never returns.
    answered: bool,
    /// The threshold was crossed, so the one notification is spent whether
    /// or not it was shown.
    notified: bool,
    /// Times the banner was closed, capped at [`CLOSES_FOR_GOOD`].
    closes: u32,
    /// After the first close: the local date the banner may show again.
    returns_on: Option<NaiveDate>,
}

impl StarPrompt {
    /// A missing or unreadable file starts the count afresh.
    pub fn load(path: &Path) -> Self {
        match fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|error| {
                tracing::warn!(%error, "star prompt file unreadable; starting afresh");
                Self::default()
            }),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Self::default(),
            Err(error) => {
                tracing::warn!(%error, "star prompt file could not be read");
                Self::default()
            }
        }
    }

    /// Writes atomically (temp file + rename), as settings do.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        let dir = path
            .parent()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "star prompt path"))?;
        fs::create_dir_all(dir)?;
        let json = serde_json::to_vec_pretty(self).map_err(io::Error::other)?;
        let temp = path.with_extension("json.tmp");
        fs::write(&temp, json)?;
        fs::rename(&temp, path)
    }

    fn threshold_met(&self) -> bool {
        self.dictations >= DICTATIONS_NEEDED && self.days.len() >= DAYS_NEEDED
    }

    /// Counts one successful dictation on the local date `today`. Returns
    /// true exactly once ever: for the dictation that crosses the threshold,
    /// when the nudge is `enabled` and not already answered.
    pub fn record(&mut self, today: NaiveDate, enabled: bool) -> bool {
        if self.threshold_met() {
            return false;
        }
        self.dictations = (self.dictations + 1).min(DICTATIONS_NEEDED);
        if self.days.len() < DAYS_NEEDED && !self.days.contains(&today) {
            self.days.push(today);
        }
        if !self.threshold_met() || self.notified {
            return false;
        }
        self.notified = true;
        enabled && !self.answered
    }

    /// Whether the window shows the banner on the local date `today`.
    pub fn show(&self, today: NaiveDate, enabled: bool) -> bool {
        let closed = match self.closes {
            0 => false,
            1 => self.returns_on.is_some_and(|returns| today < returns),
            _ => true,
        };
        enabled && !self.answered && self.threshold_met() && !closed
    }

    /// The banner's ×: the first close hides it until 30 days after
    /// `today`, the second for good. A close while the banner is already
    /// hidden (a double-click on ×) is not counted.
    pub fn close(&mut self, today: NaiveDate) {
        if self.returns_on.is_some_and(|returns| today < returns) {
            return;
        }
        self.closes = (self.closes + 1).min(CLOSES_FOR_GOOD);
        if self.closes == 1 {
            self.returns_on = today.checked_add_days(SECOND_CHANCE_AFTER);
        }
    }

    /// Star on GitHub was chosen, from the banner or the Settings card:
    /// never nudge again.
    pub fn answer(&mut self) {
        self.answered = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ON: bool = true;

    fn day(n: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 11, 1).unwrap() + Days::new(u64::from(n) - 1)
    }

    /// `count` dictations spread over `dates`, in order, the rest on the
    /// last date. Returns how many asked for the notification.
    fn dictate(prompt: &mut StarPrompt, count: u32, dates: &[NaiveDate], enabled: bool) -> u32 {
        let mut notified = 0;
        for i in 0..count {
            let date = dates[(i as usize).min(dates.len() - 1)];
            if prompt.record(date, enabled) {
                notified += 1;
            }
        }
        notified
    }

    fn three() -> [NaiveDate; 3] {
        [day(1), day(2), day(3)]
    }

    /// Eligible on day 3, with its notification spent.
    fn eligible() -> StarPrompt {
        let mut prompt = StarPrompt::default();
        assert_eq!(dictate(&mut prompt, 20, &three(), ON), 1);
        assert!(prompt.show(day(3), ON));
        prompt
    }

    #[test]
    fn nineteen_dictations_on_three_days_show_nothing() {
        let mut prompt = StarPrompt::default();
        assert_eq!(dictate(&mut prompt, 19, &three(), ON), 0);
        assert!(!prompt.show(day(3), ON));
    }

    #[test]
    fn the_twentieth_on_the_third_day_shows_the_banner_and_notifies_once() {
        let mut prompt = StarPrompt::default();
        assert_eq!(dictate(&mut prompt, 19, &three(), ON), 0);
        assert!(prompt.record(day(3), ON), "the crossing dictation");
        assert!(prompt.show(day(3), ON));
        assert_eq!(dictate(&mut prompt, 50, &[day(9)], ON), 0);
        assert!(prompt.show(day(9), ON));
    }

    #[test]
    fn twenty_on_two_days_waits_for_a_third_day() {
        let mut prompt = StarPrompt::default();
        assert_eq!(dictate(&mut prompt, 30, &three()[..2], ON), 0);
        assert!(!prompt.show(day(2), ON));
        assert!(prompt.record(day(3), ON), "the third day crosses it");
        assert!(prompt.show(day(3), ON));
    }

    #[test]
    fn answered_shows_nothing_and_never_notifies() {
        let mut prompt = StarPrompt::default();
        prompt.answer();
        assert_eq!(dictate(&mut prompt, 25, &three(), ON), 0);
        assert!(!prompt.show(day(3), ON));
    }

    #[test]
    fn setting_off_shows_nothing_and_spends_the_notification() {
        let mut prompt = StarPrompt::default();
        assert_eq!(dictate(&mut prompt, 20, &three(), false), 0);
        assert!(!prompt.show(day(3), false));
        // Turned back on later: the banner returns, the notification does not.
        assert!(prompt.show(day(10), ON));
        assert_eq!(dictate(&mut prompt, 5, &[day(10)], ON), 0);
    }

    #[test]
    fn counting_stops_at_the_threshold_so_the_file_stops_changing() {
        let mut prompt = eligible();
        let met = prompt.clone();
        dictate(&mut prompt, 10, &[day(20)], ON);
        assert_eq!(prompt, met);
    }

    #[test]
    fn the_first_close_hides_the_banner_until_day_thirty() {
        let mut prompt = eligible();
        prompt.close(day(3));
        assert!(!prompt.show(day(3), ON), "hidden at once");
        assert!(!prompt.show(day(3 + 29), ON), "day 29 still hidden");
        assert!(prompt.show(day(3 + 30), ON), "day 30 shows again");
        assert!(
            prompt.show(day(3 + 90), ON),
            "until it is answered or closed"
        );
    }

    #[test]
    fn the_banner_returns_without_a_second_notification() {
        let mut prompt = eligible();
        prompt.close(day(3));
        assert_eq!(dictate(&mut prompt, 20, &[day(40)], ON), 0);
        assert!(prompt.show(day(40), ON));
    }

    #[test]
    fn the_second_close_hides_it_for_good() {
        let mut prompt = eligible();
        prompt.close(day(3));
        prompt.close(day(33));
        assert!(!prompt.show(day(63), ON));
        assert!(!prompt.show(day(400), ON));
        prompt.close(day(400));
        assert!(!prompt.show(day(800), ON));
    }

    #[test]
    fn starring_after_a_close_ends_it() {
        let mut prompt = eligible();
        prompt.close(day(3));
        prompt.answer();
        assert!(!prompt.show(day(33), ON));
        assert!(!prompt.show(day(400), ON));
    }

    #[test]
    fn the_setting_still_hides_a_returned_banner() {
        let mut prompt = eligible();
        prompt.close(day(3));
        assert!(!prompt.show(day(33), false));
    }

    #[test]
    fn state_round_trips_and_a_missing_or_corrupt_file_starts_afresh() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Voisu").join("star-prompt.json");
        assert_eq!(StarPrompt::load(&path), StarPrompt::default());
        let mut prompt = eligible();
        prompt.close(day(3));
        prompt.save(&path).unwrap();
        assert_eq!(StarPrompt::load(&path), prompt);
        assert!(StarPrompt::load(&path).show(day(33), ON));
        fs::write(&path, b"{not json").unwrap();
        assert_eq!(StarPrompt::load(&path), StarPrompt::default());
    }

    #[test]
    fn a_double_click_on_close_still_leaves_the_second_chance() {
        let mut prompt = StarPrompt::default();
        prompt.close(day(3));
        prompt.close(day(3));
        assert_eq!(prompt.closes, 1);
        prompt.close(day(33));
        assert_eq!(prompt.closes, 2);
    }

    #[test]
    fn a_file_from_before_the_second_chance_loads_without_a_close() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("star-prompt.json");
        fs::write(
            &path,
            br#"{"dictations": 20, "days": ["2026-11-01", "2026-11-02", "2026-11-03"],
                 "answered": false, "notified": true}"#,
        )
        .unwrap();
        let prompt = StarPrompt::load(&path);
        assert_eq!(prompt, eligible());
        assert!(prompt.show(day(3), ON));
    }
}
