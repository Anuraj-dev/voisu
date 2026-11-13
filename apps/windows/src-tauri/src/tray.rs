//! The notification-area icon and its menu: a status line, Settings…,
//! Preview overlay, Test paste, Start at login, Quit. A downloaded update adds
//! a dot to the icon and an "Update to vX" item under the status line.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use tauri::image::Image;
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, Wry};

/// Fixed so the icon keeps one identity across launches. Tauri 2 exposes no
/// Shell_NotifyIcon GUID; Windows keys the user's "show/hide icon" choice on
/// the executable path, which the per-user install keeps stable.
pub const TRAY_ID: &str = "voisu";

const MENU_SETTINGS: &str = "settings";
const MENU_PREVIEW_OVERLAY: &str = "preview-overlay";
const MENU_TEST_PASTE: &str = "test-paste";
const MENU_AUTOSTART: &str = "autostart";
const MENU_QUIT: &str = "quit";
const MENU_UPDATE: &str = "update";
/// Under the status line, above the first separator.
const UPDATE_POSITION: usize = 1;

pub fn status_text(keys_ready: bool, trigger: &str) -> String {
    if keys_ready {
        format!("Voisu: ready (hold {trigger} to dictate)")
    } else {
        "Voisu: setup needed (add your API keys)".to_owned()
    }
}

/// Menu items the app updates after launch.
pub struct TrayHandles {
    status: MenuItem<Wry>,
    /// The Trigger Key's label for the ready line. Never held across a menu
    /// call.
    trigger: Mutex<String>,
    autostart: CheckMenuItem<Wry>,
    menu: Menu<Wry>,
    /// Kept out of the menu until an update is ready.
    update: MenuItem<Wry>,
    update_shown: AtomicBool,
    icon: TrayIcon<Wry>,
}

impl TrayHandles {
    pub fn set_ready(&self, keys_ready: bool) {
        let trigger = self.trigger.lock().unwrap().clone();
        if let Err(error) = self.status.set_text(status_text(keys_ready, &trigger)) {
            tracing::warn!(%error, "tray status update failed");
        }
    }

    /// Names a newly chosen Trigger Key in the ready line.
    pub fn set_trigger(&self, label: String, keys_ready: bool) {
        *self.trigger.lock().unwrap() = label;
        self.set_ready(keys_ready);
    }

    /// Shows a transient engine state such as "recording".
    pub fn set_status(&self, text: &str) {
        if let Err(error) = self.status.set_text(text) {
            tracing::warn!(%error, "tray status update failed");
        }
    }

    pub fn set_autostart(&self, enabled: bool) {
        if let Err(error) = self.autostart.set_checked(enabled) {
            tracing::warn!(%error, "tray autostart update failed");
        }
    }

    /// Marks a downloaded update: the icon dot, the tooltip and the menu item.
    pub fn show_update(&self, version: &str) {
        if let Err(error) = self.update.set_text(update_text(version)) {
            tracing::warn!(%error, "tray update item failed");
        }
        if !self.update_shown.swap(true, Ordering::SeqCst)
            && let Err(error) = self.menu.insert(&self.update, UPDATE_POSITION)
        {
            tracing::warn!(%error, "tray update item failed");
            self.update_shown.store(false, Ordering::SeqCst);
        }
        let tooltip = format!("Voisu: {} ready", update_text(version).to_lowercase());
        if let Err(error) = self.icon.set_tooltip(Some(tooltip)) {
            tracing::warn!(%error, "tray tooltip update failed");
        }
        if let Some(icon) = tray_image(self.icon.app_handle()) {
            let dotted = with_dot(icon.rgba(), icon.width(), icon.height(), TEMPLATE);
            let image = Image::new_owned(dotted, icon.width(), icon.height());
            if let Err(error) = self.icon.set_icon(Some(image)) {
                tracing::warn!(%error, "tray icon update failed");
            }
        }
    }
}

fn update_text(version: &str) -> String {
    format!("Update to v{}", version.trim_start_matches('v'))
}

/// Amber, as on the landing page and the capsule.
const DOT: [u8; 3] = [0xf0, 0xb4, 0x29];
/// Dark ring that keeps the dot visible on light and dark taskbars.
const RING: [u8; 3] = [0x0d, 0x0f, 0x12];

