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

    /// The restore-token file this portal was explicitly configured with, when
    /// one was injected instead of resolved from the environment. Token
    /// lifecycle (load, rotate, clear) must hit the same file the portal
    /// itself uses — including the clears the delivery adapters drive after a
    /// terminal failure, where only the portal knows the configured location.
    fn restore_token_file(&self) -> Option<&Path> {
        None
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

    #[cfg(test)]
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
            clear_restore_token(
                self.portal
                    .as_deref()
                    .and_then(RemoteDesktopPortal::restore_token_file),
            );
            self.terminal_failure = Some(reason);
        }
    }
}

const HYPRLAND_KEY_UP_DELAY: Duration = Duration::from_millis(50);

/// Presses a verified Hyprland chord by asking the compositor for key-down
/// then key-up. Command text stays owned by Hyprland; only sanitized tokens
/// are interpolated into a fixed `send_key_state` snippet.
pub trait HyprlandKeyEmitter: Send {
    fn send_key_state(&mut self, mods: &str, key: &str, down: bool) -> Result<(), BoundaryError>;
}

/// Focused-window probe used to choose Omarchy's terminal vs normal chord.
pub trait HyprlandActiveWindow: Send {
    fn active_window_json(&mut self) -> Result<Vec<u8>, BoundaryError>;
}

struct HyprctlKeyEmitter;

impl HyprlandKeyEmitter for HyprctlKeyEmitter {
    fn send_key_state(&mut self, mods: &str, key: &str, down: bool) -> Result<(), BoundaryError> {
        let lua = send_key_state_lua(mods, key, down);
        run_restricted_stdout("hyprctl", &["eval", &lua])
            .map(|_| ())
            .ok_or_else(|| {
                BoundaryError::new(BoundaryKind::Delivery, "Hyprland send_key_state failed")
            })
    }
}

struct HyprctlActiveWindow;

impl HyprlandActiveWindow for HyprctlActiveWindow {
    fn active_window_json(&mut self) -> Result<Vec<u8>, BoundaryError> {
        run_restricted_stdout("hyprctl", &["activewindow", "-j"])
            .ok_or_else(|| BoundaryError::new(BoundaryKind::Delivery, "active window unavailable"))
    }
}

fn send_key_state_lua(mods: &str, key: &str, down: bool) -> String {
    let state = if down { "down" } else { "up" };
    format!(
        r#"hl.dispatch(hl.dsp.send_key_state({{ mods = "{mods}", key = "{key}", state = "{state}" }}))"#
    )
}

fn classify_hyprland_active_window(payload: &[u8]) -> Result<bool, BoundaryError> {
    let window: serde_json::Value = serde_json::from_slice(payload)
        .map_err(|_| BoundaryError::new(BoundaryKind::Delivery, "active window unavailable"))?;
    let address = window
        .get("address")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    if address.is_empty() || address == "0x0" {
        return Err(BoundaryError::new(
            BoundaryKind::Delivery,
            "no focused window",
        ));
    }
    let terminal = window
        .get("tags")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|tags| {
            tags.iter()
                .filter_map(serde_json::Value::as_str)
                .any(|tag| tag.strip_suffix('*').unwrap_or(tag) == "terminal")
        });
    Ok(terminal)
}

pub struct HyprlandPasteAction {
    action: VerifiedPasteAction,
    emitter: Box<dyn HyprlandKeyEmitter>,
    window: Box<dyn HyprlandActiveWindow>,
    live_action_verifier: Option<LivePasteActionVerifier>,
}

impl HyprlandPasteAction {
    fn production(action: VerifiedPasteAction) -> Self {
        Self {
            action,
            emitter: Box::new(HyprctlKeyEmitter),
            window: Box::new(HyprctlActiveWindow),
            live_action_verifier: Some(Arc::new(
                crate::hyprland_bindings::discover_live_paste_action,
            )),
        }
    }

    pub fn with_test_runtime(
        action: VerifiedPasteAction,
        emitter: Box<dyn HyprlandKeyEmitter>,
        window: Box<dyn HyprlandActiveWindow>,
        live_action: Option<VerifiedPasteAction>,
    ) -> Self {
        Self {
            action,
            emitter,
            window,
            live_action_verifier: Some(Arc::new(move || live_action.clone())),
        }
    }

