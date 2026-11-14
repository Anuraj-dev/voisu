//! The daily update check and the "Update available" line on the TUI Home
//! screen and in `voisu status`.
//!
//! The daemon reads the latest release's version at most once a day: shortly
//! after it starts, then on a slow tick, on a thread of its own and only while
//! no Recording runs ([`spawn_daemon_check`]). The TUI does the same check
//! when it opens. Both use the restricted `curl` request
//! `voisu update --check` makes and share one cache,
//! `$XDG_CACHE_HOME/voisu/update-check.json`, so they rarely both fetch.
//! `voisu status` only reads that cache, so it never waits on the network.
//! `update_check = off` stops every automatic check. Every failure is silent:
//! the notice is a hint, never an error.
//!
//! The cache holds the release's version, not a verdict. Each read compares
//! it with the running binary, so the line goes away as soon as Voisu is
//! upgraded.

use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// How long one check, failed or not, is trusted before the next.
pub const CHECK_INTERVAL_SECS: u64 = 24 * 60 * 60;

const CACHE_FILE: &str = "update-check.json";

/// The last check: when it ran and the newest release tag known after it.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LastCheck {
    /// Unix seconds of the last attempt, successful or not.
    pub checked_at: u64,
    /// The latest release tag, such as `v0.75.0`. A failed attempt keeps the
    /// one known before it.
    pub latest: Option<String>,
}

/// `$XDG_CACHE_HOME/voisu/update-check.json`, else `~/.cache/voisu/…`. A
/// relative `XDG_CACHE_HOME` is ignored, as the XDG spec says.
pub fn cache_path_from(xdg_cache: Option<OsString>, home: Option<OsString>) -> Option<PathBuf> {
    let base = xdg_cache
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            home.map(PathBuf::from)
                .filter(|path| path.is_absolute())
                .map(|home| home.join(".cache"))
        })?;
    Some(base.join("voisu").join(CACHE_FILE))
}

pub fn cache_path() -> Option<PathBuf> {
    cache_path_from(std::env::var_os("XDG_CACHE_HOME"), std::env::var_os("HOME"))
}

/// Whether a new check should run at `now` (unix seconds). A stamp from the
/// future means the clock went back; trusting it could stop checks for good.
pub fn is_due(last: Option<&LastCheck>, now: u64) -> bool {
    match last {
        None => true,
        Some(last) => last.checked_at > now || now - last.checked_at >= CHECK_INTERVAL_SECS,
    }
}

/// The cached check. A missing, unreadable or malformed file is no cache.
pub fn read(path: &Path) -> Option<LastCheck> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Writes the cache through a temporary file and a rename, so a second TUI
/// writing at the same moment never leaves a torn file.
pub fn write(path: &Path, check: &LastCheck) -> std::io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| std::io::Error::other("cache path has no folder"))?;
    std::fs::create_dir_all(dir)?;
    let mut file = tempfile::NamedTempFile::new_in(dir)?;
    serde_json::to_writer(&mut file, check)?;
    file.write_all(b"\n")?;
    file.persist(path).map_err(|error| error.error)?;
    Ok(())
}

/// The newer release's version, checking GitHub through `fetch` only when a
/// check is due. A failed fetch still records the attempt, keeping the tag
/// known before it, so an offline machine tries once a day, not every launch.
pub fn check_daily(
    path: &Path,
    installed: &str,
    now: u64,
    fetch: impl FnOnce() -> Result<String, String>,
) -> Option<String> {
    let last = read(path);
    let latest = if is_due(last.as_ref(), now) {
        let latest = fetch().ok().or_else(|| last.and_then(|last| last.latest));
        let _ = write(
            path,
            &LastCheck {
                checked_at: now,
                latest: latest.clone(),
            },
        );
        latest
    } else {
        last.and_then(|last| last.latest)
    };
    crate::update::newer_release(installed, &latest?)
}

/// The newer release's version from the cache alone. Never touches the network.
pub fn cached_newer(path: &Path, installed: &str) -> Option<String> {
    crate::update::newer_release(installed, &read(path)?.latest?)
}

pub fn notice_line(version: &str) -> String {
    format!("Update available: {version}, run `voisu update`")
}

/// The TUI's daily check against the running binary's version. With the
/// automatic check off it only reads the cache.
pub fn check_daily_now() -> Option<String> {
    let path = cache_path()?;
    if !crate::config::switch(crate::config::Switch::UpdateCheck) {
        return cached_newer(&path, env!("CARGO_PKG_VERSION"));
    }
    check_daily(
        &path,
        env!("CARGO_PKG_VERSION"),
        unix_now(),
        crate::update::latest_release_version,
    )
}

