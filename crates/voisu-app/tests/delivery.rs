use std::fs;
use std::io::{BufRead, BufReader};
use std::os::fd::OwnedFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

use tempfile::TempDir;
use tokio::sync::Notify;

use voisu_app::focus::SharedFocusProbe;
use voisu_app::hyprland_bindings::{PasteBehavior, PasteShortcut, VerifiedPasteAction};
use voisu_app::system::{
    ClipboardBoundary, DirectDeliverySession, FedoraRemoteDesktopPortal, GuardedDelivery,
    NotificationBoundary, PasteBoundary, PortalClipboardDelivery, PortalPasteAction,
    RemoteDesktopPortal,
};
use voisu_core::{
    BoundaryError, BoundaryFuture, BoundaryKind, DeliveryAdapter, DeliveryMethod, DeliveryOutcome,
    FocusProbe, Transcript, WindowIdentity,
};

struct RecordingClipboard(Arc<Mutex<Vec<String>>>);

impl ClipboardBoundary for RecordingClipboard {
    fn preserve(&mut self, transcript: &Transcript) -> BoundaryFuture<'_, ()> {
        let text = transcript.0.clone();
        let events = Arc::clone(&self.0);
        Box::pin(async move {
            events.lock().unwrap().push(format!("clipboard:{text}"));
            Ok(())
        })
    }
}

struct FailingClipboard;

impl ClipboardBoundary for FailingClipboard {
    fn preserve(&mut self, _transcript: &Transcript) -> BoundaryFuture<'_, ()> {
        Box::pin(async {
            Err(BoundaryError::new(
                BoundaryKind::Delivery,
                "clipboard unavailable",
            ))
        })
    }
}

struct RecordingSession(Arc<Mutex<Vec<String>>>);

impl DirectDeliverySession for RecordingSession {
    fn deliver_text(&mut self, text: &str) -> BoundaryFuture<'_, ()> {
        let text = text.to_owned();
        let events = Arc::clone(&self.0);
        Box::pin(async move {
            events.lock().unwrap().push(format!("direct:{text}"));
            Ok(())
        })
    }
}

struct GrantedPortal(Arc<Mutex<Vec<String>>>);

impl RemoteDesktopPortal for GrantedPortal {
    fn connect(&mut self) -> BoundaryFuture<'_, Box<dyn DirectDeliverySession>> {
        let events = Arc::clone(&self.0);
        Box::pin(async move { Ok(Box::new(RecordingSession(events)) as _) })
    }
}

struct FailingPortal(&'static str);

impl RemoteDesktopPortal for FailingPortal {
    fn connect(&mut self) -> BoundaryFuture<'_, Box<dyn DirectDeliverySession>> {
        let reason = self.0;
        Box::pin(async move { Err(BoundaryError::new(BoundaryKind::Delivery, reason)) })
    }
}

struct CountingFailingPortal {
    reason: &'static str,
    attempts: Arc<AtomicUsize>,
}

impl RemoteDesktopPortal for CountingFailingPortal {
    fn connect(&mut self) -> BoundaryFuture<'_, Box<dyn DirectDeliverySession>> {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        let reason = self.reason;
        Box::pin(async move { Err(BoundaryError::new(BoundaryKind::Delivery, reason)) })
    }
}

struct FailingSession(&'static str);

impl DirectDeliverySession for FailingSession {
    fn deliver_text(&mut self, _text: &str) -> BoundaryFuture<'_, ()> {
        let reason = self.0;
        Box::pin(async move { Err(BoundaryError::new(BoundaryKind::Delivery, reason)) })
    }
}

struct PasteSession(Arc<Mutex<Vec<String>>>);

impl DirectDeliverySession for PasteSession {
    fn deliver_text(&mut self, _text: &str) -> BoundaryFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }

    fn deliver_shortcut(&mut self, shortcut: &str) -> BoundaryFuture<'_, ()> {
        let events = Arc::clone(&self.0);
        let shortcut = shortcut.to_owned();
        Box::pin(async move {
            events.lock().unwrap().push(format!("shortcut:{shortcut}"));
            Ok(())
        })
    }
}

struct PastePortal {
    attempts: Arc<AtomicUsize>,
    events: Arc<Mutex<Vec<String>>>,
    control: Option<Arc<PastePortalControl>>,
    failure: Option<&'static str>,
}

