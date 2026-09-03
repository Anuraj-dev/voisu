// Delivery boundaries: clipboard/paste traits, portal delivery, wl-clipboard, notifications.
//
// Split out of system.rs as a pure move; module-global items come from `super`.

use super::*;

pub trait ClipboardBoundary: Send {
    fn preserve(&mut self, transcript: &Transcript) -> BoundaryFuture<'_, ()>;
}

/// The only operation the post-copy path can perform. Implementations receive
/// a closed, source-verified action and never receive arbitrary Lua or shell
/// source to evaluate.
pub trait PasteBoundary: Send {
    fn invoke(&mut self, action: &VerifiedPasteAction) -> BoundaryFuture<'_, ()>;
}

pub trait DirectDeliverySession: Send {
    fn deliver_text(&mut self, text: &str) -> BoundaryFuture<'_, ()>;

    fn deliver_shortcut(&mut self, _shortcut: &str) -> BoundaryFuture<'_, ()> {
        Box::pin(async {
            Err(BoundaryError::new(
                BoundaryKind::Delivery,
                "keyboard shortcut submission unavailable",
            ))
        })
    }
}

pub trait RemoteDesktopPortal: Send {
    fn connect(&mut self) -> BoundaryFuture<'_, Box<dyn DirectDeliverySession>>;

    fn connect_paste(&mut self) -> BoundaryFuture<'_, Box<dyn DirectDeliverySession>> {
        self.connect()
    }
}

pub trait NotificationBoundary: Send {
    fn notify(&mut self, body: &str) -> BoundaryFuture<'_, ()>;
}

pub struct DesktopNotifier;

impl NotificationBoundary for DesktopNotifier {
    fn notify(&mut self, body: &str) -> BoundaryFuture<'_, ()> {
        let body = body.to_owned();
        Box::pin(async move {
            let notification = async {
                let connection = zbus::Connection::session().await.map_err(|_| {
                    BoundaryError::new(BoundaryKind::Delivery, "desktop notifications unavailable")
                })?;
                let proxy = zbus::Proxy::new(
                    &connection,
                    "org.freedesktop.Notifications",
                    "/org/freedesktop/Notifications",
                    "org.freedesktop.Notifications",
                )
                .await
                .map_err(|_| {
                    BoundaryError::new(BoundaryKind::Delivery, "desktop notifications unavailable")
                })?;
                let actions: Vec<String> = Vec::new();
                let hints: std::collections::HashMap<String, zbus::zvariant::OwnedValue> =
                    std::collections::HashMap::new();
                proxy
                    .call::<_, _, u32>(
                        "Notify",
                        &("Voisu", 0_u32, "", "Voisu", body, actions, hints, 5_000_i32),
                    )
                    .await
                    .map_err(|_| {
                        BoundaryError::new(BoundaryKind::Delivery, "desktop notification failed")
                    })?;
                Ok(())
            };
            tokio::time::timeout(PROCESS_DEADLINE, notification)
                .await
                .map_err(|_| {
                    BoundaryError::new(
                        BoundaryKind::Delivery,
                        "desktop notification deadline elapsed",
                    )
                })?
        })
    }
}

pub const FOCUS_GUARD_FALLBACK_REASON: &str = "focus changed during Recording";

pub const FOCUS_GUARD_NOTIFICATION: &str = "focus changed — transcript preserved on the clipboard";

pub struct GuardedDelivery {
    focus: SharedFocusProbe,
    start_identity: Option<voisu_core::WindowIdentity>,
    direct: Box<dyn DeliveryAdapter>,
    clipboard: Box<dyn DeliveryAdapter>,
    notifier: Box<dyn NotificationBoundary>,
}

impl GuardedDelivery {
    pub fn with_boundaries(
        focus: SharedFocusProbe,
        direct: Box<dyn DeliveryAdapter>,
        clipboard: Box<dyn DeliveryAdapter>,
        notifier: Box<dyn NotificationBoundary>,
    ) -> Self {
        Self {
            focus,
            start_identity: None,
            direct,
            clipboard,
            notifier,
        }
    }
}

