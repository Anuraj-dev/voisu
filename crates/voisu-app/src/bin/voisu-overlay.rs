//! Optional GTK4 Layer Shell observer. It has no command path into the daemon.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::rc::Rc;
use std::time::Duration;

use gtk4 as gtk;
use gtk::prelude::*;
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use voisu_app::overlay::{OverlayPhase, PresentationController};
use voisu_core::{Command, PROTOCOL_VERSION, Request, Response, socket_path};

fn main() {
    let application = gtk::Application::builder()
        .application_id("org.voisu.Overlay")
        .build();
    application.connect_activate(build_overlay);
    application.run();
}

fn build_overlay(application: &gtk::Application) {
    let window = gtk::ApplicationWindow::builder()
        .application(application)
        .default_width(280)
        .default_height(64)
        .resizable(false)
        .focusable(false)
        .can_focus(false)
        .build();

    window.init_layer_shell();
    window.set_layer(Layer::Overlay);
    window.set_anchor(Edge::Bottom, true);
    window.set_margin(Edge::Bottom, 24);
    window.set_keyboard_mode(KeyboardMode::None);
    window.set_exclusive_zone(-1);
    window.connect_realize(|window| {
        if let Some(surface) = window.surface() {
            let empty_region = gtk::cairo::Region::create();
            surface.set_input_region(Some(&empty_region));
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

    let controller = Rc::new(std::cell::RefCell::new(PresentationController::default()));
    let reduced_motion = gtk::Settings::default()
        .map(|settings| !settings.is_gtk_enable_animations())
        .unwrap_or(true);
    let window_ref = window.clone();
    let label_ref = label.clone();
    let meter_ref = meter.clone();
    let controller_ref = controller.clone();
    gtk::glib::timeout_add_local(Duration::from_millis(200), move || {
        let Some(response) = read_status() else {
            capsule.remove_css_class("recording");
            capsule.remove_css_class("processing");
            capsule.remove_css_class("success");
            capsule.add_css_class("failure");
            label_ref.set_label("Daemon unavailable");
            meter_ref.set_label("");
            label_ref.update_property(&[gtk::accessible::Property::Description(
                "Daemon unavailable; the optional Overlay cannot reach voisu-daemon",
            )]);
            window_ref.set_visible(true);
            return gtk::glib::ControlFlow::Continue;
        };
        let view = controller_ref.borrow_mut().observe(&response, std::time::Instant::now());
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
            window_ref.set_visible(false);
            return gtk::glib::ControlFlow::Continue;
        }
        label_ref.set_label(view.visible_label);
        label_ref.update_property(&[gtk::accessible::Property::Description(view.accessible_label)]);
        meter_ref.set_label(if view.phase == OverlayPhase::Recording {
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
        if view.is_visible() {
            window_ref.set_visible(true);
        }
        // No animation source is installed for hidden, Processing, terminal,
        // or reduced-motion states. Recording activity is status-driven.
        let _ = view.animation_interval(reduced_motion);
        gtk::glib::ControlFlow::Continue
    });
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
