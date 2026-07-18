//! Optional GTK feedback observer. It has no command path into the daemon.

use std::cell::{Cell, RefCell};
use std::env;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::process::Command as ProcessCommand;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gtk4 as gtk;
use gtk::prelude::*;
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use voisu_app::feedback::{
    after_surface_creation, select_feedback_backend, FeedbackBackend, FeedbackCapabilities,
    FeedbackDegradation, FeedbackSelection, OverlayRestartPolicy, SessionKind,
};
use voisu_app::overlay::{OverlayPhase, OverlayView, PresentationController, PresentationTracker};
use voisu_core::{Command, PROTOCOL_VERSION, Request, Response, socket_path};

fn main() {
    let arguments: Vec<_> = env::args().skip(1).collect();
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

    // Do not ask GTK to initialize when no graphical display can exist. A
    // desktop notification cannot be delivered there; the persistent journal
    // observer below is the truthful last-resort feedback backend.
    let preflight = environment_capabilities(false);
    let preliminary = select_feedback_backend(preflight);
    if preliminary.backend == FeedbackBackend::JournalLog {
        report(preliminary);
        return if report_only { 0 } else { run_journal_feedback(preliminary) };
    }

    if let Err(error) = gtk::init() {
        let selection = FeedbackSelection {
            backend: FeedbackBackend::JournalLog,
            degradation: Some(FeedbackDegradation::MissingDisplay),
        };
        report_with_error(selection, &error.to_string());
        return if report_only { 0 } else { run_journal_feedback(selection) };
    }

    let capabilities = environment_capabilities(gtk4_layer_shell::is_supported());
    let selection = select_feedback_backend(capabilities);
    report(selection);
    if report_only {
        return 0;
    }

    let application = gtk::Application::builder()
        .application_id("org.voisu.Overlay")
        .build();
    application.connect_activate(move |application| build_feedback(application, selection));
    i32::from(application.run())
}