struct PastePortalControl {
    started: Notify,
    completed: Notify,
    release: Notify,
    wait_for_release: bool,
}

impl PastePortalControl {
    fn gated() -> Arc<Self> {
        Arc::new(Self {
            started: Notify::new(),
            completed: Notify::new(),
            release: Notify::new(),
            wait_for_release: true,
        })
    }

    fn immediate() -> Arc<Self> {
        Arc::new(Self {
            started: Notify::new(),
            completed: Notify::new(),
            release: Notify::new(),
            wait_for_release: false,
        })
    }
}

impl RemoteDesktopPortal for PastePortal {
    fn connect(&mut self) -> BoundaryFuture<'_, Box<dyn DirectDeliverySession>> {
        Box::pin(async {
            Err(BoundaryError::new(
                BoundaryKind::Delivery,
                "text portal path is not used by Paste Action",
            ))
        })
    }

    fn connect_paste(&mut self) -> BoundaryFuture<'_, Box<dyn DirectDeliverySession>> {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        let control = self.control.clone();
        let failure = self.failure;
        let events = Arc::clone(&self.events);
        Box::pin(async move {
            if let Some(control) = control {
                control.started.notify_one();
                if control.wait_for_release {
                    control.release.notified().await;
                }
                control.completed.notify_one();
            }
            match failure {
                Some(reason) => Err(BoundaryError::new(BoundaryKind::Delivery, reason)),
                None => Ok(Box::new(PasteSession(events)) as Box<dyn DirectDeliverySession>),
            }
        })
    }
}

struct SequenceFocusProbe(std::collections::VecDeque<Option<WindowIdentity>>);

impl FocusProbe for SequenceFocusProbe {
    fn current(&mut self) -> BoundaryFuture<'_, Option<WindowIdentity>> {
        let identity = self.0.pop_front().flatten();
        Box::pin(async move { Ok(identity) })
    }
}

struct RecordingDelivery {
    label: &'static str,
    events: Arc<Mutex<Vec<String>>>,
    outcome: DeliveryOutcome,
}

struct RecordingPaste {
    events: Arc<Mutex<Vec<String>>>,
    failure: Option<&'static str>,
}

impl PasteBoundary for RecordingPaste {
    fn invoke(&mut self, action: &VerifiedPasteAction) -> BoundaryFuture<'_, ()> {
        let event = format!("paste:{}", action.shortcut.binding);
        let events = Arc::clone(&self.events);
        let failure = self.failure;
        Box::pin(async move {
            events.lock().unwrap().push(event);
            match failure {
                Some(reason) => Err(BoundaryError::new(BoundaryKind::Delivery, reason)),
                None => Ok(()),
            }
        })
    }
}

fn verified_paste_action() -> VerifiedPasteAction {
    VerifiedPasteAction {
        shortcut: PasteShortcut {
            binding: "CTRL + SHIFT + P".to_owned(),
        },
        description: "Paste transcript".to_owned(),
        live_binding_identity: "test-live-binding".to_owned(),
        behavior: PasteBehavior::Simple,
    }
}

impl DeliveryAdapter for RecordingDelivery {
    fn deliver(&mut self, transcript: Transcript) -> BoundaryFuture<'_, DeliveryOutcome> {
        let event = format!("{}:{}", self.label, transcript.0);
        let events = Arc::clone(&self.events);
        let outcome = self.outcome.clone();
        Box::pin(async move {
            events.lock().unwrap().push(event);
            Ok(outcome)
        })
    }
}

struct RecordingNotifier(Arc<Mutex<Vec<String>>>);

impl NotificationBoundary for RecordingNotifier {
    fn notify(&mut self, body: &str) -> BoundaryFuture<'_, ()> {
        let body = body.to_owned();
        let events = Arc::clone(&self.0);
        Box::pin(async move {
            events.lock().unwrap().push(format!("notify:{body}"));
            Ok(())
        })
    }
}

fn identity(stable_id: &str) -> WindowIdentity {
    WindowIdentity {
        stable_id: stable_id.to_owned(),
        process_id: Some(4242),
        app_id: Some("org.example.Editor".to_owned()),
    }
}