/// How long after the daemon starts its first check runs: long enough that
/// login, the first Recording and the Trigger Key setup are not competing
/// with it.
pub const DAEMON_FIRST_CHECK: Duration = Duration::from_secs(60);
/// How often the daemon looks at the cache again. A check runs only when the
/// cache is due, so this sets how late a long-running daemon can be.
pub const DAEMON_TICK: Duration = Duration::from_secs(60 * 60);

/// What one daemon tick did.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Tick {
    /// `update_check = off`, or no cache path.
    Off,
    /// Checked within the last day.
    NotDue,
    /// Due, but a Recording is running; the next tick tries again.
    Busy,
    /// Fetched and recorded, whether or not the fetch succeeded.
    Checked,
}

/// One daemon tick. Asks whether the daemon is idle only when a check is due,
/// and fetches only then. Every outcome is silent.
pub fn daemon_tick(
    path: Option<&Path>,
    enabled: bool,
    now: u64,
    idle: impl FnOnce() -> bool,
    fetch: impl FnOnce() -> Result<String, String>,
) -> Tick {
    let Some(path) = path.filter(|_| enabled) else {
        return Tick::Off;
    };
    if !is_due(read(path).as_ref(), now) {
        return Tick::NotDue;
    }
    if !idle() {
        return Tick::Busy;
    }
    check_daily(path, env!("CARGO_PKG_VERSION"), now, fetch);
    Tick::Checked
}

/// Starts the daemon's daily check on a thread of its own, so it never shares
/// a thread with, or delays, a Recording, its transcription or its Delivery.
/// It asks the daemon's own socket whether it is idle before each fetch, so no
/// check starts while a Recording runs. Test daemons never check.
pub fn spawn_daemon_check() {
    if std::env::var_os("VOISU_TEST_MODE").is_some() {
        return;
    }
    let idle = || {
        matches!(
            crate::daemon_client::send_command(voisu_core::Command::Status),
            Ok(response) if response.state == Some(voisu_core::DaemonState::Idle)
        )
    };
    let _ = std::thread::Builder::new()
        .name("voisu-update-check".to_owned())
        .spawn(move || {
            std::thread::sleep(DAEMON_FIRST_CHECK);
            // Kept in memory as well, so a cache that cannot be written
            // (full disk, read-only home) still means one fetch a day.
            let mut fetched_at: Option<u64> = None;
            loop {
                let now = unix_now();
                let fetched_today =
                    fetched_at.is_some_and(|at| now >= at && now - at < CHECK_INTERVAL_SECS);
                if !fetched_today {
                    let tick = daemon_tick(
                        cache_path().as_deref(),
                        crate::config::switch(crate::config::Switch::UpdateCheck),
                        now,
                        idle,
                        crate::update::latest_release_version,
                    );
                    if tick == Tick::Checked {
                        fetched_at = Some(now);
                    }
                }
                std::thread::sleep(DAEMON_TICK);
            }
        });
}

/// The line `voisu status` prints, from the cache alone.
pub fn cached_notice() -> Option<String> {
    cached_newer(&cache_path()?, env!("CARGO_PKG_VERSION")).map(|version| notice_line(&version))
}

/// Records a release version a successful `voisu update --check` read, so Home and
/// `voisu status` agree with it at once.
pub fn record_release(tag: &str) {
    if let Some(path) = cache_path() {
        let _ = write(
            &path,
            &LastCheck {
                checked_at: unix_now(),
                latest: Some(tag.to_owned()),
            },
        );
    }
}

