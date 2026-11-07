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

use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Listener, Manager, WebviewUrl, WebviewWindowBuilder};

use crate::settings::OverlaySize;

pub const OVERLAY_WINDOW: &str = "overlay";
/// W2 → W4 contract: emitted on every Recording lifecycle change.
pub const RECORDING_STATE: &str = "recording-state";
/// W2 → W4 contract: at most 30 Hz, only while recording or hands-free.
pub const AUDIO_LEVEL: &str = "audio-level";

/// DESIGN.md capsule at the medium size, in logical pixels. The page scales
/// itself from the window width it is given (overlay.js `BASE_WIDTH`).
const CAPSULE_WIDTH: f64 = 280.0;
const CAPSULE_HEIGHT: f64 = 64.0;
/// Transparent room around the capsule for the error state's 8 px glow.
const GLOW_PAD: f64 = 8.0;
/// DESIGN.md outer margin between the capsule and the work-area bottom.
const BOTTOM_MARGIN: f64 = 24.0;
/// Long enough to read a one-line error, then the capsule hides.
const ERROR_DISPLAY: Duration = Duration::from_secs(3);
/// The Delivered check mark, as brief as the Linux overlay's: the Transcript
/// is already in the focused app, so the confirmation only needs to register.
const DONE_DISPLAY: Duration = Duration::from_millis(600);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RecordingState {
    Idle,
    Recording,
    HandsFree,
    Processing,
    /// The transcript was delivered: a brief check mark, then hidden.
    Done,
    Error,
}

impl RecordingState {
    /// How long a terminal state stays up on its own before the capsule hides.
    fn display_time(self) -> Option<Duration> {
        match self {
            Self::Done => Some(DONE_DISPLAY),
            Self::Error => Some(ERROR_DISPLAY),
            Self::Idle | Self::Recording | Self::HandsFree | Self::Processing => None,
        }
    }
}

/// `recording-state` payload. `message` is set for `error`, and for `done`
/// when the transcript was only copied (the overlay's accessible label).
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
/// hide timer (after `done` or `error`) is still current.
#[derive(Debug, Default)]
pub struct Presenter {
    visible: bool,
    generation: u64,
    terminal_showing: bool,
}

impl Presenter {
    /// Returns the window step and, for `done` and `error`, the generation a
    /// delayed [`Presenter::on_hide_timer`] call must carry to still apply.
    pub fn on_state(&mut self, state: RecordingState) -> (Step, Option<u64>) {
        // An `idle` right after `done` or `error` (the preview ends that way)
        // leaves the check mark or message up until its own timer hides it.
        if state == RecordingState::Idle && self.terminal_showing {
            return (Step::Nothing, None);
        }
        // Any other new state supersedes a pending hide.
        self.generation += 1;
        self.terminal_showing = state.display_time().is_some();
        if state == RecordingState::Idle {
            return (self.hide(), None);
        }
        let place = !self.visible;
        self.visible = true;
        let timer = self.terminal_showing.then_some(self.generation);
        (Step::Show { place }, timer)
    }

    pub fn on_hide_timer(&mut self, generation: u64) -> Step {
        if generation == self.generation {
            self.terminal_showing = false;
            self.hide()
        } else {
            Step::Nothing
        }
    }

    pub fn is_visible(&self) -> bool {
        self.visible
    }