    async fn emit_chord(&mut self, mods: &str, key: &str) -> Result<(), BoundaryError> {
        let mods = mods.to_owned();
        let key = key.to_owned();
        let mut emitter = std::mem::replace(&mut self.emitter, Box::new(HyprctlKeyEmitter));
        let down_mods = mods.clone();
        let down_key = key.clone();
        let (emitter, down) = tokio::task::spawn_blocking(move || {
            let result = emitter.send_key_state(&down_mods, &down_key, true);
            (emitter, result)
        })
        .await
        .map_err(|_| {
            BoundaryError::new(BoundaryKind::Delivery, "Hyprland send_key_state failed")
        })?;
        if let Err(error) = down {
            self.emitter = emitter;
            return Err(error);
        }

        struct PendingUp {
            emitter: Option<Box<dyn HyprlandKeyEmitter>>,
            mods: String,
            key: String,
            pending: bool,
        }
        impl PendingUp {
            fn release_up(&mut self) {
                if self.pending
                    && let Some(emitter) = self.emitter.as_mut()
                {
                    let _ = emitter.send_key_state(&self.mods, &self.key, false);
                    self.pending = false;
                }
            }

            fn restore(mut self) -> Box<dyn HyprlandKeyEmitter> {
                self.release_up();
                self.emitter.take().expect("Paste Action emitter")
            }
        }
        impl Drop for PendingUp {
            fn drop(&mut self) {
                self.release_up();
            }
        }

        let mut pending = PendingUp {
            emitter: Some(emitter),
            mods,
            key,
            pending: true,
        };
        tokio::time::sleep(HYPRLAND_KEY_UP_DELAY).await;
        let mut emitter = pending.emitter.take().expect("Paste Action emitter");
        let up_mods = pending.mods.clone();
        let up_key = pending.key.clone();
        match tokio::task::spawn_blocking(move || {
            let result = emitter.send_key_state(&up_mods, &up_key, false);
            (emitter, result)
        })
        .await
        {
            Ok((emitter, result)) => {
                pending.emitter = Some(emitter);
                pending.pending = result.is_err();
                self.emitter = pending.restore();
                result
            }
            Err(_) => {
                pending.pending = false;
                Err(BoundaryError::new(
                    BoundaryKind::Delivery,
                    "Hyprland send_key_state failed",
                ))
            }
        }
    }

    async fn active_window_json(&mut self) -> Result<Vec<u8>, BoundaryError> {
        let mut window = std::mem::replace(&mut self.window, Box::new(HyprctlActiveWindow));
        let (window, result) = tokio::task::spawn_blocking(move || {
            let result = window.active_window_json();
            (window, result)
        })
        .await
        .map_err(|_| BoundaryError::new(BoundaryKind::Delivery, "active window unavailable"))?;
        self.window = window;
        result
    }
}

impl PasteBoundary for HyprlandPasteAction {
    fn invoke(&mut self, action: &VerifiedPasteAction) -> BoundaryFuture<'_, ()> {
        let expected = self.action.clone();
        let requested = action.clone();
        Box::pin(async move {
            if expected != requested {
                return Err(BoundaryError::new(
                    BoundaryKind::Delivery,
                    "verified Paste Action changed during Delivery",
                ));
            }
            if let Some(verifier) = self.live_action_verifier.clone() {
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
            let chord = match &requested.behavior {
                crate::hyprland_bindings::PasteBehavior::Simple => {
                    requested.shortcut.binding.clone()
                }
                crate::hyprland_bindings::PasteBehavior::OmarchyUniversal { normal, terminal } => {
                    let json = self.active_window_json().await?;
                    if classify_hyprland_active_window(&json)? {
                        terminal.binding.clone()
                    } else {
                        normal.binding.clone()
                    }
                }
            };
            let (mods, key) = crate::hyprland_bindings::sanitized_send_key_tokens(&chord)
                .ok_or_else(|| {
                    BoundaryError::new(
                        BoundaryKind::Delivery,
                        "verified Paste Action shortcut cannot be emitted safely",
                    )
                })?;
            self.emit_chord(&mods, &key).await
        })
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
        Self::with_paste_boundaries(
            Box::new(WlClipboard),
            action.clone(),
            Box::new(HyprlandPasteAction::production(action)),
        )
    }

