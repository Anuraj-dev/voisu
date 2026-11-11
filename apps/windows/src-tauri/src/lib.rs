//! Voisu for Windows and macOS: one process hosting the tray, the setup
//! window and the Recording → Transcript engine.

// Pure and tested on every platform; only the Windows and macOS hotkeys and
// capture drive them.
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
mod audio;
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
mod chord;
mod engine;
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
mod gesture;
mod logging;
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod mac_keys;
pub mod overlay;
mod paths;
pub mod platform;
pub mod settings;
pub mod setup;
mod star_prompt;
mod tray;
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
mod trigger;
pub mod trigger_key;
mod updater;
pub mod vocabulary;

use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, RunEvent, WindowEvent};
use tauri_plugin_autostart::ManagerExt as _;
use tauri_plugin_notification::NotificationExt as _;
use tauri_plugin_opener::OpenerExt as _;
use voisu_cloud::ProviderHttpClient;
use voisu_core::{Provider, SecretStore, Tone};

#[cfg(any(windows, target_os = "macos"))]
use platform::hotkey::{is_dead_key, key_label};
use platform::permissions::{self, Pane, Permissions};
use platform::secrets::CredentialManagerStore;
use settings::{OverlaySize, Settings};
use setup::{KeyInputs, KeyResult};
use star_prompt::StarPrompt;
use trigger_key::TriggerChord;

const SETTINGS_WINDOW: &str = "settings";
const STATE_CHANGED: &str = "state-changed";
/// Passed by the HKCU Run entry (the LaunchAgent on macOS) so a login launch
/// stays in the tray.
const AUTOSTART_ARG: &str = "--autostart";

pub struct AppState {
    settings: Mutex<Settings>,
    /// The local dictation count behind the star prompt. Never locked while
    /// `settings` is held.
    star: Mutex<StarPrompt>,
    /// Set once at startup and never locked: menu setters called off the main
    /// thread block until the main thread runs them, so holding a lock across
    /// them could deadlock against a main-thread menu or command handler.
    tray: OnceLock<tray::TrayHandles>,
}

pub(crate) fn keys_ready() -> bool {
    let mut store = CredentialManagerStore;
    store.has_key(Provider::Groq) && store.has_key(Provider::Deepgram)
}

