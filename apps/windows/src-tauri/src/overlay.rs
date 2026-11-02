//! The recording Overlay: a small capsule with live level bars at the bottom
//! centre of the active monitor while Voisu records.
//!
//! The window is created once at startup and never takes focus, never appears
//! in the taskbar or Alt+Tab, and lets clicks through. It is driven only by the
//! W2 events below; it never feeds anything back into Recording.
//!
//! Visibility is native (Win32), not Tauri's `show()`/`hide()`: in tao 0.37,
//! `show()` recomputes `GWL_EXSTYLE` from tao's own flags (dropping
//! `WS_EX_TOOLWINDOW`) and calls `ShowWindow(SW_SHOW)`, which activates the
//! window. `SetWindowPos(.., SWP_NOACTIVATE | SWP_SHOWWINDOW)` does not.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Listener, Manager, WebviewUrl, WebviewWindowBuilder};

pub const OVERLAY_WINDOW: &str = "overlay";
/// W2 → W4 contract: emitted on every Recording lifecycle change.
pub const RECORDING_STATE: &str = "recording-state";
/// W2 → W4 contract: at most 30 Hz, only while recording or hands-free.
pub const AUDIO_LEVEL: &str = "audio-level";

/// DESIGN.md capsule, in logical pixels.
const CAPSULE_WIDTH: f64 = 280.0;
const CAPSULE_HEIGHT: f64 = 64.0;
/// Transparent room around the capsule for the error state's 8 px glow.
const GLOW_PAD: f64 = 8.0;
/// DESIGN.md outer margin between the capsule and the work-area bottom.
const BOTTOM_MARGIN: f64 = 24.0;
/// Long enough to read a one-line error, then the capsule hides.
const ERROR_DISPLAY: Duration = Duration::from_secs(3);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RecordingState {
    Idle,
    Recording,
    HandsFree,
    Processing,
    Error,
}

/// `recording-state` payload. `message` is set only for `error`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RecordingStatePayload {
    pub state: RecordingState,
    #[serde(default)]
    pub message: Option<String>,
}

/// `audio-level` payload: 0.0–1.0, perceptual.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct AudioLevelPayload {
    pub level: f32,
}

/// What the native window should do after a state change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// Show (or keep showing) on top. `place` is true only on the hidden →
    /// visible edge, so the capsule never jumps monitors mid-Recording.
    Show {
        place: bool,
    },
    Hide,
    Nothing,
}

/// Pure visibility logic: which states show the capsule, and which pending
/// error-hide timer is still current.
#[derive(Debug, Default)]
pub struct Presenter {
    visible: bool,
    generation: u64,
}

impl Presenter {
    /// Returns the window step and, for `error`, the generation a delayed
    /// [`Presenter::on_hide_timer`] call must carry to still apply.
    pub fn on_state(&mut self, state: RecordingState) -> (Step, Option<u64>) {
        // Any new state supersedes a pending error hide.
        self.generation += 1;
        if state == RecordingState::Idle {
            return (self.hide(), None);
        }
        let place = !self.visible;
        self.visible = true;
        let timer = (state == RecordingState::Error).then_some(self.generation);
        (Step::Show { place }, timer)
    }

    pub fn on_hide_timer(&mut self, generation: u64) -> Step {
        if generation == self.generation {
            self.hide()
        } else {
            Step::Nothing
        }
    }

    fn hide(&mut self) -> Step {
        if std::mem::take(&mut self.visible) {
            Step::Hide
        } else {
            Step::Nothing
        }
    }
}

/// A screen rectangle in physical pixels (Win32 `RECT` layout).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

/// Where the overlay window goes on a monitor: centred horizontally, with the
/// capsule's bottom edge `BOTTOM_MARGIN` above the work area. `scale` is the
/// monitor's effective DPI / 96.
pub fn placement(work: Rect, scale: f64) -> Rect {
    let px = |logical: f64| (logical * scale).round() as i32;
    let width = px(CAPSULE_WIDTH + 2.0 * GLOW_PAD);
    let height = px(CAPSULE_HEIGHT + 2.0 * GLOW_PAD);
    let left = work.left + (work.right - work.left - width) / 2;
    let bottom = work.bottom - px(BOTTOM_MARGIN - GLOW_PAD);
    Rect {
        left,
        top: bottom - height,
        right: left + width,
        bottom,
    }
}