    #[cfg(test)]
    pub(super) fn hyprland_paste_skips_remote_desktop(&self) -> bool {
        self.setup.is_none()
            && self.session.is_none()
            && self.paste.is_some()
            && !self.direct_enabled
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
                                    clear_restore_token(self.portal.restore_token_file());
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
                        clear_restore_token(self.portal.restore_token_file());
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
        match clipboard_write_candidate(*tool, text, slice) {
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

/// Writes one clipboard candidate, repairing a stale Wayland selection once.
///
/// Hyprland clipboard-history readers can leave the data-control selection in a
/// state where the next `wl-copy` exits immediately. A separate writer clears
/// that state, which is why restarting Voisu appeared to fix every later
/// Recording. Keep the normal path single-shot; only an observed Wayland
/// failure clears the stale selection and retries under the original budget.
fn clipboard_write_candidate(
    tool: ClipboardTool,
    text: &[u8],
    deadline: Duration,
) -> Result<ProcessOutcome, ProcessError> {
    clipboard_write_candidate_with(tool, text, deadline, run_restricted_serving_within)
}

fn clipboard_write_candidate_with<F>(
    tool: ClipboardTool,
    text: &[u8],
    deadline: Duration,
    mut run: F,
) -> Result<ProcessOutcome, ProcessError>
where
    F: FnMut(&str, &[&str], Option<&[u8]>, Duration) -> Result<ProcessOutcome, ProcessError>,
{
    let started = Instant::now();
    let (program, default_arguments) = tool.write_command();
    let arguments: &[&str] = match tool {
        // A Transcript is always UTF-8 text. Declaring that contract avoids
        // wl-copy's content sniffing, which can classify ordinary prose as a
        // non-text file format and make GUI paste targets reject it.
        ClipboardTool::WlClipboard => &["--type", "text/plain;charset=utf-8", "--"],
        ClipboardTool::Xclip => default_arguments,
    };
    let first = run(program, arguments, Some(text), deadline);
    if matches!(&first, Ok(outcome) if outcome.success) || tool != ClipboardTool::WlClipboard {
        return first;
    }

    let remaining = deadline.saturating_sub(started.elapsed());
    if remaining.is_zero() {
        return first;
    }
    let clear_budget = remaining / 2;
    if clear_budget.is_zero()
        || !matches!(
            run("wl-copy", &["--clear"], None, clear_budget),
            Ok(outcome) if outcome.success
        )
    {
        return first;
    }
    let retry_budget = deadline.saturating_sub(started.elapsed());
    if retry_budget.is_zero() {
        return first;
    }
    run(program, arguments, Some(text), retry_budget)
}

#[cfg(test)]
mod clipboard_recovery_tests {
    use super::*;

    #[test]
    fn stale_wayland_selection_is_cleared_and_second_write_recovers() {
        let mut calls = Vec::new();
        let mut stale = true;

        let outcome = clipboard_write_candidate_with(
            ClipboardTool::WlClipboard,
            b"second Transcript",
            Duration::from_secs(4),
            |program, arguments, input, _| {
                calls.push((program.to_owned(), arguments.join(" "), input.is_some()));
                if arguments == ["--clear"] {
                    stale = false;
                    return Ok(ProcessOutcome {
                        success: true,
                        stdout: Vec::new(),
                        stderr: Vec::new(),
                    });
                }
                Ok(ProcessOutcome {
                    success: !stale,
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                })
            },
        );

        let Ok(outcome) = outcome else {
            panic!("Wayland recovery must return an outcome")
        };

        assert!(outcome.success);
        assert_eq!(
            calls,
            vec![
                (
                    "wl-copy".to_owned(),
                    "--type text/plain;charset=utf-8 --".to_owned(),
                    true,
                ),
                ("wl-copy".to_owned(), "--clear".to_owned(), false),
                (
                    "wl-copy".to_owned(),
                    "--type text/plain;charset=utf-8 --".to_owned(),
                    true,
                ),
            ]
        );
    }

    #[test]
    fn successful_wayland_write_does_not_clear_or_retry() {
        let mut calls = 0;
        let outcome = clipboard_write_candidate_with(
            ClipboardTool::WlClipboard,
            b"Transcript",
            Duration::from_secs(4),
            |_, arguments, _, _| {
                calls += 1;
                assert_eq!(arguments, ["--type", "text/plain;charset=utf-8", "--"]);
                Ok(ProcessOutcome {
                    success: true,
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                })
            },
        );

        let Ok(outcome) = outcome else {
            panic!("successful Wayland write must return an outcome")
        };

        assert!(outcome.success);
        assert_eq!(calls, 1);
    }

    #[test]
    fn xclip_failure_is_not_repaired_with_wayland_commands() {
        let mut calls = 0;
        let outcome = clipboard_write_candidate_with(
            ClipboardTool::Xclip,
            b"Transcript",
            Duration::from_secs(4),
            |program, arguments, _, _| {
                calls += 1;
                assert_eq!(program, "xclip");
                assert_eq!(arguments, ["-selection", "clipboard", "-in"]);
                Ok(ProcessOutcome {
                    success: false,
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                })
            },
        );

        let Ok(outcome) = outcome else {
            panic!("xclip status failure must remain a process outcome")
        };

        assert!(!outcome.success);
        assert_eq!(calls, 1);
    }

    #[test]
    fn failed_clear_leaves_the_first_failed_outcome_unmasked() {
        // The clear failure is injected as Err(ProcessError::Unavailable); a
        // success=false clear outcome takes the same branch, so one variant
        // pins the behaviour.
        let mut calls = Vec::new();
        let outcome = clipboard_write_candidate_with(
            ClipboardTool::WlClipboard,
            b"Transcript",
            Duration::from_secs(4),
            |program, arguments, input, _| {
                calls.push((program.to_owned(), arguments.join(" "), input.is_some()));
                if arguments == ["--clear"] {
                    return Err(ProcessError::Unavailable);
                }
                Ok(ProcessOutcome {
                    success: false,
                    stdout: b"stale".to_vec(),
                    stderr: b"first write failed".to_vec(),
                })
            },
        );

        let Ok(outcome) = outcome else {
            panic!("a failed clear must return the first outcome, not the clear error")
        };

        // The caller sees the FIRST write's failed outcome byte for byte, so
        // the failed write carries through and a regression that swapped in
        // the clear error or an empty success cannot mask it.
        assert!(!outcome.success);
        assert_eq!(outcome.stdout, b"stale".to_vec());
        assert_eq!(outcome.stderr, b"first write failed".to_vec());
        assert_eq!(
            calls,
            vec![
                (
                    "wl-copy".to_owned(),
                    "--type text/plain;charset=utf-8 --".to_owned(),
                    true,
                ),
                ("wl-copy".to_owned(), "--clear".to_owned(), false),
            ]
        );
    }

    #[test]
    fn failed_retry_outcome_becomes_the_result() {
        let mut calls = Vec::new();
        let outcome = clipboard_write_candidate_with(
            ClipboardTool::WlClipboard,
            b"Transcript",
            Duration::from_secs(4),
            |program, arguments, input, _| {
                calls.push((program.to_owned(), arguments.join(" "), input.is_some()));
                if arguments == ["--clear"] {
                    return Ok(ProcessOutcome {
                        success: true,
                        stdout: Vec::new(),
                        stderr: Vec::new(),
                    });
                }
                let attempt = if calls.len() == 1 { "first" } else { "retry" };
                Ok(ProcessOutcome {
                    success: false,
                    stdout: attempt.as_bytes().to_vec(),
                    stderr: Vec::new(),
                })
            },
        );

        let Ok(outcome) = outcome else {
            panic!("a failed retry must remain a process outcome")
        };

        // The retry's own failure is what the caller sees — not the first
        // write's outcome and not a success — so the failed retry write also
        // carries through instead of being masked.
        assert!(!outcome.success);
        assert_eq!(outcome.stdout, b"retry".to_vec());
        assert_eq!(
            calls,
            vec![
                (
                    "wl-copy".to_owned(),
                    "--type text/plain;charset=utf-8 --".to_owned(),
                    true,
                ),
                ("wl-copy".to_owned(), "--clear".to_owned(), false),
                (
                    "wl-copy".to_owned(),
                    "--type text/plain;charset=utf-8 --".to_owned(),
                    true,
                ),
            ]
        );
    }

    #[test]
    fn deadline_budget_is_partitioned_across_write_clear_and_retry() {
        // Timing model: without a production clock seam the only elapsed-time
        // control is real time, so the first write sleeps 300ms of the 400ms
        // deadline — the same 4000/3000 -> 500/500ms partition production
        // uses, scaled down to keep the suite fast. `thread::sleep` only
        // guarantees at-least, so the bounds are one-sided where the clock
        // can only move a budget down: at most 100ms remains at the clear,
        // the clear gets half of that (<= 50ms, and the > 25ms floor merely
        // tolerates scheduler jitter on the 300ms sleep), and the retry keeps
        // the rest of that remainder because the clear call itself does not
        // measurably advance the clock.
        let mut calls = Vec::new();
        let outcome = clipboard_write_candidate_with(
            ClipboardTool::WlClipboard,
            b"Transcript",
            Duration::from_millis(400),
            |program, arguments, input, budget| {
                if calls.is_empty() {
                    std::thread::sleep(Duration::from_millis(300));
                }
                calls.push((
                    program.to_owned(),
                    arguments.join(" "),
                    input.is_some(),
                    budget,
                ));
                if arguments == ["--clear"] {
                    return Ok(ProcessOutcome {
                        success: true,
                        stdout: Vec::new(),
                        stderr: Vec::new(),
                    });
                }
                Ok(ProcessOutcome {
                    success: false,
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                })
            },
        );

        let Ok(outcome) = outcome else {
            panic!("partitioned budgets must still return an outcome")
        };
        assert!(!outcome.success);

        assert_eq!(calls.len(), 3);
        // First write: the full deadline, unpartitioned.
        assert_eq!(calls[0].3, Duration::from_millis(400));
        // Clear: half of the remaining budget.
        assert!(calls[1].3 <= Duration::from_millis(50));
        assert!(calls[1].3 > Duration::from_millis(25));
        // Retry: the remainder, still far more than another half-slice.
        assert!(calls[2].3 <= Duration::from_millis(100));
        assert!(calls[2].3 > calls[1].3 * 3 / 2);
    }
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
            portal: Box::new(FedoraRemoteDesktopPortal::default()),
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
        let mut portal = FedoraRemoteDesktopPortal::default();
        portal.connect().await
    })
}

#[cfg(test)]
mod hyprland_paste_lua_tests {
    use super::send_key_state_lua;