fn guarded_delivery(
    focus: Vec<Option<WindowIdentity>>,
    events: Arc<Mutex<Vec<String>>>,
) -> GuardedDelivery {
    let probe: SharedFocusProbe = Arc::new(tokio::sync::Mutex::new(Box::new(SequenceFocusProbe(
        focus.into(),
    ))));
    GuardedDelivery::with_boundaries(
        probe,
        Box::new(RecordingDelivery {
            label: "direct",
            events: Arc::clone(&events),
            outcome: DeliveryOutcome::compositor_submitted(),
        }),
        Box::new(RecordingDelivery {
            label: "clipboard",
            events: Arc::clone(&events),
            outcome: DeliveryOutcome::clipboard_fallback("clipboard-only"),
        }),
        Box::new(RecordingNotifier(events)),
    )
}

#[tokio::test]
async fn guarded_delivery_auto_types_when_the_same_stable_window_remains_focused() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let mut delivery = guarded_delivery(
        vec![Some(identity("window-a")), Some(identity("window-a"))],
        Arc::clone(&events),
    );

    delivery.recording_started().await.unwrap();
    let outcome = delivery
        .deliver(Transcript("guarded text".to_owned()))
        .await
        .unwrap();

    assert_eq!(outcome.method, DeliveryMethod::CompositorSubmitted);
    assert_eq!(*events.lock().unwrap(), vec!["direct:guarded text"]);
}

#[tokio::test]
async fn guarded_delivery_falls_back_when_either_focus_snapshot_is_unavailable() {
    for focus in [
        vec![None, Some(identity("window-a"))],
        vec![Some(identity("window-a")), None],
    ] {
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut delivery = guarded_delivery(focus, Arc::clone(&events));

        delivery.recording_started().await.unwrap();
        let outcome = delivery
            .deliver(Transcript("fail closed".to_owned()))
            .await
            .unwrap();

        assert_eq!(outcome.method, DeliveryMethod::ClipboardFallback);
        assert_eq!(
            outcome.fallback_reason.as_deref(),
            Some("focus changed during Recording")
        );
        assert_eq!(
            *events.lock().unwrap(),
            vec![
                "clipboard:fail closed",
                "notify:focus changed — transcript preserved on the clipboard",
            ]
        );
    }
}

#[tokio::test]
async fn guarded_delivery_falls_back_and_notifies_when_the_stable_window_changes() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let mut delivery = guarded_delivery(
        vec![Some(identity("window-a")), Some(identity("window-b"))],
        Arc::clone(&events),
    );

    delivery.recording_started().await.unwrap();
    let outcome = delivery
        .deliver(Transcript("preserve me".to_owned()))
        .await
        .unwrap();

    assert_eq!(outcome.method, DeliveryMethod::ClipboardFallback);
    assert_eq!(
        outcome.fallback_reason.as_deref(),
        Some("focus changed during Recording")
    );
    assert_eq!(
        *events.lock().unwrap(),
        vec![
            "clipboard:preserve me",
            "notify:focus changed — transcript preserved on the clipboard",
        ]
    );
}

struct SessionPortal(Option<Box<dyn DirectDeliverySession>>);

impl RemoteDesktopPortal for SessionPortal {
    fn connect(&mut self) -> BoundaryFuture<'_, Box<dyn DirectDeliverySession>> {
        let session = self.0.take().expect("test portal connects once");
        Box::pin(async move { Ok(session) })
    }
}

#[tokio::test]
async fn clipboard_is_preserved_before_unicode_multiline_compositor_submission() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let mut delivery = PortalClipboardDelivery::with_boundaries(
        Box::new(RecordingClipboard(Arc::clone(&events))),
        Box::new(GrantedPortal(Arc::clone(&events))),
    );
    let transcript = Transcript("Hello, दुनिया!\nSecond line — ¿sí?".to_owned());
    let expected = transcript.0.clone();

    let outcome = delivery.deliver(transcript).await.unwrap();

    assert_eq!(outcome.method, DeliveryMethod::CompositorSubmitted);
    assert_eq!(
        *events.lock().unwrap(),
        vec![
            format!("clipboard:{expected}"),
            format!("direct:{expected}"),
        ]
    );
}

#[tokio::test]
async fn verified_paste_preserves_clipboard_then_attempts_one_paste_action() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let action = verified_paste_action();
    let mut delivery = PortalClipboardDelivery::with_paste_boundaries(
        Box::new(RecordingClipboard(Arc::clone(&events))),
        action,
        Box::new(RecordingPaste {
            events: Arc::clone(&events),
            failure: None,
        }),
    );

    let outcome = delivery
        .deliver(Transcript("final transcript".to_owned()))
        .await
        .unwrap();

    assert_eq!(outcome.method, DeliveryMethod::CompositorSubmitted);
    assert_eq!(
        events.lock().unwrap().as_slice(),
        ["clipboard:final transcript", "paste:CTRL + SHIFT + P",]
    );
}

