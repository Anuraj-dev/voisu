//! The recording Overlay: a small pill with live level bars, by default at
//! the top centre of the active monitor, while Voisu records. The user can
//! move it from Settings; the spot is kept as fractions of the work area.
//!
//! The window is created once at startup and never takes focus, never appears
//! in the taskbar or Alt+Tab, and lets clicks through. It is driven only by the
//! W2 events below; it never feeds anything back into Recording. The one
//! exception to click-through is move mode, which Settings starts and ends.
//!
//! Visibility is native (Win32), not Tauri's `show()`/`hide()`: in tao 0.37,
//! `show()` recomputes `GWL_EXSTYLE` from tao's own flags (dropping
//! `WS_EX_TOOLWINDOW`) and calls `ShowWindow(SW_SHOW)`, which activates the
//! window. `SetWindowPos(.., SWP_NOACTIVATE | SWP_SHOWWINDOW)` does not.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Listener, Manager, WebviewUrl, WebviewWindowBuilder};

use crate::settings::{OverlayPosition, OverlaySize};

pub const OVERLAY_WINDOW: &str = "overlay";
/// W2 → W4 contract: emitted on every Recording lifecycle change.
pub const RECORDING_STATE: &str = "recording-state";
/// W2 → W4 contract: at most 30 Hz, only while recording or hands-free.
pub const AUDIO_LEVEL: &str = "audio-level";
/// To the overlay page only: where in its window the pill sits, and whether
/// it is in move mode. Sent before each placed show.
const OVERLAY_LAYOUT: &str = "overlay-layout";

/// The pill at the medium size, in logical pixels. The page scales itself
/// from the window width it is given (overlay.js `BASE_WIDTH`).
const PILL_WIDTH: f64 = 120.0;
const PILL_HEIGHT: f64 = 32.0;
/// Copied and Error text widen the pill up to this: two lines at most. The
/// window is always this big (plus the glow room); the area the pill does
/// not cover is transparent and lets clicks through.
const TEXT_WIDTH: f64 = 320.0;
const TEXT_HEIGHT: f64 = 48.0;
/// Transparent room around the pill for the error state's 8 px glow.
const GLOW_PAD: f64 = 8.0;
/// Least gap between the pill and any work-area edge; the default spot is
/// this far below the top. Not scaled by the Overlay size.
const EDGE_MARGIN: f64 = 12.0;
/// Done snaps the pill to the centre line, or to the default top or bottom
/// margin, when it is dropped this close to it.
const SNAP_DISTANCE: f64 = 24.0;
/// What `None` means: centred, at the top margin.
const DEFAULT_POSITION: OverlayPosition = OverlayPosition { x: 0.5, y: 0.0 };
/// Long enough to read a one-line error, then the capsule hides.
const ERROR_DISPLAY: Duration = Duration::from_secs(3);
/// The Delivered check mark, as brief as the Linux overlay's: the Transcript
/// is already in the focused app, so the confirmation only needs to register.
const DONE_DISPLAY: Duration = Duration::from_millis(600);
/// The clipboard fallback: long enough to read that the user must paste.
const COPIED_DISPLAY: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RecordingState {
    Idle,
    Recording,
    HandsFree,
    Processing,
    /// The transcript was delivered: a brief check mark, then hidden.
    Done,
    /// The transcript is only on the clipboard and the user must paste it:
    /// amber text that says how.
    Copied,
    Error,
}

impl RecordingState {
    /// How long a terminal state stays up on its own before the capsule hides.
    fn display_time(self) -> Option<Duration> {
        match self {
            Self::Done => Some(DONE_DISPLAY),
            Self::Copied => Some(COPIED_DISPLAY),
            Self::Error => Some(ERROR_DISPLAY),
            Self::Idle | Self::Recording | Self::HandsFree | Self::Processing => None,
        }
    }
}

/// `recording-state` payload. `message` is set for `error` and `copied` (the
/// visible text), and for `done` when auto-paste is off (the accessible label).
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

/// Pure visibility logic: which states show the pill, which pending hide
/// timer (after `done` or `error`) is still current, and move mode.
#[derive(Debug, Default)]
pub struct Presenter {
    visible: bool,
    generation: u64,
    terminal_showing: bool,
    moving: bool,
}

