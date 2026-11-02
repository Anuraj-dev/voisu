//! Voisu for Windows: one process hosting the tray, the setup window and
//! (from W2 on) the Recording → Transcript engine.

mod logging;
mod paths;
pub mod platform;
pub mod settings;
pub mod setup;
mod tray;

use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, RunEvent, WindowEvent};
use tauri_plugin_autostart::ManagerExt as _;
use tauri_plugin_opener::OpenerExt as _;
use voisu_cloud::ProviderHttpClient;
use voisu_core::{Provider, SecretStore};

use platform::secrets::CredentialManagerStore;
use settings::Settings;
use setup::{KeyInputs, KeyResult};

const SETTINGS_WINDOW: &str = "settings";
const STATE_CHANGED: &str = "state-changed";
/// Passed by the HKCU Run entry so a login launch stays in the tray.
const AUTOSTART_ARG: &str = "--autostart";

pub struct AppState {
    settings: Mutex<Settings>,
    tray: Mutex<Option<tray::TrayHandles>>,
}

fn keys_ready() -> bool {
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
    if let Some(tray) = app.state::<AppState>().tray.lock().unwrap().as_ref() {
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
}

#[tauri::command]
fn get_state(app: AppHandle) -> UiState {
    let mut store = CredentialManagerStore;
    UiState {
        groq_saved: store.has_key(Provider::Groq),
        deepgram_saved: store.has_key(Provider::Deepgram),
        autostart: autostart_enabled(&app),
        version: app.package_info().version.to_string(),
        log_dir: paths::log_dir().display().to_string(),
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
    for (provider, credential) in evaluation.to_store {
        store.replace(provider, credential).map_err(|error| {
            tracing::error!(
                diagnostic = error.diagnostic(),
                ?provider,
                "key save failed"
            );
            "Windows Credential Manager refused to save the key.".to_owned()
        })?;
    }
    if saved {
        tracing::info!("keys saved");
        on_keys_saved(&app);
    }
    Ok(SaveOutcome {
        saved,
        groq: evaluation.groq,
        deepgram: evaluation.deepgram,
    })
}

/// After a successful save: refresh the tray and, the first time only,
/// switch Start at login on.
fn on_keys_saved(app: &AppHandle) {
    let state = app.state::<AppState>();
    if let Some(tray) = state.tray.lock().unwrap().as_ref() {
        tray.set_ready(keys_ready());
    }
    let first_setup = {
        let mut settings = state.settings.lock().unwrap();
        let first = !settings.autostart_default_applied;
        if first {
            settings.autostart_default_applied = true;
            if let Err(error) = settings.save(&paths::settings_file()) {
                tracing::warn!(%error, "settings save failed");
            }
        }
        first
    };
    if first_setup {
        apply_autostart(app, true);
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

pub fn run() {
    let _log_guard = logging::init(&paths::log_dir());
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
        .manage(AppState {
            settings: Mutex::new(Settings::load(&paths::settings_file())),
            tray: Mutex::new(None),
        })
        .invoke_handler(tauri::generate_handler![
            get_state,
            save_keys,
            set_autostart,
            open_key_page
        ])
        .setup(|app| {
            let handle = app.handle();
            let ready = keys_ready();
            let tray = tray::build(handle, ready, autostart_enabled(handle))?;
            *app.state::<AppState>().tray.lock().unwrap() = Some(tray);
            let at_login = std::env::args().any(|arg| arg == AUTOSTART_ARG);
            tracing::info!(keys_ready = ready, at_login, "startup");
            if setup::should_open_window(at_login, ready) {
                show_settings(handle);
            }
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

    app.run(|_app, event| {
        // Only the tray's Quit (an explicit exit code) ends the process.
        if let RunEvent::ExitRequested {
            code: None, api, ..
        } = event
        {
            api.prevent_exit();
        }
    });
}