#[tokio::test]
async fn paste_failure_keeps_the_final_transcript_on_clipboard_and_does_not_retry() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let action = verified_paste_action();
    let mut delivery = PortalClipboardDelivery::with_paste_boundaries(
        Box::new(RecordingClipboard(Arc::clone(&events))),
        action,
        Box::new(RecordingPaste {
            events: Arc::clone(&events),
            failure: Some("compositor rejected Paste Action"),
        }),
    );

    let outcome = delivery
        .deliver(Transcript("recoverable transcript".to_owned()))
        .await
        .unwrap();

    assert_eq!(outcome.method, DeliveryMethod::ClipboardFallback);
    assert!(
        outcome
            .fallback_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("Transcript remains on the clipboard"))
    );
    assert_eq!(
        events.lock().unwrap().as_slice(),
        ["clipboard:recoverable transcript", "paste:CTRL + SHIFT + P",]
    );
}

#[tokio::test]
async fn paste_portal_setup_is_backgrounded_and_later_delivers_the_shortcut() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let attempts = Arc::new(AtomicUsize::new(0));
    let control = PastePortalControl::gated();
    let action = verified_paste_action();
    let mut paste = PortalPasteAction::with_boundaries(
        action.clone(),
        Box::new(PastePortal {
            attempts: Arc::clone(&attempts),
            events: Arc::clone(&events),
            control: Some(Arc::clone(&control)),
            failure: None,
        }),
    );

    let first = paste
        .invoke(&action)
        .await
        .expect_err("setup should be pending");
    assert_eq!(
        first.diagnostic(),
        "Paste portal permission request pending"
    );
    tokio::time::timeout(Duration::from_secs(1), control.started.notified())
        .await
        .expect("paste portal setup should start");
    control.release.notify_one();
    tokio::time::timeout(Duration::from_secs(1), control.completed.notified())
        .await
        .expect("paste portal setup should complete");
    paste.invoke(&action).await.unwrap();

    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    assert_eq!(
        events.lock().unwrap().as_slice(),
        ["shortcut:CTRL + SHIFT + P"]
    );
}

#[tokio::test]
async fn paste_delivery_returns_before_permission_finishes_and_preserves_each_transcript() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let attempts = Arc::new(AtomicUsize::new(0));
    let control = PastePortalControl::gated();
    let action = verified_paste_action();
    let paste = PortalPasteAction::with_boundaries(
        action.clone(),
        Box::new(PastePortal {
            attempts: Arc::clone(&attempts),
            events: Arc::clone(&events),
            control: Some(Arc::clone(&control)),
            failure: None,
        }),
    );
    let mut delivery = PortalClipboardDelivery::with_paste_boundaries(
        Box::new(RecordingClipboard(Arc::clone(&events))),
        action,
        Box::new(paste),
    );

    let first = tokio::time::timeout(
        Duration::from_secs(1),
        delivery.deliver(Transcript("first transcript".to_owned())),
    )
    .await
    .expect("permission approval must not block Transcript completion")
    .unwrap();
    assert_eq!(first.method, DeliveryMethod::ClipboardFallback);
    assert!(
        first
            .fallback_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("permission request pending"))
    );

    tokio::time::timeout(Duration::from_secs(1), control.started.notified())
        .await
        .expect("paste portal setup should start");
    control.release.notify_one();
    tokio::time::timeout(Duration::from_secs(1), control.completed.notified())
        .await
        .expect("paste portal setup should complete");
    let second = delivery
        .deliver(Transcript("second transcript".to_owned()))
        .await
        .unwrap();
    assert_eq!(second.method, DeliveryMethod::CompositorSubmitted);
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    assert_eq!(
        events.lock().unwrap().as_slice(),
        [
            "clipboard:first transcript",
            "clipboard:second transcript",
            "shortcut:CTRL + SHIFT + P"
        ]
    );
}