/// Speech-like fake level for "Preview overlay": syllable bumps (~4 Hz) under
/// a slower phrase envelope, with a short pause.
pub fn preview_level(seconds: f64) -> f32 {
    use std::f64::consts::TAU;
    let syllable = (seconds * 4.0 * TAU).sin().abs();
    let phrase = 0.55 + 0.45 * (seconds * 0.9 * TAU).sin();
    let pause = if (1.3..1.6).contains(&seconds) {
        0.05
    } else {
        1.0
    };
    (syllable * phrase * pause).clamp(0.0, 1.0) as f32
}

/// Creates the hidden overlay window and starts listening for `recording-state`.
pub fn init(app: &AppHandle) -> tauri::Result<()> {
    let window = WebviewWindowBuilder::new(
        app,
        OVERLAY_WINDOW,
        WebviewUrl::App("overlay/overlay.html".into()),
    )
    .title("Voisu overlay")
    .inner_size(
        CAPSULE_WIDTH + 2.0 * GLOW_PAD,
        CAPSULE_HEIGHT + 2.0 * GLOW_PAD,
    )
    .decorations(false)
    .transparent(true)
    .shadow(false)
    .resizable(false)
    .always_on_top(true)
    .skip_taskbar(true)
    .focusable(false)
    .focused(false)
    .visible(false)
    .build()?;
    // tao maps this to WS_EX_TRANSPARENT | WS_EX_LAYERED: hit-testing passes
    // through to the window underneath.
    window.set_ignore_cursor_events(true)?;

    let presenter = Arc::new(Mutex::new(Presenter::default()));
    let handle = app.clone();
    app.listen(RECORDING_STATE, move |event| {
        match serde_json::from_str::<RecordingStatePayload>(event.payload()) {
            Ok(payload) => on_state(&handle, &presenter, payload.state),
            Err(error) => tracing::warn!(%error, "overlay ignored a recording-state payload"),
        }
    });
    Ok(())
}

fn on_state(app: &AppHandle, presenter: &Arc<Mutex<Presenter>>, state: RecordingState) {
    let (step, timer) = presenter.lock().unwrap().on_state(state);
    apply(app, step);
    if let Some(generation) = timer {
        let app = app.clone();
        let presenter = Arc::clone(presenter);
        std::thread::spawn(move || {
            std::thread::sleep(ERROR_DISPLAY);
            let step = presenter.lock().unwrap().on_hide_timer(generation);
            apply(&app, step);
        });
    }
}

/// Runs the native call on the main (window-owning) thread. Posting keeps the
/// emitter, which may be W2's engine thread, from ever blocking on the UI.
fn apply(app: &AppHandle, step: Step) {
    if step == Step::Nothing {
        return;
    }
    let handle = app.clone();
    let posted = app.run_on_main_thread(move || {
        let Some(window) = handle.get_webview_window(OVERLAY_WINDOW) else {
            return;
        };
        match step {
            Step::Show { place } => native::show(&window, place),
            Step::Hide => native::hide(&window),
            Step::Nothing => {}
        }
    });
    if let Err(error) = posted {
        tracing::warn!(%error, "overlay update not posted");
    }
}

static PREVIEW_RUNNING: AtomicBool = AtomicBool::new(false);

/// Tray diagnostic: plays recording (~3 s of fake levels) → processing (~1 s)
/// → idle through the same events W2 emits. A second click while one runs is
/// ignored.
pub fn preview(app: &AppHandle) {
    if PREVIEW_RUNNING.swap(true, Ordering::SeqCst) {
        return;
    }
    tracing::info!("overlay preview");
    let app = app.clone();
    std::thread::spawn(move || {
        let state = |state| {
            let payload = RecordingStatePayload {
                state,
                message: None,
            };
            let _ = app.emit(RECORDING_STATE, payload);
        };
        state(RecordingState::Recording);
        let started = Instant::now();
        while started.elapsed() < Duration::from_secs(3) {
            let level = preview_level(started.elapsed().as_secs_f64());
            let _ = app.emit(AUDIO_LEVEL, AudioLevelPayload { level });
            std::thread::sleep(Duration::from_millis(33));
        }
        state(RecordingState::Processing);
        std::thread::sleep(Duration::from_secs(1));
        state(RecordingState::Idle);
        PREVIEW_RUNNING.store(false, Ordering::SeqCst);
    });
}