impl DeliveryAdapter for GuardedDelivery {
    fn recording_started(&mut self) -> BoundaryFuture<'_, ()> {
        Box::pin(async move {
            self.start_identity = self.focus.lock().await.current().await.unwrap_or(None);
            Ok(())
        })
    }

    fn deliver(&mut self, transcript: Transcript) -> BoundaryFuture<'_, DeliveryOutcome> {
        Box::pin(async move {
            let current = self.focus.lock().await.current().await.unwrap_or(None);
            let unchanged = self
                .start_identity
                .as_ref()
                .zip(current.as_ref())
                .is_some_and(|(start, end)| start.stable_id == end.stable_id);
            self.start_identity = None;
            if unchanged {
                self.direct.deliver(transcript).await
            } else {
                eprintln!(
                    "focus guard: {FOCUS_GUARD_FALLBACK_REASON}; preserving Transcript on clipboard"
                );
                let mut outcome = self.clipboard.deliver(transcript).await?;
                outcome.fallback_reason = Some(FOCUS_GUARD_FALLBACK_REASON.to_owned());
                if let Err(error) = self.notifier.notify(FOCUS_GUARD_NOTIFICATION).await {
                    eprintln!("focus guard notification failed: {}", error.diagnostic());
                }
                Ok(outcome)
            }
        })
    }
}

pub struct PortalClipboardDelivery {
    clipboard: Box<dyn ClipboardBoundary>,
    portal: Box<dyn RemoteDesktopPortal>,
    paste: Option<Box<dyn PasteBoundary>>,
    paste_action: Option<VerifiedPasteAction>,
    direct_enabled: bool,
    clipboard_fallback_reason: String,
    session: Option<Box<dyn DirectDeliverySession>>,
    setup: Option<tokio::task::JoinHandle<Result<Box<dyn DirectDeliverySession>, BoundaryError>>>,
    setup_failure: Option<String>,
    setup_failure_terminal: bool,
    setup_retry_after: Option<Instant>,
    background_setup: bool,
}

const REMOTE_DESKTOP_RETRY_BACKOFF: Duration = Duration::from_secs(30);

/// A paste session is separate from the normal text Delivery session. The
/// portal may expose a text-capable device for Type Delivery, while a verified
/// Hyprland binding always needs a keyboard-capable device.
type PasteSetupResult = (
    Box<dyn RemoteDesktopPortal>,
    Result<Box<dyn DirectDeliverySession>, BoundaryError>,
);

type LivePasteActionVerifier = Arc<dyn Fn() -> Option<VerifiedPasteAction> + Send + Sync>;

pub struct PortalPasteAction {
    pub(super) action: VerifiedPasteAction,
    pub(super) portal: Option<Box<dyn RemoteDesktopPortal>>,
    pub(super) session: Option<Box<dyn DirectDeliverySession>>,
    pub(super) setup: Option<tokio::task::JoinHandle<PasteSetupResult>>,
    pub(super) terminal_failure: Option<String>,
    pub(super) live_action_verifier: Option<LivePasteActionVerifier>,
}

impl PortalPasteAction {
    pub fn with_boundaries(
        action: VerifiedPasteAction,
        portal: Box<dyn RemoteDesktopPortal>,
    ) -> Self {
        Self::with_options(action, portal, None)
    }

    pub(super) fn with_live_revalidation(
        action: VerifiedPasteAction,
        portal: Box<dyn RemoteDesktopPortal>,
    ) -> Self {
        let mut paste = Self::with_options(
            action,
            portal,
            Some(Arc::new(
                crate::hyprland_bindings::discover_live_paste_action,
            )),
        );
        // Overlap the portal grant with daemon lifetime, matching Type
        // Delivery, so the first Transcript is not the permission prompt.
        paste.begin_paste_setup();
        paste
    }

    pub(super) fn with_options(
        action: VerifiedPasteAction,
        portal: Box<dyn RemoteDesktopPortal>,
        live_action_verifier: Option<LivePasteActionVerifier>,
    ) -> Self {
        Self {
            action,
            portal: Some(portal),
            session: None,
            setup: None,
            terminal_failure: None,
            live_action_verifier,
        }
    }

    fn begin_paste_setup(&mut self) {
        if self.session.is_some() || self.setup.is_some() {
            return;
        }
        let Some(mut portal) = self.portal.take() else {
            return;
        };
        self.setup = Some(tokio::spawn(async move {
            let result = portal.connect_paste().await;
            (portal, result)
        }));
    }

    #[cfg(test)]
    pub(super) fn with_test_revalidation(
        action: VerifiedPasteAction,
        portal: Box<dyn RemoteDesktopPortal>,
        live_action: VerifiedPasteAction,
    ) -> Self {
        Self::with_options(
            action,
            portal,
            Some(Arc::new(move || Some(live_action.clone()))),
        )
    }
}

