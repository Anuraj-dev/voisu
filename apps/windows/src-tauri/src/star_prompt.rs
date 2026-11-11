//! The one-time GitHub star prompt. After 20 successful dictations on at
//! least 3 different local days the window asks for a star, once, until the
//! user answers. The count lives in a small local file and is never sent
//! anywhere (ADR 0006). Counting starts at the upgrade that added it and
//! stops once the threshold is met.

use std::fs;
use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// Successful dictations before the prompt shows.
pub const DICTATIONS_NEEDED: u32 = 20;
/// Distinct local calendar dates those dictations must span.
pub const DAYS_NEEDED: usize = 3;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct StarPrompt {
    /// Successful dictations counted, capped at [`DICTATIONS_NEEDED`].
    dictations: u32,
    /// Distinct local dates (`YYYY-MM-DD`) with a successful dictation,
    /// capped at [`DAYS_NEEDED`].
    days: Vec<String>,
    /// The user chose Star on GitHub; the prompt never returns.
    answered: bool,
    /// The threshold was crossed, so the one notification is spent whether
    /// or not it was shown.
    notified: bool,
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

    /// Counts one successful dictation on the local date `date`. Returns
    /// true exactly once ever: for the dictation that crosses the threshold,
    /// when the prompt is `enabled` and not already answered.
    pub fn record(&mut self, date: &str, enabled: bool) -> bool {
        if self.threshold_met() {
            return false;
        }
        self.dictations = (self.dictations + 1).min(DICTATIONS_NEEDED);
        if self.days.len() < DAYS_NEEDED && !self.days.iter().any(|day| day == date) {
            self.days.push(date.to_owned());
        }
        if !self.threshold_met() || self.notified {
            return false;
        }
        self.notified = true;
        enabled && !self.answered
    }

    /// Whether the window shows the banner.
    pub fn show(&self, enabled: bool) -> bool {
        enabled && !self.answered && self.threshold_met()
    }

    /// Star on GitHub was chosen: never ask again.
    pub fn answer(&mut self) {
        self.answered = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ON: bool = true;

    /// `count` dictations spread over `dates`, in order, the rest on the
    /// last date. Returns how many asked for the notification.
    fn dictate(prompt: &mut StarPrompt, count: u32, dates: &[&str], enabled: bool) -> u32 {
        let mut notified = 0;
        for i in 0..count {
            let date = dates[(i as usize).min(dates.len() - 1)];
            if prompt.record(date, enabled) {
                notified += 1;
            }
        }
        notified
    }

    const THREE: [&str; 3] = ["2026-11-01", "2026-11-02", "2026-11-03"];

    #[test]
    fn nineteen_dictations_on_three_days_show_nothing() {
        let mut prompt = StarPrompt::default();
        assert_eq!(dictate(&mut prompt, 19, &THREE, ON), 0);
        assert!(!prompt.show(ON));
    }

    #[test]
    fn the_twentieth_on_the_third_day_shows_the_banner_and_notifies_once() {
        let mut prompt = StarPrompt::default();
        assert_eq!(dictate(&mut prompt, 19, &THREE, ON), 0);
        assert!(prompt.record("2026-11-03", ON), "the crossing dictation");
        assert!(prompt.show(ON));
        assert_eq!(dictate(&mut prompt, 50, &["2026-11-09"], ON), 0);
        assert!(prompt.show(ON));
    }

    #[test]
    fn twenty_on_two_days_waits_for_a_third_day() {
        let mut prompt = StarPrompt::default();
        assert_eq!(dictate(&mut prompt, 30, &THREE[..2], ON), 0);
        assert!(!prompt.show(ON));
        assert!(prompt.record("2026-11-03", ON), "the third day crosses it");
        assert!(prompt.show(ON));
    }

    #[test]
    fn answered_shows_nothing_and_never_notifies() {
        let mut prompt = StarPrompt::default();
        prompt.answer();
        assert_eq!(dictate(&mut prompt, 25, &THREE, ON), 0);
        assert!(!prompt.show(ON));
    }

    #[test]
    fn setting_off_shows_nothing_and_spends_the_notification() {
        let mut prompt = StarPrompt::default();
        assert_eq!(dictate(&mut prompt, 20, &THREE, false), 0);
        assert!(!prompt.show(false));
        // Turned back on later: the banner returns, the notification does not.
        assert!(prompt.show(ON));
        assert_eq!(dictate(&mut prompt, 5, &["2026-11-10"], ON), 0);
    }

    #[test]
    fn counting_stops_at_the_threshold_so_the_file_stops_changing() {
        let mut prompt = StarPrompt::default();
        dictate(&mut prompt, 20, &THREE, ON);
        let met = prompt.clone();
        dictate(&mut prompt, 10, &["2026-11-20"], ON);
        assert_eq!(prompt, met);
    }

    #[test]
    fn state_round_trips_and_a_missing_or_corrupt_file_starts_afresh() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Voisu").join("star-prompt.json");
        assert_eq!(StarPrompt::load(&path), StarPrompt::default());
        let mut prompt = StarPrompt::default();
        dictate(&mut prompt, 20, &THREE, ON);
        prompt.save(&path).unwrap();
        assert_eq!(StarPrompt::load(&path), prompt);
        assert!(StarPrompt::load(&path).show(ON));
        fs::write(&path, b"{not json").unwrap();
        assert_eq!(StarPrompt::load(&path), StarPrompt::default());
    }
}
