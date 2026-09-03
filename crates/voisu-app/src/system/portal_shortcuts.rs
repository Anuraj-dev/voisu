// Fedora XDG desktop portal: GlobalShortcuts bind/activate and shortcut sessions.
//
// Split out of system.rs as a pure move; module-global items come from `super`.

use super::*;

pub(super) const PORTAL_BUS_NAME: &str = "org.freedesktop.portal.Desktop";

pub(super) const PORTAL_OBJECT_PATH: &str = "/org/freedesktop/portal/desktop";

pub(super) const GLOBAL_SHORTCUTS_INTERFACE: &str = "org.freedesktop.portal.GlobalShortcuts";

const PORTAL_REQUEST_INTERFACE: &str = "org.freedesktop.portal.Request";

pub(super) const PORTAL_SESSION_INTERFACE: &str = "org.freedesktop.portal.Session";

/// The single shortcut id Voisu binds: its activation toggles the Recording.
pub const TRIGGER_KEY_ID: &str = "voisu-toggle";

const TRIGGER_KEY_DESCRIPTION: &str = "Toggle Voisu Recording";

/// Bound wait for the CreateSession portal round trip — no user interaction is
/// involved, so a portal that does not answer within this is treated as absent.
pub(super) const PORTAL_SESSION_DEADLINE: Duration = Duration::from_secs(10);

/// Bound wait for the BindShortcuts response. Binding can require the user to
/// approve the Trigger Key in a desktop dialog, so this is generous; if the
/// user walks away the listener fails closed and CLI control stays usable.
pub(super) const PORTAL_BIND_DEADLINE: Duration = Duration::from_secs(300);

/// Bound wait for the best-effort Session.Close on retirement.
const PORTAL_CLOSE_DEADLINE: Duration = Duration::from_secs(2);

fn shortcut_error(detail: impl Into<String>) -> BoundaryError {
    BoundaryError::new(BoundaryKind::Shortcut, detail)
}

/// Production Global Shortcuts portal edge
/// (`org.freedesktop.portal.GlobalShortcuts`). It binds the Trigger Key through
/// the desktop portal so Voisu never touches raw input devices.
///
/// The portal delivers `Activated` signals — and resolves request/session
/// handles — against the caller's own D-Bus identity, so the session must live
/// on a persistent native connection owned by the daemon; a per-call
/// `busctl`/`gdbus` subprocess can create a session but can never receive its
/// activations (see docs/adr/). Every failure — no session bus, portal
/// name absent, permission denied — fails closed with a `Shortcut` boundary and
/// never fabricates a binding.
pub struct FedoraShortcutPortal;

impl FedoraShortcutPortal {
    pub fn new() -> Self {
        Self
    }
}

impl Default for FedoraShortcutPortal {
    fn default() -> Self {
        Self::new()
    }
}

/// The Global Shortcuts `session_handle_token`. This is DELIBERATELY constant
/// rather than per-process: xdg-desktop-portal-kde, unable to resolve an
/// app_id, persists a kglobalaccel component named after this token. A token
/// that varied per daemon process (e.g. by embedding the PID) presented a new
/// identity on every start, so KWin had no stored binding for it — it
/// re-prompted the user for a shortcut and leaked an orphaned
/// `[token_voisu_session_<pid>]` section into `kglobalshortcutsrc` on every
/// launch. Per the XDG GlobalShortcuts spec the `session_handle_token` need
/// only be unique among the app's *concurrently active* sessions, and this
/// daemon binds at most one Global Shortcuts session per run, so a constant
/// token is spec-valid and lets the desktop re-resolve the same persistent
/// binding silently across restarts.
const SHORTCUT_SESSION_TOKEN: &str = "voisu_session";

/// The portal tokens one shortcut bind cycle constructs. Extracted so the
/// stable-session-token invariant is testable without a live portal.
///
/// `create` and `bind` are request `handle_token`s: they identify in-flight
/// Request objects and are a *different* mechanism from the session handle
/// token, so they MUST stay unique per daemon process.
pub(super) struct ShortcutBindTokens {
    pub(super) session: &'static str,
    pub(super) create: String,
    pub(super) bind: String,
}