impl Presenter {
    /// Returns the window step and, for `done` and `error`, the generation a
    /// delayed [`Presenter::on_hide_timer`] call must carry to still apply.
    /// Any state but `idle` ends move mode first: Recording always wins.
    pub fn on_state(&mut self, state: RecordingState) -> (Step, Option<u64>) {
        if self.moving {
            if state == RecordingState::Idle {
                return (Step::Nothing, None);
            }
            self.moving = false;
        }
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

    /// Enters move mode, which takes over the window: a check mark or message
    /// still up is dropped and its timer goes stale. The caller shows the
    /// window itself. False if already moving.
    pub fn begin_move(&mut self) -> bool {
        if self.moving {
            return false;
        }
        self.generation += 1;
        self.terminal_showing = false;
        self.visible = false;
        self.moving = true;
        true
    }

    /// Leaves move mode; false if it was not on. The caller hides the window.
    pub fn end_move(&mut self) -> bool {
        std::mem::take(&mut self.moving)
    }

    pub fn is_moving(&self) -> bool {
        self.moving
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

/// The pill centre as fractions of the overlay window, y down, for the page.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Anchor {
    pub x: f64,
    pub y: f64,
}

/// Where the overlay window goes, and where in it the pill is drawn.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Placement {
    pub window: Rect,
    pub anchor: Anchor,
}

/// `v` within `lo..=hi`, or the middle when the range is empty (a work area
/// smaller than what must fit in it).
fn fit(v: f64, lo: f64, hi: f64) -> f64 {
    if lo > hi {
        (lo + hi) / 2.0
    } else {
        v.clamp(lo, hi)
    }
}

/// One monitor's work area with the pill and window sizes on it, in physical
/// pixels.
struct Area {
    left: f64,
    top: f64,
    width: f64,
    height: f64,
    /// Pill half sizes plus the edge margin: how near an edge its centre may go.
    inset_x: f64,
    inset_y: f64,
    window_width: i32,
    window_height: i32,
    snap: f64,
}

impl Area {
    /// `dpi_scale` is the monitor's effective DPI / 96. The pill, its text
    /// room and the glow room grow with `size`; the margins do not.
    fn new(work: Rect, dpi_scale: f64, size: OverlaySize) -> Self {
        let s = size.scale() * dpi_scale;
        let px = |logical: f64| (logical * s).round() as i32;
        Self {
            left: f64::from(work.left),
            top: f64::from(work.top),
            width: f64::from(work.right - work.left),
            height: f64::from(work.bottom - work.top),
            inset_x: PILL_WIDTH * s / 2.0 + EDGE_MARGIN * dpi_scale,
            inset_y: PILL_HEIGHT * s / 2.0 + EDGE_MARGIN * dpi_scale,
            window_width: px(TEXT_WIDTH + 2.0 * GLOW_PAD),
            window_height: px(TEXT_HEIGHT + 2.0 * GLOW_PAD),
            snap: SNAP_DISTANCE * dpi_scale,
        }
    }

    /// The nearest pill centre that keeps the whole pill inside the margins.
    fn clamp(&self, x: f64, y: f64) -> (f64, f64) {
        (
            fit(
                x,
                self.left + self.inset_x,
                self.left + self.width - self.inset_x,
            ),
            fit(
                y,
                self.top + self.inset_y,
                self.top + self.height - self.inset_y,
            ),
        )
    }
}

/// Where the overlay window goes on a monitor for the saved `position` (the
/// default top-centre for `None`). The pill centre is the saved fraction of
/// this work area, clamped so the pill stays inside it on any monitor size,
/// DPI or taskbar position. The window, sized for the widest text, is
/// centred on the pill and pushed back inside the work area only where it
/// would cross an edge, so Copied and Error text grow from the pill centre
/// and never leave the screen.
pub fn placement(
    work: Rect,
    dpi_scale: f64,
    size: OverlaySize,
    position: Option<OverlayPosition>,
) -> Placement {
    let area = Area::new(work, dpi_scale, size);
    let p = position.unwrap_or(DEFAULT_POSITION);
    let (x, y) = area.clamp(area.left + p.x * area.width, area.top + p.y * area.height);
    let (w, h) = (area.window_width, area.window_height);
    let left = fit(
        x - f64::from(w) / 2.0,
        area.left,
        area.left + area.width - f64::from(w),
    )
    .round() as i32;
    let top = fit(
        y - f64::from(h) / 2.0,
        area.top,
        area.top + area.height - f64::from(h),
    )
    .round() as i32;
    Placement {
        window: Rect {
            left,
            top,
            right: left + w,
            bottom: top + h,
        },
        anchor: Anchor {
            x: (x - f64::from(left)) / f64::from(w),
            y: (y - f64::from(top)) / f64::from(h),
        },
    }
}

/// The position to save after the user dragged the window to `window` on a
/// monitor with this work area, the pill drawn at `anchor` in it. The pill
/// centre snaps to the centre line, and to the default top or bottom margin,
/// within 24 px. `None` is the default spot, so Reset has nothing to undo.
pub fn dropped_position(
    work: Rect,
    dpi_scale: f64,
    size: OverlaySize,
    window: Rect,
    anchor: Anchor,
) -> Option<OverlayPosition> {
    let area = Area::new(work, dpi_scale, size);
    let (x, y) = area.clamp(
        f64::from(window.left) + anchor.x * f64::from(window.right - window.left),
        f64::from(window.top) + anchor.y * f64::from(window.bottom - window.top),
    );
    let near = |a: f64, b: f64| (a - b).abs() <= area.snap;
    let (_, top_line) = area.clamp(x, area.top);
    let (_, bottom_line) = area.clamp(x, area.top + area.height);
    let fx = if near(x, area.left + area.width / 2.0) {
        0.5
    } else {
        (x - area.left) / area.width
    };
    let fy = if near(y, top_line) {
        0.0
    } else if near(y, bottom_line) {
        1.0
    } else {
        (y - area.top) / area.height
    };
    OverlayPosition::new(fx.clamp(0.0, 1.0), fy.clamp(0.0, 1.0))
        .filter(|position| *position != DEFAULT_POSITION)
}

/// A rectangle in AppKit points: origin at the bottom left, y growing up.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MacFrame {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl MacFrame {
    /// The same rectangle with y flipped to grow down, as [`placement`] works.
    fn flipped(self) -> Rect {
        Rect {
            left: self.x.round() as i32,
            top: -(self.y + self.height).round() as i32,
            right: (self.x + self.width).round() as i32,
            bottom: -self.y.round() as i32,
        }
    }

    fn unflipped(r: Rect) -> Self {
        Self {
            x: f64::from(r.left),
            y: f64::from(-r.bottom),
            width: f64::from(r.right - r.left),
            height: f64::from(r.bottom - r.top),
        }
    }
}

