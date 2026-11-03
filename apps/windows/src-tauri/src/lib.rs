//! Voisu for Windows: one process hosting the tray, the setup window and
//! (from W2 on) the Recording → Transcript engine.

// Pure and tested on every platform; only the Windows hotkey and capture
// drive them.
#[cfg_attr(not(windows), allow(dead_code))]
mod audio;
#[cfg_attr(not(windows), allow(dead_code))]
mod chord;
mod engine;
#[cfg_attr(not(windows), allow(dead_code))]
mod gesture;
mod logging;
pub mod overlay;
mod paths;
pub mod platform;
pub mod settings;
pub mod setup;
mod tray;
#[cfg_attr(not(windows), allow(dead_code))]
mod trigger;
pub mod trigger_key;
mod updater;
pub mod vocabulary;

use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, RunEvent, WindowEvent};
use tauri_plugin_autostart::ManagerExt as _;
use tauri_plugin_opener::OpenerExt as _;
use voisu_cloud::ProviderHttpClient;
use voisu_core::{Provider, SecretStore};

#[cfg(windows)]
use platform::hotkey::{is_dead_key, key_label};
use platform::secrets::CredentialManagerStore;
use settings::{OverlaySize, Settings};
use setup::{KeyInputs, KeyResult};
use trigger_key::TriggerChord;

const SETTINGS_WINDOW: &str = "settings";
const STATE_CHANGED: &str = "state-changed";
/// Shortcut-recorder updates for the settings window.
#[cfg_attr(not(windows), allow(dead_code))]
const TRIGGER_CAPTURE: &str = "trigger-capture";
/// Passed by the HKCU Run entry so a login launch stays in the tray.
const AUTOSTART_ARG: &str = "--autostart";