pub(super) fn shortcut_bind_tokens() -> ShortcutBindTokens {
    let unique = std::process::id();
    ShortcutBindTokens {
        session: SHORTCUT_SESSION_TOKEN,
        create: format!("voisu_create_{unique}"),
        bind: format!("voisu_bind_{unique}"),
    }
}

/// The portal request/session handle convention: predictable object paths are
/// derived from the caller's unique name (`:1.42` -> `1_42`) plus a
/// caller-chosen token, letting the caller subscribe to the `Response` signal
/// BEFORE issuing the request so no response can be missed.
pub(super) fn escaped_sender(connection: &zbus::Connection) -> Result<String, BoundaryError> {
    Ok(connection
        .unique_name()
        .ok_or_else(|| shortcut_error("session bus assigned no unique name"))?
        .trim_start_matches(':')
        .replace('.', "_"))
}

/// Performs one portal request round trip. Before invoking `method` it
/// subscribes to EVERY `org.freedesktop.portal.Request.Response` signal (a
/// broad match rule, not one keyed to the predictable handle path) so that a
/// portal answering on a divergent request handle can never emit its response
/// into a subscription gap; once the method returns the authoritative handle,
/// the buffered stream is filtered down to it. Returns the response's results
/// vardict; a non-zero response code (the user or desktop denied or cancelled
/// the request) fails closed.
pub(super) async fn portal_request<B>(
    connection: &zbus::Connection,
    portal: &zbus::Proxy<'_>,
    kind: BoundaryKind,
    method: &str,
    body: &B,
    deadline: Duration,
) -> Result<std::collections::HashMap<String, zbus::zvariant::OwnedValue>, BoundaryError>
where
    B: zbus::export::serde::ser::Serialize + zbus::zvariant::DynamicType,
{
    use zbus::export::ordered_stream::OrderedStreamExt;

    let rule = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface(PORTAL_REQUEST_INTERFACE)
        .and_then(|builder| builder.member("Response"))
        .map_err(|error| {
            BoundaryError::new(kind, format!("portal response rule invalid: {error}"))
        })?
        .build();
    let mut responses = zbus::MessageStream::for_match_rule(rule, connection, Some(16))
        .await
        .map_err(|error| {
            BoundaryError::new(
                kind,
                format!("portal response subscription failed: {error}"),
            )
        })?;

    let reply = portal
        .call_method(method, body)
        .await
        .map_err(|error| BoundaryError::new(kind, format!("portal {method} failed: {error}")))?;
    // Since xdg-desktop-portal 0.9 the returned handle equals the predictable
    // path; on an older portal it differs — either way the broad subscription
    // above already buffers its Response, so only the filter changes.
    let handle: zbus::zvariant::OwnedObjectPath = reply.body().deserialize().map_err(|error| {
        BoundaryError::new(kind, format!("portal {method} returned no handle: {error}"))
    })?;
    let deadline_at = tokio::time::Instant::now() + deadline;
    loop {
        let message = tokio::time::timeout_at(deadline_at, responses.next())
            .await
            .map_err(|_| {
                BoundaryError::new(kind, format!("portal {method} response deadline elapsed"))
            })?
            .ok_or_else(|| {
                BoundaryError::new(kind, format!("portal {method} response stream ended"))
            })?
            .map_err(|error| {
                BoundaryError::new(kind, format!("portal {method} response failed: {error}"))
            })?;
        let header = message.header();
        if header.path().map(|path| path.as_str()) != Some(handle.as_str()) {
            continue;
        }
        let (code, results): (
            u32,
            std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
        ) = message.body().deserialize().map_err(|error| {
            BoundaryError::new(kind, format!("portal {method} response malformed: {error}"))
        })?;
        if code != 0 {
            let error = BoundaryError::new(
                kind,
                format!("the desktop did not approve the {method} request (response {code})"),
            );
            // Only response 1 is an explicit user cancellation — a deliberate,
            // permanent decision. Any other non-zero code (e.g. 2, "interaction
            // ended some other way") can be a transient backend hiccup during
            // warmup, so it stays retryable rather than retiring the listener.
            return Err(if code == 1 { error.permanent() } else { error });
        }
        return Ok(results);
    }
}