    /// Visible for a Recording in progress, not just a check mark or error
    /// waiting out its timer.
    pub fn is_live(&self) -> bool {
        self.visible && !self.terminal_showing
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
/// capsule's bottom edge `BOTTOM_MARGIN` above the work area. `dpi_scale` is
/// the monitor's effective DPI / 96. The capsule and its glow room grow with
/// `size`; the margin does not.
pub fn placement(work: Rect, dpi_scale: f64, size: OverlaySize) -> Rect {
    let px = |logical: f64| (logical * dpi_scale).round() as i32;
    let s = size.scale();
    let width = px((CAPSULE_WIDTH + 2.0 * GLOW_PAD) * s);
    let height = px((CAPSULE_HEIGHT + 2.0 * GLOW_PAD) * s);
    let left = work.left + (work.right - work.left - width) / 2;
    let bottom = work.bottom - px(BOTTOM_MARGIN - GLOW_PAD * s);
    Rect {
        left,
        top: bottom - height,
        right: left + width,
        bottom,
    }
}

/// A rectangle in AppKit points: origin at the bottom left, y growing up.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MacFrame {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// The overlay's frame on a Mac screen whose visible frame (below the menu
/// bar, above the Dock) is `visible`. [`placement`] with y flipped: AppKit
/// works in points, so the backing scale needs no part in it.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn mac_placement(visible: MacFrame, size: OverlaySize) -> MacFrame {
    let work = Rect {
        left: visible.x.round() as i32,
        top: -(visible.y + visible.height).round() as i32,
        right: (visible.x + visible.width).round() as i32,
        bottom: -visible.y.round() as i32,
    };
    let r = placement(work, 1.0, size);
    MacFrame {
        x: f64::from(r.left),
        y: f64::from(-r.bottom),
        width: f64::from(r.right - r.left),
        height: f64::from(r.bottom - r.top),
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

/// The size the next show places the capsule at. Read on the main thread at
/// each hidden → visible edge, so a change applies from the next Recording.
static SIZE: Mutex<OverlaySize> = Mutex::new(OverlaySize::Medium);

pub fn set_size(size: OverlaySize) {
    *SIZE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = size;
}

/// Only the Windows and macOS placements read it.
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
fn size() -> OverlaySize {
    *SIZE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The app's one [`Presenter`]. It is only touched on the main thread, so
/// decisions and the native calls they produce keep the order events arrived.
struct OverlayPresenter(Mutex<Presenter>);

/// Creates the hidden overlay window and starts listening for `recording-state`.
pub fn init(app: &AppHandle) -> tauri::Result<()> {
    app.manage(OverlayPresenter(Mutex::new(Presenter::default())));
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

    let handle = app.clone();
    app.listen(RECORDING_STATE, move |event| {
        match serde_json::from_str::<RecordingStatePayload>(event.payload()) {
            Ok(payload) => on_main_thread(&handle, move |app| on_state(app, payload.state)),
            Err(error) => tracing::warn!(%error, "overlay ignored a recording-state payload"),
        }
    });
    Ok(())
}

/// Queues `work` on the main (window-owning) thread. The queue is FIFO, so
/// state changes apply in the order they were posted, and the emitter (W2's
/// engine thread, or the error timer) never blocks on the UI.
fn on_main_thread(app: &AppHandle, work: impl FnOnce(&AppHandle) + Send + 'static) {
    let handle = app.clone();
    if let Err(error) = app.run_on_main_thread(move || work(&handle)) {
        tracing::warn!(%error, "overlay update not posted");
    }
}

fn on_state(app: &AppHandle, state: RecordingState) {
    let (step, timer) = presenter(app).on_state(state);
    apply(app, step);
    if let (Some(generation), Some(delay)) = (timer, state.display_time()) {
        let app = app.clone();
        std::thread::spawn(move || {
            std::thread::sleep(delay);
            on_main_thread(&app, move |app| {
                let step = presenter(app).on_hide_timer(generation);
                apply(app, step);
            });
        });
    }
}

fn presenter(app: &AppHandle) -> std::sync::MutexGuard<'_, Presenter> {
    let state = app.state::<OverlayPresenter>().inner();
    state
        .0
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Main thread only.
fn apply(app: &AppHandle, step: Step) {
    let Some(window) = app.get_webview_window(OVERLAY_WINDOW) else {
        return;
    };
    match step {
        Step::Show { place } => native::show(&window, place),
        Step::Hide => native::hide(&window),
        Step::Nothing => {}
    }
}

/// Generation of the running tray preview. A real Recording bumps it, and
/// the preview emits only while holding this lock with its own generation
/// current, so none of its fake events can land after the real `recording`.
static PREVIEW: Mutex<PreviewGate> = Mutex::new(PreviewGate::new());

/// Pure state behind [`PREVIEW`].
#[derive(Debug)]
struct PreviewGate {
    generation: u64,
    running: bool,
}

impl PreviewGate {
    const fn new() -> Self {
        Self {
            generation: 0,
            running: false,
        }
    }

    /// Starts a preview and returns its token, or `None` if one is running.
    fn begin(&mut self) -> Option<u64> {
        if self.running {
            return None;
        }
        self.running = true;
        self.generation += 1;
        Some(self.generation)
    }

    /// Whether the preview holding `token` may still emit.
    fn is_current(&self, token: u64) -> bool {
        self.running && self.generation == token
    }

    /// Ends whatever preview is running; its token goes stale.
    fn end(&mut self) {
        self.running = false;
        self.generation += 1;
    }

    /// Ends the preview holding `token`, unless it was already superseded.
    fn finish(&mut self, token: u64) {
        if self.is_current(token) {
            self.running = false;
        }
    }
}

fn preview_gate() -> std::sync::MutexGuard<'static, PreviewGate> {
    PREVIEW
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Called by the engine as a real Recording starts: stops any preview.
pub fn end_preview() {
    preview_gate().end();
}

/// Emits only while the preview holding `token` is current; false once a
/// real Recording has taken over.
fn emit_preview<S: Serialize + Clone>(
    app: &AppHandle,
    token: u64,
    event: &str,
    payload: S,
) -> bool {
    let gate = preview_gate();
    if !gate.is_current(token) {
        return false;
    }
    let _ = app.emit(event, payload);
    true
}

fn play_preview(app: &AppHandle, token: u64) -> bool {
    let state = |state| {
        let payload = RecordingStatePayload {
            state,
            message: None,
        };
        emit_preview(app, token, RECORDING_STATE, payload)
    };
    if !state(RecordingState::Recording) {
        return false;
    }
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(3) {
        let level = preview_level(started.elapsed().as_secs_f64());
        if !emit_preview(app, token, AUDIO_LEVEL, AudioLevelPayload { level }) {
            return false;
        }
        std::thread::sleep(Duration::from_millis(33));
    }
    if !state(RecordingState::Processing) {
        return false;
    }
    std::thread::sleep(Duration::from_secs(1));
    state(RecordingState::Done)
}

/// Shows the capsule at the size just chosen: re-places a preview already
/// on screen, or starts one. A real Recording on screen keeps its size until
/// it ends.
pub fn preview_size(app: &AppHandle) {
    if !preview_gate().running {
        return preview(app);
    }
    on_main_thread(app, |app| {
        let previewing = preview_gate().running && presenter(app).is_visible();
        if previewing {
            apply(app, Step::Show { place: true });
        }
    });
}

/// Tray diagnostic: plays recording (~3 s of fake levels) → processing (~1 s)
/// → done through the same events W2 emits. It does nothing while a real
/// Recording is on screen or another preview runs, and stops at once when a
/// real Recording starts.
pub fn preview(app: &AppHandle) {
    if presenter(app).is_live() {
        return;
    }
    let Some(token) = preview_gate().begin() else {
        return;
    };
    tracing::info!("overlay preview");
    let app = app.clone();
    std::thread::spawn(move || {
        if play_preview(&app, token) {
            preview_gate().finish(token);
        }
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

    use super::{Rect, placement, size};

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
    ///
    /// tao still believes this window is hidden. Calling a tao setter that
    /// changes its window flags (always-on-top, resizable, …) while the capsule
    /// is up would rewrite the styles without WS_VISIBLE; don't.
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
                size(),
            ))
        }
    }
}

/// The capsule as a Mac panel: shown with `orderFrontRegardless`, so Voisu
/// never activates and the focused app keeps the keyboard (tao's `show`
/// makes the window key). Above full-screen apps on every Space, and
/// click-through.
#[cfg(target_os = "macos")]
mod native {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{
        NSEvent, NSScreen, NSStatusWindowLevel, NSWindow, NSWindowCollectionBehavior,
    };
    use objc2_foundation::{NSPoint, NSRect, NSSize};
    use tauri::WebviewWindow;

    use super::{MacFrame, mac_placement, size};

    fn ns_window(window: &WebviewWindow) -> Option<&NSWindow> {
        let ptr = window.ns_window().ok()?.cast::<NSWindow>();
        // SAFETY: tao's live NSWindow for this window, which the WebviewWindow
        // keeps alive; only used on the main thread for this call.
        unsafe { ptr.as_ref() }
    }

    pub fn show(window: &WebviewWindow, place: bool) {
        let (Some(mtm), Some(ns)) = (MainThreadMarker::new(), ns_window(window)) else {
            return;
        };
        ns.setLevel(NSStatusWindowLevel);
        ns.setIgnoresMouseEvents(true);
        ns.setHasShadow(false);
        ns.setCollectionBehavior(
            NSWindowCollectionBehavior::CanJoinAllSpaces
                | NSWindowCollectionBehavior::Stationary
                | NSWindowCollectionBehavior::IgnoresCycle
                | NSWindowCollectionBehavior::FullScreenAuxiliary,
        );
        if place && let Some(frame) = target_frame(mtm) {
            let rect = NSRect::new(
                NSPoint::new(frame.x, frame.y),
                NSSize::new(frame.width, frame.height),
            );
            ns.setFrame_display(rect, true);
        }
        ns.orderFrontRegardless();
    }

    pub fn hide(window: &WebviewWindow) {
        if let Some(ns) = ns_window(window) {
            ns.orderOut(None);
        }
    }

    /// The screen under the mouse pointer, else the main screen.
    fn target_frame(mtm: MainThreadMarker) -> Option<MacFrame> {
        let mouse = NSEvent::mouseLocation();
        let inside = |screen: &NSScreen| {
            let f = screen.frame();
            (f.origin.x..f.origin.x + f.size.width).contains(&mouse.x)
                && (f.origin.y..f.origin.y + f.size.height).contains(&mouse.y)
        };
        let screen = NSScreen::screens(mtm)
            .iter()
            .find(|screen| inside(screen))
            .or_else(|| NSScreen::mainScreen(mtm))?;
        let v = screen.visibleFrame();
        Some(mac_placement(
            MacFrame {
                x: v.origin.x,
                y: v.origin.y,
                width: v.size.width,
                height: v.size.height,
            },
            size(),
        ))
    }
}

/// Linux builds exist only for local checks and tests.
#[cfg(not(any(windows, target_os = "macos")))]
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

    #[test]
    fn a_mac_capsule_sits_centred_above_the_dock_in_points() {
        // A 1440x900 point screen with a 25 pt menu bar and a 70 pt Dock.
        let visible = MacFrame {
            x: 0.0,
            y: 70.0,
            width: 1440.0,
            height: 805.0,
        };
        let frame = mac_placement(visible, OverlaySize::Medium);
        let windows = placement(
            Rect {
                left: 0,
                top: 25,
                right: 1440,
                bottom: 830,
            },
            1.0,
            OverlaySize::Medium,
        );
        assert_eq!(frame.width, f64::from(windows.right - windows.left));
        assert_eq!(frame.height, f64::from(windows.bottom - windows.top));
        assert_eq!(frame.x, f64::from(windows.left));
        // The same gap above the Dock as above the taskbar.
        assert_eq!(frame.y - visible.y, f64::from(830 - windows.bottom));
        // A screen to the left of and below the main one keeps its offset.
        let side = mac_placement(
            MacFrame {
                x: -1920.0,
                y: -300.0,
                ..visible
            },
            OverlaySize::Medium,
        );
        assert_eq!(side.x - frame.x, -1920.0);
        assert_eq!(side.y - frame.y, -370.0);
    }

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
    fn done_shows_then_hides_on_its_own_timer_and_ignores_a_following_idle() {
        let mut presenter = Presenter::default();
        presenter.on_state(RecordingState::Processing);
        let (step, timer) = presenter.on_state(RecordingState::Done);
        assert_eq!(step, Step::Show { place: false });
        assert_eq!(presenter.on_state(RecordingState::Idle).0, Step::Nothing);
        assert!(presenter.is_visible());
        assert!(!presenter.is_live(), "a preview may replace the check mark");
        assert_eq!(presenter.on_hide_timer(timer.unwrap()), Step::Hide);
        assert_eq!(
            RecordingState::Done.display_time(),
            Some(Duration::from_millis(600))
        );
    }

    #[test]
    fn a_new_recording_supersedes_the_check_mark() {
        let mut presenter = Presenter::default();
        let (_, timer) = presenter.on_state(RecordingState::Done);
        assert_eq!(
            presenter.on_state(RecordingState::Recording).0,
            Step::Show { place: false }
        );
        assert_eq!(presenter.on_hide_timer(timer.unwrap()), Step::Nothing);
        assert!(presenter.is_live());
        assert_eq!(presenter.on_state(RecordingState::Idle).0, Step::Hide);
    }

    #[test]
    fn idle_right_after_an_error_leaves_the_message_up_until_its_timer() {
        let mut presenter = Presenter::default();
        let (_, timer) = presenter.on_state(RecordingState::Error);
        assert_eq!(presenter.on_state(RecordingState::Idle).0, Step::Nothing);
        assert!(presenter.is_visible());
        assert_eq!(presenter.on_hide_timer(timer.unwrap()), Step::Hide);
        assert_eq!(presenter.on_state(RecordingState::Idle).0, Step::Nothing);
        assert_eq!(
            presenter.on_state(RecordingState::Recording).0,
            Step::Show { place: true }
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
            placement(work, 1.0, OverlaySize::Medium),
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
        let r = placement(left_monitor, 1.5, OverlaySize::Medium);
        assert_eq!((r.right - r.left, r.bottom - r.top), (444, 120));
        assert_eq!(r.left - left_monitor.left, left_monitor.right - r.right);
        assert_eq!(left_monitor.bottom - r.bottom, 24);
    }

    #[test]
    fn sizes_scale_the_window_but_keep_the_capsule_margin() {
        let work = Rect {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1040,
        };
        for (size, (w, h)) in [
            (OverlaySize::Small, (237, 64)),
            (OverlaySize::Large, (370, 100)),
        ] {
            let r = placement(work, 1.0, size);
            assert_eq!((r.right - r.left, r.bottom - r.top), (w, h), "{size:?}");
            let (left_gap, right_gap) = (r.left - work.left, work.right - r.right);
            assert!((left_gap - right_gap).abs() <= 1, "{size:?} stays centred");
            // The glow room scales too, so the capsule edge stays 24 px up.
            let glow = (GLOW_PAD * size.scale()).round() as i32;
            assert_eq!(work.bottom - (r.bottom - glow), 24, "{size:?}");
        }
        // 125% DPI and large: physical pixels, still bottom-centred.
        let r = placement(work, 1.25, OverlaySize::Large);
        assert_eq!((r.right - r.left, r.bottom - r.top), (463, 125));
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

    #[test]
    fn a_real_recording_silences_a_running_preview() {
        let mut gate = PreviewGate::new();
        let token = gate.begin().unwrap();
        assert!(gate.is_current(token));
        assert_eq!(gate.begin(), None, "one preview at a time");

        gate.end();
        assert!(!gate.is_current(token), "stale preview must stop emitting");
        // Its late finish must not end a newer preview.
        let next = gate.begin().unwrap();
        gate.finish(token);
        assert!(gate.is_current(next));
        gate.finish(next);
        assert!(gate.begin().is_some());
    }
}