/// Paints an update dot into the top-right corner of an RGBA icon. On a
/// `template` (macOS) icon only alpha counts, so the dot is solid and the
/// ring is a transparent gap that parts it from the bars.
fn with_dot(rgba: &[u8], width: u32, height: u32, template: bool) -> Vec<u8> {
    let mut out = rgba.to_vec();
    let size = width.min(height) as f32;
    let radius = size * 0.2;
    let ring = (size * 0.06).max(1.0);
    let outer = radius + ring;
    let (cx, cy) = (width as f32 - outer, outer);
    for y in 0..height {
        for x in 0..width {
            let distance = ((x as f32 + 0.5 - cx).powi(2) + (y as f32 + 0.5 - cy).powi(2)).sqrt();
            // One pixel of anti-aliasing at each edge.
            let in_outer = (outer - distance + 0.5).clamp(0.0, 1.0);
            if in_outer == 0.0 {
                continue;
            }
            let in_dot = (radius - distance + 0.5).clamp(0.0, 1.0);
            let i = ((y * width + x) * 4) as usize;
            // Straight-alpha "over": a soft edge over a transparent pixel
            // keeps the dot's colour instead of fading towards black.
            let below = out[i + 3] as f32 / 255.0 * (1.0 - in_outer);
            if template {
                out[i..i + 3].fill(0);
                out[i + 3] = ((in_dot + below) * 255.0).round() as u8;
                continue;
            }
            let alpha = in_outer + below;
            for c in 0..3 {
                let colour = RING[c] as f32 * (1.0 - in_dot) + DOT[c] as f32 * in_dot;
                let under = out[i + c] as f32;
                out[i + c] = ((colour * in_outer + under * below) / alpha).round() as u8;
            }
            out[i + 3] = (alpha * 255.0).round() as u8;
        }
    }
    out
}

const MENU_ON_CLICK: bool = cfg!(target_os = "macos");
/// macOS draws a menu bar icon as a template: only its alpha counts, tinted
/// to suit a light or dark bar.
const TEMPLATE: bool = cfg!(target_os = "macos");

/// The tray icon: the app icon on Windows, its template on macOS.
fn tray_image(app: &AppHandle) -> Option<Image<'static>> {
    let icon = app.default_window_icon()?;
    let rgba = if TEMPLATE {
        template(icon.rgba())
    } else {
        icon.rgba().to_vec()
    };
    Some(Image::new_owned(rgba, icon.width(), icon.height()))
}

/// Keeps only the app icon's green bars, in black; the dark tile they sit
/// on drops out. How much greener than red a pixel is says how much of it is
/// bar, so the bars' anti-aliased edges stay soft.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn template(rgba: &[u8]) -> Vec<u8> {
    rgba.as_chunks::<4>()
        .0
        .iter()
        .flat_map(|px| {
            let green = f32::from(px[1].saturating_sub(px[0]));
            let mark = ((green - 16.0) / 80.0).clamp(0.0, 1.0);
            [0, 0, 0, (f32::from(px[3]) * mark).round() as u8]
        })
        .collect()
}