#[tokio::test]
async fn paste_portal_terminal_denial_is_not_retried() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let attempts = Arc::new(AtomicUsize::new(0));
    let control = PastePortalControl::immediate();
    let action = verified_paste_action();
    let mut paste = PortalPasteAction::with_boundaries(
        action.clone(),
        Box::new(PastePortal {
            attempts: Arc::clone(&attempts),
            events,
            control: Some(Arc::clone(&control)),
            failure: Some("permission denied"),
        }),
    );

    let _ = paste.invoke(&action).await;
    tokio::time::timeout(Duration::from_secs(1), control.completed.notified())
        .await
        .expect("paste portal denial should complete");
    let first = paste.invoke(&action).await.expect_err("denial must fail");
    let second = paste
        .invoke(&action)
        .await
        .expect_err("denial must stay terminal");

    assert_eq!(first.diagnostic(), "permission denied");
    assert_eq!(second.diagnostic(), "permission denied");
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn direct_delivery_opt_out_reports_its_actual_reason() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let mut delivery = PortalClipboardDelivery::clipboard_only_with_reason(
        Box::new(RecordingClipboard(Arc::clone(&events))),
        "direct Delivery disabled by test; Transcript remains on the clipboard",
    );

    let outcome = delivery
        .deliver(Transcript("opt out".to_owned()))
        .await
        .unwrap();

    assert_eq!(outcome.method, DeliveryMethod::ClipboardFallback);
    assert_eq!(
        outcome.fallback_reason.as_deref(),
        Some("direct Delivery disabled by test; Transcript remains on the clipboard")
    );
}

#[tokio::test]
async fn no_verified_paste_action_is_explicitly_clipboard_only() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let mut delivery = PortalClipboardDelivery::clipboard_only_with_boundary(Box::new(
        RecordingClipboard(Arc::clone(&events)),
    ));

    let outcome = delivery
        .deliver(Transcript("clipboard only".to_owned()))
        .await
        .unwrap();

    assert_eq!(outcome.method, DeliveryMethod::ClipboardFallback);
    assert_eq!(
        outcome.fallback_reason.as_deref(),
        Some("no verified Hyprland Paste Action; Transcript remains on the clipboard")
    );
    assert_eq!(
        events.lock().unwrap().as_slice(),
        ["clipboard:clipboard only"]
    );
}

#[tokio::test]
async fn direct_delivery_is_never_attempted_when_clipboard_preservation_fails() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let mut delivery = PortalClipboardDelivery::with_boundaries(
        Box::new(FailingClipboard),
        Box::new(GrantedPortal(Arc::clone(&events))),
    );

    let error = delivery
        .deliver(Transcript("must remain recoverable".to_owned()))
        .await
        .unwrap_err();

    assert_eq!(error.public_message(), "Transcript Delivery failed");
    assert!(events.lock().unwrap().is_empty());
}

#[tokio::test]
async fn portal_denial_and_unavailable_input_capability_fall_back_explicitly() {
    for reason in ["permission denied", "libei capability unavailable"] {
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut delivery = PortalClipboardDelivery::with_boundaries(
            Box::new(RecordingClipboard(Arc::clone(&events))),
            Box::new(FailingPortal(reason)),
        );

        let outcome = delivery
            .deliver(Transcript("final only".to_owned()))
            .await
            .unwrap();

        assert_eq!(outcome.method, DeliveryMethod::ClipboardFallback);
        assert_eq!(outcome.fallback_reason.as_deref(), Some(reason));
        assert_eq!(*events.lock().unwrap(), vec!["clipboard:final only"]);
    }
}

#[tokio::test]
async fn permission_denial_is_terminal_for_the_daemon_lifetime() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let events = Arc::new(Mutex::new(Vec::new()));
    let mut delivery = PortalClipboardDelivery::with_boundaries(
        Box::new(RecordingClipboard(Arc::clone(&events))),
        Box::new(CountingFailingPortal {
            reason: "permission denied",
            attempts: Arc::clone(&attempts),
        }),
    );

    for text in ["first", "second"] {
        let outcome = delivery.deliver(Transcript(text.to_owned())).await.unwrap();
        assert_eq!(outcome.method, DeliveryMethod::ClipboardFallback);
        assert_eq!(
            outcome.fallback_reason.as_deref(),
            Some("permission denied")
        );
    }

    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    assert_eq!(
        *events.lock().unwrap(),
        vec!["clipboard:first", "clipboard:second"]
    );
}