/// Extracts the desktop-approved trigger description for `TRIGGER_KEY_ID` from
/// a BindShortcuts response (`shortcuts: a(sa{sv})`).
fn approved_trigger_description(
    results: &std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
) -> Option<String> {
    use zbus::zvariant::Value;
    let Value::Array(shortcuts) = &**results.get("shortcuts")? else {
        return None;
    };
    for entry in shortcuts.iter() {
        let Value::Structure(fields) = entry else {
            continue;
        };
        let [Value::Str(id), Value::Dict(properties)] = fields.fields() else {
            continue;
        };
        if id.as_str() != TRIGGER_KEY_ID {
            continue;
        }
        if let Ok(Some(description)) =
            properties.get::<_, zbus::zvariant::Str<'_>>(&"trigger_description")
        {
            return Some(description.as_str().to_owned());
        }
    }
    None
}

impl ShortcutPortal for FedoraShortcutPortal {
    fn bind(&mut self) -> BoundaryFuture<'_, Box<dyn ShortcutSession>> {
        Box::pin(async move {
            use zbus::zvariant::Value;

            let connection = zbus::Connection::session()
                .await
                .map_err(|error| shortcut_error(format!("session bus is unavailable: {error}")))?;
            let portal = zbus::Proxy::new(
                &connection,
                PORTAL_BUS_NAME,
                PORTAL_OBJECT_PATH,
                GLOBAL_SHORTCUTS_INTERFACE,
            )
            .await
            .map_err(|error| shortcut_error(format!("portal proxy failed: {error}")))?;

            // The session_handle_token is deliberately CONSTANT so the desktop
            // re-resolves the same persistent binding across restarts (see
            // SHORTCUT_SESSION_TOKEN). The request handle_tokens stay unique per
            // daemon process — a different mechanism identifying in-flight
            // Request objects. The daemon binds at most one session per run.
            let ShortcutBindTokens {
                session: session_token,
                create: create_token,
                bind: bind_token,
            } = shortcut_bind_tokens();
            let session_path = format!(
                "/org/freedesktop/portal/desktop/session/{}/{session_token}",
                escaped_sender(&connection)?
            );

            let create_options: std::collections::HashMap<&str, Value<'_>> =
                std::collections::HashMap::from([
                    ("handle_token", Value::from(create_token.as_str())),
                    ("session_handle_token", Value::from(session_token)),
                ]);
            let create_results = portal_request(
                &connection,
                &portal,
                BoundaryKind::Shortcut,
                "CreateSession",
                &(create_options,),
                PORTAL_SESSION_DEADLINE,
            )
            .await?;
            // The session handle returned by the portal is authoritative; the
            // predictable path is only the fallback for a portal that omits it.
            let session_path = session_handle_from(&create_results).unwrap_or(session_path);

            // Subscribe to this session's signals BEFORE binding so an
            // activation racing the bind response cannot be missed.
            let session_object_path: zbus::zvariant::OwnedObjectPath =
                zbus::zvariant::ObjectPath::try_from(session_path.as_str())
                    .map_err(|error| shortcut_error(format!("session handle malformed: {error}")))?
                    .into();
            let activations = portal.receive_signal("Activated").await.map_err(|error| {
                shortcut_error(format!("activation subscription failed: {error}"))
            })?;
            let session_proxy = zbus::Proxy::new(
                &connection,
                PORTAL_BUS_NAME,
                session_path.as_str().to_owned(),
                PORTAL_SESSION_INTERFACE,
            )
            .await
            .map_err(|error| shortcut_error(format!("session proxy failed: {error}")))?;
            let closures = session_proxy
                .receive_signal("Closed")
                .await
                .map_err(|error| shortcut_error(format!("closure subscription failed: {error}")))?;
            // Watch the portal's bus-name ownership: a crashed or restarted
            // portal emits no Session.Closed, so owner changes are the only
            // signal that the binding went stale and a rebind is due.
            let bus_proxy = zbus::Proxy::new(
                &connection,
                "org.freedesktop.DBus",
                "/org/freedesktop/DBus",
                "org.freedesktop.DBus",
            )
            .await
            .map_err(|error| shortcut_error(format!("bus proxy failed: {error}")))?;
            let owner_changes = bus_proxy
                .receive_signal_with_args("NameOwnerChanged", &[(0, PORTAL_BUS_NAME)])
                .await
                .map_err(|error| {
                    shortcut_error(format!("portal owner subscription failed: {error}"))
                })?;

            let shortcut_properties: std::collections::HashMap<&str, Value<'_>> =
                std::collections::HashMap::from([(
                    "description",
                    Value::from(TRIGGER_KEY_DESCRIPTION),
                )]);
            let shortcuts = vec![(TRIGGER_KEY_ID, shortcut_properties)];
            let bind_options: std::collections::HashMap<&str, Value<'_>> =
                std::collections::HashMap::from([(
                    "handle_token",
                    Value::from(bind_token.as_str()),
                )]);
            let results = match portal_request(
                &connection,
                &portal,
                BoundaryKind::Shortcut,
                "BindShortcuts",
                &(
                    session_object_path.clone(),
                    shortcuts,
                    // No parent window: the daemon has no surface of its own.
                    "",
                    bind_options,
                ),
                PORTAL_BIND_DEADLINE,
            )
            .await
            {
                Ok(results) => results,
                Err(error) => {
                    // The portal session already exists: a denied or failed
                    // bind must not leak it on the desktop.
                    close_portal_session(&connection, session_object_path.as_str()).await;
                    return Err(error);
                }
            };
            let binding = TriggerKeyBinding::new(
                approved_trigger_description(&results)
                    .unwrap_or_else(|| TRIGGER_KEY_DESCRIPTION.to_owned()),
            );

            Ok(Box::new(FedoraShortcutSession {
                connection,
                session_path: session_object_path,
                binding,
                activations,
                closures,
                owner_changes,
                retired: false,
            }) as Box<dyn ShortcutSession>)
        })
    }
}