impl PasteBoundary for PortalPasteAction {
    fn invoke(&mut self, action: &VerifiedPasteAction) -> BoundaryFuture<'_, ()> {
        // The action is captured at construction and checked again here so a
        // caller cannot swap a discovered action for another one mid-round.
        let expected = self.action.clone();
        let requested = action.clone();
        Box::pin(async move {
            if expected != requested {
                return Err(BoundaryError::new(
                    BoundaryKind::Delivery,
                    "verified Paste Action changed during Delivery",
                ));
            }
            if let Some(reason) = self.terminal_failure.clone() {
                return Err(BoundaryError::new(BoundaryKind::Delivery, reason));
            }
            if let Some(verifier) = self.live_action_verifier.clone() {
                // hyprctl is short; await it on the invoke that needs the
                // result so this Recording is not dropped as clipboard-only.
                let live_action = tokio::task::spawn_blocking(move || verifier())
                    .await
                    .ok()
                    .flatten();
                if live_action.as_ref() != Some(&requested) {
                    return Err(BoundaryError::new(
                        BoundaryKind::Delivery,
                        "verified Paste Action is no longer active",
                    ));
                }
            }
            if self.session.is_none() {
                self.begin_paste_setup();
                let setup = self.setup.take().ok_or_else(|| {
                    BoundaryError::new(BoundaryKind::Delivery, "Paste portal unavailable")
                })?;
                if !setup.is_finished() {
                    self.setup = Some(setup);
                    return Err(BoundaryError::new(
                        BoundaryKind::Delivery,
                        "Paste portal permission request pending",
                    ));
                }
                match setup.await {
                    Ok((portal, Ok(session))) => {
                        self.portal = Some(portal);
                        self.session = Some(session);
                    }
                    Ok((portal, Err(error))) => {
                        self.portal = Some(portal);
                        self.remember_failure(&error);
                        return Err(error);
                    }
                    Err(_) => {
                        let reason = "Paste portal setup unavailable".to_owned();
                        self.terminal_failure = Some(reason.clone());
                        return Err(BoundaryError::new(BoundaryKind::Delivery, reason));
                    }
                }
            }
            let result = self
                .session
                .as_mut()
                .expect("Paste Action session was established")
                .deliver_shortcut(&requested.shortcut.binding)
                .await;
            if result.is_err() {
                self.session = None;
                if let Err(error) = &result {
                    self.remember_failure(error);
                }
            }
            result
        })
    }
}

impl PortalPasteAction {
    fn remember_failure(&mut self, error: &BoundaryError) {
        let reason = error.diagnostic().to_owned();
        if error.is_permanent() || terminal_remote_desktop_failure(&reason) {
            clear_restore_token();
            self.terminal_failure = Some(reason);
        }
    }
}

impl PortalClipboardDelivery {
    pub fn with_boundaries(
        clipboard: Box<dyn ClipboardBoundary>,
        portal: Box<dyn RemoteDesktopPortal>,
    ) -> Self {
        Self {
            clipboard,
            portal,
            paste: None,
            paste_action: None,
            direct_enabled: true,
            clipboard_fallback_reason:
                "no verified Hyprland Paste Action; Transcript remains on the clipboard".to_owned(),
            session: None,
            setup: None,
            setup_failure: None,
            setup_failure_terminal: false,
            setup_retry_after: None,
            background_setup: false,
        }
    }

    pub fn with_paste_boundaries(
        clipboard: Box<dyn ClipboardBoundary>,
        action: VerifiedPasteAction,
        paste: Box<dyn PasteBoundary>,
    ) -> Self {
        let mut delivery = Self::with_boundaries(clipboard, Box::new(DisabledRemoteDesktopPortal));
        delivery.paste = Some(paste);
        delivery.paste_action = Some(action);
        delivery.direct_enabled = false;
        delivery
    }

    pub fn clipboard_only() -> Self {
        Self::clipboard_only_with_boundary(Box::new(WlClipboard))
    }

    pub fn clipboard_only_with_boundary(clipboard: Box<dyn ClipboardBoundary>) -> Self {
        Self::clipboard_only_with_reason(
            clipboard,
            "no verified Hyprland Paste Action; Transcript remains on the clipboard",
        )
    }

