//! The Trigger Key on macOS: a listen-only Quartz event tap on its own thread
//! and run loop.
//!
//! Listen-only needs Input Monitoring, not Accessibility, and can never stall
//! or eat the user's typing. The cost is that nothing is swallowed: the Mac
//! presets are modifier-only, so there is no chord key to hide, and the Esc
//! that cancels a Recording still reaches the focused app.
//!
//! The callback classifies the event through [`mac_keys`], feeds
//! [`ChordTracker`] and `try_send`s any signal to the gesture thread. It never
//! blocks. A tap the system disables (it timed out, or secure input took the
//! keyboard) is re-enabled from the callback. Without Input Monitoring the
//! tap cannot be created; the thread asks once, which shows the system
//! prompt, then retries every [`RETRY`] and reports [`is_running`] when the
//! tap comes up. A Trigger Key chosen in the settings window is picked up on
//! the next event, or within [`RETRY`] if a Recording must end.

use std::cell::RefCell;
use std::ffi::c_void;
use std::io;
use std::panic::{self, AssertUnwindSafe};
use std::ptr::{self, NonNull};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::thread;
use std::time::Duration;

use objc2_core_foundation::{CFMachPort, CFRetained, CFRunLoop, kCFRunLoopDefaultMode};
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement,
    CGEventTapProxy, CGEventType, CGPreflightListenEventAccess, CGRequestListenEventAccess,
};

use crate::chord::{ChordTracker, Key, MASK_TAG};
use crate::mac_keys::{self, FlagsChange};
use crate::trigger::Trigger;
use crate::trigger_key::TriggerChord;

/// How often a missing tap is retried and a live one checked.
const RETRY: Duration = Duration::from_secs(2);

/// The chord [`set_chord`] last asked for, packed; the tap thread applies it.
static CHORD: AtomicU16 = AtomicU16::new(0);
/// The tap thread exists, so [`set_chord`] takes effect.
static STARTED: AtomicBool = AtomicBool::new(false);
/// The tap is installed and enabled.
static TAP_LIVE: AtomicBool = AtomicBool::new(false);
/// What [`HEALTH_CHANGED`] was last told [`is_running`] returns.
static REPORTED_OK: AtomicBool = AtomicBool::new(false);
static HEALTH_CHANGED: OnceLock<Box<dyn Fn() + Send + Sync>> = OnceLock::new();

struct TapState {
    tracker: ChordTracker,
    chord: TriggerChord,
    trigger: Trigger,
    /// Kept so the callback can re-enable the tap.
    port: Option<CFRetained<CFMachPort>>,
}

thread_local! {
    /// Only the tap thread touches this: the callback runs on its run loop.
    static STATE: RefCell<Option<TapState>> = const { RefCell::new(None) };
}

/// Starts the tap thread for `chord`. Returns once the thread runs, whether
/// or not Input Monitoring is granted yet; [`is_running`] says whether the
/// tap is live. `health_changed` runs on the tap thread whenever
/// [`is_running`] flips.
pub fn install(
    trigger: Trigger,
    chord: TriggerChord,
    health_changed: impl Fn() + Send + Sync + 'static,
) -> io::Result<()> {
    let _ = HEALTH_CHANGED.set(Box::new(health_changed));
    CHORD.store(chord.pack(), Ordering::Release);
    thread::Builder::new()
        .name("voisu-hotkey".into())
        .spawn(move || run(trigger, chord))?;
    STARTED.store(true, Ordering::Release);
    Ok(())
}

/// Whether the tap is installed and enabled, for the window's "needs
/// attention" line.
pub fn is_running() -> bool {
    TAP_LIVE.load(Ordering::Acquire)
}

/// Switches to a new chord from the next key event. False when the tap
/// thread never started.
#[must_use]
pub fn set_chord(chord: TriggerChord) -> bool {
    CHORD.store(chord.pack(), Ordering::Release);
    STARTED.load(Ordering::Acquire)
}

/// macOS gives no layout name for the Mac presets' keys: they have none.
pub fn key_label(_vk: u8) -> Option<String> {
    None
}

pub fn is_dead_key(_vk: u8) -> bool {
    false
}

fn run(trigger: Trigger, chord: TriggerChord) {
    STATE.with(|cell| {
        *cell.borrow_mut() = Some(TapState {
            tracker: ChordTracker::new(chord),
            chord,
            trigger,
            port: None,
        });
    });
    let Some(run_loop) = CFRunLoop::current() else {
        tracing::error!("hotkey thread has no run loop");
        return;
    };
    let mut asked = false;
    loop {
        if !TAP_LIVE.load(Ordering::Acquire) {
            if !CGPreflightListenEventAccess() && !asked {
                asked = true;
                // Shows the system prompt once; the user grants it in System
                // Settings, and the next retry finds it.
                tracing::info!("asking for Input Monitoring");
                CGRequestListenEventAccess();
            }
            match create_tap(&run_loop) {
                Some(port) => {
                    with_state(|state| {
                        if let Some(signal) = state.tracker.reset() {
                            state.trigger.signal(signal);
                        }
                        state.port = Some(port);
                    });
                    TAP_LIVE.store(true, Ordering::Release);
                    tracing::info!("Trigger Key event tap installed");
                }
                None => tracing::debug!("event tap not available yet"),
            }
            report_health();
        }
        if TAP_LIVE.load(Ordering::Acquire) {
            // Events are handled inside; returns after `RETRY`.
            CFRunLoop::run_in_mode(
                // SAFETY: a static CoreFoundation constant.
                unsafe { kCFRunLoopDefaultMode },
                RETRY.as_secs_f64(),
                false,
            );
            check_tap();
        } else {
            thread::sleep(RETRY);
        }
        with_state(apply_chord);
    }
}