#[tokio::test]
async fn revocation_disconnection_and_compositor_rejection_fall_back_explicitly() {
    for reason in [
        "permission revoked",
        "libei disconnected",
        "compositor rejected libei submission",
    ] {
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut delivery = PortalClipboardDelivery::with_boundaries(
            Box::new(RecordingClipboard(Arc::clone(&events))),
            Box::new(SessionPortal(Some(Box::new(FailingSession(reason))))),
        );

        let outcome = delivery
            .deliver(Transcript("final only".to_owned()))
            .await
            .unwrap();

        assert_eq!(outcome.method, DeliveryMethod::ClipboardFallback);
        assert_eq!(outcome.fallback_reason.as_deref(), Some(reason));
        assert_eq!(*events.lock().unwrap(), vec!["clipboard:final only"]);
    }
}

struct PrivateBus {
    child: Child,
    address: String,
    _config: TempDir,
}

impl PrivateBus {
    fn start() -> Self {
        let config = TempDir::new().unwrap();
        let path = config.path().join("bus.conf");
        fs::write(
            &path,
            format!(
                r#"<busconfig>
<type>session</type><listen>unix:dir={}</listen><auth>EXTERNAL</auth>
<policy context="default"><allow send_destination="*" eavesdrop="true"/><allow eavesdrop="true"/><allow own="*"/></policy>
</busconfig>"#,
                config.path().display()
            ),
        )
        .unwrap();
        let mut child = Command::new("dbus-daemon")
            .arg(format!("--config-file={}", path.display()))
            .args(["--nofork", "--print-address"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut address = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut address)
            .unwrap();
        Self {
            child,
            address: address.trim().to_owned(),
            _config: config,
        }
    }
}

impl Drop for PrivateBus {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[derive(Default)]
struct RemoteDesktopCalls {
    selected_types: u32,
    persist_mode: u32,
    restore_tokens: Vec<Option<String>>,
    started: usize,
    connected_to_eis: usize,
}

struct SessionService;

#[zbus::interface(name = "org.freedesktop.portal.Session")]
impl SessionService {
    async fn close(&self) {}
}

struct RemoteDesktopService(Arc<Mutex<RemoteDesktopCalls>>);

fn sender(header: &zbus::message::Header<'_>) -> String {
    header
        .sender()
        .unwrap()
        .as_str()
        .trim_start_matches(':')
        .replace('.', "_")
}

fn token(
    options: &std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
    name: &str,
) -> String {
    options[name]
        .downcast_ref::<zbus::zvariant::Str<'_>>()
        .unwrap()
        .as_str()
        .to_owned()
}

async fn respond(
    connection: &zbus::Connection,
    path: &str,
    results: std::collections::HashMap<&str, zbus::zvariant::Value<'_>>,
) {
    connection
        .emit_signal(
            None::<zbus::names::BusName<'_>>,
            path,
            "org.freedesktop.portal.Request",
            "Response",
            &(0_u32, results),
        )
        .await
        .unwrap();
}

#[zbus::interface(name = "org.freedesktop.portal.RemoteDesktop")]
impl RemoteDesktopService {
    async fn create_session(
        &self,
        options: std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
        #[zbus(header)] header: zbus::message::Header<'_>,
        #[zbus(connection)] connection: &zbus::Connection,
    ) -> zbus::zvariant::OwnedObjectPath {
        let sender = sender(&header);
        let request = format!(
            "/org/freedesktop/portal/desktop/request/{sender}/{}",
            token(&options, "handle_token")
        );
        let session = format!(
            "/org/freedesktop/portal/desktop/session/{sender}/{}",
            token(&options, "session_handle_token")
        );
        connection
            .object_server()
            .at(session.as_str(), SessionService)
            .await
            .unwrap();
        respond(
            connection,
            &request,
            std::collections::HashMap::from([(
                "session_handle",
                zbus::zvariant::Value::from(session.as_str()),
            )]),
        )
        .await;
        zbus::zvariant::OwnedObjectPath::try_from(request).unwrap()
    }