pub(crate) fn show_settings(app: &AppHandle) {
    if let Some(window) = app.get_webview_window(SETTINGS_WINDOW) {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
}

fn autostart_enabled(app: &AppHandle) -> bool {
    app.autolaunch().is_enabled().unwrap_or_else(|error| {
        tracing::warn!(%error, "autostart state unreadable");
        false
    })
}

/// Applies a Start-at-login choice and returns what Windows now reports,
/// keeping the tray check mark and the window in step with it.
pub(crate) fn apply_autostart(app: &AppHandle, enabled: bool) -> bool {
    let manager = app.autolaunch();
    let result = if enabled {
        manager.enable()
    } else {
        manager.disable()
    };
    if let Err(error) = result {
        tracing::warn!(%error, enabled, "autostart change failed");
    }
    let actual = autostart_enabled(app);
    tracing::info!(enabled = actual, "start at login");
    if let Some(tray) = app.state::<AppState>().tray.get() {
        tray.set_autostart(actual);
    }
    let _ = app.emit(STATE_CHANGED, ());
    actual
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct UiState {
    groq_saved: bool,
    deepgram_saved: bool,
    autostart: bool,
    version: String,
    auto_paste: bool,
    overlay_size: OverlaySize,
    boost_words: Vec<String>,
    tone: Tone,
    trigger: UiTrigger,
    /// The only Trigger Keys the window offers, the default first.
    trigger_presets: Vec<UiPreset>,
    /// False when the keyboard hook is down, so the Trigger Key does nothing:
    /// on Windows until Voisu restarts, on macOS until Input Monitoring is
    /// granted.
    hook_ok: bool,
    /// `"mac"` or `"windows"`, for the wording and the permissions list.
    platform: &'static str,
    /// macOS privacy permissions; `None` on Windows.
    permissions: Option<Permissions>,
    /// A downloaded update waiting for "Restart to update".
    update_version: Option<String>,
    /// The Star prompt switch in Settings.
    star_prompt: bool,
    /// Whether the window shows the "Enjoying Voisu?" banner, judged on
    /// today's local date so a closed banner returns after 30 days without
    /// waiting for a dictation.
    show_star: bool,
    /// The dictation under way, if any; later changes arrive as
    /// `recording-state` events.
    phase: Option<engine::PhaseNow>,
}

#[derive(Serialize)]
struct UiPreset {
    /// The written form, as `set_trigger_key` takes it.
    key: String,
    name: &'static str,
}

fn ui_presets() -> Vec<UiPreset> {
    trigger_key::presets()
        .iter()
        .map(|preset| UiPreset {
            key: preset.chord.to_string(),
            name: preset.name,
        })
        .collect()
}

/// The Trigger Key as the settings window shows it.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct UiTrigger {
    /// The written form, as stored and as `set_trigger_key` takes it.
    key: String,
    /// The key named as the active keyboard layout names it.
    label: String,
    warnings: Vec<String>,
    is_default: bool,
}

impl UiTrigger {
    fn of(chord: TriggerChord) -> Self {
        let mut warnings: Vec<String> = chord
            .preset()
            .and_then(|preset| preset.warning)
            .map(String::from)
            .into_iter()
            .collect();
        if chord.key().is_some_and(is_dead_key) {
            warnings.push(
                "On your keyboard layout this key starts an accent, so pressing it outside the shortcut may not type straight away."
                    .to_owned(),
            );
        }
        Self {
            key: chord.to_string(),
            label: trigger_label(chord),
            warnings,
            is_default: chord == TriggerChord::default(),
        }
    }
}

fn trigger_label(chord: TriggerChord) -> String {
    chord.label(chord.key().and_then(key_label).as_deref())
}

/// On Linux there is no active layout to ask: the US names stand.
#[cfg(not(any(windows, target_os = "macos")))]
fn key_label(_vk: u8) -> Option<String> {
    None
}

#[cfg(not(any(windows, target_os = "macos")))]
fn is_dead_key(_vk: u8) -> bool {
    false
}

#[cfg(any(windows, target_os = "macos"))]
fn hook_ok() -> bool {
    platform::hotkey::is_running()
}

/// On Linux there is no hook to lose.
#[cfg(not(any(windows, target_os = "macos")))]
fn hook_ok() -> bool {
    true
}

const PLATFORM: &str = if cfg!(target_os = "macos") {
    "mac"
} else {
    "windows"
};

#[tauri::command]
fn get_state(app: AppHandle) -> UiState {
    let mut store = CredentialManagerStore;
    let settings = app.state::<AppState>().settings.lock().unwrap().clone();
    UiState {
        groq_saved: store.has_key(Provider::Groq),
        deepgram_saved: store.has_key(Provider::Deepgram),
        autostart: autostart_enabled(&app),
        version: app.package_info().version.to_string(),
        auto_paste: settings.auto_paste,
        overlay_size: settings.overlay_size,
        boost_words: settings.boost_words,
        tone: settings.tone,
        trigger: UiTrigger::of(settings.hotkey),
        trigger_presets: ui_presets(),
        hook_ok: hook_ok(),
        platform: PLATFORM,
        permissions: permissions::current(),
        update_version: updater::pending_version(&app),
        star_prompt: settings.star_prompt,
        show_star: app
            .state::<AppState>()
            .star
            .lock()
            .unwrap()
            .show(local_today(), settings.star_prompt),
        phase: engine::current_phase(&app),
    }
}

/// Saves and applies one of the platform's presets without a restart; anything else
/// is refused. An error is the message to show.
#[tauri::command]
fn set_trigger_key(app: AppHandle, key: String) -> Result<UiTrigger, String> {
    let chord = TriggerChord::try_from(key).map_err(|error| error.to_string())?;
    // Applied before saving, so a key that can't take effect is never stored.
    #[cfg(any(windows, target_os = "macos"))]
    if !platform::hotkey::set_chord(chord) {
        return Err(HOOK_DOWN.into());
    }
    update_settings(&app, |settings| settings.hotkey = chord);
    let trigger = UiTrigger::of(chord);
    if let Some(tray) = app.state::<AppState>().tray.get() {
        tray.set_trigger(trigger.label.clone(), keys_ready());
    }
    let _ = app.emit(STATE_CHANGED, ());
    Ok(trigger)
}

#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
const HOOK_DOWN: &str =
    "Voisu's keyboard hook isn't running, so the Trigger Key can't change. Restart Voisu.";

/// Applies `change`, saves, and returns the settings as now held. A failed
/// write is logged; the change still applies for this run.
fn update_settings(app: &AppHandle, change: impl FnOnce(&mut Settings)) -> Settings {
    let state = app.state::<AppState>();
    let mut settings = state.settings.lock().unwrap();
    change(&mut settings);
    if let Err(error) = settings.save(&paths::settings_file()) {
        tracing::warn!(%error, "settings save failed");
    }
    settings.clone()
}

/// Read at each delivery, so it applies from the next Transcript.
#[tauri::command]
fn set_auto_paste(app: AppHandle, enabled: bool) -> bool {
    tracing::info!(enabled, "auto-paste");
    update_settings(&app, |settings| settings.auto_paste = enabled).auto_paste
}

#[tauri::command]
fn set_overlay_size(app: AppHandle, size: OverlaySize) -> OverlaySize {
    tracing::info!(?size, "overlay size");
    let size = update_settings(&app, |settings| settings.overlay_size = size).overlay_size;
    overlay::set_size(size);
    overlay::preview_size(&app);
    size
}

/// Returns the list as stored (normalized), so the window shows what will be
/// used. Read as each Recording starts. Only the count is logged.
#[tauri::command]
fn set_boost_words(app: AppHandle, words: Vec<String>) -> Vec<String> {
    let words = vocabulary::normalize(words);
    tracing::info!(count = words.len(), "boost words saved");
    update_settings(&app, |settings| settings.boost_words = words).boost_words
}

/// Read as each Recording starts, so it applies from the next one.
#[tauri::command]
fn set_tone(app: AppHandle, tone: Tone) -> Tone {
    tracing::info!(tone = tone.as_str(), "tone");
    update_settings(&app, |settings| settings.tone = tone).tone
}

fn local_today() -> chrono::NaiveDate {
    chrono::Local::now().date_naive()
}

/// The Settings switch: off hides the banner and the notification, not the
/// Settings card.
#[tauri::command]
fn set_star_prompt(app: AppHandle, enabled: bool) -> bool {
    tracing::info!(enabled, "star prompt");
    let enabled = update_settings(&app, |settings| settings.star_prompt = enabled).star_prompt;
    let _ = app.emit(STATE_CHANGED, ());
    enabled
}

const REPO_URL: &str = "https://github.com/Anuraj-dev/voisu";

/// "Star on GitHub" on the banner or the Settings card: opens the repository
/// page and records the answer, so the banner never returns. Nothing checks whether a star was
/// given. The answer is kept even if the browser does not open.
#[tauri::command]
fn star_on_github(app: AppHandle) -> Result<(), String> {
    tracing::info!("star prompt answered");
    {
        let state = app.state::<AppState>();
        let mut star = state.star.lock().unwrap();
        star.answer();
        if let Err(error) = star.save(&paths::star_prompt_file()) {
            tracing::warn!(%error, "star prompt save failed");
        }
    }
    let _ = app.emit(STATE_CHANGED, ());
    app.opener()
        .open_url(REPO_URL, None::<&str>)
        .map_err(|_| "Couldn't open the browser.".to_owned())
}

/// The banner's ×: hides it for 30 days the first time, for good the
/// second.
#[tauri::command]
fn close_star_banner(app: AppHandle) {
    {
        let state = app.state::<AppState>();
        let mut star = state.star.lock().unwrap();
        star.close(local_today());
        tracing::info!("star banner closed");
        if let Err(error) = star.save(&paths::star_prompt_file()) {
            tracing::warn!(%error, "star prompt save failed");
        }
    }
    let _ = app.emit(STATE_CHANGED, ());
}

/// Called by the engine after a Transcript was delivered. Counts it toward
/// the star prompt and, for the dictation that crosses the threshold, shows
/// the banner and sends the one notification.
pub(crate) fn dictation_delivered(app: &AppHandle) {
    let enabled = app.state::<AppState>().settings.lock().unwrap().star_prompt;
    let today = local_today();
    let notify = {
        let state = app.state::<AppState>();
        let mut star = state.star.lock().unwrap();
        let before = star.clone();
        let notify = star.record(today, enabled);
        if *star != before
            && let Err(error) = star.save(&paths::star_prompt_file())
        {
            tracing::warn!(%error, "star prompt save failed");
        }
        notify
    };
    if notify {
        tracing::info!("star prompt shown");
        notify_star(app);
        let _ = app.emit(STATE_CHANGED, ());
    }
}

/// The one star notification, sent once ever. It carries no actions: the
/// window's banner holds the two answers. A failure only logs; the banner
/// still shows.
fn notify_star(app: &AppHandle) {
    let shown = app
        .notification()
        .builder()
        .title("Enjoying Voisu?")
        .body("A GitHub star helps other people find it.")
        .show();
    if let Err(error) = shown {
        tracing::warn!(%error, "star notification not shown");
    }
}

#[derive(Serialize)]
struct SaveOutcome {
    saved: bool,
    groq: KeyResult,
    deepgram: KeyResult,
}

#[tauri::command]
async fn save_keys(
    app: AppHandle,
    groq: Option<String>,
    deepgram: Option<String>,
) -> Result<SaveOutcome, String> {
    let mut store = CredentialManagerStore;
    let inputs = KeyInputs {
        groq,
        deepgram,
        groq_saved: store.has_key(Provider::Groq),
        deepgram_saved: store.has_key(Provider::Deepgram),
    };
    let evaluation = setup::evaluate(inputs, |provider, credential| async move {
        ProviderHttpClient.check(provider, credential).await
    })
    .await;
    // Statuses only: the outcome never includes key material.
    tracing::info!(
        groq = ?evaluation.groq.state,
        deepgram = ?evaluation.deepgram.state,
        "key check"
    );

    let saved = !evaluation.to_store.is_empty();
    let mut written = 0;
    let mut failed = None;
    for (provider, credential) in evaluation.to_store {
        if let Err(error) = store.replace(provider, credential) {
            tracing::error!(
                diagnostic = error.diagnostic(),
                ?provider,
                "key save failed"
            );
            failed = Some(provider);
            break;
        }
        written += 1;
    }
    if written > 0 {
        tracing::info!(written, "keys saved");
        on_keys_saved(&app);
    }
    if let Some(provider) = failed {
        let also = if written > 0 {
            " The other key was saved."
        } else {
            ""
        };
        return Err(format!(
            "{KEY_STORE} refused to save the {} key.{also}",
            setup::provider_name(provider)
        ));
    }
    Ok(SaveOutcome {
        saved,
        groq: evaluation.groq,
        deepgram: evaluation.deepgram,
    })
}

const KEY_STORE: &str = if cfg!(target_os = "macos") {
    "The macOS Keychain"
} else {
    "Windows Credential Manager"
};

/// After keys are written: refresh the tray and, once both keys are stored
/// for the first time, switch Start at login on. The default counts as
/// applied only when Windows confirms it, so a failed write retries on the
/// next save.
fn on_keys_saved(app: &AppHandle) {
    let state = app.state::<AppState>();
    let ready = keys_ready();
    if let Some(tray) = state.tray.get() {
        tray.set_ready(ready);
    }
    let pending = ready && !state.settings.lock().unwrap().autostart_default_applied;
    if pending && apply_autostart(app, true) {
        let mut settings = state.settings.lock().unwrap();
        settings.autostart_default_applied = true;
        if let Err(error) = settings.save(&paths::settings_file()) {
            tracing::warn!(%error, "settings save failed");
        }
    }
    let _ = app.emit(STATE_CHANGED, ());
}

#[tauri::command]
fn set_autostart(app: AppHandle, enabled: bool) -> bool {
    apply_autostart(&app, enabled)
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum KeyPage {
    Groq,
    Deepgram,
}

/// Opens a fixed provider page; the webview cannot choose the URL.
#[tauri::command]
fn open_key_page(app: AppHandle, provider: KeyPage) -> Result<(), String> {
    let url = match provider {
        KeyPage::Groq => "https://console.groq.com/keys",
        KeyPage::Deepgram => "https://console.deepgram.com/",
    };
    app.opener()
        .open_url(url, None::<&str>)
        .map_err(|_| "Couldn't open the browser.".to_owned())
}

/// The Windows setup guide on the project site; fixed like the key pages.
const GUIDE_URL: &str = "https://anuraj-dev.github.io/voisu/docs/windows-setup.html";

#[tauri::command]
fn open_guide(app: AppHandle) -> Result<(), String> {
    app.opener()
        .open_url(GUIDE_URL, None::<&str>)
        .map_err(|_| "Couldn't open the browser.".to_owned())
}

/// Opens the local log folder in Explorer or Finder; the logs never leave this PC
/// unless the user sends them (ADR 0006).
#[tauri::command]
fn open_logs(app: AppHandle) -> Result<(), String> {
    let dir = paths::log_dir();
    app.opener()
        .open_path(dir.to_string_lossy(), None::<&str>)
        .map_err(|_| "Couldn't open the logs folder.".to_owned())
}

/// Asks macOS for one privacy permission, then opens its pane of System
/// Settings so the user can switch Voisu on. The pane is one of three fixed
/// links; the webview cannot choose the URL.
#[tauri::command]
fn open_permission_settings(app: AppHandle, pane: Pane) -> Result<(), String> {
    tracing::info!(?pane, "opening privacy settings");
    permissions::request(pane);
    let _ = app.emit(STATE_CHANGED, ());
    app.opener()
        .open_url(pane.url(), None::<&str>)
        .map_err(|_| "Couldn't open System Settings.".to_owned())
}

/// The window's "Restart to update": the tray item's install path.
#[tauri::command]
fn install_update(app: AppHandle) -> Result<(), String> {
    let result = updater::install(&app, true);
    // Reached only when the installer did not start; the window rereads
    // whether the update is still pending.
    let _ = app.emit(STATE_CHANGED, ());
    result
}

/// Wires hook → gesture thread → engine. The engine reports each finished
/// Recording back to the gesture thread, which is created after it, so the
/// sink reads the trigger from a cell filled once both exist.
fn start_dictation(app: &AppHandle) {
    let chord = app.state::<AppState>().settings.lock().unwrap().hotkey;
    let trigger_cell: std::sync::Arc<OnceLock<trigger::Trigger>> = Default::default();
    let sink_cell = std::sync::Arc::clone(&trigger_cell);
    let engine = engine::start(
        app,
        std::sync::Arc::new(move |id| {
            if let Some(trigger) = sink_cell.get() {
                trigger.ended(id);
            }
        }),
    );
    let trigger = match trigger::spawn(engine) {
        Ok(trigger) => trigger,
        Err(error) => {
            tracing::error!(%error, "gesture thread failed to start");
            return;
        }
    };
    let _ = trigger_cell.set(trigger.clone());
    #[cfg(any(windows, target_os = "macos"))]
    {
        // The window's "needs a restart" line follows `hook_ok`.
        let health_app = app.clone();
        let health_changed = move || {
            let _ = health_app.emit(STATE_CHANGED, ());
        };
        if let Err(error) = platform::hotkey::install(trigger, chord, health_changed) {
            tracing::error!(%error, "keyboard hook failed to start");
            let _ = app.emit(STATE_CHANGED, ());
        }
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    drop((trigger, chord));
}

pub fn run() {
    logging::init(&paths::log_dir());
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "Voisu starting");

    let app = tauri::Builder::default()
        // Registered first so a second launch exits before anything else
        // starts, and focuses the running instance's window instead.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            show_settings(app)
        }))
        // Start at login: the HKCU Run key on Windows; on macOS the plugin's
        // default, a LaunchAgent plist in ~/Library/LaunchAgents.
        .plugin(
            tauri_plugin_autostart::Builder::new()
                .app_name("Voisu")
                .arg(AUTOSTART_ARG)
                .build(),
        )
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(updater::UpdaterState::default())
        .manage(AppState {
            settings: Mutex::new(Settings::load(&paths::settings_file())),
            star: Mutex::new(StarPrompt::load(&paths::star_prompt_file())),
            tray: OnceLock::new(),
        })
        .invoke_handler(tauri::generate_handler![
            get_state,
            save_keys,
            set_autostart,
            set_auto_paste,
            set_overlay_size,
            set_boost_words,
            set_tone,
            set_trigger_key,
            open_key_page,
            open_guide,
            open_logs,
            open_permission_settings,
            install_update,
            set_star_prompt,
            star_on_github,
            close_star_banner,
            engine::last_transcript
        ])
        .setup(|app| {
            // A menu bar app: no Dock icon and no app menu; the tray opens
            // the window (Info.plist's LSUIElement says the same at launch).
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);
            let handle = app.handle();
            let ready = keys_ready();
            let chord = app.state::<AppState>().settings.lock().unwrap().hotkey;
            let tray = tray::build(
                handle,
                ready,
                autostart_enabled(handle),
                trigger_label(chord),
            )?;
            let _ = app.state::<AppState>().tray.set(tray);
            overlay::set_size(
                app.state::<AppState>()
                    .settings
                    .lock()
                    .unwrap()
                    .overlay_size,
            );
            overlay::init(handle)?;
            let at_login = std::env::args().any(|arg| arg == AUTOSTART_ARG);
            tracing::info!(keys_ready = ready, at_login, "startup");
            if setup::should_open_window(at_login, ready) {
                show_settings(handle);
            }
            start_dictation(handle);
            updater::start(handle);
            Ok(())
        })
        .on_window_event(|window, event| {
            // Closing the window hides it; Voisu keeps running in the tray.
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .build(tauri::generate_context!())
        .expect("Voisu failed to start");

    app.run(|_app, event| match event {
        // Only the tray's Quit (an explicit exit code) ends the process.
        RunEvent::ExitRequested {
            code: None, api, ..
        } => api.prevent_exit(),
        RunEvent::Exit => tracing::info!("Voisu exiting"),
        _ => {}
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_window_learns_the_trigger_key_its_warnings_and_whether_it_is_the_default() {
        let default = UiTrigger::of(TriggerChord::default());
        let expected = if cfg!(target_os = "macos") {
            "Ctrl+Alt"
        } else {
            "Ctrl+Win"
        };
        assert_eq!(default.key, expected);
        assert!(default.is_default && default.warnings.is_empty());
        let warned = trigger_key::presets()
            .iter()
            .find(|preset| preset.warning.is_some())
            .expect("a preset with a warning");
        let other = UiTrigger::of(warned.chord);
        assert!(!other.is_default);
        assert_eq!(other.warnings[0], warned.warning.unwrap());
    }

    #[test]
    fn the_window_offers_every_preset_default_first_as_set_trigger_key_takes_it() {
        let presets = ui_presets();
        assert_eq!(presets.len(), trigger_key::presets().len());
        assert_eq!(presets[0].key, TriggerChord::default().to_string());
        for preset in &presets {
            let chord = TriggerChord::try_from(preset.key.clone()).expect(&preset.key);
            assert_eq!(UiTrigger::of(chord).key, preset.key);
        }
        let (index, expected) = if cfg!(target_os = "macos") {
            (2, r#"{"key":"Right Win","name":"Right Cmd"}"#)
        } else {
            (3, r#"{"key":"CapsLock","name":"Caps Lock"}"#)
        };
        let json = serde_json::to_string(&presets[index]).unwrap();
        assert_eq!(json, expected);
    }
}