#[cfg(windows)]
mod native {
    use tauri::WebviewWindow;
    use windows::Win32::Foundation::{COLORREF, HWND, POINT};
    use windows::Win32::Graphics::Gdi::{
        GetMonitorInfoW, HMONITOR, MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTONULL, MONITORINFO,
        MonitorFromPoint, MonitorFromWindow,
    };
    use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
    use windows::Win32::UI::WindowsAndMessaging::{
        GWL_EXSTYLE, GetCursorPos, GetForegroundWindow, GetWindowLongPtrW, HWND_TOPMOST, LWA_ALPHA,
        SW_HIDE, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER,
        SWP_SHOWWINDOW, SetLayeredWindowAttributes, SetWindowLongPtrW, SetWindowPos, ShowWindow,
        WS_EX_APPWINDOW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TRANSPARENT,
    };

    use super::{Rect, placement};

    pub fn show(window: &WebviewWindow, place: bool) {
        let Ok(hwnd) = window.hwnd() else { return };
        assert_styles(hwnd);
        let target = if place { target_rect() } else { None };
        // SAFETY: plain Win32 calls on our own live top-level window, made on
        // the thread that owns it.
        unsafe {
            if let Some(r) = target {
                let (x, y, w, h) = (r.left, r.top, r.right - r.left, r.bottom - r.top);
                // Moving onto a monitor with another DPI sends WM_DPICHANGED,
                // which tao answers by resizing. Setting the rect a second
                // time makes ours the final one.
                let _ = SetWindowPos(hwnd, None, x, y, w, h, SWP_NOACTIVATE | SWP_NOZORDER);
                let _ = SetWindowPos(
                    hwnd,
                    Some(HWND_TOPMOST),
                    x,
                    y,
                    w,
                    h,
                    SWP_NOACTIVATE | SWP_SHOWWINDOW | SWP_FRAMECHANGED,
                );
            } else {
                // Re-assert topmost each time so the capsule rises above a
                // fullscreen-windowed app that went topmost after us.
                let _ = SetWindowPos(
                    hwnd,
                    Some(HWND_TOPMOST),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW | SWP_FRAMECHANGED,
                );
            }
        }
    }

    pub fn hide(window: &WebviewWindow) {
        if let Ok(hwnd) = window.hwnd() {
            // SAFETY: hiding our own window; SW_HIDE never activates anything.
            unsafe {
                let _ = ShowWindow(hwnd, SW_HIDE);
            }
        }
    }

    /// No activation, no taskbar button or Alt+Tab entry, click-through.
    /// Re-applied before every show because tao rewrites GWL_EXSTYLE from its
    /// own flags whenever one of them changes, and it has no tool-window flag.
    /// The showing `SetWindowPos` carries SWP_FRAMECHANGED so the change lands.
    fn assert_styles(hwnd: HWND) {
        let add = (WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW | WS_EX_TRANSPARENT | WS_EX_LAYERED).0;
        // SAFETY: reading and writing our own window's extended style.
        unsafe {
            let current = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
            let wanted = (current | add as isize) & !(WS_EX_APPWINDOW.0 as isize);
            if wanted != current {
                SetWindowLongPtrW(hwnd, GWL_EXSTYLE, wanted);
            }
            // A layered window stays invisible until its layering is set, and
            // tao (which adds WS_EX_LAYERED for click-through) never sets it.
            // Full alpha keeps the page's own per-pixel transparency.
            if let Err(error) = SetLayeredWindowAttributes(hwnd, COLORREF(0), 255, LWA_ALPHA) {
                tracing::warn!(%error, "overlay layering not set");
            }
        }
    }

    /// The monitor holding the foreground window, else the cursor's monitor.
    fn target_rect() -> Option<Rect> {
        // SAFETY: read-only Win32 queries with valid out-pointers.
        unsafe {
            let mut monitor = HMONITOR::default();
            let foreground = GetForegroundWindow();
            if !foreground.0.is_null() {
                monitor = MonitorFromWindow(foreground, MONITOR_DEFAULTTONULL);
            }
            if monitor.0.is_null() {
                let mut cursor = POINT::default();
                let _ = GetCursorPos(&mut cursor);
                monitor = MonitorFromPoint(cursor, MONITOR_DEFAULTTONEAREST);
            }
            let mut info = MONITORINFO {
                cbSize: size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            if !GetMonitorInfoW(monitor, &mut info).as_bool() {
                return None;
            }
            let (mut dpi_x, mut dpi_y) = (96, 96);
            if GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y).is_err() {
                dpi_x = 96;
            }
            let work = info.rcWork;
            Some(placement(
                Rect {
                    left: work.left,
                    top: work.top,
                    right: work.right,
                    bottom: work.bottom,
                },
                f64::from(dpi_x) / 96.0,
            ))
        }
    }
}