    pub fn clipboard_only_with_reason(
        clipboard: Box<dyn ClipboardBoundary>,
        reason: impl Into<String>,
    ) -> Self {
        let mut delivery = Self::with_boundaries(clipboard, Box::new(DisabledRemoteDesktopPortal));
        delivery.direct_enabled = false;
        delivery.clipboard_fallback_reason = reason.into();
        delivery
    }

    pub fn with_hyprland_paste(action: VerifiedPasteAction) -> Self {
        let paste = PortalPasteAction::with_live_revalidation(
            action.clone(),
            Box::new(FedoraRemoteDesktopPortal),
        );
        Self::with_paste_boundaries(Box::new(WlClipboard), action, Box::new(paste))
    }
}

impl DeliveryAdapter for PortalClipboardDelivery {
    fn deliver(&mut self, transcript: Transcript) -> BoundaryFuture<'_, DeliveryOutcome> {
        Box::pin(async move {
            // Clipboard preservation is the recoverability guarantee.
            // Compositor submission is never reported unless this succeeds.
            self.clipboard.preserve(&transcript).await?;

            if let (Some(paste), Some(action)) = (self.paste.as_mut(), self.paste_action.clone()) {
                match paste.invoke(&action).await {
                    Ok(()) => return Ok(DeliveryOutcome::compositor_submitted()),
                    Err(error) => {
                        return Ok(DeliveryOutcome::clipboard_fallback(format!(
                            "Paste Action failed; Transcript remains on the clipboard: {}",
                            error.diagnostic()
                        )));
                    }
                }
            }

            if !self.direct_enabled {
                return Ok(DeliveryOutcome::clipboard_fallback(
                    self.clipboard_fallback_reason.clone(),
                ));
            }

            if self.session.is_none() {
                if let Some(reason) = self.setup_failure.clone() {
                    let retry_due = self
                        .setup_retry_after
                        .is_some_and(|deadline| Instant::now() >= deadline);
                    if !self.setup_failure_terminal && self.background_setup && retry_due {
                        self.setup_failure = None;
                        self.setup_retry_after = None;
                        self.setup = Some(spawn_remote_desktop_setup());
                    }
                    return Ok(DeliveryOutcome::clipboard_fallback(reason));
                }
                if let Some(setup) = self.setup.take() {
                    if setup.is_finished() {
                        match setup.await {
                            Ok(Ok(session)) => self.session = Some(session),
                            Ok(Err(error)) => {
                                let reason = error.diagnostic().to_owned();
                                self.setup_failure = Some(reason.clone());
                                self.setup_failure_terminal =
                                    terminal_remote_desktop_failure(&reason);
                                if self.background_setup && self.setup_failure_terminal {
                                    clear_restore_token();
                                }
                                self.setup_retry_after = (!self.setup_failure_terminal)
                                    .then(|| Instant::now() + REMOTE_DESKTOP_RETRY_BACKOFF);
                                return Ok(DeliveryOutcome::clipboard_fallback(reason));
                            }
                            Err(_) => {
                                return Ok(DeliveryOutcome::clipboard_fallback(
                                    "RemoteDesktop setup unavailable",
                                ));
                            }
                        }
                    } else {
                        self.setup = Some(setup);
                        return Ok(DeliveryOutcome::clipboard_fallback(
                            "RemoteDesktop permission request pending",
                        ));
                    }
                } else {
                    match self.portal.connect().await {
                        Ok(session) => self.session = Some(session),
                        Err(error) => {
                            let reason = error.diagnostic().to_owned();
                            if terminal_remote_desktop_failure(&reason) {
                                self.setup_failure = Some(reason.clone());
                                self.setup_failure_terminal = true;
                            }
                            return Ok(DeliveryOutcome::clipboard_fallback(reason));
                        }
                    }
                }
            }

            let result = self
                .session
                .as_mut()
                .expect("RemoteDesktop session was established")
                .deliver_text(&transcript.0)
                .await;
            match result {
                Ok(()) => Ok(DeliveryOutcome::compositor_submitted()),
                Err(error) => {
                    // A revoked/disconnected/rejecting libei session cannot be
                    // reused. The next Recording may request a fresh grant.
                    self.session = None;
                    let reason = error.diagnostic().to_owned();
                    self.setup_failure_terminal = terminal_remote_desktop_failure(&reason);
                    if self.background_setup && self.setup_failure_terminal {
                        clear_restore_token();
                    }
                    self.setup_failure = Some(reason.clone());
                    self.setup_retry_after = (!self.setup_failure_terminal)
                        .then(|| Instant::now() + REMOTE_DESKTOP_RETRY_BACKOFF);
                    Ok(DeliveryOutcome::clipboard_fallback(reason))
                }
            }
        })
    }
}