pub fn build(
    app: &AppHandle,
    keys_ready: bool,
    autostart: bool,
    trigger: String,
) -> tauri::Result<TrayHandles> {
    let status = MenuItem::with_id(
        app,
        "status",
        status_text(keys_ready, &trigger),
        false,
        None::<&str>,
    )?;
    let settings = MenuItem::with_id(app, MENU_SETTINGS, "Settings…", true, None::<&str>)?;
    let preview = MenuItem::with_id(
        app,
        MENU_PREVIEW_OVERLAY,
        "Preview overlay",
        true,
        None::<&str>,
    )?;
    let test_paste = MenuItem::with_id(
        app,
        MENU_TEST_PASTE,
        "Test paste (in 3 s)",
        true,
        None::<&str>,
    )?;
    let autostart = CheckMenuItem::with_id(
        app,
        MENU_AUTOSTART,
        "Start at login",
        true,
        autostart,
        None::<&str>,
    )?;
    let quit = MenuItem::with_id(app, MENU_QUIT, "Quit Voisu", true, None::<&str>)?;
    let update = MenuItem::with_id(app, MENU_UPDATE, "Update", true, None::<&str>)?;
    let menu = Menu::with_items(
        app,
        &[
            &status,
            &PredefinedMenuItem::separator(app)?,
            &settings,
            &preview,
            &test_paste,
            &autostart,
            &PredefinedMenuItem::separator(app)?,
            &quit,
        ],
    )?;

    let mut builder = TrayIconBuilder::with_id(TRAY_ID)
        .tooltip("Voisu")
        .menu(&menu)
        // A Mac menu bar item opens its menu on a click; on Windows a left
        // click opens the window and the right click the menu.
        .show_menu_on_left_click(MENU_ON_CLICK)
        .icon_as_template(TEMPLATE)
        .on_menu_event(|app, event| match event.id().as_ref() {
            MENU_SETTINGS => crate::show_settings(app),
            MENU_PREVIEW_OVERLAY => crate::overlay::preview(app),
            MENU_TEST_PASTE => crate::platform::delivery::test_paste(app),
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
            MENU_UPDATE => {
                let _ = crate::updater::install(app, true);
            }
            MENU_QUIT => {
                // A downloaded update installs on the way out, without relaunching.
                let _ = crate::updater::install(app, false);
                app.exit(0);
            }
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
                && !MENU_ON_CLICK
            {
                crate::show_settings(tray.app_handle());
            }
        });
    if let Some(icon) = tray_image(app) {
        builder = builder.icon(icon);
    }
    let icon = builder.build(app)?;
    Ok(TrayHandles {
        status,
        trigger: Mutex::new(trigger),
        autostart,
        menu,
        update,
        update_shown: AtomicBool::new(false),
        icon,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_mac_template_keeps_the_bars_and_drops_the_tile() {
        let bar = [104, 222, 165, 255];
        let tile = [24, 26, 30, 255];
        let clear = [0, 0, 0, 0];
        let half_bar = [104, 222, 165, 128];
        let pixels = [bar, tile, clear, half_bar].concat();
        assert_eq!(
            template(&pixels),
            [[0, 0, 0, 255], [0, 0, 0, 0], [0, 0, 0, 0], [0, 0, 0, 128]].concat()
        );
    }

    #[test]
    fn status_line_tells_ready_from_setup_needed() {
        assert!(status_text(false, "Ctrl+Win").contains("setup needed"));
        assert!(status_text(true, "Ctrl+Win").contains("ready"));
    }

    #[test]
    fn ready_line_names_the_chosen_trigger_key() {
        assert_eq!(
            status_text(true, "Ctrl+`"),
            "Voisu: ready (hold Ctrl+` to dictate)"
        );
    }

    #[test]
    fn update_item_names_the_version_once() {
        assert_eq!(update_text("0.66.0"), "Update to v0.66.0");
        assert_eq!(update_text("v0.66.0"), "Update to v0.66.0");
    }

    #[test]
    fn the_dot_sits_top_right_and_leaves_the_rest_alone() {
        let (w, h) = (32u32, 32u32);
        let clear = vec![0u8; (w * h * 4) as usize];
        let dotted = with_dot(&clear, w, h, false);
        assert_eq!(dotted.len(), clear.len());
        let pixel = |x: u32, y: u32| {
            let i = ((y * w + x) * 4) as usize;
            [dotted[i], dotted[i + 1], dotted[i + 2], dotted[i + 3]]
        };
        // The dot's centre is opaque amber.
        let outer = 32.0 * 0.2 + 32.0 * 0.06;
        let (cx, cy) = ((32.0 - outer) as u32, outer as u32);
        assert_eq!(pixel(cx, cy), [DOT[0], DOT[1], DOT[2], 255]);
        // The opposite corner is untouched.
        assert_eq!(pixel(0, h - 1), [0, 0, 0, 0]);
        assert_eq!(pixel(w / 4, h / 2), [0, 0, 0, 0]);
        // Soft edges over transparency keep the ring's colour, not black.
        for px in dotted.chunks(4).filter(|px| px[3] > 0) {
            for c in 0..3 {
                assert!(px[c] >= RING[c].min(DOT[c]), "edge darkened: {px:?}");
            }
        }
    }

    #[test]
    fn the_mac_dot_is_solid_with_a_transparent_gap_around_it() {
        let (w, h) = (32u32, 32u32);
        let filled = [0, 0, 0, 255].repeat((w * h) as usize);
        let dotted = with_dot(&filled, w, h, true);
        let alpha = |x: u32, y: u32| dotted[((y * w + x) * 4 + 3) as usize];
        let radius = 32.0 * 0.2;
        let outer = radius + 32.0 * 0.06;
        let (cx, cy) = (32.0 - outer, outer);
        // The dot's centre stays opaque; the ring between dot and bars is cut.
        assert_eq!(alpha(cx as u32, cy as u32), 255);
        let gap = (radius + outer) / 2.0;
        assert_eq!(alpha((cx - gap) as u32, cy as u32), 0);
        // The rest of the icon is untouched, and everything stays black.
        assert_eq!(alpha(0, h - 1), 255);
        assert!(dotted.chunks(4).all(|px| px[..3] == [0, 0, 0]));
    }
}