    fn expected(mods: &str, key: &str, state: &str) -> String {
        format!(
            r#"hl.dispatch(hl.dsp.send_key_state({{ mods = "{mods}", key = "{key}", state = "{state}" }}))"#
        )
    }

    fn skeleton(lua: &str, mods: &str, key: &str, state: &str) -> String {
        lua.replace(&format!("mods = \"{mods}\""), "mods = \"\"")
            .replace(&format!("key = \"{key}\""), "key = \"\"")
            .replace(&format!("state = \"{state}\""), "state = \"\"")
    }

    #[test]
    fn hyprland_send_key_state_lua_interpolates_only_sanitized_tokens() {
        assert_eq!(
            send_key_state_lua("CTRL", "V", true),
            expected("CTRL", "V", "down")
        );
        assert_eq!(
            send_key_state_lua("CTRL", "V", false),
            expected("CTRL", "V", "up")
        );
        assert_eq!(
            send_key_state_lua("SHIFT", "Insert", true),
            expected("SHIFT", "Insert", "down")
        );
        assert_eq!(
            send_key_state_lua("SHIFT", "Insert", false),
            expected("SHIFT", "Insert", "up")
        );

        let ctrl_down = send_key_state_lua("CTRL", "V", true);
        let shift_up = send_key_state_lua("SHIFT", "Insert", false);
        assert_eq!(
            skeleton(&ctrl_down, "CTRL", "V", "down"),
            skeleton(&shift_up, "SHIFT", "Insert", "up")
        );
        assert_eq!(
            skeleton(&ctrl_down, "CTRL", "V", "down"),
            r#"hl.dispatch(hl.dsp.send_key_state({ mods = "", key = "", state = "" }))"#
        );
    }
}
