//! Optional GTK feedback observer. It has no command path into the daemon.

use std::cell::{Cell, RefCell};
use std::env;
use std::process::Command as ProcessCommand;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use gtk::prelude::*;
use gtk4 as gtk;
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use voisu_app::config::OverlayPosition;
use voisu_app::feedback::{
    FeedbackBackend, FeedbackCapabilities, FeedbackDegradation, FeedbackSelection,
    OverlayRestartPolicy, SessionKind, after_surface_creation, announcement_for_overlay,
    select_feedback_backend,
};
use voisu_app::local_overlay::TriggerRepeatLatch;
use voisu_app::overlay::move_mode::{
    CancelReason, MoveEnd, MoveKey, MoveListener, MoveMode, MoveReply, MoveRequester,
    save_may_commit,
};
use voisu_app::overlay::placement::{self, MOVE_PILL, Size};
use voisu_app::overlay::waveform::{Waveform, poll_target};
use voisu_app::overlay::{
    LIMIT_APPROACHING_CLASS, LIMIT_WARNING_CLASS, LevelPollAction, LevelPollLatch, LimitWarning,
    LimitWarningLatch, NoSpeechNotifyLatch, ObservedSignal, OverlayPhase, OverlayView,
    PILL_BAR_COUNT, PILL_BAR_GAP, PILL_BAR_WIDTH, PILL_HEIGHT, PILL_MAX_LINES, PILL_WIDTH,
    PositionWatch, PresentationController, PresentationTracker, RecordingNotifyLatch,
    RungNotification, STATUS_POLL_PERIOD, STATUS_ROUND_TRIP_DEADLINE, StatusCadence, StatusWorker,
    TickAction, TickObservation, connect_within, edge_falloff_alpha, fetch_status,
    limit_notification_body, limit_warning_class, limit_warning_from_response,
    notification_rung_choice, phase_glyph, pill_text, poll_tick, read_until_eof_within,
    recording_bar_height, recording_bar_rgb, recording_identity, recording_remaining,
    resting_floor, sweep_brightness, text_pill_text_width, text_pill_width, write_within,
};
use voisu_core::{Command, Request, Response, socket_path};

fn main() {
    let arguments: Vec<_> = env::args().skip(1).collect();
    // The released binary must answer the standard version/help probes with
    // exit 0 before any GTK/overlay work.
    match arguments.as_slice() {
        [flag] if flag == "--version" || flag == "-V" => {
            println!("voisu-overlay {}", env!("CARGO_PKG_VERSION"));
            return;
        }
        [flag] if flag == "--help" || flag == "-h" => {
            println!(
                "voisu-overlay — the optional Voisu feedback observer.\n\nusage: voisu-overlay [--report-backend|--supervise|--version|-V|--help|-h]"
            );
            return;
        }
        _ => {}
    }
    let exit_code = if arguments.as_slice() == ["--supervise"] {
        supervise_overlay()
    } else {
        run_overlay(arguments.as_slice())
    };
    std::process::exit(exit_code);
}

/// Runs the observer under a bounded, process-local restart policy. It only
/// respawns this executable; the daemon is never addressed, signalled, or
/// restarted by this code.
fn supervise_overlay() -> i32 {
    let executable = match env::current_exe() {
        Ok(executable) => executable,
        Err(error) => {
            eprintln!(
                "overlay_feedback backend=none degradation=supervisor-current-exe-error error={error}"
            );
            return 1;
        }
    };
    let started = Instant::now();
    let mut policy = OverlayRestartPolicy::default();
    loop {
        let status = match ProcessCommand::new(&executable).status() {
            Ok(status) => status,
            Err(error) => {
                eprintln!(
                    "overlay_feedback backend=none degradation=supervisor-spawn-failure error={error}"
                );
                return 1;
            }
        };
        if status.success() {
            return 0;
        }
        if !policy.record_failure(started.elapsed()).should_restart() {
            eprintln!("overlay_feedback backend=none degradation=restart-limit-reached");
            // Exit cleanly so a systemd unit invoking --supervise cannot undo
            // this process's bounded policy with an outer failure restart.
            return 0;
        }
        eprintln!(
            "overlay_feedback backend=none degradation=overlay-process-failure action=restart"
        );
        std::thread::sleep(Duration::from_secs(1));
    }
}

fn run_overlay(arguments: &[String]) -> i32 {
    let report_only = match arguments {
        [] => false,
        [flag] if flag == "--report-backend" => true,
        _ => {
            eprintln!("usage: voisu-overlay [--report-backend|--supervise]");
            return 2;
        }
    };

    // No graphical display at all: the persistent journal observer is the
    // truthful last-resort feedback backend, and GTK is never asked to init.
    let preflight = environment_capabilities(false);
    if !preflight.display_available {
        let selection = select_feedback_backend(preflight);
        report(selection);
        return if report_only {
            0
        } else {
            run_journal_feedback(selection)
        };
    }

    // Only a Wayland session can use Layer Shell (rung 1), and probing that
    // needs GTK. Every other display session goes straight to the
    // desktop-notification rung, which talks to org.freedesktop.Notifications
    // and needs no GTK display — so GTK is never initialized there (an X11 /
    // Cinnamon host must not be gated behind a GDK init it does not need).
    if preflight.session != SessionKind::Wayland {
        let selection = select_feedback_backend(preflight);
        report(selection);
        return if report_only {
            0
        } else {
            run_notification_feedback(selection)
        };
    }

    // A Wayland session: initialize GTK to probe Layer Shell. If GTK/GDK cannot
    // init, notifications still work over D-Bus, so fall to the notification
    // rung rather than the journal.
    if let Err(error) = gtk::init() {
        let selection = FeedbackSelection {
            backend: FeedbackBackend::DesktopNotification,
            degradation: Some(FeedbackDegradation::LayerShellUnavailable),
        };
        report_with_error(selection, &error.to_string());
        return if report_only {
            0
        } else {
            run_notification_feedback(selection)
        };
    }

    let capabilities = environment_capabilities(gtk4_layer_shell::is_supported());
    let selection = select_feedback_backend(capabilities);
    report(selection);
    if report_only {
        return 0;
    }

    if selection.backend == FeedbackBackend::LayerShell {
        let application = gtk::Application::builder()
            .application_id("org.voisu.Overlay")
            .build();
        application.connect_activate(move |application| build_feedback(application, selection));
        return i32::from(application.run());
    }

    // Wayland without Layer Shell: rung 2 is skipped, so use the notification
    // rung directly. GTK was initialized only to probe Layer Shell.
    run_notification_feedback(selection)
}

/// Inputs collected outside the pure selector. GTK and Layer Shell APIs stay
/// in this adapter so contract tests need neither a compositor nor a display.
fn environment_capabilities(layer_shell_supported: bool) -> FeedbackCapabilities {
    let wayland_display = env::var("WAYLAND_DISPLAY").ok();
    let x11_display = env::var("DISPLAY").ok();
    // Session detection is the single source of truth in voisu-core, shared with
    // the clipboard backend and doctor; the adapter only gathers the raw facts.
    // Empty values name no endpoint and count as absent.
    let session_type = env::var("XDG_SESSION_TYPE").ok();
    let resolution = voisu_core::resolve_session(
        wayland_display.as_deref(),
        x11_display.as_deref(),
        session_type.as_deref(),
    );
    let has_wayland = wayland_display
        .as_deref()
        .is_some_and(|value| !value.is_empty());
    let has_x11 = x11_display
        .as_deref()
        .is_some_and(|value| !value.is_empty());
    let display_available = match resolution.session {
        SessionKind::Wayland => has_wayland,
        SessionKind::X11 => has_x11,
        SessionKind::Unknown => has_wayland || has_x11,
    };
    FeedbackCapabilities {
        session: resolution.session,
        display_available,
        xwayland_fallback: resolution.xwayland_fallback,
        layer_shell_supported,
    }
}

fn report(selection: FeedbackSelection) {
    eprintln!("{}", selection.report_line());
}

fn report_with_error(selection: FeedbackSelection, error: &str) {
    eprintln!("{} error={error}", selection.report_line());
}