fn create_tap(run_loop: &CFRunLoop) -> Option<CFRetained<CFMachPort>> {
    let mask = [
        CGEventType::KeyDown,
        CGEventType::KeyUp,
        CGEventType::FlagsChanged,
    ]
    .iter()
    .fold(0u64, |mask, kind| mask | 1 << kind.0);
    // SAFETY: `tap_callback` matches `CGEventTapCallBack`, touches only this
    // thread's state and lives for the whole process; no user data is passed.
    let port = unsafe {
        CGEvent::tap_create(
            CGEventTapLocation::SessionEventTap,
            CGEventTapPlacement::HeadInsertEventTap,
            CGEventTapOptions::ListenOnly,
            mask,
            Some(tap_callback),
            ptr::null_mut(),
        )
    }?;
    let source = CFMachPort::new_run_loop_source(None, Some(&port), 0)?;
    // SAFETY: a static CoreFoundation constant.
    run_loop.add_source(Some(&source), unsafe { kCFRunLoopDefaultMode });
    Some(port)
}

/// Re-enables a tap the system switched off, and drops one that is gone
/// (Input Monitoring revoked), so the retry loop builds a new one.
fn check_tap() {
    let mut lost = false;
    with_state(|state| {
        let Some(port) = &state.port else { return };
        if !CGEvent::tap_is_enabled(port) {
            CGEvent::tap_enable(port, true);
        }
        if !port.is_valid() || !CGEvent::tap_is_enabled(port) {
            port.invalidate();
            state.port = None;
            if let Some(signal) = state.tracker.reset() {
                state.trigger.signal(signal);
            }
            lost = true;
        }
    });
    if lost {
        TAP_LIVE.store(false, Ordering::Release);
        tracing::warn!("Trigger Key event tap lost; retrying");
        report_health();
    }
}

fn with_state(f: impl FnOnce(&mut TapState)) {
    STATE.with(|cell| {
        if let Ok(mut state) = cell.try_borrow_mut()
            && let Some(state) = state.as_mut()
        {
            f(state);
        }
    });
}

/// Moves the tracker to the chord [`set_chord`] last stored, ending a held
/// chord's Recording.
fn apply_chord(state: &mut TapState) {
    let chord = TriggerChord::unpack(CHORD.load(Ordering::Acquire));
    if chord != state.chord {
        state.chord = chord;
        if let Some(signal) = state.tracker.set_chord(chord) {
            state.trigger.signal(signal);
        }
        tracing::info!(%chord, "Trigger Key changed");
    }
}

/// Tells the window when [`is_running`] changed since it was last told.
fn report_health() {
    let ok = is_running();
    if REPORTED_OK.swap(ok, Ordering::Relaxed) != ok
        && let Some(health_changed) = HEALTH_CHANGED.get()
    {
        health_changed();
    }
}

fn on_event(kind: CGEventType, event: &CGEvent) {
    // Voisu's own paste keystroke carries the tag.
    let user_data = CGEvent::integer_value_field(Some(event), CGEventField::EventSourceUserData);
    if user_data == MASK_TAG as i64 {
        return;
    }
    let code = CGEvent::integer_value_field(Some(event), CGEventField::KeyboardEventKeycode) as u16;
    let event_flags = CGEvent::flags(Some(event)).0;
    let changes: [Option<(Key, bool)>; 2] = match kind {
        CGEventType::KeyDown => [Some((mac_keys::key(code), true)), None],
        CGEventType::KeyUp => [Some((mac_keys::key(code), false)), None],
        CGEventType::FlagsChanged => match mac_keys::flags_changed(code, event_flags) {
            FlagsChange::Modifier(key, down) => [Some((key, down)), None],
            FlagsChange::Tap(key) => [Some((key, true)), Some((key, false))],
            FlagsChange::Ignore => [None, None],
        },
        _ => [None, None],
    };
    with_state(|state| {
        apply_chord(state);
        for (key, down) in changes.into_iter().flatten() {
            let armed = state.trigger.armed();
            let verdict =
                state
                    .tracker
                    .handle(key, down, || mac_keys::modifiers(event_flags), armed);
            if let Some(signal) = verdict.signal {
                state.trigger.signal(signal);
            }
        }
    });
}

unsafe extern "C-unwind" fn tap_callback(
    _proxy: CGEventTapProxy,
    kind: CGEventType,
    event: NonNull<CGEvent>,
    _user_info: *mut c_void,
) -> *mut CGEvent {
    // A panic must not unwind into the window server's callback: log it and
    // let the event through.
    let handled = panic::catch_unwind(AssertUnwindSafe(|| match kind {
        CGEventType::TapDisabledByTimeout | CGEventType::TapDisabledByUserInput => {
            with_state(|state| {
                // Key-ups may have gone by while it was off.
                if let Some(signal) = state.tracker.reset() {
                    state.trigger.signal(signal);
                }
                if let Some(port) = &state.port {
                    CGEvent::tap_enable(port, true);
                }
            });
            tracing::debug!(?kind, "event tap re-enabled");
        }
        // SAFETY: the system passes a live event for the callback's duration.
        _ => on_event(kind, unsafe { event.as_ref() }),
    }));
    if handled.is_err() {
        tracing::error!(
            ?kind,
            "Trigger Key event handler panicked; event passed through"
        );
    }
    // A listen-only tap's return value is ignored; hand the event back.
    event.as_ptr()
}
