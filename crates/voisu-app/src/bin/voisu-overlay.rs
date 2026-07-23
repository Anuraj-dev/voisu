//! Optional GTK feedback observer. It has no command path into the daemon.

use std::cell::{Cell, RefCell};
use std::env;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;
use std::process::Command as ProcessCommand;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use gtk4 as gtk;
use gtk::prelude::*;
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use voisu_app::feedback::{
    after_surface_creation, select_feedback_backend, FeedbackBackend, FeedbackCapabilities,
    FeedbackDegradation, FeedbackSelection, OverlayRestartPolicy, SessionKind,
};
use voisu_app::overlay::{
    edge_falloff_alpha, phase_glyph, poll_tick, recording_bar_height, resting_floor,
    sweep_brightness, BarSmoother, LevelPollAction, LevelPollLatch, NoSpeechNotifyLatch,
    ObservedSignal, OverlayPhase, OverlayView, PresentationController, PresentationTracker,
    RecordingNotifyLatch, TickAction,
};
use voisu_core::{Command, PROTOCOL_VERSION, Request, Response, socket_path};

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
            eprintln!("overlay_feedback backend=none degradation=supervisor-current-exe-error error={error}");
            return 1;
        }
    };
    let started = Instant::now();
    let mut policy = OverlayRestartPolicy::default();
    loop {
        let status = match ProcessCommand::new(&executable).status() {
            Ok(status) => status,
            Err(error) => {
                eprintln!("overlay_feedback backend=none degradation=supervisor-spawn-failure error={error}");
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
        eprintln!("overlay_feedback backend=none degradation=overlay-process-failure action=restart");
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
        return if report_only { 0 } else { run_journal_feedback(selection) };
    }

    // Only a Wayland session can use Layer Shell (rung 1), and probing that
    // needs GTK. Every other display session goes straight to the
    // desktop-notification rung, which talks to org.freedesktop.Notifications
    // and needs no GTK display — so GTK is never initialized there (an X11 /
    // Cinnamon host must not be gated behind a GDK init it does not need).
    if preflight.session != SessionKind::Wayland {
        let selection = select_feedback_backend(preflight);
        report(selection);
        return if report_only { 0 } else { run_notification_feedback(selection) };
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
        return if report_only { 0 } else { run_notification_feedback(selection) };
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
    let has_wayland = wayland_display.as_deref().is_some_and(|value| !value.is_empty());
    let has_x11 = x11_display.as_deref().is_some_and(|value| !value.is_empty());
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
    let mut controller = PresentationController::default();
    let mut previous = OverlayView::HIDDEN;
    loop {
        let now = Instant::now();
        let view = match read_status() {
            Some(response) => controller.observe(&response, now),
            None => controller.observe_unreachable(now),
        };
        if view != previous {
            eprintln!("{} phase={}", selection.report_line(), overlay_phase_label(view.phase));
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
        OverlayPhase::NoSpeech => "no_speech",
    }
}

fn build_feedback(application: &gtk::Application, selection: FeedbackSelection) {
    if selection.backend == FeedbackBackend::DesktopNotification {
        install_notification_feedback(application);
        return;
    }

    let window = gtk::ApplicationWindow::builder()
        .application(application)
        .default_width(280)
        .default_height(64)
        .resizable(false)
        .decorated(false)
        .focusable(false)
        .can_focus(false)
        .build();

    if selection.backend == FeedbackBackend::LayerShell {
        window.init_layer_shell();
        window.set_layer(Layer::Overlay);
        window.set_anchor(Edge::Bottom, true);
        window.set_margin(Edge::Bottom, 24);
        window.set_keyboard_mode(KeyboardMode::None);
        window.set_exclusive_zone(-1);
    }
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
    window.connect_realize({
        let application = application.clone();
        let switched = Rc::clone(&switched);
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
                install_notification_feedback(&application);
            }
        }
    });

    let label = gtk::Label::builder()
        .label("")
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .build();
    label.add_css_class("state-label");
    label.set_hexpand(true);
    label.set_vexpand(true);
    let meter = gtk::DrawingArea::builder()
        .content_width(150)
        .content_height(32)
        .build();
    // The pre-waveform text glyphs ("⋯" while Processing, "⚠" on Failure) keep
    // their slot: only the Recording presentation is replaced by the bar meter.
    let glyph = gtk::Label::builder().label("").build();
    glyph.add_css_class("meter");
    let rendered_bars = Rc::new(RefCell::new([0_u8; 20]));
    let reduced_motion = gtk::Settings::default()
        .map(|settings| !settings.is_gtk_enable_animations())
        .unwrap_or(true);
    let sweep_started = Instant::now();
    let rendered_phase = Rc::new(Cell::new(OverlayPhase::Hidden));
    meter.set_draw_func({
        let rendered_bars = Rc::clone(&rendered_bars);
        let rendered_phase = Rc::clone(&rendered_phase);
        move |_, context, width, height| {
            draw_meter(
                context,
                width,
                height,
                &rendered_bars.borrow(),
                rendered_phase.get(),
                sweep_started,
                reduced_motion,
            );
        }
    });
    let capsule = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    capsule.set_margin_start(20);
    capsule.set_margin_end(20);
    capsule.set_margin_top(12);
    capsule.set_margin_bottom(12);
    capsule.append(&label);
    capsule.append(&meter);
    capsule.append(&glyph);
    window.set_child(Some(&capsule));
    window.set_visible(false);

    let css = gtk::CssProvider::new();
    css.load_from_data(
        "window.background { background: transparent; }
         .capsule { background: rgba(23, 25, 29, 0.96); border-radius: 32px; }
         .capsule .state-label, .capsule .meter { color: #F4F5F7; font-size: 11pt; font-weight: 600; }
         .capsule.processing .state-label, .capsule.processing .meter { color: #8FB4FF; }
         .capsule.success .state-label, .capsule.success .meter { color: #65D6A0; font-size: 14pt; }
         .capsule.failure { border: 1px solid rgba(255, 138, 138, 0.9); box-shadow: 0 0 8px rgba(255, 138, 138, 0.35); }
         .capsule.failure .state-label, .capsule.failure .meter { color: #FF8A8A; }
         .capsule.nospeech .state-label, .capsule.nospeech .meter { color: #FFB454; }",
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
        window,
        label,
        meter,
        glyph,
        rendered_bars,
        rendered_phase,
        capsule,
        switched,
    );
}

fn install_surface_feedback(
    application: gtk::Application,
    // Rung 2 is gone, so the only windowed backend is Layer Shell and the
    // fallback branch below is inert; the selection is no longer consulted here.
    _selection: FeedbackSelection,
    window: gtk::ApplicationWindow,
    label: gtk::Label,
    meter: gtk::DrawingArea,
    glyph: gtk::Label,
    rendered_bars: Rc<RefCell<[u8; 20]>>,
    rendered_phase: Rc<Cell<OverlayPhase>>,
    capsule: gtk::Box,
    switched: Rc<Cell<bool>>,
) {
    let controller = Rc::new(RefCell::new(PresentationController::default()));
    // Rung 2 (a plain GTK window) is skipped, so the only windowed backend that
    // reaches here is Layer Shell, which the compositor keeps above and which
    // has no "buried window" problem. The resurface/notify fallback behavior is
    // therefore never needed on this path.
    let is_fallback = false;
    let tracker = Rc::new(RefCell::new(PresentationTracker::default()));
    let notify_latch = Rc::new(RefCell::new(RecordingNotifyLatch::default()));
    let no_speech_latch = Rc::new(RefCell::new(NoSpeechNotifyLatch::default()));
    let level_latch = Rc::new(RefCell::new(LevelPollLatch::default()));
    let level_poll = Rc::new(RefCell::new(None::<(gtk::glib::SourceId, Rc<LevelWorker>)>));
    gtk::glib::timeout_add_local(Duration::from_millis(200), move || {
        if switched.get() {
            // A genuine surface-creation failure handed feedback to the
            // notification backend; stop driving the retired window.
            if let Some((source, worker)) = level_poll.borrow_mut().take() {
                source.remove();
                worker.stop();
            }
            return gtk::glib::ControlFlow::Break;
        }
        let now = Instant::now();
        // The notify edge is driven by the OBSERVED daemon signal, kept separate
        // from the rendered phase: a failed status read renders an unavailable
        // capsule but is not a reachable observation, so it must not disturb the
        // Recording notification latch.
        let (view, signal) = match read_status() {
            Some(response) => {
                let view = controller.borrow_mut().observe(&response, now);
                (view, ObservedSignal::Reachable(view.phase))
            }
            None => (
                controller.borrow_mut().observe_unreachable(now),
                ObservedSignal::Unreachable,
            ),
        };
        render_surface(&window, &label, &meter, &glyph, &capsule, &rendered_phase, view);
        if view.phase == OverlayPhase::Processing {
            meter.queue_draw();
        }
        match level_latch.borrow_mut().observe(signal) {
            LevelPollAction::Arm => {
                let worker = Rc::new(LevelWorker::spawn());
                let source = install_level_drain(
                    meter.clone(),
                    Rc::clone(&rendered_bars),
                    Rc::clone(&worker),
                );
                *level_poll.borrow_mut() = Some((source, worker));
            }
            LevelPollAction::Disarm => {
                if let Some((source, worker)) = level_poll.borrow_mut().take() {
                    source.remove();
                    worker.stop();
                }
                *rendered_bars.borrow_mut() = [0; 20];
                meter.queue_draw();
            }
            LevelPollAction::Keep => {}
        }
        // render_surface realizes the window on its first real show; the realize
        // callback may have found no surface and handed feedback to the
        // notification backend, setting `switched`. The pure `poll_tick` owns the
        // ordering: it breaks on that handoff BEFORE the tracker or latch observe
        // this tick, so a retired window is never re-presented and no duplicate
        // notification is sent. The bin only runs the resulting side effects.
        match poll_tick(
            switched.get(),
            is_fallback,
            view,
            signal,
            &mut tracker.borrow_mut(),
            &mut notify_latch.borrow_mut(),
            &mut no_speech_latch.borrow_mut(),
        ) {
            TickAction::Break => {
                if let Some((source, worker)) = level_poll.borrow_mut().take() {
                    source.remove();
                    worker.stop();
                }
                gtk::glib::ControlFlow::Break
            }
            TickAction::Continue { resurface, notify, notify_no_speech } => {
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
                if notify {
                    let notification = gtk::gio::Notification::new("Voisu");
                    notification.set_body(Some(view.visible_label));
                    application.send_notification(Some("overlay-recording"), &notification);
                }
                if notify_no_speech {
                    let notification = gtk::gio::Notification::new("Voisu");
                    notification.set_body(Some(OverlayView::no_speech().visible_label));
                    application.send_notification(Some("overlay-nospeech"), &notification);
                }
                gtk::glib::ControlFlow::Continue
            }
        }
    });
}

/// Drive the desktop-notification rung from a plain (non-GTK) poll loop.
/// `org.freedesktop.Notifications` needs no GTK display, so this backend never
/// initializes GTK — the caller reaches it directly for X11/Unknown sessions
/// and for a Wayland session without Layer Shell.
fn run_notification_feedback(selection: FeedbackSelection) -> i32 {
    let notifier = Notifier::start(selection);
    let mut controller = PresentationController::default();
    let mut previous_phase = OverlayView::HIDDEN.phase;
    loop {
        notification_tick(&mut controller, &mut previous_phase, &notifier);
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// The GTK-context notification driver, used only when a Layer Shell surface was
/// selected but then failed to realize locally. It shares the same `Notifier`;
/// the glib timeout only hands it the label to show.
fn install_notification_feedback(application: &gtk::Application) {
    // A notification backend has no window. Keep its GApplication alive for
    // the source lifetime so the polling timeout can actually run.
    let hold = application.hold();
    let notifier = Notifier::start(FeedbackSelection {
        backend: FeedbackBackend::DesktopNotification,
        degradation: Some(FeedbackDegradation::SurfaceCreationFailure),
    });
    let controller = Rc::new(RefCell::new(PresentationController::default()));
    let previous_phase = Rc::new(RefCell::new(OverlayView::HIDDEN.phase));
    gtk::glib::timeout_add_local(Duration::from_millis(200), move || {
        let _hold = &hold;
        notification_tick(
            &mut controller.borrow_mut(),
            &mut previous_phase.borrow_mut(),
            &notifier,
        );
        gtk::glib::ControlFlow::Continue
    });
}

/// One poll of the daemon status: on a transition into a visible phase, hand the
/// new label to the notifier (which fires a desktop notification, or logs the
/// transition to the journal when the bus is unavailable).
fn notification_tick(
    controller: &mut PresentationController,
    previous_phase: &mut OverlayPhase,
    notifier: &Notifier,
) {
    let now = Instant::now();
    let view = match read_status() {
        Some(response) => controller.observe(&response, now),
        None => controller.observe_unreachable(now),
    };
    // Fire only on a PHASE transition into a visible phase. Comparing the whole
    // view would re-fire on every meter/activity tick within one Recording.
    if view.is_visible() && *previous_phase != view.phase {
        notifier.notify(view.visible_label);
    }
    *previous_phase = view.phase;
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
                let Ok(runtime) =
                    tokio::runtime::Builder::new_current_thread().enable_all().build()
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
            Self { sender: Some(sender), selection }
        } else {
            Self { sender: None, selection }
        }
    }

    fn notify(&self, label: &str) {
        match &self.sender {
            // try_send is non-blocking: a full channel means a request is
            // already in flight, so this transition coalesces away.
            Some(sender) => {
                let _ = sender.try_send(label.to_owned());
            }
            None => journal_transition(self.selection, label),
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
    let arguments = ("Voisu", replaces_id, "", "Voisu", body, actions, hints, 3000_i32);
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

fn render_surface(
    window: &gtk::ApplicationWindow,
    label: &gtk::Label,
    meter: &gtk::DrawingArea,
    glyph: &gtk::Label,
    capsule: &gtk::Box,
    rendered_phase: &Cell<OverlayPhase>,
    view: OverlayView,
) {
    for class in ["recording", "processing", "success", "failure", "nospeech"] {
        capsule.remove_css_class(class);
    }
    let class = match view.phase {
        OverlayPhase::Recording => "recording",
        OverlayPhase::Processing => "processing",
        OverlayPhase::Success => "success",
        OverlayPhase::Failure => "failure",
        OverlayPhase::NoSpeech => "nospeech",
        OverlayPhase::Hidden => "",
    };
    if !class.is_empty() {
        capsule.add_css_class(class);
    }
    rendered_phase.set(view.phase);
    if view.phase == OverlayPhase::Hidden {
        window.set_visible(false);
        return;
    }
    label.set_label(view.capsule_text());
    label.update_property(&[gtk::accessible::Property::Description(view.accessible_label)]);
    meter.set_visible(matches!(
        view.phase,
        OverlayPhase::Recording | OverlayPhase::Processing | OverlayPhase::NoSpeech
    ));
    glyph.set_label(phase_glyph(view.phase));
    window.set_visible(true);
    meter.queue_draw();
}

/// Cadence of the Level poll on its dedicated worker thread, and of the GTK
/// drain that repaints from it.
const LEVEL_POLL_PERIOD: Duration = Duration::from_millis(20);
/// One absolute deadline for a whole Level round trip — connect, write, and
/// read together. A skipped frame is invisible; a stalled poll must never be.
const LEVEL_POLL_DEADLINE: Duration = Duration::from_millis(10);
/// A Level response is at most 8 frames of ~120 bytes; anything past this
/// bound is a malformed peer and the poll fails closed.
const MAX_LEVEL_RESPONSE_BYTES: usize = 64 * 1024;

/// The dedicated Level-poll worker. ALL Level IPC runs on this thread; the
/// GTK main thread only ever takes the latest smoothed bars nonblockingly,
/// so a stalled socket backlog or trickling peer can never freeze drawing,
/// the 200 ms status poll, or timer disarming. The worker owns the stateless
/// cursor and the BarSmoother: every polled frame passes through the
/// smoother in order (coalesced peaks survive), and only the newest smoothed
/// result is published, which keeps the hand-off slot bounded at one value.
struct LevelWorker {
    stop: Arc<AtomicBool>,
    latest: Arc<Mutex<Option<[u8; 20]>>>,
}

impl LevelWorker {
    fn spawn() -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let latest = Arc::new(Mutex::new(None));
        let thread_stop = Arc::clone(&stop);
        let thread_latest = Arc::clone(&latest);
        thread::spawn(move || {
            let mut cursor = 0_u64;
            let mut smoother = BarSmoother::default();
            while !thread_stop.load(Ordering::Relaxed) {
                let bars = match read_levels(cursor, Instant::now() + LEVEL_POLL_DEADLINE) {
                    Some(frames) => {
                        if let Some(last) = frames.last() {
                            cursor = last.seq;
                        }
                        smoother.observe_all(frames.into_iter().map(|frame| frame.bands))
                    }
                    // Isolation rule: a failed Level poll only decays the
                    // bars. Daemon liveness belongs to the status poll alone.
                    None => smoother.observe_failure(),
                };
                *thread_latest.lock().unwrap() = Some(bars);
                thread::sleep(LEVEL_POLL_PERIOD);
            }
        });
        Self { stop, latest }
    }

    /// Nonblocking: the newest published bars, if any arrived since the last
    /// take. `try_lock` keeps the GTK thread from ever waiting on the worker.
    fn take_latest(&self) -> Option<[u8; 20]> {
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

/// The GTK-side drain: nonblocking channel read plus drawing, nothing else.
fn install_level_drain(
    meter: gtk::DrawingArea,
    rendered_bars: Rc<RefCell<[u8; 20]>>,
    worker: Rc<LevelWorker>,
) -> gtk::glib::SourceId {
    gtk::glib::timeout_add_local(LEVEL_POLL_PERIOD, move || {
        if let Some(bars) = worker.take_latest() {
            *rendered_bars.borrow_mut() = bars;
        }
        // Redraw every tick regardless: keeps the meter live even on a tick
        // where the worker published no new bars.
        meter.queue_draw();
        gtk::glib::ControlFlow::Continue
    })
}

fn draw_meter(
    context: &gtk::cairo::Context,
    width: i32,
    height: i32,
    bands: &[u8; 20],
    phase: OverlayPhase,
    sweep_started: Instant,
    reduced_motion: bool,
) {
    let centre = f64::from(height) / 2.0;
    let drawable = f64::from(height) - 5.0;
    let gap = 2.0;
    let bar_width = (f64::from(width) - gap * 19.0) / 20.0;
    for (index, level) in bands.iter().enumerate() {
        let (bar_height, alpha, colour) = match phase {
            OverlayPhase::Recording => (
                recording_bar_height(*level, drawable),
                edge_falloff_alpha(index, 20),
                (0.949, 0.949, 0.949), // #F2F2F2
            ),
            OverlayPhase::Processing => (
                resting_floor(drawable),
                edge_falloff_alpha(index, 20)
                    * sweep_brightness(index, 20, sweep_started.elapsed().as_secs_f64(), reduced_motion),
                (0.949, 0.949, 0.949),
            ),
            OverlayPhase::NoSpeech => (
                resting_floor(drawable),
                edge_falloff_alpha(index, 20),
                (1.0, 0.706, 0.329), // #FFB454
            ),
            // Meter is hidden in every other phase; draw nothing defensively.
            _ => return,
        };
        let x = index as f64 * (bar_width + gap);
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

fn read_status() -> Option<Response> {
    let mut stream = UnixStream::connect(socket_path().ok()?).ok()?;
    let request = serde_json::to_vec(&Request {
        version: PROTOCOL_VERSION,
        command: Command::OverlayStatus,
    })
    .ok()?;
    stream.write_all(&request).ok()?;
    stream.write_all(b"\n").ok()?;
    stream.set_read_timeout(Some(Duration::from_millis(150))).ok()?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response).ok()?;
    serde_json::from_slice(&response).ok()
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
    let mut request = serde_json::to_vec(&Request {
        version: PROTOCOL_VERSION,
        command: Command::Level { after_seq },
    })
    .ok()?;
    request.push(b'\n');
    write_within(&mut stream, &request, deadline)?;
    let response = read_until_eof_within(&mut stream, deadline)?;
    serde_json::from_slice::<Response>(&response).ok()?.level_frames
}

/// Nonblocking connect to the daemon socket, bounded by the shared deadline.
fn connect_within(path: &std::path::Path, deadline: Instant) -> Option<UnixStream> {
    // SAFETY: socket(2) has no memory preconditions; the raw descriptor is
    // moved into an OwnedFd immediately, so every early return closes it.
    let raw = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
            0,
        )
    };
    if raw < 0 {
        return None;
    }
    // SAFETY: raw is a freshly created, valid descriptor owned by nothing else.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    // SAFETY: sockaddr_un is plain old data; all-zeroes is a valid value.
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    let bytes = path.as_os_str().as_bytes();
    if bytes.len() >= address.sun_path.len() {
        return None;
    }
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (slot, byte) in address.sun_path.iter_mut().zip(bytes) {
        *slot = *byte as libc::c_char;
    }
    let length = (std::mem::size_of::<libc::sa_family_t>() + bytes.len() + 1) as libc::socklen_t;
    // SAFETY: address is a properly initialized sockaddr_un and length is
    // within its size.
    let connected =
        unsafe { libc::connect(fd.as_raw_fd(), std::ptr::addr_of!(address).cast(), length) };
    if connected != 0 {
        match std::io::Error::last_os_error().raw_os_error() {
            Some(libc::EINPROGRESS) | Some(libc::EAGAIN) => {
                wait_within(fd.as_raw_fd(), libc::POLLOUT, deadline)?;
                let mut error: libc::c_int = 0;
                let mut error_len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
                // SAFETY: error and error_len are valid out-pointers of the
                // exact size SO_ERROR writes.
                let sockopt = unsafe {
                    libc::getsockopt(
                        fd.as_raw_fd(),
                        libc::SOL_SOCKET,
                        libc::SO_ERROR,
                        std::ptr::addr_of_mut!(error).cast(),
                        &mut error_len,
                    )
                };
                if sockopt != 0 || error != 0 {
                    return None;
                }
            }
            _ => return None,
        }
    }
    Some(UnixStream::from(fd))
}

/// Wait for descriptor readiness until the shared deadline elapses.
fn wait_within(fd: std::os::fd::RawFd, events: libc::c_short, deadline: Instant) -> Option<()> {
    loop {
        let remaining = deadline.checked_duration_since(Instant::now())?;
        let mut descriptor = libc::pollfd { fd, events, revents: 0 };
        let millis = remaining.as_millis().min(i32::MAX as u128) as libc::c_int;
        // SAFETY: descriptor is a valid pollfd for the duration of the call.
        match unsafe { libc::poll(&mut descriptor, 1, millis) } {
            1.. => return Some(()),
            0 => return None,
            _ if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted => {}
            _ => return None,
        }
    }
}

fn write_within(stream: &mut UnixStream, bytes: &[u8], deadline: Instant) -> Option<()> {
    let mut remaining = bytes;
    while !remaining.is_empty() {
        match stream.write(remaining) {
            Ok(0) => return None,
            Ok(written) => remaining = &remaining[written..],
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                wait_within(stream.as_raw_fd(), libc::POLLOUT, deadline)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return None,
        }
    }
    Some(())
}

fn read_until_eof_within(stream: &mut UnixStream, deadline: Instant) -> Option<Vec<u8>> {
    let mut response = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
        match stream.read(&mut buffer) {
            Ok(0) => return Some(response),
            Ok(read) => {
                response.extend_from_slice(&buffer[..read]);
                if response.len() > MAX_LEVEL_RESPONSE_BYTES {
                    return None;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                wait_within(stream.as_raw_fd(), libc::POLLIN, deadline)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return None,
        }
    }
}
