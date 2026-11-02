//! The notification-area icon and its menu: a status line, Settings…,
//! Start at login, Quit.

use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, Wry};

/// Fixed so the icon keeps one identity across launches. Tauri 2 exposes no
/// Shell_NotifyIcon GUID; Windows keys the user's "show/hide icon" choice on
/// the executable path, which the per-user install keeps stable.
pub const TRAY_ID: &str = "voisu";

const MENU_SETTINGS: &str = "settings";
const MENU_AUTOSTART: &str = "autostart";
const MENU_QUIT: &str = "quit";

pub fn status_text(keys_ready: bool) -> &'static str {
    if keys_ready {
        "Voisu: keys saved (dictation comes in the next update)"
    } else {
        "Voisu: setup needed (add your API keys)"
    }
}

/// Menu items the app updates after launch.
pub struct TrayHandles {
    status: MenuItem<Wry>,
    autostart: CheckMenuItem<Wry>,
}

impl TrayHandles {
    pub fn set_ready(&self, keys_ready: bool) {
        if let Err(error) = self.status.set_text(status_text(keys_ready)) {
            tracing::warn!(%error, "tray status update failed");
        }
    }

    pub fn set_autostart(&self, enabled: bool) {
        if let Err(error) = self.autostart.set_checked(enabled) {
            tracing::warn!(%error, "tray autostart update failed");
        }
    }
}

pub fn build(app: &AppHandle, keys_ready: bool, autostart: bool) -> tauri::Result<TrayHandles> {
    let status = MenuItem::with_id(app, "status", status_text(keys_ready), false, None::<&str>)?;
    let settings = MenuItem::with_id(app, MENU_SETTINGS, "Settings…", true, None::<&str>)?;
    let autostart = CheckMenuItem::with_id(
        app,
        MENU_AUTOSTART,
        "Start at login",
        true,
        autostart,
        None::<&str>,
    )?;
    let quit = MenuItem::with_id(app, MENU_QUIT, "Quit Voisu", true, None::<&str>)?;
    let menu = Menu::with_items(
        app,
        &[
            &status,
            &PredefinedMenuItem::separator(app)?,
            &settings,
            &autostart,
            &PredefinedMenuItem::separator(app)?,
            &quit,
        ],
    )?;

    let mut builder = TrayIconBuilder::with_id(TRAY_ID)
        .tooltip("Voisu")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id().as_ref() {
            MENU_SETTINGS => crate::show_settings(app),
            MENU_AUTOSTART => {
                // The menu has already flipped the check mark; apply what it
                // now shows, then let `set_autostart` resync it to the truth.
                let wanted = app
                    .state::<crate::AppState>()
                    .tray
                    .get()
                    .and_then(|tray| tray.autostart.is_checked().ok());
                if let Some(wanted) = wanted {
                    crate::apply_autostart(app, wanted);
                }
            }
            MENU_QUIT => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                crate::show_settings(tray.app_handle());
            }
        });
    if let Some(icon) = app.default_window_icon() {
        builder = builder.icon(icon.clone());
    }
    builder.build(app)?;
    Ok(TrayHandles { status, autostart })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_line_tells_ready_from_setup_needed() {
        assert!(status_text(false).contains("setup needed"));
        assert!(status_text(true).contains("keys saved"));
    }
}