pub(super) fn terminal_remote_desktop_failure(reason: &str) -> bool {
    matches!(
        reason,
        "permission denied" | "permission revoked" | "keyboard permission unavailable"
    )
}

pub struct WlClipboard;

/// The total budget for the clipboard-write candidate loop: an Unknown session
/// may try Wayland then X11, and neither a failure nor a timeout on the first
/// backend may stop the second — but the whole loop stays bounded.
const CLIPBOARD_WRITE_DEADLINE: Duration = Duration::from_secs(4);

/// Write the Transcript to the clipboard through the backend that matches the
/// detected session, keeping the resident-serving semantics both stacks need
/// (`wl-copy` forks a serving child; `xclip` stays resident as the ICCCM
/// selection owner). Candidates are tried in order under one shared deadline,
/// but each attempt gets only a FAIR SLICE of the remaining budget (the rest
/// divided by the candidates still to try), so a hanging first backend can
/// never consume the whole deadline and starve the fallback: an Unknown session
/// still reaches X11 after a Wayland backend times out. Returns which tool
/// succeeded, or the last error.
fn clipboard_write(text: &[u8]) -> Result<ClipboardTool, ProcessError> {
    let session = current_session().session;
    let candidates = clipboard_candidates(session);
    let started = Instant::now();
    let mut last_error = ProcessError::Unavailable;
    for (index, tool) in candidates.iter().enumerate() {
        let remaining = CLIPBOARD_WRITE_DEADLINE.saturating_sub(started.elapsed());
        // Divide what is left evenly among the candidates not yet tried (this
        // one included), so time is reserved for the ones after it.
        let candidates_left = (candidates.len() - index) as u32;
        let slice = remaining / candidates_left;
        if slice.is_zero() {
            last_error = ProcessError::TimedOut;
            break;
        }
        let (program, arguments) = tool.write_command();
        match run_restricted_serving_within(program, arguments, Some(text), slice) {
            Ok(outcome) if outcome.success => return Ok(*tool),
            // Every backend-specific failure — a wrong session, a missing tool,
            // even a timeout — falls through to the next candidate rather than
            // stopping the loop.
            Err(error) => last_error = error,
            Ok(_) => last_error = ProcessError::Output,
        }
    }
    Err(last_error)
}

impl ClipboardBoundary for WlClipboard {
    fn preserve(&mut self, transcript: &Transcript) -> BoundaryFuture<'_, ()> {
        let text = transcript.0.clone();
        Box::pin(async move {
            let result = tokio::task::spawn_blocking(move || clipboard_write(text.as_bytes()))
                .await
                .map_err(|_| BoundaryError::new(BoundaryKind::Delivery, "clipboard task failed"))?;
            match result {
                Ok(_tool) => Ok(()),
                Err(ProcessError::TimedOut) => Err(BoundaryError::new(
                    BoundaryKind::Delivery,
                    "clipboard write deadline elapsed",
                )),
                Err(_) => Err(BoundaryError::new(
                    BoundaryKind::Delivery,
                    "no working clipboard backend (install wl-clipboard on Wayland or xclip on X11)",
                )),
            }
        })
    }
}

impl Default for PortalClipboardDelivery {
    fn default() -> Self {
        Self {
            clipboard: Box::new(WlClipboard),
            portal: Box::new(FedoraRemoteDesktopPortal),
            paste: None,
            paste_action: None,
            direct_enabled: true,
            clipboard_fallback_reason:
                "no verified Hyprland Paste Action; Transcript remains on the clipboard".to_owned(),
            session: None,
            setup: Some(spawn_remote_desktop_setup()),
            setup_failure: None,
            setup_failure_terminal: false,
            setup_retry_after: None,
            background_setup: true,
        }
    }
}

fn spawn_remote_desktop_setup()
-> tokio::task::JoinHandle<Result<Box<dyn DirectDeliverySession>, BoundaryError>> {
    tokio::spawn(async {
        let mut portal = FedoraRemoteDesktopPortal;
        portal.connect().await
    })
}