    async fn select_devices(
        &self,
        _session: zbus::zvariant::OwnedObjectPath,
        options: std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
        #[zbus(header)] header: zbus::message::Header<'_>,
        #[zbus(connection)] connection: &zbus::Connection,
    ) -> zbus::zvariant::OwnedObjectPath {
        {
            let mut calls = self.0.lock().unwrap();
            calls.selected_types = options["types"].downcast_ref::<u32>().unwrap();
            calls.persist_mode = options["persist_mode"].downcast_ref::<u32>().unwrap();
            calls.restore_tokens.push(
                options
                    .get("restore_token")
                    .and_then(|value| value.downcast_ref::<zbus::zvariant::Str<'_>>().ok())
                    .map(|value| value.as_str().to_owned()),
            );
        }
        let request = format!(
            "/org/freedesktop/portal/desktop/request/{}/{}",
            sender(&header),
            token(&options, "handle_token")
        );
        respond(connection, &request, std::collections::HashMap::new()).await;
        zbus::zvariant::OwnedObjectPath::try_from(request).unwrap()
    }

    async fn start(
        &self,
        _session: zbus::zvariant::OwnedObjectPath,
        _parent_window: String,
        options: std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
        #[zbus(header)] header: zbus::message::Header<'_>,
        #[zbus(connection)] connection: &zbus::Connection,
    ) -> zbus::zvariant::OwnedObjectPath {
        let restore_token = {
            let mut calls = self.0.lock().unwrap();
            calls.started += 1;
            format!("restore-{}", calls.started)
        };
        let request = format!(
            "/org/freedesktop/portal/desktop/request/{}/{}",
            sender(&header),
            token(&options, "handle_token")
        );
        respond(
            connection,
            &request,
            std::collections::HashMap::from([
                ("devices", zbus::zvariant::Value::from(1_u32)),
                (
                    "restore_token",
                    zbus::zvariant::Value::from(restore_token.as_str()),
                ),
            ]),
        )
        .await;
        zbus::zvariant::OwnedObjectPath::try_from(request).unwrap()
    }

    #[zbus(name = "ConnectToEIS")]
    async fn connect_to_eis(
        &self,
        _session: zbus::zvariant::OwnedObjectPath,
        _options: std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
    ) -> zbus::zvariant::OwnedFd {
        let (client, server) = UnixStream::pair().unwrap();
        drop(server);
        let client: OwnedFd = client.into();
        let mut calls = self.0.lock().unwrap();
        calls.connected_to_eis += 1;
        client.into()
    }
}

#[tokio::test(flavor = "current_thread")]
async fn production_portal_rotates_persistent_permission_and_connects_libei() {
    let bus = PrivateBus::start();
    let calls = Arc::new(Mutex::new(RemoteDesktopCalls::default()));
    let (ready_tx, ready_rx) = mpsc::channel();
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let address = bus.address.clone();
    let service_calls = Arc::clone(&calls);
    let service = thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async move {
            let _connection = zbus::connection::Builder::address(address.as_str())
                .unwrap()
                .name("org.freedesktop.portal.Desktop")
                .unwrap()
                .serve_at(
                    "/org/freedesktop/portal/desktop",
                    RemoteDesktopService(service_calls),
                )
                .unwrap()
                .build()
                .await
                .unwrap();
            ready_tx.send(()).unwrap();
            // Await the stop signal asynchronously so the current-thread runtime
            // keeps driving zbus's executor; a blocking recv here would park the
            // only worker thread and starve the mock portal's method dispatch.
            let _ = stop_rx.await;
        });
    });
    ready_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    let token_dir = TempDir::new().unwrap();
    let token_file = token_dir.path().join("restore-token");

    let events = Arc::new(Mutex::new(Vec::new()));
    for text in ["first", "second"] {
        // The portal is bound to the private bus and the token file
        // explicitly, so the harness never mutates this process's
        // environment (which concurrently running tests read).
        let mut delivery = PortalClipboardDelivery::with_boundaries(
            Box::new(RecordingClipboard(Arc::clone(&events))),
            Box::new(FedoraRemoteDesktopPortal::with_endpoints(
                bus.address.clone(),
                token_file.clone(),
            )),
        );
        let outcome = delivery.deliver(Transcript(text.to_owned())).await.unwrap();
        assert_eq!(outcome.method, DeliveryMethod::ClipboardFallback);
    }

    let _ = stop_tx.send(());
    let _ = service.join();
    let calls = calls.lock().unwrap();
    assert_eq!(calls.selected_types, 1);
    assert_eq!(calls.persist_mode, 2);
    assert_eq!(
        calls.restore_tokens,
        vec![None, Some("restore-1".to_owned())]
    );
    assert_eq!(calls.started, 2);
    assert_eq!(calls.connected_to_eis, 2);
    assert_eq!(fs::read_to_string(token_file).unwrap(), "restore-2");
    assert_eq!(
        fs::metadata(token_dir.path().join("restore-token"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}