/// Inputs collected outside the pure selector. GTK and Layer Shell APIs stay
/// in this adapter so contract tests need neither a compositor nor a display.
fn environment_capabilities(layer_shell_supported: bool) -> FeedbackCapabilities {
    let wayland_display = env::var_os("WAYLAND_DISPLAY").is_some();
    let x11_display = env::var_os("DISPLAY").is_some();
    let declared_wayland = matches!(
        env::var("XDG_SESSION_TYPE").ok().as_deref(),
        Some(value) if value.eq_ignore_ascii_case("wayland")
    );
    let xwayland_fallback = declared_wayland && !wayland_display && x11_display;
    let session = if xwayland_fallback {
        SessionKind::X11
    } else {
        match env::var("XDG_SESSION_TYPE").ok().as_deref() {
        Some(value) if value.eq_ignore_ascii_case("wayland") => SessionKind::Wayland,
        Some(value) if value.eq_ignore_ascii_case("x11") => SessionKind::X11,
        _ if wayland_display => SessionKind::Wayland,
        _ if x11_display => SessionKind::X11,
        _ => SessionKind::Unknown,
        }
    };
    let display_available = match session {
        SessionKind::Wayland => wayland_display,
        SessionKind::X11 => x11_display,
        SessionKind::Unknown => wayland_display || x11_display,
    };
    FeedbackCapabilities { session, display_available, xwayland_fallback, layer_shell_supported }
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
    let meter = gtk::Label::builder().label("").build();
    meter.add_css_class("meter");
    let capsule = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    capsule.set_margin_start(20);
    capsule.set_margin_end(20);
    capsule.set_margin_top(12);
    capsule.set_margin_bottom(12);
    capsule.append(&label);
    capsule.append(&meter);
    window.set_child(Some(&capsule));
    window.set_visible(false);

    let css = gtk::CssProvider::new();
    css.load_from_data(
        "window.background { background: transparent; }
         .capsule { background: rgba(23, 25, 29, 0.96); border-radius: 32px; }
         .capsule .state-label, .capsule .meter { color: #F4F5F7; font-size: 11pt; font-weight: 600; }
         .capsule.recording .state-label, .capsule.recording .meter { color: #65D6A0; }
         .capsule.processing .state-label, .capsule.processing .meter { color: #8FB4FF; }
         .capsule.success .state-label, .capsule.success .meter { color: #B8E986; }
         .capsule.failure .state-label, .capsule.failure .meter { color: #FF8A8A; }",
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
    install_surface_feedback(application.clone(), selection, window, label, meter, capsule, switched);
}

fn install_surface_feedback(
    application: gtk::Application,
    selection: FeedbackSelection,
    window: gtk::ApplicationWindow,
    label: gtk::Label,
    meter: gtk::Label,
    capsule: gtk::Box,
    switched: Rc<Cell<bool>>,
) {
    let controller = Rc::new(RefCell::new(PresentationController::default()));
    // Resurfacing and the Recording-start notification are only needed on the
    // fallback path: a layer-shell surface is kept above by the compositor and
    // has no "buried window" problem, so its behavior is left untouched.
    let is_fallback = selection.backend == FeedbackBackend::RegularSurface;
    let tracker = Rc::new(RefCell::new(PresentationTracker::default()));
    let reduced_motion = gtk::Settings::default()
        .map(|settings| !settings.is_gtk_enable_animations())
        .unwrap_or(true);
    gtk::glib::timeout_add_local(Duration::from_millis(200), move || {
        if switched.get() {
            // A genuine surface-creation failure handed feedback to the
            // notification backend; stop driving the retired window.
            return gtk::glib::ControlFlow::Break;
        }
        let now = Instant::now();
        let view = match read_status() {
            Some(response) => controller.borrow_mut().observe(&response, now),
            None => controller.borrow_mut().observe_unreachable(now),
        };
        render_surface(&window, &label, &meter, &capsule, view, reduced_motion);
        if is_fallback {
            let decision = tracker.borrow_mut().observe(view);
            // Wayland denies a plain toplevel keep-above; re-present it on each
            // transition into a visible phase so it resurfaces above occluders.
            if decision.resurface {
                window.present();
            }
            // A buried fallback window may be missed on GNOME, so signal
            // Recording start with a bounded desktop notification. Failure here
            // never breaks the overlay — send_notification cannot panic and its
            // delivery is the compositor's concern.
            if decision.entered_recording {
                let notification = gtk::gio::Notification::new("Voisu");
                notification.set_body(Some(view.visible_label));
                application.send_notification(Some("overlay-recording"), &notification);
            }
        }
        gtk::glib::ControlFlow::Continue
    });
}

fn install_notification_feedback(application: &gtk::Application) {
    // A notification backend has no window. Keep its GApplication alive for
    // the source lifetime so the polling timeout can actually run.
    let hold = application.hold();
    let controller = Rc::new(RefCell::new(PresentationController::default()));
    let previous = Rc::new(RefCell::new(OverlayView::HIDDEN));
    let application = application.clone();
    gtk::glib::timeout_add_local(Duration::from_millis(200), move || {
        let _hold = &hold;
        let now = Instant::now();
        let view = match read_status() {
            Some(response) => controller.borrow_mut().observe(&response, now),
            None => controller.borrow_mut().observe_unreachable(now),
        };
        if view.is_visible() && *previous.borrow() != view {
            let notification = gtk::gio::Notification::new("Voisu");
            notification.set_body(Some(view.visible_label));
            application.send_notification(Some("overlay-feedback"), &notification);
        }
        *previous.borrow_mut() = view;
        gtk::glib::ControlFlow::Continue
    });
}

fn render_surface(
    window: &gtk::ApplicationWindow,
    label: &gtk::Label,
    meter: &gtk::Label,
    capsule: &gtk::Box,
    view: OverlayView,
    reduced_motion: bool,
) {
    for class in ["recording", "processing", "success", "failure"] {
        capsule.remove_css_class(class);
    }
    let class = match view.phase {
        OverlayPhase::Recording => "recording",
        OverlayPhase::Processing => "processing",
        OverlayPhase::Success => "success",
        OverlayPhase::Failure => "failure",
        OverlayPhase::Hidden => "",
    };
    if !class.is_empty() {
        capsule.add_css_class(class);
    }
    if view.phase == OverlayPhase::Hidden {
        window.set_visible(false);
        return;
    }
    label.set_label(view.visible_label);
    label.update_property(&[gtk::accessible::Property::Description(view.accessible_label)]);
    meter.set_label(if view.phase == OverlayPhase::Recording {
        match view.activity {
            3 => "▂▆█",
            2 => "▂▅▆",
            _ => "▂▃▂",
        }
    } else if view.phase == OverlayPhase::Processing {
        "⋯"
    } else if view.phase == OverlayPhase::Failure {
        "⚠"
    } else {
        ""
    });
    window.set_visible(true);
    // No animation source is installed for hidden, Processing, terminal, or
    // reduced-motion states. Recording activity is status-driven.
    let _ = view.animation_interval(reduced_motion);
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