pub(crate) fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    const DAY: u64 = CHECK_INTERVAL_SECS;
    const NOW: u64 = 1_800_000_000;

    fn cache() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("voisu").join(CACHE_FILE);
        (dir, path)
    }

    fn checked(at: u64, latest: Option<&str>) -> LastCheck {
        LastCheck {
            checked_at: at,
            latest: latest.map(str::to_owned),
        }
    }

    #[test]
    fn the_cache_lives_under_xdg_cache_home_or_the_home_cache() {
        assert_eq!(
            cache_path_from(Some("/x/cache".into()), Some("/home/u".into())),
            Some(PathBuf::from("/x/cache/voisu/update-check.json"))
        );
        assert_eq!(
            cache_path_from(Some("relative".into()), Some("/home/u".into())),
            Some(PathBuf::from("/home/u/.cache/voisu/update-check.json"))
        );
        assert_eq!(cache_path_from(None, Some("home".into())), None);
        assert_eq!(cache_path_from(None, None), None);
    }

    #[test]
    fn a_check_is_due_once_a_day_and_when_the_clock_went_back() {
        assert!(is_due(None, NOW));
        assert!(!is_due(Some(&checked(NOW, None)), NOW));
        assert!(!is_due(Some(&checked(NOW - DAY + 1, None)), NOW));
        assert!(is_due(Some(&checked(NOW - DAY, None)), NOW));
        assert!(is_due(Some(&checked(NOW + 60, None)), NOW));
    }

    #[test]
    fn a_due_check_fetches_and_records_the_latest_tag() {
        let (_dir, path) = cache();
        let found = check_daily(&path, "0.74.0", NOW, || Ok("v0.75.0".to_owned()));
        assert_eq!(found.as_deref(), Some("0.75.0"));
        assert_eq!(read(&path), Some(checked(NOW, Some("v0.75.0"))));
    }

    #[test]
    fn a_fresh_cache_answers_without_fetching() {
        let (_dir, path) = cache();
        write(&path, &checked(NOW - 60, Some("v0.75.0"))).unwrap();
        let found = check_daily(&path, "0.74.0", NOW, || panic!("fetched a fresh cache"));
        assert_eq!(found.as_deref(), Some("0.75.0"));
    }

    #[test]
    fn a_failed_check_is_silent_keeps_the_known_tag_and_waits_a_day() {
        let (_dir, path) = cache();
        write(&path, &checked(NOW - DAY, Some("v0.75.0"))).unwrap();
        let found = check_daily(&path, "0.74.0", NOW, || Err("offline".to_owned()));
        assert_eq!(found.as_deref(), Some("0.75.0"));
        assert_eq!(read(&path), Some(checked(NOW, Some("v0.75.0"))));

        let fetched = Cell::new(false);
        let again = check_daily(&path, "0.74.0", NOW + 3_600, || {
            fetched.set(true);
            Err("offline".to_owned())
        });
        assert!(!fetched.get(), "retried within the day");
        assert_eq!(again.as_deref(), Some("0.75.0"));
    }

    #[test]
    fn a_first_check_that_fails_shows_nothing_and_still_records_the_attempt() {
        let (_dir, path) = cache();
        assert_eq!(
            check_daily(&path, "0.74.0", NOW, || Err("offline".to_owned())),
            None
        );
        assert_eq!(read(&path), Some(checked(NOW, None)));
    }

    #[test]
    fn the_cached_tag_is_compared_with_the_running_version_on_every_read() {
        let (_dir, path) = cache();
        write(&path, &checked(NOW, Some("v0.75.0"))).unwrap();
        assert_eq!(cached_newer(&path, "0.74.0").as_deref(), Some("0.75.0"));
        // Upgraded since the check: no line, even though the cache is fresh.
        assert_eq!(cached_newer(&path, "0.75.0"), None);
        assert_eq!(cached_newer(&path, "0.76.0"), None);
    }

    #[test]
    fn a_malformed_or_missing_cache_is_no_cache() {
        let (_dir, path) = cache();
        assert_eq!(cached_newer(&path, "0.74.0"), None);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(read(&path), None);
        let found = check_daily(&path, "0.74.0", NOW, || Ok("v0.75.0".to_owned()));
        assert_eq!(found.as_deref(), Some("0.75.0"));
    }

    #[test]
    fn the_notice_names_the_version_and_the_command() {
        assert_eq!(
            notice_line("0.75.0"),
            "Update available: 0.75.0, run `voisu update`"
        );
    }

    #[test]
    fn the_daemon_checks_only_when_due_idle_and_switched_on() {
        let (_dir, path) = cache();
        let never = || -> Result<String, String> { panic!("fetched") };

        // Off: nothing is read, asked or fetched.
        assert_eq!(
            daemon_tick(Some(&path), false, NOW, || panic!("asked"), never),
            Tick::Off
        );
        assert_eq!(read(&path), None);
        assert_eq!(daemon_tick(None, true, NOW, || true, never), Tick::Off);

        // Due but a Recording runs: no fetch, nothing recorded.
        assert_eq!(
            daemon_tick(Some(&path), true, NOW, || false, never),
            Tick::Busy
        );
        assert_eq!(read(&path), None);

        // Due and idle: fetch once and record.
        assert_eq!(
            daemon_tick(Some(&path), true, NOW, || true, || Ok("0.99.0".to_owned())),
            Tick::Checked
        );
        assert_eq!(read(&path), Some(checked(NOW, Some("0.99.0"))));

        // Fresh: the next ticks that day neither ask nor fetch.
        assert_eq!(
            daemon_tick(Some(&path), true, NOW + 3_600, || panic!("asked"), never),
            Tick::NotDue
        );
    }

    #[test]
    fn a_failed_daemon_check_is_recorded_and_not_retried_that_day() {
        let (_dir, path) = cache();
        let tick = |now| {
            let fetches = Cell::new(0);
            let tick = daemon_tick(
                Some(&path),
                true,
                now,
                || true,
                || {
                    fetches.set(fetches.get() + 1);
                    Err("404".to_owned())
                },
            );
            (tick, fetches.get())
        };
        assert_eq!(tick(NOW), (Tick::Checked, 1));
        assert_eq!(read(&path), Some(checked(NOW, None)));
        assert_eq!(tick(NOW + DAY - 1), (Tick::NotDue, 0));
        assert_eq!(tick(NOW + DAY), (Tick::Checked, 1));
    }
}
