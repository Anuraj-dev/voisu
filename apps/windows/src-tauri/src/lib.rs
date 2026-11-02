//! Voisu for Windows: one process hosting the tray, the setup window and
//! (from W2 on) the Recording → Transcript engine.

mod logging;
pub mod overlay;
mod paths;
pub mod platform;
pub mod settings;
pub mod setup;
mod tray;

use std::sync::{Mutex, OnceLock};

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
    /// Set once at startup and never locked: menu setters called off the main
    /// thread block until the main thread runs them, so holding a lock across
    /// them could deadlock against a main-thread menu or command handler.
    tray: OnceLock<tray::TrayHandles>,
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

pub fn run() {
    let mut log_guard = logging::init(&paths::log_dir());
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
            tray: OnceLock::new(),
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
            let _ = app.state::<AppState>().tray.set(tray);
            overlay::init(handle)?;
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

    app.run(move |_app, event| match event {
        // Only the tray's Quit (an explicit exit code) ends the process.
        RunEvent::ExitRequested {
            code: None, api, ..
        } => api.prevent_exit(),
        // `run` exits the process without returning on Windows, so the log
        // writer is flushed here rather than by `log_guard` going out of scope.
        RunEvent::Exit => {
            tracing::info!("Voisu exiting");
            drop(log_guard.take());
        }
        _ => {}
    });
}