/// Extracts the authoritative session handle from CreateSession results
/// (`session_handle` is a string per the portal contract; an object path is
/// tolerated).
pub(super) fn session_handle_from(
    results: &std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
) -> Option<String> {
    let value = results.get("session_handle")?;
    if let Ok(handle) = value.downcast_ref::<zbus::zvariant::Str<'_>>() {
        return Some(handle.as_str().to_owned());
    }
    value
        .downcast_ref::<zbus::zvariant::ObjectPath<'_>>()
        .ok()
        .map(|path| path.as_str().to_owned())
}

/// Best-effort, bounded `org.freedesktop.portal.Session.Close`.
pub(super) async fn close_portal_session(connection: &zbus::Connection, session_path: &str) {
    let close = async {
        if let Ok(session) = zbus::Proxy::new(
            connection,
            PORTAL_BUS_NAME,
            session_path.to_owned(),
            PORTAL_SESSION_INTERFACE,
        )
        .await
        {
            let _ = session.call_method("Close", &()).await;
        }
    };
    let _ = tokio::time::timeout(PORTAL_CLOSE_DEADLINE, close).await;
}

/// A live Global Shortcuts session on the daemon's persistent D-Bus connection.
/// The session owns the connection and all three signal subscriptions
/// (Activated, Session.Closed, portal NameOwnerChanged); retirement closes the
/// portal session with a bounded best-effort `Session.Close` so the desktop
/// does not keep a dangling session for a listener that is gone.
pub struct FedoraShortcutSession {
    connection: zbus::Connection,
    session_path: zbus::zvariant::OwnedObjectPath,
    binding: TriggerKeyBinding,
    activations: zbus::proxy::SignalStream<'static>,
    closures: zbus::proxy::SignalStream<'static>,
    owner_changes: zbus::proxy::SignalStream<'static>,
    retired: bool,
}