/// The overlay's frame on a Mac screen whose visible frame (below the menu
/// bar, above the Dock) is `visible`. [`placement`] with y flipped: AppKit
/// works in points, so the backing scale needs no part in it. The anchor is
/// for the page, whose y grows down.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn mac_placement(
    visible: MacFrame,
    size: OverlaySize,
    position: Option<OverlayPosition>,
) -> (MacFrame, Anchor) {
    let p = placement(visible.flipped(), 1.0, size, position);
    (MacFrame::unflipped(p.window), p.anchor)
}

/// [`dropped_position`] for a window frame on a Mac screen.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn mac_dropped_position(
    visible: MacFrame,
    size: OverlaySize,
    frame: MacFrame,
    anchor: Anchor,
) -> Option<OverlayPosition> {
    dropped_position(visible.flipped(), 1.0, size, frame.flipped(), anchor)
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

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The size and saved position the next show places the pill at. Read on the
/// main thread at each hidden → visible edge, so a change applies from the
/// next Recording.
static SIZE: Mutex<OverlaySize> = Mutex::new(OverlaySize::Medium);
static POSITION: Mutex<Option<OverlayPosition>> = Mutex::new(None);
/// Where the pill was last drawn in the window. Done reads it to find the
/// pill centre in the dragged window.
static ANCHOR: Mutex<Anchor> = Mutex::new(Anchor { x: 0.5, y: 0.5 });

pub fn set_size(size: OverlaySize) {
    *lock(&SIZE) = size;
}

pub fn set_position(position: Option<OverlayPosition>) {
    *lock(&POSITION) = position;
}

/// Only the Windows and macOS placements read these.
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
fn size() -> OverlaySize {
    *lock(&SIZE)
}

#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
fn position() -> Option<OverlayPosition> {
    *lock(&POSITION)
}

/// Which monitor a placed show uses.
#[derive(Clone, Copy)]
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
enum Source<'a> {
    /// The one being typed on: the Recording rule.
    Typing,
    /// The one holding this window: move mode, beside Settings.
    Beside(&'a tauri::WebviewWindow),
}

/// `overlay-layout` payload.
#[derive(Clone, Serialize)]
struct LayoutPayload {
    x: f64,
    y: f64,
    moving: bool,
}

fn emit_layout(app: &AppHandle, moving: bool) {
    let anchor = *lock(&ANCHOR);
    let payload = LayoutPayload {
        x: anchor.x,
        y: anchor.y,
        moving,
    };
    let _ = app.emit_to(OVERLAY_WINDOW, OVERLAY_LAYOUT, payload);
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
    .inner_size(TEXT_WIDTH + 2.0 * GLOW_PAD, TEXT_HEIGHT + 2.0 * GLOW_PAD)
    .decorations(false)
    .transparent(true)
    .shadow(false)
    .resizable(false)
    .always_on_top(true)
    .skip_taskbar(true)
    .focusable(false)
    .focused(false)
    // macOS: the first click on the never-key pill reaches the page, so a
    // drag can start in move mode.
    .accept_first_mouse(true)
    .visible(false)
    .build()?;
    // tao maps this to WS_EX_TRANSPARENT | WS_EX_LAYERED: hit-testing passes
    // through to the window underneath. tao's flag stays set for good; move
    // mode lifts only the native bit (see `native::show`).
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

/// Runs `work` on the main thread, behind every overlay update already
/// queued, and returns its result.
async fn on_main<T: Send + 'static>(
    app: &AppHandle,
    work: impl FnOnce(&AppHandle) -> T + Send + 'static,
) -> Result<T, String> {
    let (send, receive) = tokio::sync::oneshot::channel();
    let handle = app.clone();
    app.run_on_main_thread(move || {
        let _ = send.send(work(&handle));
    })
    .map_err(|error| error.to_string())?;
    receive.await.map_err(|error| error.to_string())
}

fn on_state(app: &AppHandle, state: RecordingState) {
    let (step, timer, move_cancelled) = {
        let mut presenter = presenter(app);
        let was_moving = presenter.is_moving();
        let (step, timer) = presenter.on_state(state);
        (step, timer, was_moving && !presenter.is_moving())
    };
    if move_cancelled {
        tracing::info!("overlay move cancelled by a Recording");
        leave_move(app);
    }
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
    lock(&app.state::<OverlayPresenter>().inner().0)
}

/// Main thread only.
fn apply(app: &AppHandle, step: Step) {
    let Some(window) = app.get_webview_window(OVERLAY_WINDOW) else {
        return;
    };
    match step {
        Step::Show { place } => show(app, &window, place.then_some(Source::Typing), false),
        Step::Hide => native::hide(&window),
        Step::Nothing => {}
    }
}

/// Shows the window, first placing it on `source`'s monitor when given. The
/// page hears where the pill goes before the window appears. Main thread only.
fn show(app: &AppHandle, window: &tauri::WebviewWindow, source: Option<Source>, moving: bool) {
    let frame = source.and_then(native::target).map(|(frame, anchor)| {
        *lock(&ANCHOR) = anchor;
        emit_layout(app, moving);
        frame
    });
    native::show(window, frame, !moving);
}

/// Settings' "Move overlay": shows the pill, grabbable, on the monitor that
/// holds the Settings window. Refused while a real Recording is on screen.
pub async fn start_move(app: &AppHandle) -> Result<(), String> {
    // A tray preview gives way. Its events already emitted are queued ahead
    // of the work below, and it emits nothing after this.
    let previewing = preview_gate().running;
    end_preview();
    on_main(app, move |app| {
        let Some(window) = app.get_webview_window(OVERLAY_WINDOW) else {
            return Err("The overlay isn't available.".to_string());
        };
        {
            let mut presenter = presenter(app);
            if presenter.is_moving() {
                return Ok(());
            }
            if presenter.is_live() && !previewing {
                return Err("Finish the Recording first.".to_string());
            }
            presenter.begin_move();
        }
        tracing::info!("overlay move started");
        native::hide(&window);
        let settings = app.get_webview_window(crate::SETTINGS_WINDOW);
        let source = settings.as_ref().map_or(Source::Typing, Source::Beside);
        show(app, &window, Some(source), true);
        let _ = app.emit(crate::STATE_CHANGED, ());
        Ok(())
    })
    .await?
}

/// Done (`save`) or Cancel. Hides the pill and puts the overlay back to
/// click-through. With `save`, returns where the pill was dropped (`None` is
/// the default spot) for the caller to store; `None` if move mode was not on.
pub async fn finish_move(
    app: &AppHandle,
    save: bool,
) -> Result<Option<Option<OverlayPosition>>, String> {
    on_main(app, move |app| finish_move_now(app, save)).await
}

/// [`finish_move`] on the main thread, also used when Settings closes.
pub fn finish_move_now(app: &AppHandle, save: bool) -> Option<Option<OverlayPosition>> {
    if !presenter(app).end_move() {
        return None;
    }
    let dropped = app
        .get_webview_window(OVERLAY_WINDOW)
        .filter(|_| save)
        .and_then(|window| native::dropped(&window, *lock(&ANCHOR)));
    tracing::info!(save, saved = dropped.is_some(), "overlay move finished");
    leave_move(app);
    dropped
}

/// Hidden and click-through again; Settings and the page are told.
fn leave_move(app: &AppHandle) {
    if let Some(window) = app.get_webview_window(OVERLAY_WINDOW) {
        native::hide(&window);
        native::set_click_through(&window);
    }
    emit_layout(app, false);
    let _ = app.emit(crate::STATE_CHANGED, ());
}

pub fn is_moving(app: &AppHandle) -> bool {
    app.try_state::<OverlayPresenter>()
        .is_some_and(|state| lock(&state.inner().0).is_moving())
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
    lock(&PREVIEW)
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

/// Shows the pill at the size or position just chosen: re-places a preview
/// already on screen, or starts one. A real Recording on screen keeps its
/// place until it ends.
pub fn preview_placement(app: &AppHandle) {
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
    {
        let presenter = presenter(app);
        if presenter.is_live() || presenter.is_moving() {
            return;
        }
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
    use windows::Win32::Foundation::{COLORREF, HWND, POINT, RECT};
    use windows::Win32::Graphics::Gdi::{
        GetMonitorInfoW, HMONITOR, MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTONULL, MONITORINFO,
        MonitorFromPoint, MonitorFromWindow,
    };
    use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
    use windows::Win32::UI::WindowsAndMessaging::{
        GWL_EXSTYLE, GetCursorPos, GetForegroundWindow, GetWindowLongPtrW, GetWindowRect,
        HWND_TOPMOST, LWA_ALPHA, SW_HIDE, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
        SWP_NOZORDER, SWP_SHOWWINDOW, SetLayeredWindowAttributes, SetWindowLongPtrW, SetWindowPos,
        ShowWindow, WS_EX_APPWINDOW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
        WS_EX_TRANSPARENT,
    };

    use super::{Anchor, Rect, Source, dropped_position, placement, position, size};
    use crate::settings::OverlayPosition;

    /// Shows the window topmost without activating it, at `frame` if given.
    /// Click-through unless in move mode.
    pub fn show(window: &WebviewWindow, frame: Option<Rect>, click_through: bool) {
        let Ok(hwnd) = window.hwnd() else { return };
        assert_styles(hwnd, click_through);
        // SAFETY: plain Win32 calls on our own live top-level window, made on
        // the thread that owns it.
        unsafe {
            if let Some(r) = frame {
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
                // Re-assert topmost each time so the pill rises above a
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

    /// After move mode: the hidden window lets clicks through again.
    pub fn set_click_through(window: &WebviewWindow) {
        if let Ok(hwnd) = window.hwnd() {
            assert_styles(hwnd, true);
        }
    }

    /// No activation, no taskbar button or Alt+Tab entry, and click-through
    /// except in move mode. Re-applied before every show because tao
    /// rewrites GWL_EXSTYLE from its own flags whenever one of them changes,
    /// and it has no tool-window flag. The showing `SetWindowPos` carries
    /// SWP_FRAMECHANGED so the change lands.
    ///
    /// Move mode clears only the native WS_EX_TRANSPARENT; tao's own
    /// ignore-cursor flag stays set, so any tao rewrite errs towards
    /// click-through.
    ///
    /// tao still believes this window is hidden. Calling a tao setter that
    /// changes its window flags (always-on-top, resizable, …) while the pill
    /// is up would rewrite the styles without WS_VISIBLE; don't.
    fn assert_styles(hwnd: HWND, click_through: bool) {
        let add = (WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW | WS_EX_LAYERED).0 as isize;
        let transparent = WS_EX_TRANSPARENT.0 as isize;
        // SAFETY: reading and writing our own window's extended style.
        unsafe {
            let current = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
            let mut wanted = (current | add) & !(WS_EX_APPWINDOW.0 as isize);
            if click_through {
                wanted |= transparent;
            } else {
                wanted &= !transparent;
            }
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

    /// The window rect and pill anchor for a placed show on `source`'s monitor.
    pub fn target(source: Source) -> Option<(Rect, Anchor)> {
        let monitor = match source {
            Source::Typing => typing_monitor(),
            // SAFETY: a read-only query on our own Settings window.
            Source::Beside(other) => unsafe {
                MonitorFromWindow(other.hwnd().ok()?, MONITOR_DEFAULTTONEAREST)
            },
        };
        let (work, dpi_scale) = work_area(monitor)?;
        let p = placement(work, dpi_scale, size(), position());
        Some((p.window, p.anchor))
    }

    /// Where the user dropped the pill, on the monitor its centre is over.
    pub fn dropped(window: &WebviewWindow, anchor: Anchor) -> Option<Option<OverlayPosition>> {
        let hwnd = window.hwnd().ok()?;
        let mut r = RECT::default();
        // SAFETY: read-only Win32 queries on our own window with valid
        // out-pointers.
        let monitor = unsafe {
            GetWindowRect(hwnd, &mut r).ok()?;
            let centre = POINT {
                x: r.left + (anchor.x * f64::from(r.right - r.left)).round() as i32,
                y: r.top + (anchor.y * f64::from(r.bottom - r.top)).round() as i32,
            };
            MonitorFromPoint(centre, MONITOR_DEFAULTTONEAREST)
        };
        let (work, dpi_scale) = work_area(monitor)?;
        let window = Rect {
            left: r.left,
            top: r.top,
            right: r.right,
            bottom: r.bottom,
        };
        Some(dropped_position(work, dpi_scale, size(), window, anchor))
    }

    /// The monitor holding the foreground window, else the cursor's monitor.
    fn typing_monitor() -> HMONITOR {
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
            monitor
        }
    }

    /// The monitor's work area (taskbar excluded) and effective DPI / 96.
    fn work_area(monitor: HMONITOR) -> Option<(Rect, f64)> {
        // SAFETY: read-only Win32 queries with valid out-pointers.
        unsafe {
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
            Some((
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

/// The pill as a Mac panel: shown with `orderFrontRegardless`, so Voisu
/// never activates and the focused app keeps the keyboard (tao's `show`
/// makes the window key). Above full-screen apps on every Space, and
/// click-through except in move mode.
#[cfg(target_os = "macos")]
mod native {
    use objc2::MainThreadMarker;
    use objc2::rc::Retained;
    use objc2_app_kit::{
        NSEvent, NSScreen, NSStatusWindowLevel, NSWindow, NSWindowCollectionBehavior,
    };
    use objc2_foundation::{NSPoint, NSRect, NSSize};
    use tauri::WebviewWindow;

    use super::{Anchor, MacFrame, Source, mac_dropped_position, mac_placement, position, size};
    use crate::settings::OverlayPosition;

    fn ns_window(window: &WebviewWindow) -> Option<&NSWindow> {
        let ptr = window.ns_window().ok()?.cast::<NSWindow>();
        // SAFETY: tao's live NSWindow for this window, which the WebviewWindow
        // keeps alive; only used on the main thread for this call.
        unsafe { ptr.as_ref() }
    }

    pub fn show(window: &WebviewWindow, frame: Option<MacFrame>, click_through: bool) {
        let Some(ns) = ns_window(window) else {
            return;
        };
        ns.setLevel(NSStatusWindowLevel);
        ns.setIgnoresMouseEvents(click_through);
        ns.setHasShadow(false);
        ns.setCollectionBehavior(
            NSWindowCollectionBehavior::CanJoinAllSpaces
                | NSWindowCollectionBehavior::Stationary
                | NSWindowCollectionBehavior::IgnoresCycle
                | NSWindowCollectionBehavior::FullScreenAuxiliary,
        );
        if let Some(frame) = frame {
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

    /// After move mode: the hidden window lets clicks through again.
    pub fn set_click_through(window: &WebviewWindow) {
        if let Some(ns) = ns_window(window) {
            ns.setIgnoresMouseEvents(true);
        }
    }

    /// The frame and pill anchor for a placed show: on the screen under the
    /// mouse pointer, or the one holding `Source::Beside`'s window; else the
    /// main screen.
    pub fn target(source: Source) -> Option<(MacFrame, Anchor)> {
        let mtm = MainThreadMarker::new()?;
        let screen = match source {
            Source::Typing => screen_at(mtm, NSEvent::mouseLocation()),
            Source::Beside(other) => ns_window(other).and_then(NSWindow::screen),
        }
        .or_else(|| NSScreen::mainScreen(mtm))?;
        Some(mac_placement(visible_frame(&screen), size(), position()))
    }

    /// Where the user dropped the pill, on the screen its centre is over.
    pub fn dropped(window: &WebviewWindow, anchor: Anchor) -> Option<Option<OverlayPosition>> {
        let mtm = MainThreadMarker::new()?;
        let ns = ns_window(window)?;
        let f = ns.frame();
        let frame = MacFrame {
            x: f.origin.x,
            y: f.origin.y,
            width: f.size.width,
            height: f.size.height,
        };
        // The anchor's y grows down from the window top; AppKit's grows up.
        let centre = NSPoint::new(
            frame.x + anchor.x * frame.width,
            frame.y + frame.height - anchor.y * frame.height,
        );
        let screen = screen_at(mtm, centre)
            .or_else(|| ns.screen())
            .or_else(|| NSScreen::mainScreen(mtm))?;
        Some(mac_dropped_position(
            visible_frame(&screen),
            size(),
            frame,
            anchor,
        ))
    }

    fn screen_at(mtm: MainThreadMarker, point: NSPoint) -> Option<Retained<NSScreen>> {
        let inside = |screen: &NSScreen| {
            let f = screen.frame();
            (f.origin.x..f.origin.x + f.size.width).contains(&point.x)
                && (f.origin.y..f.origin.y + f.size.height).contains(&point.y)
        };
        NSScreen::screens(mtm).iter().find(|screen| inside(screen))
    }

    /// Below the menu bar and above the Dock.
    fn visible_frame(screen: &NSScreen) -> MacFrame {
        let v = screen.visibleFrame();
        MacFrame {
            x: v.origin.x,
            y: v.origin.y,
            width: v.size.width,
            height: v.size.height,
        }
    }
}

/// Linux builds exist only for local checks and tests: no placement and no
/// move mode.
#[cfg(not(any(windows, target_os = "macos")))]
mod native {
    use tauri::WebviewWindow;

    use super::{Anchor, Source};
    use crate::settings::OverlayPosition;

    pub fn show(window: &WebviewWindow, _frame: Option<()>, _click_through: bool) {
        let _ = window.show();
    }

    pub fn hide(window: &WebviewWindow) {
        let _ = window.hide();
    }

    pub fn set_click_through(_window: &WebviewWindow) {}

    pub fn target(_source: Source) -> Option<((), Anchor)> {
        None
    }

    pub fn dropped(_window: &WebviewWindow, _anchor: Anchor) -> Option<Option<OverlayPosition>> {
        None
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
    fn copied_holds_two_seconds_through_a_following_idle() {
        let mut presenter = Presenter::default();
        presenter.on_state(RecordingState::Processing);
        let (_, timer) = presenter.on_state(RecordingState::Copied);
        assert_eq!(presenter.on_state(RecordingState::Idle).0, Step::Nothing);
        assert_eq!(presenter.on_hide_timer(timer.unwrap()), Step::Hide);
        assert_eq!(
            RecordingState::Copied.display_time(),
            Some(Duration::from_secs(2))
        );
        let copied = state_json(r#"{"state":"copied","message":"Copied · ⌘V"}"#).unwrap();
        assert_eq!(copied.state, RecordingState::Copied);
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

    const FULL_HD: Rect = Rect {
        left: 0,
        top: 0,
        right: 1920,
        bottom: 1040,
    };

    fn at(x: f64, y: f64) -> Option<OverlayPosition> {
        Some(OverlayPosition::new(x, y).unwrap())
    }

    /// The pill's rectangle on screen, in physical pixels.
    fn pill(p: Placement, dpi_scale: f64, size: OverlaySize) -> (f64, f64, f64, f64) {
        let s = size.scale() * dpi_scale;
        let w = f64::from(p.window.right - p.window.left);
        let h = f64::from(p.window.bottom - p.window.top);
        let x = f64::from(p.window.left) + p.anchor.x * w;
        let y = f64::from(p.window.top) + p.anchor.y * h;
        let (half_w, half_h) = (PILL_WIDTH * s / 2.0, PILL_HEIGHT * s / 2.0);
        (x - half_w, y - half_h, x + half_w, y + half_h)
    }

    fn inside(r: Rect, outer: Rect) -> bool {
        r.left >= outer.left
            && r.top >= outer.top
            && r.right <= outer.right
            && r.bottom <= outer.bottom
    }

    #[test]
    fn the_default_pill_sits_top_centre_twelve_px_down_at_any_scale() {
        // 1920x1040 work area at 100%: a 336x64 window, the pill 120x32.
        let p = placement(FULL_HD, 1.0, OverlaySize::Medium, None);
        assert_eq!(
            (
                p.window.right - p.window.left,
                p.window.bottom - p.window.top
            ),
            (336, 64)
        );
        assert_eq!(
            pill(p, 1.0, OverlaySize::Medium),
            (900.0, 12.0, 1020.0, 44.0)
        );
        // The window cannot centre on a pill this near the top, so it stops
        // at the work area and the pill sits above its middle.
        assert_eq!(p.window.top, 0);
        assert!(p.anchor.y < 0.5);

        // A second monitor left of the primary at 150%, with a top taskbar.
        let left_monitor = Rect {
            left: -2560,
            top: 60,
            right: 0,
            bottom: 1440,
        };
        let p = placement(left_monitor, 1.5, OverlaySize::Medium, None);
        let (l, t, r, b) = pill(p, 1.5, OverlaySize::Medium);
        assert_eq!((r - l, b - t), (180.0, 48.0));
        assert_eq!(t - 60.0, 18.0, "12 logical px at 150%");
        assert_eq!(l - -2560.0, 0.0 - r, "centred");

        for (size, (w, h)) in [
            (OverlaySize::Small, (96.0, 25.6)),
            (OverlaySize::Large, (150.0, 40.0)),
        ] {
            let (l, t, r, b) = pill(placement(FULL_HD, 1.0, size, None), 1.0, size);
            assert!(
                (r - l - w).abs() < 1e-9 && (b - t - h).abs() < 1e-9,
                "{size:?}"
            );
            assert!((t - 12.0).abs() < 1e-9, "{size:?} keeps the 12 px margin");
            assert!(
                ((l + r) / 2.0 - 960.0).abs() < 1e-9,
                "{size:?} stays centred"
            );
        }
    }

    #[test]
    fn a_saved_position_is_the_pill_centre_as_fractions_of_the_work_area() {
        let p = placement(FULL_HD, 1.0, OverlaySize::Medium, at(0.25, 0.5));
        let (l, t, r, b) = pill(p, 1.0, OverlaySize::Medium);
        assert!(((l + r) / 2.0 - 480.0).abs() < 1e-9);
        assert!(((t + b) / 2.0 - 520.0).abs() < 1e-9);
        // Away from the edges the window is centred on the pill.
        assert!((p.anchor.x - 0.5).abs() < 1e-9 && (p.anchor.y - 0.5).abs() < 1e-9);

        // The same fractions on a smaller, denser monitor elsewhere.
        let laptop = Rect {
            left: 1920,
            top: 0,
            right: 1920 + 2880,
            bottom: 1800 - 72,
        };
        let p = placement(laptop, 2.0, OverlaySize::Medium, at(0.25, 0.5));
        let (l, t, r, b) = pill(p, 2.0, OverlaySize::Medium);
        assert!(((l + r) / 2.0 - (1920.0 + 720.0)).abs() < 1.0);
        assert!(((t + b) / 2.0 - 864.0).abs() < 1.0);
    }

    #[test]
    fn placing_then_dropping_where_it_was_gives_the_same_position() {
        for position in [at(0.25, 0.5), at(0.8, 0.3), at(0.1, 0.9), at(0.62, 0.71)] {
            for (work, dpi) in [
                (FULL_HD, 1.0),
                (
                    Rect {
                        left: -1280,
                        top: 40,
                        right: 0,
                        bottom: 1024,
                    },
                    1.25,
                ),
            ] {
                for size in [OverlaySize::Small, OverlaySize::Medium, OverlaySize::Large] {
                    let p = placement(work, dpi, size, position);
                    let back = dropped_position(work, dpi, size, p.window, p.anchor).unwrap();
                    let want = position.unwrap();
                    assert!(
                        (back.x - want.x).abs() < 1e-3 && (back.y - want.y).abs() < 1e-3,
                        "{want:?} came back as {back:?} on {work:?} at {dpi} {size:?}"
                    );
                }
            }
        }
        // The default spot comes back as the default: nothing to reset.
        let p = placement(FULL_HD, 1.0, OverlaySize::Medium, None);
        assert_eq!(
            dropped_position(FULL_HD, 1.0, OverlaySize::Medium, p.window, p.anchor),
            None
        );
    }

    #[test]
    fn the_pill_and_its_window_stay_inside_the_work_area_at_every_edge_and_corner() {
        let monitors = [
            (FULL_HD, 1.0),
            // Taskbar on the left, 125%.
            (
                Rect {
                    left: 62,
                    top: 0,
                    right: 1920,
                    bottom: 1080,
                },
                1.25,
            ),
            // A small monitor above the primary, 175%.
            (
                Rect {
                    left: 200,
                    top: -900,
                    right: 1640,
                    bottom: 0,
                },
                1.75,
            ),
        ];
        let corners = [0.0, 0.5, 1.0];
        for (work, dpi) in monitors {
            for size in [OverlaySize::Small, OverlaySize::Medium, OverlaySize::Large] {
                let margin = 12.0 * dpi;
                for &x in &corners {
                    for &y in &corners {
                        let p = placement(work, dpi, size, at(x, y));
                        assert!(inside(p.window, work), "{x},{y} {work:?} {size:?}: {p:?}");
                        let (l, t, r, b) = pill(p, dpi, size);
                        let tol = 1e-6;
                        assert!(l >= f64::from(work.left) + margin - tol, "{x},{y} left");
                        assert!(t >= f64::from(work.top) + margin - tol, "{x},{y} top");
                        assert!(r <= f64::from(work.right) - margin + tol, "{x},{y} right");
                        assert!(b <= f64::from(work.bottom) - margin + tol, "{x},{y} bottom");
                        if x == 0.0 {
                            assert!((l - f64::from(work.left) - margin).abs() < 1e-6);
                        }
                        if y == 1.0 {
                            assert!((f64::from(work.bottom) - b - margin).abs() < 1e-6);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn text_in_a_corner_grows_from_the_pill_and_never_leaves_the_screen() {
        // The page widens the pill about its centre and pushes it back inside
        // the window (less the glow room) where it would cross an edge. The
        // window is inside the work area, so the widest two-line text is too.
        for position in [
            None,
            at(0.0, 0.0),
            at(1.0, 0.0),
            at(0.0, 1.0),
            at(1.0, 1.0),
            at(0.5, 1.0),
        ] {
            for (work, dpi) in [
                (FULL_HD, 1.0),
                (
                    Rect {
                        left: 0,
                        top: 0,
                        right: 1366,
                        bottom: 728,
                    },
                    1.5,
                ),
            ] {
                let size = OverlaySize::Large;
                let p = placement(work, dpi, size, position);
                let s = size.scale() * dpi;
                let (w, h) = (
                    f64::from(p.window.right - p.window.left),
                    f64::from(p.window.bottom - p.window.top),
                );
                let pad = GLOW_PAD * s;
                let (text_w, text_h) = (TEXT_WIDTH * s, TEXT_HEIGHT * s);
                let centre_x = fit(p.anchor.x * w, pad + text_w / 2.0, w - pad - text_w / 2.0);
                let centre_y = fit(p.anchor.y * h, pad + text_h / 2.0, h - pad - text_h / 2.0);
                let left = f64::from(p.window.left) + centre_x - text_w / 2.0;
                let top = f64::from(p.window.top) + centre_y - text_h / 2.0;
                assert!(left - pad >= f64::from(work.left) - 1e-6, "{position:?}");
                assert!(top - pad >= f64::from(work.top) - 1e-6, "{position:?}");
                assert!(
                    left + text_w + pad <= f64::from(work.right) + 1e-6,
                    "{position:?}"
                );
                assert!(
                    top + text_h + pad <= f64::from(work.bottom) + 1e-6,
                    "{position:?}"
                );
            }
        }
        // A monitor narrower than the window: centred, not panicking.
        let tiny = Rect {
            left: 0,
            top: 0,
            right: 200,
            bottom: 50,
        };
        let p = placement(tiny, 1.0, OverlaySize::Medium, at(1.0, 1.0));
        assert_eq!(p.window.left + p.window.right, 200);
    }

    #[test]
    fn dropping_near_the_centre_line_or_a_default_margin_snaps_to_it() {
        let size = OverlaySize::Medium;
        // Builds the dragged window around a pill centred at (x, y).
        let drop = |work: Rect, dpi: f64, x: f64, y: f64| {
            let (w, h) = ((336.0 * dpi).round() as i32, (64.0 * dpi).round() as i32);
            let window = Rect {
                left: (x - f64::from(w) / 2.0).round() as i32,
                top: (y - f64::from(h) / 2.0).round() as i32,
                right: (x - f64::from(w) / 2.0).round() as i32 + w,
                bottom: (y - f64::from(h) / 2.0).round() as i32 + h,
            };
            dropped_position(work, dpi, size, window, Anchor { x: 0.5, y: 0.5 })
        };
        // 20 px off the centre line, 23 px below the default top: default.
        assert_eq!(drop(FULL_HD, 1.0, 980.0, 28.0 + 23.0), None);
        // 30 px off the centre line stays there.
        let p = drop(FULL_HD, 1.0, 990.0, 28.0).unwrap();
        assert!((p.x - 990.0 / 1920.0).abs() < 1e-9 && p.y == 0.0);
        // Near the bottom margin line (pill bottom 12 px above the work area).
        let p = drop(FULL_HD, 1.0, 400.0, 1040.0 - 28.0 - 20.0).unwrap();
        assert_eq!(p.y, 1.0);
        assert!((p.x - 400.0 / 1920.0).abs() < 1e-9);
        // Mid-screen is kept as is.
        let p = drop(FULL_HD, 1.0, 1500.0, 700.0).unwrap();
        assert!((p.y - 700.0 / 1040.0).abs() < 1e-9);
        // Dragged partly off screen: clamped, then snapped to the top margin.
        assert_eq!(drop(FULL_HD, 1.0, 960.0, -40.0), None);
        // The snap distance is logical: 24 px at 200% is 48 physical px.
        let hi = Rect {
            left: 0,
            top: 0,
            right: 3840,
            bottom: 2080,
        };
        assert_eq!(drop(hi, 2.0, 1920.0 + 46.0, 56.0 + 40.0), None);
        assert!(drop(hi, 2.0, 1920.0 + 50.0, 56.0).is_some());
    }

    #[test]
    fn a_mac_pill_uses_the_same_rule_with_y_flipped() {
        // A 1440x900 point screen with a 25 pt menu bar and a 70 pt Dock.
        let visible = MacFrame {
            x: 0.0,
            y: 70.0,
            width: 1440.0,
            height: 805.0,
        };
        let work = Rect {
            left: 0,
            top: 25,
            right: 1440,
            bottom: 830,
        };
        for position in [None, at(0.3, 0.8)] {
            let (frame, anchor) = mac_placement(visible, OverlaySize::Medium, position);
            let windows = placement(work, 1.0, OverlaySize::Medium, position);
            assert_eq!(
                frame.width,
                f64::from(windows.window.right - windows.window.left)
            );
            assert_eq!(
                frame.height,
                f64::from(windows.window.bottom - windows.window.top)
            );
            assert_eq!(frame.x, f64::from(windows.window.left));
            // The same gap above the Dock as above the taskbar.
            assert_eq!(frame.y - visible.y, f64::from(830 - windows.window.bottom));
            assert!((anchor.x - windows.anchor.x).abs() < 1e-9);
            assert!((anchor.y - windows.anchor.y).abs() < 1e-9);
            // Dropping the frame where it was gives the position back.
            let back = mac_dropped_position(visible, OverlaySize::Medium, frame, anchor);
            match (back, position) {
                (None, None) => {}
                (Some(b), Some(p)) => assert!((b.x - p.x).abs() < 1e-3 && (b.y - p.y).abs() < 1e-3),
                other => panic!("{other:?}"),
            }
        }
        // A screen to the left of and below the main one keeps its offset.
        let (main, _) = mac_placement(visible, OverlaySize::Medium, None);
        let (side, _) = mac_placement(
            MacFrame {
                x: -1920.0,
                y: -300.0,
                ..visible
            },
            OverlaySize::Medium,
            None,
        );
        assert_eq!(side.x - main.x, -1920.0);
        assert_eq!(side.y - main.y, -370.0);
    }

    #[test]
    fn move_mode_takes_over_the_window_and_a_recording_ends_it() {
        let mut presenter = Presenter::default();
        // A check mark still up gives way; its timer goes stale.
        let (_, timer) = presenter.on_state(RecordingState::Done);
        assert!(presenter.begin_move());
        assert!(!presenter.begin_move(), "already moving");
        assert!(presenter.is_moving());
        assert_eq!(presenter.on_hide_timer(timer.unwrap()), Step::Nothing);
        // A stray idle leaves the pill where the user is dragging it.
        assert_eq!(presenter.on_state(RecordingState::Idle).0, Step::Nothing);
        assert!(presenter.is_moving());
        // Recording always wins: move mode ends and the pill is placed anew.
        assert_eq!(
            presenter.on_state(RecordingState::Recording),
            (Step::Show { place: true }, None)
        );
        assert!(!presenter.is_moving());
        assert!(!presenter.end_move(), "already ended by the Recording");
        assert_eq!(presenter.on_state(RecordingState::Idle).0, Step::Hide);

        // Done or Cancel ends it, and the next Recording places as usual.
        assert!(presenter.begin_move());
        assert!(presenter.end_move());
        assert_eq!(
            presenter.on_state(RecordingState::Recording).0,
            Step::Show { place: true }
        );
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