pub struct AppState {
    settings: Mutex<Settings>,
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
    log_dir: String,
    auto_paste: bool,
    overlay_size: OverlaySize,
    boost_words: Vec<String>,
    trigger: UiTrigger,
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
        let mut warnings: Vec<String> = chord.warning().map(String::from).into_iter().collect();
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

/// Off Windows there is no active layout to ask: the US names stand.
#[cfg(not(windows))]
fn key_label(_vk: u8) -> Option<String> {
    None
}

#[cfg(not(windows))]
fn is_dead_key(_vk: u8) -> bool {
    false
}

#[tauri::command]
fn get_state(app: AppHandle) -> UiState {
    let mut store = CredentialManagerStore;
    let settings = app.state::<AppState>().settings.lock().unwrap().clone();
    UiState {
        groq_saved: store.has_key(Provider::Groq),
        deepgram_saved: store.has_key(Provider::Deepgram),
        autostart: autostart_enabled(&app),
        version: app.package_info().version.to_string(),
        log_dir: paths::log_dir().display().to_string(),
        auto_paste: settings.auto_paste,
        overlay_size: settings.overlay_size,
        boost_words: settings.boost_words,
        trigger: UiTrigger::of(settings.hotkey),
    }
}

/// Validates, saves and applies a new Trigger Key without a restart. An
/// error is the message to show.
#[tauri::command]
fn set_trigger_key(app: AppHandle, key: String) -> Result<UiTrigger, String> {
    let chord = TriggerChord::try_from(key).map_err(|error| error.to_string())?;
    // Applied before saving, so a key that can't take effect is never stored.
    #[cfg(windows)]
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

#[cfg_attr(not(windows), allow(dead_code))]
const HOOK_DOWN: &str =
    "Voisu's keyboard hook isn't running, so the Trigger Key can't change. Restart Voisu.";

/// Starts recording a shortcut; updates arrive as `trigger-capture` events.
#[tauri::command]
fn start_trigger_capture() -> Result<(), String> {
    #[cfg(windows)]
    if !platform::hotkey::capture(true) {
        return Err(HOOK_DOWN.into());
    }
    Ok(())
}

#[tauri::command]
fn stop_trigger_capture() {
    #[cfg(windows)]
    let _ = platform::hotkey::capture(false);
}

#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
enum CaptureEvent {
    Preview {
        label: String,
    },
    /// A usable chord: the window saves it with `set_trigger_key`.
    Done {
        key: String,
        label: String,
    },
    Invalid {
        message: String,
    },
    Cancelled,
}

#[cfg_attr(not(windows), allow(dead_code))]
impl CaptureEvent {
    fn of(capture: chord::Capture) -> Self {
        use chord::Capture;
        match capture {
            Capture::Preview(chord) => Self::Preview {
                label: trigger_label(chord),
            },
            Capture::Done(chord) => match chord.check() {
                Ok(()) => Self::Done {
                    key: chord.to_string(),
                    label: trigger_label(chord),
                },
                Err(rejection) => Self::Invalid {
                    message: rejection.to_string(),
                },
            },
            Capture::Invalid(rejection) => Self::Invalid {
                message: rejection.to_string(),
            },
            Capture::Cancelled => Self::Cancelled,
        }
    }
}

/// Relays recorder updates from the hook thread to the settings window.
#[cfg_attr(not(windows), allow(dead_code))]
fn forward_captures(
    app: &AppHandle,
) -> std::io::Result<std::sync::mpsc::SyncSender<chord::Capture>> {
    let (tx, rx) = std::sync::mpsc::sync_channel(64);
    let app = app.clone();
    std::thread::Builder::new()
        .name("voisu-capture".into())
        .spawn(move || {
            for capture in rx {
                let _ = app.emit(TRIGGER_CAPTURE, CaptureEvent::of(capture));
            }
        })?;
    Ok(tx)
}

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
            "Windows Credential Manager refused to save the {} key.{also}",
            setup::provider_name(provider)
        ));
    }
    Ok(SaveOutcome {
        saved,
        groq: evaluation.groq,
        deepgram: evaluation.deepgram,
    })
}

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
    #[cfg(windows)]
    if let Err(error) = forward_captures(app)
        .and_then(|captures| platform::hotkey::install(trigger, chord, captures))
    {
        tracing::error!(%error, "keyboard hook failed to start");
    }
    #[cfg(not(windows))]
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
        .plugin(
            tauri_plugin_autostart::Builder::new()
                .app_name("Voisu")
                .arg(AUTOSTART_ARG)
                .build(),
        )
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(updater::UpdaterState::default())
        .manage(AppState {
            settings: Mutex::new(Settings::load(&paths::settings_file())),
            tray: OnceLock::new(),
        })
        .invoke_handler(tauri::generate_handler![
            get_state,
            save_keys,
            set_autostart,
            set_auto_paste,
            set_overlay_size,
            set_boost_words,
            set_trigger_key,
            start_trigger_capture,
            stop_trigger_capture,
            open_key_page,
            engine::last_transcript
        ])
        .setup(|app| {
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
    use chord::Capture;

    #[test]
    fn a_recorded_chord_is_checked_before_the_window_saves_it() {
        let ctrl_c: TriggerChord = "Ctrl+C".parse().unwrap();
        assert!(matches!(
            CaptureEvent::of(Capture::Done(ctrl_c)),
            CaptureEvent::Invalid { .. }
        ));
        let ctrl_tick: TriggerChord = "Ctrl+`".parse().unwrap();
        assert_eq!(
            CaptureEvent::of(Capture::Done(ctrl_tick)),
            CaptureEvent::Done {
                key: "Ctrl+`".into(),
                label: "Ctrl+`".into()
            }
        );
    }

    #[test]
    fn the_window_learns_the_trigger_key_its_warnings_and_whether_it_is_the_default() {
        let default = UiTrigger::of(TriggerChord::default());
        assert_eq!(default.key, "Ctrl+Win");
        assert!(default.is_default && default.warnings.is_empty());
        let tick = UiTrigger::of("Ctrl+`".parse().unwrap());
        assert!(!tick.is_default);
        assert!(tick.warnings[0].contains("VS Code"), "{:?}", tick.warnings);
    }

    #[test]
    fn every_preset_the_window_offers_is_allowed_and_stored_as_written() {
        // Read from the window itself, so the two can't drift apart.
        const APP: &str = include_str!("../../ui/App.svelte");
        let after = |marker: &str| APP.split_once(marker).expect(marker).1;
        let default = after("const DEFAULT_TRIGGER = '")
            .split_once('\'')
            .unwrap()
            .0;
        assert_eq!(default, TriggerChord::default().to_string());
        let list = after("const TRIGGER_PRESETS = [")
            .split_once("];")
            .unwrap()
            .0;
        let presets: Vec<&str> = list
            .split("key: '")
            .skip(1)
            .map(|rest| rest.split_once('\'').unwrap().0)
            .collect();
        assert!(
            presets.len() >= 5 && presets.contains(&default),
            "{presets:?}"
        );
        for preset in presets {
            let chord = TriggerChord::try_from(preset.to_owned()).expect(preset);
            assert_eq!(UiTrigger::of(chord).key, preset);
        }
    }

    #[test]
    fn capture_events_reach_the_window_tagged_by_kind() {
        let json = serde_json::to_string(&CaptureEvent::Cancelled).unwrap();
        assert_eq!(json, r#"{"kind":"cancelled"}"#);
    }
}