/// Non-Windows builds exist only for local checks and tests.
#[cfg(not(windows))]
mod native {
    use tauri::WebviewWindow;

    pub fn show(window: &WebviewWindow, _place: bool) {
        let _ = window.show();
    }

    pub fn hide(window: &WebviewWindow) {
        let _ = window.hide();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state_json(json: &str) -> Result<RecordingStatePayload, serde_json::Error> {
        serde_json::from_str(json)
    }

    #[test]
    fn parses_the_w2_contract_payloads() {
        let hands_free = state_json(r#"{"state":"handsFree","message":null}"#).unwrap();
        assert_eq!(hands_free.state, RecordingState::HandsFree);
        let error = state_json(r#"{"state":"error","message":"Mic blocked"}"#).unwrap();
        assert_eq!(error.message.as_deref(), Some("Mic blocked"));
        assert_eq!(state_json(r#"{"state":"idle"}"#).unwrap().message, None);
        assert!(state_json(r#"{"state":"paused","message":null}"#).is_err());
        let level: AudioLevelPayload = serde_json::from_str(r#"{"level":0.5}"#).unwrap();
        assert_eq!(level.level, 0.5);
    }

    #[test]
    fn places_only_on_the_hidden_to_visible_edge() {
        let mut presenter = Presenter::default();
        assert_eq!(
            presenter.on_state(RecordingState::Recording),
            (Step::Show { place: true }, None)
        );
        assert_eq!(
            presenter.on_state(RecordingState::HandsFree).0,
            Step::Show { place: false }
        );
        assert_eq!(
            presenter.on_state(RecordingState::Processing).0,
            Step::Show { place: false }
        );
        assert_eq!(presenter.on_state(RecordingState::Idle).0, Step::Hide);
        assert_eq!(presenter.on_state(RecordingState::Idle).0, Step::Nothing);
        assert_eq!(
            presenter.on_state(RecordingState::Recording).0,
            Step::Show { place: true }
        );
    }

    #[test]
    fn error_hides_after_its_timer_unless_a_newer_state_arrived() {
        let mut presenter = Presenter::default();
        let (step, timer) = presenter.on_state(RecordingState::Error);
        assert_eq!(step, Step::Show { place: true });
        assert_eq!(presenter.on_hide_timer(timer.unwrap()), Step::Hide);

        let (_, stale) = presenter.on_state(RecordingState::Error);
        presenter.on_state(RecordingState::Recording);
        assert_eq!(presenter.on_hide_timer(stale.unwrap()), Step::Nothing);
        assert_eq!(
            presenter.on_state(RecordingState::Processing).0,
            Step::Show { place: false }
        );
    }

    #[test]
    fn places_bottom_centre_of_the_work_area_at_any_scale() {
        // 1920x1040 work area (taskbar excluded) at 100%: 296x80 window,
        // capsule bottom 24 px above the work area.
        let work = Rect {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1040,
        };
        assert_eq!(
            placement(work, 1.0),
            Rect {
                left: 812,
                top: 944,
                right: 1108,
                bottom: 1024
            }
        );
        // A second monitor left of the primary at 150%.
        let left_monitor = Rect {
            left: -2560,
            top: 0,
            right: 0,
            bottom: 1392,
        };
        let r = placement(left_monitor, 1.5);
        assert_eq!((r.right - r.left, r.bottom - r.top), (444, 120));
        assert_eq!(r.left - left_monitor.left, left_monitor.right - r.right);
        assert_eq!(left_monitor.bottom - r.bottom, 24);
    }

    #[test]
    fn preview_levels_stay_in_range_and_move() {
        let levels: Vec<f32> = (0..90)
            .map(|i| preview_level(f64::from(i) / 30.0))
            .collect();
        assert!(levels.iter().all(|l| (0.0..=1.0).contains(l)));
        let max = levels.iter().copied().fold(0.0, f32::max);
        assert!(max > 0.6, "preview should reach loud levels, got {max}");
        assert!(
            levels.iter().any(|&l| l < 0.1),
            "preview should have quiet frames"
        );
    }
}