impl FedoraShortcutSession {
    /// The daemon's own D-Bus connection ended: all three signal streams close
    /// together. That is a transient, recoverable failure — not a revocation —
    /// so the dead session is retired and a stream error is reported, which the
    /// listener answers by rebinding once the portal is reachable again.
    fn stream_ended(&mut self) -> Result<voisu_core::ShortcutEvent, BoundaryError> {
        self.retired = true;
        Err(shortcut_error("Trigger Key activation stream ended"))
    }
}

impl ShortcutSession for FedoraShortcutSession {
    fn binding(&self) -> TriggerKeyBinding {
        self.binding.clone()
    }

    fn next_event(&mut self) -> BoundaryFuture<'_, voisu_core::ShortcutEvent> {
        Box::pin(async move {
            use voisu_core::ShortcutEvent;
            use zbus::export::ordered_stream::OrderedStreamExt;
            loop {
                tokio::select! {
                    activated = self.activations.next() => match activated {
                        Some(message) => {
                            // Activated(session_handle o, shortcut_id s,
                            //           timestamp t, options a{sv})
                            let Ok((session, shortcut_id, _timestamp, _options)) =
                                message.body().deserialize::<(
                                    zbus::zvariant::OwnedObjectPath,
                                    String,
                                    u64,
                                    std::collections::HashMap<
                                        String,
                                        zbus::zvariant::OwnedValue,
                                    >,
                                )>()
                            else {
                                continue;
                            };
                            if session == self.session_path && shortcut_id == TRIGGER_KEY_ID {
                                return Ok(ShortcutEvent::Activated);
                            }
                        }
                        None => return self.stream_ended(),
                    },
                    closed = self.closures.next() => match closed {
                        // The desktop emitted Session.Closed. That means only
                        // "the session ended", with no reason — a compositor or
                        // backend reset closes it the same way a revocation does.
                        // Report it as a recoverable closure; the listener
                        // rebinds and a genuine revocation refuses the next bind.
                        Some(_) => {
                            self.retired = true;
                            return Ok(ShortcutEvent::SessionClosed);
                        }
                        // The stream ended because the connection died, not
                        // because the desktop closed the session: recoverable.
                        None => return self.stream_ended(),
                    },
                    owner_change = self.owner_changes.next() => {
                        let Some(message) = owner_change else {
                            return self.stream_ended();
                        };
                        // NameOwnerChanged(name s, old_owner s, new_owner s):
                        // an empty new owner means the portal left the bus; a
                        // non-empty one means a (restarted) portal now owns it
                        // and this session is stale on the wrong owner.
                        let Ok((_name, _old_owner, new_owner)) =
                            message.body().deserialize::<(String, String, String)>()
                        else {
                            continue;
                        };
                        // No portal process that knows this session exists any
                        // more, so there is nothing to Close — mark it retired
                        // either way. On PortalLost the caller keeps polling
                        // this same session (its owner watch stays live) until
                        // a new owner yields PortalRestarted; on
                        // PortalRestarted the caller drops it and rebinds.
                        self.retired = true;
                        return Ok(if new_owner.is_empty() {
                            ShortcutEvent::PortalLost
                        } else {
                            ShortcutEvent::PortalRestarted
                        });
                    }
                }
            }
        })
    }
}

impl Drop for FedoraShortcutSession {
    fn drop(&mut self) {
        if self.retired {
            return;
        }
        // Backstop only: graceful retirement paths already awaited `close`.
        // Drop cannot await, so the bounded close is detached onto the runtime
        // when one is still available.
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let connection = self.connection.clone();
            let session_path = self.session_path.clone();
            handle.spawn(async move {
                close_portal_session(&connection, session_path.as_str()).await;
            });
        }
    }
}
