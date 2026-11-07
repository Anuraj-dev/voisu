//! Self-update from GitHub Releases (W5).
//!
//! Checks the release's `latest.json` at start and every 6 h, downloads a
//! newer installer in the background (the plugin verifies its signature),
//! then marks the tray: a dot on the icon and an "Update to vX" item.
//! Clicking it installs and relaunches; Quit installs without relaunching.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_updater::{Update, UpdaterExt};

/// What `plugins.updater.pubkey` holds until a signing key exists. Builds
/// that still carry it never check, and the release job refuses them.
pub const PUBKEY_UNSET: &str = "VOISU_UPDATER_PUBKEY_UNSET";

const CHECK_EVERY: Duration = Duration::from_secs(6 * 60 * 60);
/// How often the loop wakes to see whether a check is due. Due-ness uses
/// wall-clock time, so a laptop that slept through the 6 h still checks on
/// the next wake.
const TICK: Duration = Duration::from_secs(15 * 60);

/// A downloaded, signature-verified installer waiting for the user.
struct Ready {
    update: Update,
    bytes: Vec<u8>,
}

#[derive(Default)]
pub struct UpdaterState {
    ready: Mutex<Option<Ready>>,
    /// Set by the pre-install hook, which hides windows and drops the tray.
    torn_down: AtomicBool,
}

/// Whether this build carries a real updater public key.
fn configured(app: &AppHandle) -> bool {
    let pubkey = app
        .config()
        .plugins
        .0
        .get("updater")
        .and_then(|updater| updater.get("pubkey"))
        .and_then(|pubkey| pubkey.as_str())
        .unwrap_or_default();
    !pubkey.is_empty() && pubkey != PUBKEY_UNSET
}

pub fn start(app: &AppHandle) {
    if !configured(app) {
        tracing::info!("updates off: this build has no updater public key");
        return;
    }
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let mut last = None;
        loop {
            let now = SystemTime::now();
            // A failed check (often no network yet at login) retries on the
            // next tick instead of waiting out the full interval.
            if is_due(last, now) && check(&app).await {
                last = Some(now);
            }
            tokio::time::sleep(TICK).await;
        }
    });
}

fn is_due(last: Option<SystemTime>, now: SystemTime) -> bool {
    match last {
        None => true,
        // A clock set backwards counts as due rather than stalling checks.
        Some(last) => now
            .duration_since(last)
            .map_or(true, |elapsed| elapsed >= CHECK_EVERY),
    }
}

/// Whether the endpoint answered; a download failure still counts, since
/// the next due check retries it.
async fn check(app: &AppHandle) -> bool {
    let handle = app.clone();
    let updater = app
        .updater_builder()
        // Replaces the plugin's hook: repeats its cleanup and records that
        // the app is torn down, which `install` needs if the installer then
        // fails to start.
        .on_before_exit(move || {
            handle.cleanup_before_exit();
            handle
                .state::<UpdaterState>()
                .torn_down
                .store(true, Ordering::SeqCst);
        })
        .build();
    let found = match updater {
        Ok(updater) => updater.check().await,
        Err(error) => Err(error),
    };
    let update = match found {
        Ok(Some(update)) => update,
        Ok(None) => {
            tracing::info!("up to date");
            return true;
        }
        // Releases carry no macOS build yet, so a Mac finds no entry for
        // itself: that is "no update", not a failure worth retrying.
        Err(
            tauri_plugin_updater::Error::TargetNotFound(_)
            | tauri_plugin_updater::Error::TargetsNotFound(_),
        ) if cfg!(target_os = "macos") => {
            tracing::debug!("no update published for this platform");
            return true;
        }
        Err(error) => {
            tracing::warn!(%error, "update check failed");
            return false;
        }
    };
    let version = update.version.clone();
    if pending_version(app).as_deref() == Some(version.as_str()) {
        return true;
    }
    tracing::info!(%version, "update found; downloading");
    match update.download(|_, _| {}, || {}).await {
        Ok(bytes) => {
            *app.state::<UpdaterState>().ready.lock().unwrap() = Some(Ready { update, bytes });
            tracing::info!(%version, "update ready");
            if let Some(tray) = app.state::<crate::AppState>().tray.get() {
                tray.show_update(&version);
            }
            // The window's update banner reads it from `get_state`.
            let _ = app.emit(crate::STATE_CHANGED, ());
        }
        Err(error) => tracing::warn!(%error, %version, "update download failed"),
    }
    true
}

/// The downloaded update's version, if one waits to be installed.
pub fn pending_version(app: &AppHandle) -> Option<String> {
    let state = app.state::<UpdaterState>();
    let ready = state.ready.lock().unwrap();
    ready.as_ref().map(|ready| ready.update.version.clone())
}

/// Runs the downloaded installer, which ends this process on Windows (on
/// macOS the plugin swaps the app bundle in place). Does
/// nothing when no update is pending. An installer that fails before the
/// app is torn down stays pending so the menu item can retry it; one that
/// fails after leaves no tray, so the app exits and the next launch finds
/// the update again. An error is the message to show.
pub fn install(app: &AppHandle, relaunch: bool) -> Result<(), String> {
    let state = app.state::<UpdaterState>();
    let Some(ready) = state.ready.lock().unwrap().take() else {
        return Ok(());
    };
    tracing::info!(version = %ready.update.version, relaunch, "installing update");
    let update = ready.update.clone().restart_after_install(relaunch);
    if let Err(error) = update.install(&ready.bytes) {
        tracing::warn!(%error, "update install failed");
        if state.torn_down.load(Ordering::SeqCst) {
            app.exit(1);
        } else {
            // A newer download that landed meanwhile wins.
            state.ready.lock().unwrap().get_or_insert(ready);
        }
        return Err(INSTALL_FAILED.to_owned());
    }
    Ok(())
}

const INSTALL_FAILED: &str = "The update couldn't start. Try again, or restart Voisu.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checks_at_start_then_every_six_hours() {
        let start = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        assert!(is_due(None, start));
        assert!(!is_due(Some(start), start + CHECK_EVERY - TICK));
        assert!(is_due(Some(start), start + CHECK_EVERY));
    }

    #[test]
    fn a_clock_set_backwards_checks_again() {
        let start = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        assert!(is_due(Some(start), start - Duration::from_secs(60)));
    }
}