/// Keeps the observer useful where no graphical feedback backend can exist.
/// This has no GTK application or daemon control path: it only polls the
/// public OverlayStatus response and writes state transitions to the journal.
fn run_journal_feedback(selection: FeedbackSelection) -> i32 {
    let _moves =
        refuse_moves("The Overlay has no display to show the pill on, so it can't be moved here.");
    let mut controller = PresentationController::default();
    let mut previous = OverlayView::HIDDEN;
    loop {
        let now = Instant::now();
        let view = match read_status() {
            Some(response) => controller.observe(&response, now),
            None => controller.observe_unreachable(now),
        };
        if view != previous {
            eprintln!(
                "{} phase={}",
                selection.report_line(),
                overlay_phase_label(view.phase)
            );
            previous = view;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

const fn overlay_phase_label(phase: OverlayPhase) -> &'static str {
    match phase {
        OverlayPhase::Hidden => "hidden",
        OverlayPhase::Recording => "recording",
        OverlayPhase::Processing => "processing",
        OverlayPhase::Success => "success",
        OverlayPhase::Failure => "failure",
        OverlayPhase::Loading => "loading",
        OverlayPhase::Unavailable => "unavailable",
        OverlayPhase::NoSpeech => "no_speech",
        OverlayPhase::Copied => "copied",
    }
}

fn build_feedback(application: &gtk::Application, selection: FeedbackSelection) {
    if selection.backend == FeedbackBackend::DesktopNotification {
        install_notification_feedback(application);
        return;
    }

    let window = gtk::ApplicationWindow::builder()
        .application(application)
        .default_width(PILL_WIDTH)
        .default_height(PILL_HEIGHT)
        .resizable(false)
        .decorated(false)
        .focusable(false)
        .can_focus(false)
        .build();

    if selection.backend == FeedbackBackend::LayerShell {
        window.init_layer_shell();
        window.set_layer(Layer::Overlay);
        // Anchored to the top-left corner only and pushed into place with
        // margins (see `place_pill`), so the pill can sit anywhere.
        for (edge, anchored) in [
            (Edge::Top, true),
            (Edge::Left, true),
            (Edge::Bottom, false),
            (Edge::Right, false),
        ] {
            window.set_anchor(edge, anchored);
        }
        window.set_keyboard_mode(KeyboardMode::None);
        window.set_exclusive_zone(-1);
    }
    // Move requests from `voisu overlay-position move` and the TUI. Without
    // the socket the pill still works; it just can't be dragged.
    let move_listener = match MoveListener::bind() {
        Ok(listener) => Some(Rc::new(listener)),
        Err(error) => {
            eprintln!("overlay_move listener=unavailable error={error}");
            None
        }
    };
    // Fallback (regular-surface) path, e.g. GNOME/Mutter which does not
    // implement zwlr_layer_shell_v1. The window is already frameless
    // (decorated(false)) and non-resizable at the capsule's default size, so it
    // reads as an overlay rather than a normal app window. Corner positioning is
    // best-effort only: Wayland gives a regular toplevel no global positioning
    // API, so we do NOT fight the compositor for a screen corner — it places the
    // window, and resurfacing (below) keeps it visible. Keep-above is likewise
    // impossible for a plain toplevel, so `install_surface_feedback` re-presents
    // the window on each transition into a visible phase instead.
    // Realization creates the GdkSurface on the first real show. A present
    // surface is honest proof of local surface creation, so install the
    // click-through input region. GTK realizing without a surface is the only
    // surface failure detectable in-process; fall back to a desktop
    // notification then. A compositor that REJECTS the surface — e.g. a Layer
    // Shell protocol error — instead terminates the process, and the bounded
    // `--supervise` policy converts that into explicit degraded behavior, never
    // a false in-process timer on a healthy compositor.
    let switched = Rc::new(Cell::new(false));
    let status_worker = Rc::new(StatusWorker::spawn());
    window.connect_realize({
        let application = application.clone();
        let switched = Rc::clone(&switched);
        let status_worker = Rc::clone(&status_worker);
        let move_listener = move_listener.clone();
        move |window| match window.surface() {
            Some(surface) => {
                let empty_region = gtk::cairo::Region::create();
                surface.set_input_region(Some(&empty_region));
            }
            None => {
                let effective = after_surface_creation(selection, false);
                report(effective);
                switched.set(true);
                window.set_visible(false);
                status_worker.stop();
                if let Some(listener) = &move_listener {
                    listener.refuse(NO_SURFACE_MOVE);
                }
                install_notification_feedback(&application);
            }
        }
    });

    let label = gtk::Label::builder()
        .label("")
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .justify(gtk::Justification::Center)
        .build();
    label.add_css_class("state-label");
    label.set_hexpand(true);
    label.set_vexpand(true);
    let meter = gtk::DrawingArea::builder().content_height(24).build();
    // Graphics-first phases (Recording/Processing/NoSpeech) hide the label and
    // glyph widgets entirely (see render_surface), so the meter is the only
    // visible child and must be free to fill the whole pill interior.
    meter.set_hexpand(true);
    meter.set_halign(gtk::Align::Fill);
    // The pre-waveform glyph slot ("⚠" on Failure, "✓" on Success). It is
    // hidden whenever empty (see render_surface) and centers itself when it
    // is the capsule's only visible child.
    let glyph = gtk::Label::builder().label("").build();
    glyph.add_css_class("meter");
    glyph.set_hexpand(true);
    glyph.set_halign(gtk::Align::Center);
    let waveform = Rc::new(RefCell::new(Waveform::default()));
    let reduced_motion = gtk::Settings::default()
        .map(|settings| !settings.is_gtk_enable_animations())
        .unwrap_or(true);
    // Reset on each entry into Processing so the sweep always enters from the
    // left, as on Windows.
    let sweep_started = Rc::new(Cell::new(Instant::now()));
    // What the meter is currently painted for. One cell, not two: the phase and
    // its warning stage are always set together, and the draw func runs on
    // GTK's schedule rather than the poll's, so it must never see a half-update.
    let rendered = Rc::new(Cell::new(MeterState::HIDDEN));
    meter.set_draw_func({
        let waveform = Rc::clone(&waveform);
        let rendered = Rc::clone(&rendered);
        let sweep_started = Rc::clone(&sweep_started);
        move |_, context, width, height| {
            draw_meter(
                context,
                width,
                height,
                &waveform.borrow(),
                rendered.get(),
                sweep_started.get(),
                reduced_motion,
            );
        }
    });
    let capsule = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    // The capsule must BE the 120x32 pill, not a smaller box floated inside it.
    // GTK widget margins sit OUTSIDE the styled background, so set_margin_* would
    // shrink the drawn pill and let peak bars bleed past its rounded corners.
    // Fill the whole window instead and push the interior spacing inward with CSS
    // `padding` (which GTK paints INSIDE the background) — see the CSS below.
    // Result: capsule 120x32 (radius 16), meter interior 104x24. A text state
    // widens it (see `size_pill`).
    capsule.set_hexpand(true);
    capsule.set_vexpand(true);
    capsule.append(&label);
    capsule.append(&meter);
    capsule.append(&glyph);
    window.set_child(Some(&capsule));
    window.set_visible(false);

    let css = gtk::CssProvider::new();
    css.load_from_data(
        "window.background { background: transparent; }
         .capsule { background: rgba(23, 25, 29, 0.96); border-radius: 16px; border: 1px solid transparent; padding: 3px 7px; }
         .capsule.failure, .capsule.loading, .capsule.unavailable, .capsule.copied, .capsule.nospeech, .capsule.limitsoon { padding: 4px 14px; }
         .capsule .state-label, .capsule .meter { color: #F4F5F7; font-size: 10pt; font-weight: 600; }
         .capsule.processing .state-label, .capsule.processing .meter { color: #8FB4FF; }
         .capsule.success .state-label, .capsule.success .meter { color: #65D6A0; font-size: 13pt; }
         .capsule.failure { border: 1px solid rgba(255, 138, 138, 0.9); box-shadow: 0 0 8px rgba(255, 138, 138, 0.35); }
         .capsule.failure .state-label, .capsule.failure .meter { color: #FF8A8A; }
         .capsule.loading .state-label, .capsule.loading .meter { color: #8FB4FF; }
         .capsule.unavailable .state-label, .capsule.unavailable .meter { color: #FFB454; }
         .capsule.nospeech .state-label, .capsule.nospeech .meter { color: #FFB454; }
         .capsule.copied .state-label { color: #FFB454; }
         .capsule.limitsoon .state-label { color: #FFB454; }
         .capsule.limitwarn { border: 1px solid rgba(255, 138, 138, 0.9); box-shadow: 0 0 8px rgba(255, 138, 138, 0.35); }
         .capsule.moving { border: 1px dashed rgba(244, 245, 247, 0.55); padding: 0 3px; }
         .move-button { min-width: 24px; min-height: 24px; padding: 0; margin: 0; border-radius: 12px; background: transparent; background-image: none; border: 0; box-shadow: none; outline: none; color: #F4F5F7; font-size: 11pt; font-weight: 700; }
         .move-button:hover { background: rgba(244, 245, 247, 0.14); }
         .move-button:active { background: rgba(244, 245, 247, 0.22); }
         .move-button.confirm:hover { background: rgba(101, 214, 160, 0.28); }",
    );
    capsule.add_css_class("capsule");
    gtk::style_context_add_provider_for_display(
        &gtk::prelude::RootExt::display(&window),
        &css,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );

    // Hidden at Idle: no startup present() and no styled empty-capsule flash
    // (window.set_visible(false) above stays in effect). The window becomes
    // visible only when a visible phase arrives via render_surface, and the
    // realize probe above runs on that first real show — not on a startup
    // flash. Polling starts immediately so an early Recording is shown without
    // the old 500 ms + 200 ms grace.
    install_surface_feedback(
        application.clone(),
        selection,
        CapsuleFeedback {
            window,
            label,
            meter,
            glyph,
            capsule,
            waveform,
            rendered,
            sweep_started,
            reduced_motion,
            switched,
            status_worker,
            move_listener,
        },
    );
}

/// Owned handles for the capsule's widgets and shared render state, moved into
/// the poll loop's closure. Grouping them keeps `install_surface_feedback` under
/// clippy's `too_many_arguments` limit; construction and destructuring are
/// plain moves — no behavior change. (The borrowed view handed to
/// `render_surface` each tick is `CapsuleSurface`.)
struct CapsuleFeedback {
    window: gtk::ApplicationWindow,
    label: gtk::Label,
    meter: gtk::DrawingArea,
    glyph: gtk::Label,
    capsule: gtk::Box,
    waveform: Rc<RefCell<Waveform>>,
    rendered: Rc<Cell<MeterState>>,
    sweep_started: Rc<Cell<Instant>>,
    reduced_motion: bool,
    switched: Rc<Cell<bool>>,
    status_worker: Rc<StatusWorker>,
    move_listener: Option<Rc<MoveListener>>,
}

fn install_surface_feedback(
    application: gtk::Application,
    // Rung 2 is gone, so the only windowed backend is Layer Shell and the
    // fallback branch below is inert; the selection is no longer consulted here.
    _selection: FeedbackSelection,
    feedback: CapsuleFeedback,
) {
    let CapsuleFeedback {
        window,
        label,
        meter,
        glyph,
        capsule,
        waveform,
        rendered,
        sweep_started,
        reduced_motion,
        switched,
        status_worker,
        move_listener,
    } = feedback;
    let controller = Rc::new(RefCell::new(PresentationController::default()));
    // Rung 2 (a plain GTK window) is skipped, so the only windowed backend that
    // reaches here is Layer Shell, which the compositor keeps above and which
    // has no "buried window" problem. The resurface/notify fallback behavior is
    // therefore never needed on this path.
    let is_fallback = false;
    let tracker = Rc::new(RefCell::new(PresentationTracker::default()));
    let notify_latch = Rc::new(RefCell::new(RecordingNotifyLatch::default()));
    let no_speech_latch = Rc::new(RefCell::new(NoSpeechNotifyLatch::default()));
    let limit_latch = Rc::new(RefCell::new(LimitWarningLatch::default()));
    let level_latch = Rc::new(RefCell::new(LevelPollLatch::default()));
    let level_worker = Rc::new(RefCell::new(None::<LevelWorker>));
    let animation = MeterAnimation::default();
    let trigger_latch = Rc::new(RefCell::new(TriggerRepeatLatch::default()));
    let cadence = Rc::new(Cell::new(STATUS_POLL_PERIOD));
    let tick_cadence = Rc::clone(&cadence);
    let mut position_watch = PositionWatch::default();
    // Where the pill goes, and the margins last applied for it.
    let position = Rc::new(Cell::new(voisu_app::config::overlay_position()));
    let placed = Cell::new(None);
    // The output the pill is bound to, once a move has picked one.
    let output = Rc::new(RefCell::new(None::<gtk::gdk::Monitor>));
    // Set when the config file changes or a move is saved: re-read the
    // position, so a change lands even while a result pill is on screen.
    let reread = Rc::new(Cell::new(false));
    let config_monitor = watch_config(&reread);
    let busy = Rc::new(Cell::new(false));
    let idle_as_of = Rc::new(Cell::new(None::<Instant>));
    let moves = move_listener.map(|listener| {
        let moves = Rc::new(RefCell::new(MoveController {
            application: application.clone(),
            main_window: window.clone(),
            listener,
            position: Rc::clone(&position),
            output: Rc::clone(&output),
            reread: Rc::clone(&reread),
            busy: Rc::clone(&busy),
            idle_as_of: Rc::clone(&idle_as_of),
            session: None,
        }));
        // Its own clock, so ✓, ✗ and the timeouts never wait on a slow daemon.
        gtk::glib::timeout_add_local(MOVE_TICK, {
            let moves = Rc::clone(&moves);
            move || {
                moves.borrow_mut().tick();
                gtk::glib::ControlFlow::Continue
            }
        });
        moves
    });
    poll_at_cadence(cadence, move || {
        let _monitor = &config_monitor;
        if switched.get() {
            // A genuine surface-creation failure handed feedback to the
            // notification backend; stop driving the retired window.
            animation.stop();
            if let Some(worker) = level_worker.borrow_mut().take() {
                worker.stop();
            }
            status_worker.stop();
            return gtk::glib::ControlFlow::Break;
        }
        let Some(observation) = status_worker.take_latest() else {
            return gtk::glib::ControlFlow::Continue;
        };
        tick_cadence.set(observation.period);
        let now = Instant::now();
        // Status I/O runs on StatusWorker. The GTK tick only takes the newest
        // finished round trip, so a stalled daemon cannot freeze the loop.
        let (view, signal, warning, identity, remaining) = match observation.response {
            Some(response) => {
                let view = controller.borrow_mut().observe(&response, now);
                // Headroom and identity are read off the same reply the phase
                // came from, so the capsule can never be amber for a Recording
                // that already ended, nor latched against a different one.
                (
                    view,
                    ObservedSignal::Reachable(view.phase),
                    limit_warning_from_response(&response),
                    recording_identity(&response).map(str::to_owned),
                    recording_remaining(&response),
                )
            }
            None => (
                controller.borrow_mut().observe_unreachable(now),
                ObservedSignal::Unreachable,
                None,
                None,
                None,
            ),
        };
        if view.phase == OverlayPhase::Processing
            && rendered.get().phase != OverlayPhase::Processing
        {
            sweep_started.set(Instant::now());
        }
        // A Recording ends move mode at once, so the pill shows it. A ✓
        // pressed meanwhile waits for a status sent after it (`idle_as_of`),
        // so a Recording that started first always wins over the save.
        busy.set(matches!(
            view.phase,
            OverlayPhase::Recording | OverlayPhase::Processing
        ));
        idle_as_of.set((!busy.get()).then_some(observation.requested_at));
        if let Some(moves) = &moves
            && busy.get()
        {
            moves.borrow_mut().cancel_for_recording();
        }
        // Re-read the position whenever a Recording begins, the pill comes up
        // from hidden or the config changes, off this thread; the answer is
        // applied below.
        if reread.take() {
            position_watch.refresh();
        }
        let before = rendered.get().phase;
        if view.phase != OverlayPhase::Hidden
            && (before == OverlayPhase::Hidden
                || (view.phase == OverlayPhase::Recording && before != OverlayPhase::Recording))
        {
            position_watch.refresh();
        }
        if let Some(saved) = position_watch.take() {
            position.set(saved);
        }
        // While the pill is being moved, the move window stands in for it.
        let moving = moves
            .as_ref()
            .is_some_and(|moves| moves.borrow().is_moving());
        if !moving
            && let Some(pill) = render_surface(
                CapsuleSurface {
                    window: &window,
                    label: &label,
                    meter: &meter,
                    glyph: &glyph,
                    container: &capsule,
                },
                &rendered,
                view,
                warning,
                remaining,
            )
        {
            // Placed before it is shown, so it never appears at a stale spot.
            if window.is_layer_window() {
                place_pill(&window, position.get(), pill, &placed, &output.borrow());
            }
            window.set_visible(true);
            meter.queue_draw();
        }
        match level_latch.borrow_mut().observe(signal) {
            LevelPollAction::Arm => {
                // A new Recording starts from silence, never from the last
                // one's tail.
                waveform.borrow_mut().reset();
                *level_worker.borrow_mut() = Some(LevelWorker::spawn());
            }
            LevelPollAction::Disarm => {
                if let Some(worker) = level_worker.borrow_mut().take() {
                    worker.stop();
                }
                waveform.borrow_mut().reset();
                meter.queue_draw();
            }
            LevelPollAction::Keep => {}
        }
        // Frames are drawn only while something on the meter moves: live
        // levels while Recording, the light sweep while Processing (static
        // under reduced motion). Every other phase, hidden included, has no
        // tick callback at all.
        let animating = match view.phase {
            OverlayPhase::Recording => true,
            OverlayPhase::Processing => !reduced_motion,
            _ => false,
        };
        if animating {
            animation.start(&meter, &waveform, &level_worker);
        } else {
            animation.stop();
        }
        // render_surface realizes the window on its first real show; the realize
        // callback may have found no surface and handed feedback to the
        // notification backend, setting `switched`. The pure `poll_tick` owns the
        // ordering: it breaks on that handoff BEFORE the tracker or latch observe
        // this tick, so a retired window is never re-presented and no duplicate
        // notification is sent. The bin only runs the resulting side effects.
        // Bind before matching: a `match` scrutinee keeps its temporary `RefMut`
        // guards alive for the whole block, and the warning arm below re-borrows
        // `limit_latch` to commit — inside this C callback that double borrow is
        // a non-unwinding panic that aborts the whole overlay process.
        let action = poll_tick(
            switched.get(),
            is_fallback,
            TickObservation {
                view,
                signal,
                identity: identity.as_deref(),
                warning,
            },
            &mut tracker.borrow_mut(),
            &mut notify_latch.borrow_mut(),
            &mut no_speech_latch.borrow_mut(),
            &mut limit_latch.borrow_mut(),
        );
        match action {
            TickAction::Break => {
                animation.stop();
                if let Some(worker) = level_worker.borrow_mut().take() {
                    worker.stop();
                }
                gtk::glib::ControlFlow::Break
            }
            TickAction::Continue {
                resurface,
                notify,
                notify_no_speech,
                notify_limit,
            } => {
                // Wayland denies a plain toplevel keep-above; re-present it on
                // each transition into a visible phase to resurface above
                // occluders.
                if resurface {
                    window.present();
                }
                // A buried fallback window may be missed on GNOME, so signal
                // Recording start with a bounded desktop notification. Failure
                // here never breaks the overlay — send_notification cannot panic
                // and its delivery is the compositor's concern.
                let first_identity = trigger_latch.borrow_mut().observe(identity.as_deref());
                if notify && (identity.is_none() || first_identity) {
                    let notification = gtk::gio::Notification::new("Voisu");
                    notification.set_body(Some(announcement_for_overlay(view)));
                    application.send_notification(Some("overlay-recording"), &notification);
                }
                if notify_no_speech {
                    let notification = gtk::gio::Notification::new("Voisu");
                    notification.set_body(Some(OverlayView::no_speech().visible_label));
                    application.send_notification(Some("overlay-nospeech"), &notification);
                }
                // Approaching-limit warning. Same fire-and-forget contract as
                // the notifications above: it cannot fail into, block, or
                // otherwise touch the Recording's delivery, which the daemon
                // owns and which this process only observes.
                if notify_limit.is_some() {
                    if let Some(body) = notify_limit
                        .zip(remaining)
                        .and_then(|(warning, left)| limit_notification_body(warning, left))
                    {
                        let notification = gtk::gio::Notification::new("Voisu");
                        notification.set_body(Some(body.as_str()));
                        application.send_notification(Some("overlay-limit"), &notification);
                    }
                    // send_notification cannot refuse and suppression is
                    // deliberate, so this path always settles the stage. Left
                    // outstanding it would be rollback-able by a later caller.
                    limit_latch.borrow_mut().commit();
                }
                gtk::glib::ControlFlow::Continue
            }
        }
    });
}

/// Runs `tick` on the GTK main loop at the period last stored in `cadence`.
/// A glib timeout's interval is fixed once added, so when the tick asks for a
/// different cadence (fast while Processing, slow otherwise) the current
/// source is retired and an identical one is re-added at the new period.
fn poll_at_cadence(
    cadence: Rc<Cell<Duration>>,
    tick: impl FnMut() -> gtk::glib::ControlFlow + 'static,
) {
    let tick: Rc<RefCell<dyn FnMut() -> gtk::glib::ControlFlow>> = Rc::new(RefCell::new(tick));
    schedule_at_cadence(cadence, tick);
}

fn schedule_at_cadence(
    cadence: Rc<Cell<Duration>>,
    tick: Rc<RefCell<dyn FnMut() -> gtk::glib::ControlFlow>>,
) {
    let period = cadence.get();
    gtk::glib::timeout_add_local(period, move || {
        let flow = (tick.borrow_mut())();
        if matches!(flow, gtk::glib::ControlFlow::Break) {
            return gtk::glib::ControlFlow::Break;
        }
        if cadence.get() != period {
            schedule_at_cadence(Rc::clone(&cadence), Rc::clone(&tick));
            return gtk::glib::ControlFlow::Break;
        }
        gtk::glib::ControlFlow::Continue
    });
}

/// How often move mode looks for requests, ✓/✗, a vanished client and the
/// idle timeout. Its own clock, independent of the status poll.
const MOVE_TICK: Duration = Duration::from_millis(100);

/// Why a move is refused on the notification fallback.
const NO_LAYER_SHELL_MOVE: &str = "This desktop has no layer-shell support (the Overlay is using notifications), so the pill can't be moved here.";

/// Why a move is refused after the pill's surface failed.
const NO_SURFACE_MOVE: &str = "The Overlay couldn't show its pill on this desktop (it is using notifications), so it can't be moved.";

/// One move-mode button, round, in logical pixels (Windows' size).
const MOVE_BUTTON: i32 = 24;

/// Space between each button and the bars.
const MOVE_BUTTON_GAP: i32 = 5;

/// Holds the move socket on a backend with no pill to drag, so a move request
/// gets a clear refusal instead of "isn't running". Keep the result alive.
fn refuse_moves(reason: &str) -> Option<MoveListener> {
    MoveListener::bind_refusing(reason)
        .map_err(|error| eprintln!("overlay_move listener=unavailable error={error}"))
        .ok()
}

/// Runs move mode on the GTK thread: takes requests off the move socket,
/// shows the move window in place of the pill, and reports how it ended.
struct MoveController {
    application: gtk::Application,
    main_window: gtk::ApplicationWindow,
    listener: Rc<MoveListener>,
    /// Where the pill goes, as last read from the config. A move starts from
    /// it; a save reaches it only through the config, once the client has
    /// written it.
    position: Rc<Cell<OverlayPosition>>,
    /// The output the pill is bound to; a saved move binds it to the output
    /// the move happened on.
    output: Rc<RefCell<Option<gtk::gdk::Monitor>>>,
    /// Asks the status tick to re-read the saved position.
    reread: Rc<Cell<bool>>,
    /// A Recording or its processing is on screen.
    busy: Rc<Cell<bool>>,
    /// When the newest status that showed Voisu idle was sent.
    idle_as_of: Rc<Cell<Option<Instant>>>,
    session: Option<MoveSession>,
}

/// One move in progress.
struct MoveSession {
    window: gtk::ApplicationWindow,
    requester: MoveRequester,
    state: Rc<RefCell<MoveState>>,
}

/// Shared between the move window's handlers and the controller's tick.
struct MoveState {
    mode: MoveMode,
    /// Set by ✓, ✗, Enter or Esc, with when; acted on at the next tick (a
    /// save once [`save_may_commit`] allows it).
    end: Option<(MoveEnd, Instant)>,
}

impl MoveController {
    fn is_moving(&self) -> bool {
        self.session.is_some()
    }

    fn tick(&mut self) {
        let now = Instant::now();
        while let Some(mut requester) = self.listener.take() {
            let refusal = if self.session.is_some() {
                Some("The Overlay is already being moved.")
            } else if self.busy.get() {
                Some("Voisu is busy with a Recording. Try again when it finishes.")
            } else if !self.main_window.is_layer_window() {
                Some(NO_LAYER_SHELL_MOVE)
            } else {
                None
            };
            match refusal {
                Some(reason) => {
                    requester.reply(&MoveReply::Error(reason.to_owned()));
                }
                None => self.start(requester, now),
            }
        }
        let end = self.session.as_ref().and_then(|session| {
            let state = session.state.borrow();
            match state.end {
                // A Recording seen first wins; otherwise the save waits for
                // a status sent after ✓.
                Some((MoveEnd::Save(_), _)) if self.busy.get() => {
                    Some(MoveEnd::Cancel(CancelReason::Recording))
                }
                Some((MoveEnd::Save(position), pressed)) => {
                    save_may_commit(pressed, self.idle_as_of.get(), now)
                        .then_some(MoveEnd::Save(position))
                }
                Some((end, _)) => Some(end),
                None => state
                    .mode
                    .tick(now, self.busy.get(), session.requester.gone()),
            }
        });
        if let Some(end) = end {
            self.finish(end);
        }
    }

    /// A Recording started: drop the move so the pill can show it.
    fn cancel_for_recording(&mut self) {
        if self.session.is_some() {
            self.finish(MoveEnd::Cancel(CancelReason::Recording));
        }
    }

    fn start(&mut self, mut requester: MoveRequester, now: Instant) {
        let original = self.position.get();
        let state = Rc::new(RefCell::new(MoveState {
            mode: MoveMode::start(original, now),
            end: None,
        }));
        let window = build_move_window(&self.application, &state);
        if !requester.reply(&MoveReply::Moving) {
            window.destroy();
            return;
        }
        self.main_window.set_visible(false);
        window.present();
        eprintln!("overlay_move start position={original}");
        self.session = Some(MoveSession {
            window,
            requester,
            state,
        });
    }

    fn finish(&mut self, end: MoveEnd) {
        let Some(mut session) = self.session.take() else {
            return;
        };
        // The client writes the config: the Overlay's sandbox can't.
        let told = session.requester.reply(&end.reply());
        // The output the move happened on, read before the window goes.
        let moved_on = session.window.surface().and_then(|surface| {
            gtk::prelude::RootExt::display(&session.window).monitor_at_surface(&surface)
        });
        session.window.destroy();
        match end {
            MoveEnd::Save(position) => {
                eprintln!("overlay_move end=save position={position} told={told}");
                // Not adopted here: the pill takes the spot by re-reading the
                // config (the file watch fires when the client writes it), so
                // a save that fails, or a client that left before it could
                // write, leaves the pill where the config says.
                if told && let Some(monitor) = moved_on {
                    *self.output.borrow_mut() = Some(monitor);
                }
                self.reread.set(true);
            }
            MoveEnd::Cancel(reason) => {
                eprintln!("overlay_move end=cancel reason={reason:?}");
            }
        }
        // The status tick shows the pill again, if there is one to show.
    }
}

/// The move-mode surface: a transparent Layer Shell window over the whole
/// output holding one wider pill, as on Windows: ✗, the idle bars and ✓,
/// with a dashed outline round the lot. Only the pill takes input; everywhere
/// else clicks through. The keyboard is on demand, for Enter and Esc, and only
/// while it exists. The window is left to the compositor's choice of output
/// (the focused one), and a save binds the pill to that output.
fn build_move_window(
    application: &gtk::Application,
    state: &Rc<RefCell<MoveState>>,
) -> gtk::ApplicationWindow {
    let window = gtk::ApplicationWindow::builder()
        .application(application)
        .decorated(false)
        // Resizable, or GTK keeps its 200x200 default instead of taking the
        // whole output the four anchors ask the compositor for.
        .title("Move the Voisu Overlay")
        .build();
    window.init_layer_shell();
    window.set_namespace(Some("voisu-overlay-move"));
    window.set_layer(Layer::Overlay);
    for edge in [Edge::Top, Edge::Bottom, Edge::Left, Edge::Right] {
        window.set_anchor(edge, true);
    }
    window.set_exclusive_zone(-1);
    window.set_keyboard_mode(KeyboardMode::OnDemand);
    window.connect_realize(|window| {
        if let Some(surface) = window.surface() {
            surface.set_input_region(Some(&gtk::cairo::Region::create()));
        }
    });

    let bars = gtk::DrawingArea::builder().content_height(24).build();
    bars.set_hexpand(true);
    bars.set_valign(gtk::Align::Center);
    bars.set_draw_func(|_, context, width, height| draw_resting(context, width, height));

    let button = |glyph: &str, class: &str, label: &str| {
        let button = gtk::Button::with_label(glyph);
        button.add_css_class("move-button");
        button.add_css_class(class);
        button.set_size_request(MOVE_BUTTON, MOVE_BUTTON);
        button.set_valign(gtk::Align::Center);
        button.set_focusable(false);
        button.set_cursor_from_name(Some("pointer"));
        button.update_property(&[gtk::accessible::Property::Label(label)]);
        button
    };
    let cancel = button("✗", "cancel", "Cancel moving the Overlay");
    let confirm = button("✓", "confirm", "Save the Overlay position");
    for (button, save) in [(&confirm, true), (&cancel, false)] {
        let state = Rc::clone(state);
        button.connect_clicked(move |_| {
            let mut state = state.borrow_mut();
            let end = if save {
                state.mode.confirm()
            } else {
                state.mode.cancel()
            };
            state.end = Some((end, Instant::now()));
        });
    }

    // ✗ · bars · ✓ in one capsule, exactly MOVE_PILL in size (CSS border
    // and padding included), so what is painted, placed, saved and clickable
    // are the same rectangle.
    let pill = gtk::Box::new(gtk::Orientation::Horizontal, MOVE_BUTTON_GAP);
    pill.add_css_class("capsule");
    pill.add_css_class("moving");
    pill.set_size_request(MOVE_PILL.0, MOVE_PILL.1);
    pill.append(&cancel);
    pill.append(&bars);
    pill.append(&confirm);
    pill.set_cursor_from_name(Some("grab"));
    pill.update_property(&[gtk::accessible::Property::Label(
        "Voisu Overlay. Drag to move; Enter saves, Escape cancels.",
    )]);

    let fixed = gtk::Fixed::new();
    fixed.put(&pill, 0.0, 0.0);
    // Hidden until the output's size is known and the pill is in place.
    fixed.set_opacity(0.0);
    // The base child is only there to report the output's size.
    let base = gtk::DrawingArea::new();
    base.set_hexpand(true);
    base.set_vexpand(true);
    let layers = gtk::Overlay::new();
    layers.set_child(Some(&base));
    layers.add_overlay(&fixed);
    window.set_child(Some(&layers));

    let output = Rc::new(Cell::new(None::<Size>));
    // The pill's real size: its request, or more if its CSS ever asks for
    // more. Placement, the saved centre and the input region all use it.
    let pill_size = {
        let pill = pill.clone();
        move || -> Size {
            let (_, width, _, _) = pill.measure(gtk::Orientation::Horizontal, -1);
            let (_, height, _, _) = pill.measure(gtk::Orientation::Vertical, width);
            (width.max(MOVE_PILL.0), height.max(MOVE_PILL.1))
        }
    };
    // Puts the pill at the current position and the input region around
    // exactly it.
    let place: Rc<dyn Fn()> = Rc::new({
        let window = window.downgrade();
        let fixed = fixed.clone();
        let pill = pill.clone();
        let output = Rc::clone(&output);
        let state = Rc::clone(state);
        let pill_size = pill_size.clone();
        move || {
            let (Some(window), Some(output)) = (window.upgrade(), output.get()) else {
                return;
            };
            let size = pill_size();
            let origin = placement::pill_margins(state.borrow().mode.current(), output, size);
            fixed.move_(&pill, f64::from(origin.0), f64::from(origin.1));
            fixed.set_opacity(1.0);
            if let Some(surface) = window.surface() {
                let region = gtk::cairo::Region::create_rectangle(&gtk::cairo::RectangleInt::new(
                    origin.0, origin.1, size.0, size.1,
                ));
                surface.set_input_region(Some(&region));
            }
        }
    });
    base.connect_resize({
        let output = Rc::clone(&output);
        let place = Rc::clone(&place);
        move |_, width, height| {
            if width > 0 && height > 0 {
                output.set(Some((width, height)));
                place();
            }
        }
    });

    // Dragging: the gesture sits on the still Fixed, not the moving pill, so
    // its offsets stay true while the pill follows the pointer. A press on
    // ✓ or ✗ is theirs, never a drag.
    let drag = gtk::GestureDrag::new();
    let grabbed = Rc::new(Cell::new(None::<(f64, f64)>));
    drag.connect_drag_begin({
        let output = Rc::clone(&output);
        let state = Rc::clone(state);
        let grabbed = Rc::clone(&grabbed);
        let pill = pill.clone();
        let fixed = fixed.clone();
        let pill_size = pill_size.clone();
        move |gesture, x, y| {
            let picked = fixed.pick(x, y, gtk::PickFlags::DEFAULT);
            let on_pill = picked.as_ref().is_some_and(|widget| {
                (widget == pill.upcast_ref::<gtk::Widget>() || widget.is_ancestor(&pill))
                    && widget.ancestor(gtk::Button::static_type()).is_none()
            });
            let Some(output) = output.get().filter(|_| on_pill) else {
                gesture.set_state(gtk::EventSequenceState::Denied);
                return;
            };
            gesture.set_state(gtk::EventSequenceState::Claimed);
            let size = pill_size();
            let origin = placement::pill_margins(state.borrow().mode.current(), output, size);
            grabbed.set(Some((
                f64::from(origin.0) + f64::from(size.0) / 2.0,
                f64::from(origin.1) + f64::from(size.1) / 2.0,
            )));
            pill.set_cursor_from_name(Some("grabbing"));
        }
    });
    drag.connect_drag_update({
        let output = Rc::clone(&output);
        let state = Rc::clone(state);
        let grabbed = Rc::clone(&grabbed);
        let place = Rc::clone(&place);
        let pill_size = pill_size.clone();
        move |_, dx, dy| {
            let (Some(start), Some(output)) = (grabbed.get(), output.get()) else {
                return;
            };
            let position =
                placement::position_at((start.0 + dx, start.1 + dy), output, pill_size());
            state.borrow_mut().mode.drag_to(position, Instant::now());
            place();
        }
    });
    drag.connect_drag_end({
        let grabbed = Rc::clone(&grabbed);
        let pill = pill.clone();
        move |_, _, _| {
            grabbed.set(None);
            pill.set_cursor_from_name(Some("grab"));
        }
    });
    fixed.add_controller(drag);

    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    keys.connect_key_pressed({
        let state = Rc::clone(state);
        move |_, key, _, _| {
            let key = match key {
                gtk::gdk::Key::Return | gtk::gdk::Key::KP_Enter | gtk::gdk::Key::ISO_Enter => {
                    MoveKey::Enter
                }
                gtk::gdk::Key::Escape => MoveKey::Escape,
                _ => MoveKey::Other,
            };
            let mut state = state.borrow_mut();
            let now = Instant::now();
            match state.mode.key(key, now) {
                Some(end) => {
                    state.end = Some((end, now));
                    gtk::glib::Propagation::Stop
                }
                None => gtk::glib::Propagation::Proceed,
            }
        }
    });
    window.add_controller(keys);

    let motion = gtk::EventControllerMotion::new();
    motion.connect_motion({
        let state = Rc::clone(state);
        move |_, _, _| state.borrow_mut().mode.touch(Instant::now())
    });
    window.add_controller(motion);
    window
}

/// The idle pill's resting bars, in the pill's own white: the same thin
/// 2 px bars as the pill, as many as fit between ✗ and ✓.
fn draw_resting(context: &gtk::cairo::Context, width: i32, height: i32) {
    let centre = f64::from(height) / 2.0;
    let bar_height = resting_floor(f64::from(height) - 2.0);
    let pitch = PILL_BAR_WIDTH + PILL_BAR_GAP;
    let fits = ((f64::from(width) + PILL_BAR_GAP) / pitch).floor().max(0.0) as usize;
    let count = PILL_BAR_COUNT.min(fits);
    if count == 0 {
        return;
    }
    let row = PILL_BAR_WIDTH * count as f64 + PILL_BAR_GAP * (count - 1) as f64;
    let left = ((f64::from(width) - row) / 2.0).max(0.0);
    for index in 0..count {
        let x = left + index as f64 * (PILL_BAR_WIDTH + PILL_BAR_GAP);
        context.set_source_rgba(0.957, 0.961, 0.969, edge_falloff_alpha(index, count));
        rounded_rectangle(
            context,
            x,
            centre - bar_height / 2.0,
            PILL_BAR_WIDTH,
            bar_height,
        );
        let _ = context.fill();
    }
}

/// Drive the desktop-notification rung from a plain (non-GTK) poll loop.
/// `org.freedesktop.Notifications` needs no GTK display, so this backend never
/// initializes GTK — the caller reaches it directly for X11/Unknown sessions
/// and for a Wayland session without Layer Shell.
fn run_notification_feedback(selection: FeedbackSelection) -> i32 {
    let _moves = refuse_moves(NO_LAYER_SHELL_MOVE);
    let notifier = Notifier::start(selection);
    let mut controller = PresentationController::default();
    let mut tick = NotificationTickState::default();
    let mut cadence = StatusCadence::default();
    loop {
        let observation = read_status();
        let period = cadence.next(observation.as_ref(), Instant::now());
        notification_tick(&mut controller, &mut tick, &notifier, observation);
        std::thread::sleep(period);
    }
}

/// The GTK-context notification driver, used only when a Layer Shell surface was
/// selected but then failed to realize locally. It shares the same `Notifier`;
/// the glib timeout only hands it the label to show. The status fetch runs on
/// a dedicated worker thread so a slow daemon reply can never stall this GTK
/// main loop — the tick only takes the newest finished round trip
/// nonblockingly, and keeps the current state when none has landed yet.
fn install_notification_feedback(application: &gtk::Application) {
    // A notification backend has no window. Keep its GApplication alive for
    // the source lifetime so the polling timeout can actually run.
    let hold = application.hold();
    let notifier = Notifier::start(FeedbackSelection {
        backend: FeedbackBackend::DesktopNotification,
        degradation: Some(FeedbackDegradation::SurfaceCreationFailure),
    });
    let status_worker = StatusWorker::spawn();
    let controller = Rc::new(RefCell::new(PresentationController::default()));
    let tick = Rc::new(RefCell::new(NotificationTickState::default()));
    let cadence = Rc::new(Cell::new(STATUS_POLL_PERIOD));
    let tick_cadence = Rc::clone(&cadence);
    poll_at_cadence(cadence, move || {
        let _hold = &hold;
        if let Some(observation) = status_worker.take_latest() {
            tick_cadence.set(observation.period);
            notification_tick(
                &mut controller.borrow_mut(),
                &mut tick.borrow_mut(),
                &notifier,
                observation.response,
            );
        }
        gtk::glib::ControlFlow::Continue
    });
}

/// One poll of the daemon status: on a transition into a visible phase, hand the
/// new label to the notifier (which fires a desktop notification, or logs the
/// transition to the journal when the bus is unavailable). `observation` is the
/// round trip this tick renders — pre-fetched off the caller's thread by the
/// status worker, or read inline by the non-GTK loop. NoSpeech is gated
/// through `NoSpeechNotifyLatch` instead of the plain phase-transition check:
/// a retained NoSpeech capsule can span an unreachable blip that recovers
/// before the terminal window expires, which would otherwise look like two
/// separate transitions into NoSpeech and fire a duplicate notification for
/// one episode. The latch fires once per episode and does not re-arm on an
/// `Unreachable` observation.
struct NotificationTickState {
    previous_phase: OverlayPhase,
    previous_label: &'static str,
    no_speech_latch: NoSpeechNotifyLatch,
    limit_latch: LimitWarningLatch,
    trigger_latch: TriggerRepeatLatch,
}

impl Default for NotificationTickState {
    fn default() -> Self {
        Self {
            previous_phase: OverlayView::HIDDEN.phase,
            previous_label: OverlayView::HIDDEN.visible_label,
            no_speech_latch: NoSpeechNotifyLatch::default(),
            limit_latch: LimitWarningLatch::default(),
            trigger_latch: TriggerRepeatLatch::default(),
        }
    }
}

fn notification_tick(
    controller: &mut PresentationController,
    tick: &mut NotificationTickState,
    notifier: &Notifier,
    observation: Option<Response>,
) {
    let now = Instant::now();
    let (view, signal, warning, identity, remaining) = match observation {
        Some(response) => {
            let view = controller.observe(&response, now);
            (
                view,
                ObservedSignal::Reachable(view.phase),
                limit_warning_from_response(&response),
                recording_identity(&response).map(str::to_owned),
                recording_remaining(&response),
            )
        }
        None => (
            controller.observe_unreachable(now),
            ObservedSignal::Unreachable,
            None,
            None,
            None,
        ),
    };
    // Both latches observe every tick: they are state machines, not senders,
    // and skipping one would corrupt the next tick's decision.
    let announced = tick
        .limit_latch
        .observe(signal, identity.as_deref(), warning);
    let limit_body = announced
        .zip(remaining)
        .and_then(|(warning, left)| limit_notification_body(warning, left));
    if announced.is_some() && limit_body.is_none() {
        // Deliberately suppressed (no headroom left to report): the stage is
        // spent on purpose, not lost, so settle it rather than leaving it
        // outstanding.
        tick.limit_latch.commit();
    }
    let fire_no_speech = tick.no_speech_latch.observe(signal);
    let first_identity = tick.trigger_latch.observe(identity.as_deref());
    // This rung sends at most one bubble per tick; the pure chooser decides
    // which. This rung has no capsule to turn amber, so the notification is
    // the whole warning.
    match notification_rung_choice(
        view,
        tick.previous_phase,
        tick.previous_label,
        limit_body.is_some(),
        fire_no_speech,
    ) {
        Some(RungNotification::Limit) => {
            // One attempt, no retry loop: if the queue is busy with a call
            // already in flight, hand the stage back so the NEXT tick re-selects
            // it. Committing a warning the sink refused would be silence for
            // the rest of the Recording.
            if let Some(body) = limit_body {
                if notifier.notify(&body) {
                    tick.limit_latch.commit();
                } else {
                    tick.limit_latch.rollback();
                }
            }
        }
        Some(RungNotification::Label(label))
            if view.phase != OverlayPhase::Recording || identity.is_none() || first_identity =>
        {
            let _ = notifier.notify(label);
        }
        Some(RungNotification::Label(_)) | None => {}
    }
    // Always advances, whichever bubble won: the transition has been observed
    // either way, and a stale previous_phase would re-announce it later.
    tick.previous_phase = view.phase;
    tick.previous_label = view.visible_label;
}

/// The desktop-notification sink. Rung 3 talks to `org.freedesktop.Notifications`
/// directly over the session bus (not through GNotification): the service is
/// present on Cinnamon, GNOME, KDE, and XFCE without a registered desktop entry
/// or D-Bus activation. The zbus connection lives on its own thread with a
/// contained tokio runtime (the overlay's loops are not tokio); requests cross a
/// bounded, coalescing channel. If the bus/service is not reachable within a
/// bounded startup handshake, or a call later fails, the notifier degrades to
/// logging transitions to the journal — never a silent success.
struct Notifier {
    sender: Option<std::sync::mpsc::SyncSender<String>>,
    selection: FeedbackSelection,
}

impl Notifier {
    fn start(selection: FeedbackSelection) -> Self {
        // Depth-1 so a stalled Notify call cannot make requests accumulate: a
        // send while one is pending is dropped (the next transition coalesces).
        let (sender, receiver) = std::sync::mpsc::sync_channel::<String>(1);
        let (ready_sender, ready_receiver) = std::sync::mpsc::channel::<bool>();
        let spawned = std::thread::Builder::new()
            .name("voisu-overlay-notify".to_owned())
            .spawn(move || {
                let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                else {
                    let _ = ready_sender.send(false);
                    return;
                };
                runtime.block_on(async move {
                    let connection = match zbus::Connection::session().await {
                        Ok(connection) => connection,
                        Err(_) => {
                            let _ = ready_sender.send(false);
                            return;
                        }
                    };
                    // Prove the notification SERVICE is reachable, not just the
                    // bus, before reporting success.
                    let reachable = notification_service_reachable(&connection).await;
                    let _ = ready_sender.send(reachable);
                    if !reachable {
                        return;
                    }
                    let mut replaces_id = 0_u32;
                    while let Ok(body) = receiver.recv() {
                        match notify_call(&connection, replaces_id, &body).await {
                            Ok(id) => replaces_id = id,
                            // A call failure after a healthy start: fall back to
                            // the journal for this transition rather than losing it.
                            Err(()) => journal_transition(selection, &body),
                        }
                    }
                });
            })
            .is_ok();
        // Bounded startup handshake: only claim the bus path once the service has
        // answered; otherwise degrade to journal logging.
        let bus_ready = spawned
            && matches!(
                ready_receiver.recv_timeout(Duration::from_secs(2)),
                Ok(true)
            );
        if bus_ready {
            Self {
                sender: Some(sender),
                selection,
            }
        } else {
            Self {
                sender: None,
                selection,
            }
        }
    }

    /// Returns whether the message was ACCEPTED. A phase transition may
    /// coalesce away on a full channel without anyone caring, but a limit
    /// warning is announced once and never again, so its caller has to know
    /// whether it actually got in.
    #[must_use]
    fn notify(&self, label: &str) -> bool {
        match &self.sender {
            // try_send is non-blocking: a full channel means a request is
            // already in flight, so this transition coalesces away.
            Some(sender) => sender.try_send(label.to_owned()).is_ok(),
            None => {
                // The journal rung always lands: it is a write to stderr, not a
                // queue, so there is nothing to refuse.
                journal_transition(self.selection, label);
                true
            }
        }
    }
}

/// Log a state transition to the journal (stderr under systemd) — the honest
/// fallback when the notification bus is unavailable.
fn journal_transition(selection: FeedbackSelection, label: &str) {
    eprintln!(
        "{} degradation=notification-unavailable phase={label}",
        selection.report_line()
    );
}

/// Whether `org.freedesktop.Notifications` answers — a real service probe, not
/// just a bus connection.
async fn notification_service_reachable(connection: &zbus::Connection) -> bool {
    connection
        .call_method(
            Some("org.freedesktop.Notifications"),
            "/org/freedesktop/Notifications",
            Some("org.freedesktop.Notifications"),
            "GetServerInformation",
            &(),
        )
        .await
        .is_ok()
}

/// Send (or replace) a Voisu desktop notification. Returns the server-assigned
/// id so the next transition replaces this notification in place rather than
/// stacking; `Err(())` signals the caller to fall back to the journal.
async fn notify_call(
    connection: &zbus::Connection,
    replaces_id: u32,
    body: &str,
) -> Result<u32, ()> {
    use std::collections::HashMap;
    use zbus::zvariant::Value;

    let actions: Vec<&str> = Vec::new();
    let hints: HashMap<&str, Value<'_>> = HashMap::new();
    // Notify(app_name, replaces_id, app_icon, summary, body, actions, hints,
    //        expire_timeout) -> u32
    let arguments = (
        "Voisu",
        replaces_id,
        "",
        "Voisu",
        body,
        actions,
        hints,
        3000_i32,
    );
    match connection
        .call_method(
            Some("org.freedesktop.Notifications"),
            "/org/freedesktop/Notifications",
            Some("org.freedesktop.Notifications"),
            "Notify",
            &arguments,
        )
        .await
    {
        // A malformed reply is a failure too: map it to Err so the caller logs
        // the transition to the journal rather than silently counting success.
        Ok(reply) => reply.body().deserialize::<u32>().map_err(|_| ()),
        Err(_) => Err(()),
    }
}

/// The capsule's widget set, grouped so rendering one tick reads as "this
/// surface, this state" rather than a positional list of five widgets.
struct CapsuleSurface<'a> {
    window: &'a gtk::ApplicationWindow,
    label: &'a gtk::Label,
    meter: &'a gtk::DrawingArea,
    glyph: &'a gtk::Label,
    container: &'a gtk::Box,
}

/// What the meter paints for: the phase, and how close its Recording is to the
/// Deadline. The two always change together, so they travel together.
#[derive(Clone, Copy, Debug)]
struct MeterState {
    phase: OverlayPhase,
    warning: Option<LimitWarning>,
}

impl MeterState {
    const HIDDEN: Self = Self {
        phase: OverlayPhase::Hidden,
        warning: None,
    };
}

fn render_surface(
    surface: CapsuleSurface<'_>,
    rendered: &Cell<MeterState>,
    view: OverlayView,
    warning: Option<LimitWarning>,
    remaining: Option<Duration>,
) -> Option<Size> {
    let CapsuleSurface {
        window,
        label,
        meter,
        glyph,
        container: capsule,
    } = surface;
    for class in [
        "recording",
        "processing",
        "success",
        "failure",
        "loading",
        "unavailable",
        "nospeech",
        "copied",
        LIMIT_WARNING_CLASS,
        LIMIT_APPROACHING_CLASS,
    ] {
        capsule.remove_css_class(class);
    }
    let class = match view.phase {
        OverlayPhase::Recording => "recording",
        OverlayPhase::Processing => "processing",
        OverlayPhase::Success => "success",
        OverlayPhase::Failure => "failure",
        OverlayPhase::Loading => "loading",
        OverlayPhase::Unavailable => "unavailable",
        OverlayPhase::NoSpeech => "nospeech",
        OverlayPhase::Copied => "copied",
        OverlayPhase::Hidden => "",
    };
    if !class.is_empty() {
        capsule.add_css_class(class);
    }
    // The warn border rides on top of the phase class, and only while the phase
    // it belongs to is still on screen.
    let warning = (view.phase == OverlayPhase::Recording)
        .then_some(warning)
        .flatten();
    if let Some(warn_class) = limit_warning_class(warning) {
        capsule.add_css_class(warn_class);
    }
    rendered.set(MeterState {
        phase: view.phase,
        warning,
    });
    if view.phase == OverlayPhase::Hidden {
        window.set_visible(false);
        return None;
    }
    let text = pill_text(view, warning, remaining);
    if view.phase == OverlayPhase::Recording && !text.is_empty() {
        capsule.add_css_class(LIMIT_APPROACHING_CLASS);
    }
    label.set_label(&text);
    label.set_visible(!text.is_empty());
    // The accessible description lives on the capsule box, not the label: the
    // label is hidden for every graphics-first phase (Recording/Processing/
    // Success/NoSpeech), and a hidden widget is excluded from assistive
    // presentation. The capsule is visible in every non-Hidden phase, so it
    // is the one widget that can always carry the announcement.
    capsule.update_property(&[gtk::accessible::Property::Description(
        announcement_for_overlay(view),
    )]);
    meter.set_visible(
        text.is_empty()
            && matches!(
                view.phase,
                OverlayPhase::Recording | OverlayPhase::Processing | OverlayPhase::NoSpeech
            ),
    );
    let glyph_text = phase_glyph(view.phase);
    glyph.set_label(glyph_text);
    glyph.set_visible(!glyph_text.is_empty());
    Some(size_pill(window, capsule, label, glyph))
}

/// Binds the top-left-anchored surface to one output and pushes it so the
/// pill's centre sits at `position` on that output, wholly on screen. The
/// margins always come from the output the surface is bound to: an unbound
/// surface placed by another output's size could land wholly off screen,
/// where no output would ever claim it. Called on every visible tick, so a
/// new output, pill size or position lands within one tick; the surface is
/// only touched when something changed.
fn place_pill(
    window: &gtk::ApplicationWindow,
    position: OverlayPosition,
    pill: Size,
    placed: &Cell<Option<(i32, i32)>>,
    preferred: &Option<gtk::gdk::Monitor>,
) {
    let Some(monitor) = pill_output(window, preferred) else {
        return;
    };
    if LayerShell::monitor(window).as_ref() != Some(&monitor) {
        window.set_monitor(Some(&monitor));
        placed.set(None);
    }
    let geometry = monitor.geometry();
    let margins = placement::pill_margins(position, (geometry.width(), geometry.height()), pill);
    if placed.get() == Some(margins) {
        return;
    }
    placed.set(Some(margins));
    window.set_margin(Edge::Left, margins.0);
    window.set_margin(Edge::Top, margins.1);
}

/// The output for the pill: the one the last saved move happened on, else
/// the one it is already bound to, else the first, skipping any that have
/// been unplugged. Kept in memory only: a restarted Overlay starts on the
/// first output again.
fn pill_output(
    window: &gtk::ApplicationWindow,
    preferred: &Option<gtk::gdk::Monitor>,
) -> Option<gtk::gdk::Monitor> {
    let usable = |monitor: &gtk::gdk::Monitor| {
        let geometry = monitor.geometry();
        monitor.is_valid() && geometry.width() > 0 && geometry.height() > 0
    };
    preferred
        .clone()
        .filter(usable)
        .or_else(|| LayerShell::monitor(window).filter(usable))
        .or_else(|| {
            let monitors = gtk::prelude::RootExt::display(window).monitors();
            (0..monitors.n_items())
                .filter_map(|index| monitors.item(index)?.downcast::<gtk::gdk::Monitor>().ok())
                .find(usable)
        })
}

/// Watches the config file, so a position set by the CLI or the TUI moves the
/// pill without waiting for the next Recording. The returned monitor must be
/// kept alive.
fn watch_config(reread: &Rc<Cell<bool>>) -> Option<gtk::gio::FileMonitor> {
    let file = gtk::gio::File::for_path(voisu_app::config::config_path());
    let monitor = file
        .monitor_file(
            gtk::gio::FileMonitorFlags::WATCH_MOVES,
            None::<&gtk::gio::Cancellable>,
        )
        .map_err(|error| eprintln!("overlay_position config_watch=unavailable error={error}"))
        .ok()?;
    let reread = Rc::clone(reread);
    monitor.connect_changed(move |_, _, _, _| reread.set(true));
    Some(monitor)
}

/// Sizes the pill for what it shows: 120x32 for bars and the check mark, and
/// for text widened to fit, up to `PILL_MAX_WIDTH`, wrapping to two lines.
/// The label's own width request is the text width, so the capsule is exactly
/// the pill and the window follows it.
fn size_pill(
    window: &gtk::ApplicationWindow,
    capsule: &gtk::Box,
    label: &gtk::Label,
    glyph: &gtk::Label,
) -> Size {
    if !label.is_visible() {
        // Back to the bare pill: reset the window's size explicitly, so a wide
        // text state never leaves the surface wider than the bars.
        label.set_size_request(-1, -1);
        capsule.set_size_request(PILL_WIDTH, PILL_HEIGHT);
        window.set_default_size(PILL_WIDTH, PILL_HEIGHT);
        window.queue_resize();
        return (PILL_WIDTH, PILL_HEIGHT);
    }
    // Measure the text on one line to know how wide it wants to be.
    label.set_wrap(false);
    label.set_size_request(-1, -1);
    label.set_max_width_chars(-1);
    let natural = label.measure(gtk::Orientation::Horizontal, -1).1;
    let glyph_width = if glyph.is_visible() {
        glyph.measure(gtk::Orientation::Horizontal, -1).1
    } else {
        0
    };
    let width = text_pill_width(natural, glyph_width);
    let text_width = text_pill_text_width(width, glyph_width);
    // Wrapped, ellipsized at the second line, with the width pinned: the
    // label's natural width then is the request, not its unwrapped length.
    label.set_wrap(true);
    label.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    label.set_lines(PILL_MAX_LINES);
    label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    label.set_max_width_chars(1);
    label.set_width_chars(1);
    let height = label
        .measure(gtk::Orientation::Vertical, text_width)
        .1
        .max(0);
    label.set_size_request(text_width, height);
    capsule.set_size_request(width, -1);
    window.set_default_size(width, -1);
    window.queue_resize();
    // A text state's CSS padding (4 px) and border (1 px) above and below.
    (width, (height + TEXT_PILL_PAD_Y).max(PILL_HEIGHT))
}

/// Vertical padding plus border around a text pill's label, both sides.
const TEXT_PILL_PAD_Y: i32 = 2 * (4 + 1);

/// Cadence of the Level poll on its dedicated worker thread. The meter itself
/// repaints on the frame clock, independent of this.
const LEVEL_POLL_PERIOD: Duration = Duration::from_millis(20);
/// One absolute deadline for a whole Level round trip — connect, write, and
/// read together. A skipped frame is invisible; a stalled poll must never be.
const LEVEL_POLL_DEADLINE: Duration = Duration::from_millis(10);
/// A Level response is at most 8 frames of ~120 bytes; anything past this
/// bound is a malformed peer and the poll fails closed.
const MAX_LEVEL_RESPONSE_BYTES: usize = 64 * 1024;

/// The dedicated Level-poll worker. ALL Level IPC runs on this thread; the
/// GTK main thread only ever takes the latest target level nonblockingly, so
/// a stalled socket backlog or trickling peer can never freeze drawing, the
/// 200 ms status poll, or timer disarming. The worker owns the stateless
/// cursor and turns each poll into one target: the loudest new frame (a
/// coalesced peak survives), or silence after a failed poll. The hand-off slot
/// holds at most one value; the easing itself runs on the frame clock.
struct LevelWorker {
    stop: Arc<AtomicBool>,
    latest: Arc<Mutex<Option<f64>>>,
}

impl LevelWorker {
    fn spawn() -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let latest = Arc::new(Mutex::new(None));
        let thread_stop = Arc::clone(&stop);
        let thread_latest = Arc::clone(&latest);
        thread::spawn(move || {
            let mut cursor = 0_u64;
            while !thread_stop.load(Ordering::Relaxed) {
                let poll = read_levels(cursor, Instant::now() + LEVEL_POLL_DEADLINE);
                if let Some(last) = poll.as_ref().and_then(|frames| frames.last()) {
                    cursor = last.seq;
                }
                // Isolation rule: a failed Level poll only decays the bars.
                // Daemon liveness belongs to the status poll alone.
                if let Some(target) = poll_target(poll.as_deref()) {
                    *thread_latest.lock().unwrap() = Some(target);
                }
                thread::sleep(LEVEL_POLL_PERIOD);
            }
        });
        Self { stop, latest }
    }

    /// Nonblocking: the newest published target, if any arrived since the
    /// last take. `try_lock` keeps the GTK thread from ever waiting on the
    /// worker.
    fn take_latest(&self) -> Option<f64> {
        self.latest.try_lock().ok().and_then(|mut slot| slot.take())
    }

    fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl Drop for LevelWorker {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The meter's frame-clock animation. While installed, GTK calls it once per
/// display frame: it takes the newest level target, advances the waveform by
/// the real frame interval, and repaints. Removing it leaves the overlay with
/// no per-frame work at all.
#[derive(Default)]
struct MeterAnimation {
    tick: RefCell<Option<gtk::TickCallbackId>>,
}

impl MeterAnimation {
    fn start(
        &self,
        meter: &gtk::DrawingArea,
        waveform: &Rc<RefCell<Waveform>>,
        level_worker: &Rc<RefCell<Option<LevelWorker>>>,
    ) {
        let mut tick = self.tick.borrow_mut();
        if tick.is_some() {
            return;
        }
        let waveform = Rc::clone(waveform);
        let level_worker = Rc::clone(level_worker);
        // Frame time of the previous tick, in the clock's microseconds. The
        // first tick after a start advances nothing, so a re-armed meter
        // never fast-forwards over the time it was stopped.
        let previous_frame = Cell::new(None::<i64>);
        *tick = Some(meter.add_tick_callback(move |meter, clock| {
            let now = clock.frame_time();
            let frame_secs = previous_frame
                .replace(Some(now))
                .map_or(0.0, |previous| (now - previous) as f64 / 1_000_000.0);
            let mut waveform = waveform.borrow_mut();
            if let Some(target) = level_worker
                .borrow()
                .as_ref()
                .and_then(LevelWorker::take_latest)
            {
                waveform.set_target(target);
            }
            waveform.advance(frame_secs);
            meter.queue_draw();
            gtk::glib::ControlFlow::Continue
        }));
    }

    fn stop(&self) {
        if let Some(tick) = self.tick.borrow_mut().take() {
            tick.remove();
        }
    }
}

fn draw_meter(
    context: &gtk::cairo::Context,
    width: i32,
    height: i32,
    waveform: &Waveform,
    state: MeterState,
    sweep_started: Instant,
    reduced_motion: bool,
) {
    let MeterState { phase, warning } = state;
    let centre = f64::from(height) / 2.0;
    // content_height 24 minus a 2px inset -> 22px drawable (floor 2.2, max swing 19.8).
    let drawable = f64::from(height) - 2.0;
    let gap = PILL_BAR_GAP;
    let count = PILL_BAR_COUNT;
    // Fixed thin bars, the row centred in the meter.
    let bar_width = PILL_BAR_WIDTH;
    let row = bar_width * count as f64 + gap * (count - 1) as f64;
    let left = ((f64::from(width) - row) / 2.0).max(0.0);
    // Corner-fit proof (no explicit clip needed): the 104x24 meter sits inside
    // the 120x32 pill (radius 16) at 8px horizontal / 4px vertical (padding plus
    // border), centred on the window's mid-line (y=16). The tightest point is the
    // first/last bar's outer edge at window x>=8, <=8px from the corner arc's
    // centre; there the pill's half-height is sqrt(16^2 - 8^2) ~= 13.9px versus
    // a 22px max bar (11px each side). 11 < 13.9, so even a full-height peak bar
    // stays inside the rounded corner.
    let elapsed = sweep_started.elapsed().as_secs_f64();
    for index in 0..count {
        let (bar_height, alpha, colour) = match phase {
            OverlayPhase::Recording => (
                recording_bar_height(waveform.bar_level(index, count), drawable),
                edge_falloff_alpha(index, count),
                // Amber from the moment the limit is approaching; the colour is
                // decided purely in `overlay.rs` so it stays testable.
                recording_bar_rgb(warning),
            ),
            OverlayPhase::Processing => (
                resting_floor(drawable),
                edge_falloff_alpha(index, count)
                    * sweep_brightness(index, count, elapsed, reduced_motion),
                (0.949, 0.949, 0.949),
            ),
            OverlayPhase::NoSpeech => (
                resting_floor(drawable),
                edge_falloff_alpha(index, count),
                (1.0, 0.706, 0.329), // #FFB454
            ),
            // Meter is hidden in every other phase; draw nothing defensively.
            _ => return,
        };
        let x = left + index as f64 * (bar_width + gap);
        context.set_source_rgba(colour.0, colour.1, colour.2, alpha);
        rounded_rectangle(context, x, centre - bar_height / 2.0, bar_width, bar_height);
        let _ = context.fill();
    }
}

fn rounded_rectangle(context: &gtk::cairo::Context, x: f64, y: f64, width: f64, height: f64) {
    let radius = (width / 2.0).min(height / 2.0);
    context.new_sub_path();
    context.arc(
        x + width - radius,
        y + radius,
        radius,
        -std::f64::consts::FRAC_PI_2,
        0.0,
    );
    context.arc(
        x + width - radius,
        y + height - radius,
        radius,
        0.0,
        std::f64::consts::FRAC_PI_2,
    );
    context.arc(
        x + radius,
        y + height - radius,
        radius,
        std::f64::consts::FRAC_PI_2,
        std::f64::consts::PI,
    );
    context.arc(
        x + radius,
        y + radius,
        radius,
        std::f64::consts::PI,
        std::f64::consts::PI * 1.5,
    );
    context.close_path();
}

/// One poll of the daemon status: the bounded round trip (total deadline plus
/// reply byte cap, shared with the status worker) against the observer socket.
/// Synchronous callers — the journal loop and this file's non-GTK notification
/// loop — run on their own threads; the GTK status tick consumes the worker's
/// published observations instead.
fn read_status() -> Option<Response> {
    fetch_status(Instant::now() + STATUS_ROUND_TRIP_DEADLINE)
}

/// One Level round trip bounded by a single absolute deadline across
/// connect, write, and read. The socket is nonblocking from creation and
/// every readiness wait goes through poll(2) with the REMAINING time — a
/// per-syscall timeout only bounds one syscall, so a trickling peer could
/// otherwise stretch the round trip far past its budget. Runs only on the
/// worker thread.
fn read_levels(after_seq: u64, deadline: Instant) -> Option<Vec<voisu_core::LevelFrame>> {
    let path = socket_path().ok()?;
    let mut stream = connect_within(&path, deadline)?;
    let mut request = serde_json::to_vec(&Request::new(Command::Level { after_seq })).ok()?;
    request.push(b'\n');
    write_within(&mut stream, &request, deadline)?;
    let response = read_until_eof_within(&mut stream, deadline, MAX_LEVEL_RESPONSE_BYTES)?;
    serde_json::from_slice::<Response>(&response)
        .ok()?
        .level_frames
}
