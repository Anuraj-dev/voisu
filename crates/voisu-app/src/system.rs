use std::collections::VecDeque;
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use voisu_core::{
    clipboard_candidates, install_instruction, resolve_session, scan_wav_pcm, socket_path,
    ActiveCapture, AudioCapture, AudioChunk, BoundaryError, BoundaryFuture,
    BoundaryKind, CancelRegistry, CapturedAudio, ClipboardTool, Command as DaemonCommand, Credential,
    DeliveryAdapter, DeliveryOutcome, KeyDiagnosis, KeyLocation, PackageManager, Provider,
    ProviderAuthenticator, ProviderKeyStatus, ProviderStream, ReadinessCapability, ReadinessFinding,
    MergeResult, ReadinessInspector, ReadinessStatus, ReconciliationKind, ReconciliationModel,
    Request, Response, SecretStore, SessionKind, SessionResolution, ShortcutPortal, ShortcutSession,
    SourceTranscript, Transcript, TranscriptDecision, TranscriptDecisionPipeline, TranscriptProvider,
    TranscriptValidator, TriggerKeyBinding, VersionEnvelope, WavScan, PACKAGE_MANAGERS,
    PROTOCOL_VERSION,
};

use crate::focus::SharedFocusProbe;
use crate::process::guard_external_child;
use crate::secret_file::{FileSecretStore, RemoveError};

const PROCESS_DEADLINE: Duration = Duration::from_secs(2);
pub const CAPTURE_FINALIZE_DEADLINE: Duration = PROCESS_DEADLINE;
pub const PROVIDER_COMPLETION_DEADLINE: Duration = Duration::from_secs(15);
pub const CLIPBOARD_DELIVERY_DEADLINE: Duration = PROCESS_DEADLINE;
pub const LIBEI_DELIVERY_DEADLINE: Duration = Duration::from_secs(5);
/// Grace granted to the bounded capture/provider aborts that run when a
/// Recording fails or a partial start is rolled back.
pub const RECOVERY_ABORT_DEADLINE: Duration = PROCESS_DEADLINE;
pub const RECONCILIATION_DEADLINE: Duration = Duration::from_secs(3);
pub const PROCESSING_RESPONSE_DEADLINE: Duration = Duration::from_secs(
    CAPTURE_FINALIZE_DEADLINE.as_secs()
        + PROVIDER_COMPLETION_DEADLINE.as_secs()
        + CLIPBOARD_DELIVERY_DEADLINE.as_secs()
        + LIBEI_DELIVERY_DEADLINE.as_secs()
        + RECOVERY_ABORT_DEADLINE.as_secs()
        + RECONCILIATION_DEADLINE.as_secs() * 2
        + 1,
);
const PROCESS_POLL: Duration = Duration::from_millis(10);
const MAX_DAEMON_RESPONSE_BYTES: usize = 16 * 1024;
const MAX_RETAINED_STDERR_BYTES: usize = 4 * 1024;
const MAX_RETAINED_STDOUT_BYTES: usize = 64 * 1024;
const PROVIDER_PROCESS_DEADLINE: Duration = Duration::from_secs(14);
const RECONCILIATION_PROCESS_DEADLINE: Duration = Duration::from_secs(2);
const PCM_CHUNK_BYTES: usize = 3_200;
const MIN_RECORDING_BYTES: usize = PCM_CHUNK_BYTES;
const MAX_RECORDING_BYTES: usize = 16_000 * 2 * 60 * 5;
/// Recordings at or below this length (120 s of 16 kHz s16le mono) take a
/// single full-audio Groq request at finalize: no pre-streamed chunks, no
/// seams, full context for Whisper. Only Recordings that grow past this switch
/// to pre-streamed chunking.
const GROQ_FULL_AUDIO_MAX_BYTES: usize = 16_000 * 2 * 120;
/// Pre-streamed chunk length for Recordings longer than the full-audio limit:
/// 60 s windows with a 4 s overlap so the word-overlap dedup can stitch seams.
const GROQ_CHUNK_BYTES: usize = 16_000 * 2 * 60;
const GROQ_CHUNK_OVERLAP_BYTES: usize = 16_000 * 2 * 4;
/// Word-overlap window for `merge_chunk_transcripts`, widened from the old 24
/// to cover the 4 s chunk overlap comfortably.
const GROQ_MERGE_OVERLAP_WORDS: usize = 48;
/// Bounded app-level redials for the Deepgram streaming websocket, covering
/// ONLY failed dials and connections that dropped before any audio was
/// delivered on them. Once audio has been accepted by a socket a drop is
/// unrecoverable (Deepgram has no server-side resume, and unfinalized audio
/// cannot be replayed), so it fails the provider visibly and the parallel
/// Groq stream carries the Recording.
const DEEPGRAM_RECONNECT_ATTEMPTS: usize = 2;
const DEEPGRAM_RECONNECT_BACKOFF: Duration = Duration::from_millis(250);
/// Whole-handshake bound (DNS + TCP + TLS + websocket upgrade) for one dial
/// of the streaming endpoint, so a black-holing network cannot pin the I/O
/// task past the Provider Deadline.
const DEEPGRAM_CONNECT_DEADLINE: Duration = Duration::from_secs(5);
/// Poll cadence at which the streaming I/O task observes `CancelRegistry`
/// (a poll-style flag, matching the subprocess poll-bound discipline).
const DEEPGRAM_CANCEL_POLL: Duration = Duration::from_millis(100);
/// Deepgram closes idle streaming sockets after ~10-12s without data; a JSON
/// `KeepAlive` text frame is sent whenever nothing else has gone out for this
/// long, well under that window.
const DEEPGRAM_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(5);
/// After `CloseStream` is sent, bounded wait for Deepgram to flush the final
/// `Results`, send the terminal summary `Metadata`, and close. A server that
/// never confirms within this grace fails the provider visibly — returning
/// the accumulated prefix would deliver a plausible but truncated Transcript.
const DEEPGRAM_CLOSE_GRACE: Duration = Duration::from_secs(10);

pub struct FedoraReadiness;

impl ReadinessInspector for FedoraReadiness {
    fn inspect(&mut self) -> Vec<ReadinessFinding> {
        if let Some(value) = std::env::var_os("VOISU_TEST_READINESS") {
            return controlled_readiness(&value.to_string_lossy());
        }
        let mut findings = vec![
            session_finding(),
            pipewire_finding(),
            microphone_finding(),
            portals_finding(),
            clipboard_finding(),
            secret_service_finding(),
            daemon_finding(),
        ];
        // Appended only when it can demonstrate a problem, so the common case
        // stays quiet and the golden table is unaffected.
        if let Some(finding) = service_display_env_finding() {
            findings.push(finding);
        }
        findings
    }
}

const PORTAL_BUS_NAME: &str = "org.freedesktop.portal.Desktop";
const PORTAL_OBJECT_PATH: &str = "/org/freedesktop/portal/desktop";
const GLOBAL_SHORTCUTS_INTERFACE: &str = "org.freedesktop.portal.GlobalShortcuts";
const PORTAL_REQUEST_INTERFACE: &str = "org.freedesktop.portal.Request";
const PORTAL_SESSION_INTERFACE: &str = "org.freedesktop.portal.Session";
/// The single shortcut id Voisu binds: its activation toggles the Recording.
pub const TRIGGER_KEY_ID: &str = "voisu-toggle";
const TRIGGER_KEY_DESCRIPTION: &str = "Toggle Voisu Recording";
/// Bound wait for the CreateSession portal round trip — no user interaction is
/// involved, so a portal that does not answer within this is treated as absent.
const PORTAL_SESSION_DEADLINE: Duration = Duration::from_secs(10);
/// Bound wait for the BindShortcuts response. Binding can require the user to
/// approve the Trigger Key in a desktop dialog, so this is generous; if the
/// user walks away the listener fails closed and CLI control stays usable.
const PORTAL_BIND_DEADLINE: Duration = Duration::from_secs(300);
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
struct ShortcutBindTokens {
    session: &'static str,
    create: String,
    bind: String,
}

fn shortcut_bind_tokens() -> ShortcutBindTokens {
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
fn escaped_sender(connection: &zbus::Connection) -> Result<String, BoundaryError> {
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
async fn portal_request<B>(
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
        .map_err(|error| BoundaryError::new(kind, format!("portal response rule invalid: {error}")))?
        .build();
    let mut responses = zbus::MessageStream::for_match_rule(rule, connection, Some(16))
        .await
        .map_err(|error| BoundaryError::new(kind, format!("portal response subscription failed: {error}")))?;

    let reply = portal
        .call_method(method, body)
        .await
        .map_err(|error| BoundaryError::new(kind, format!("portal {method} failed: {error}")))?;
    // Since xdg-desktop-portal 0.9 the returned handle equals the predictable
    // path; on an older portal it differs — either way the broad subscription
    // above already buffers its Response, so only the filter changes.
    let handle: zbus::zvariant::OwnedObjectPath = reply
        .body()
        .deserialize()
        .map_err(|error| BoundaryError::new(kind, format!("portal {method} returned no handle: {error}")))?;
    let deadline_at = tokio::time::Instant::now() + deadline;
    loop {
        let message = tokio::time::timeout_at(deadline_at, responses.next())
            .await
            .map_err(|_| BoundaryError::new(kind, format!("portal {method} response deadline elapsed")))?
            .ok_or_else(|| BoundaryError::new(kind, format!("portal {method} response stream ended")))?
            .map_err(|error| BoundaryError::new(kind, format!("portal {method} response failed: {error}")))?;
        let header = message.header();
        if header.path().map(|path| path.as_str()) != Some(handle.as_str()) {
            continue;
        }
        let (code, results): (u32, std::collections::HashMap<String, zbus::zvariant::OwnedValue>) =
            message.body().deserialize().map_err(|error| {
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
            let activations = portal
                .receive_signal("Activated")
                .await
                .map_err(|error| shortcut_error(format!("activation subscription failed: {error}")))?;
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
                std::collections::HashMap::from([("handle_token", Value::from(bind_token.as_str()))]);
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
fn session_handle_from(
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
async fn close_portal_session(connection: &zbus::Connection, session_path: &str) {
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

pub struct SecretToolStore;

/// Why the desktop Secret Service could not serve a request. It selects the
/// fallback warning wording — ticket 06 established that an unowned/activatable
/// name is a distinct failure from an owned-but-locked collection, and a missing
/// helper binary is distinct from both.
#[derive(Clone, Copy)]
enum FallbackReason {
    /// No Secret Service owns `org.freedesktop.secrets`, or it could not start.
    Unavailable,
    /// The service answered but is locked or refused access.
    Locked,
    /// The `secret-tool` helper binary is not installed.
    ToolMissing,
}

impl FallbackReason {
    fn detail(self) -> &'static str {
        match self {
            Self::Unavailable => {
                "no desktop Secret Service is available on this session (no keyring is running or activatable)"
            }
            Self::Locked => "the desktop keyring is locked or refused access",
            Self::ToolMissing => "the secret-tool helper is not installed",
        }
    }

    fn remedy(self) -> &'static str {
        match self {
            Self::Unavailable => {
                "start a Secret Service (KWallet or GNOME Keyring) then re-run `voisu setup` to migrate"
            }
            Self::Locked => "unlock your keyring (e.g. in KWallet) then re-run `voisu setup` to migrate",
            Self::ToolMissing => {
                "install secret-tool (libsecret-tools) then re-run `voisu setup` to migrate"
            }
        }
    }

    /// The reason-specific error surfaced when no credential can be produced —
    /// its public message steers the user at the real fix, not a generic hint.
    fn load_error(self) -> BoundaryError {
        BoundaryError::new(BoundaryKind::SecretStorage, "keyring load failed")
            .with_public_message(match self {
                Self::Unavailable => {
                    "no desktop Secret Service is available; run `voisu setup` to store a key"
                }
                Self::Locked => "the desktop keyring is locked; unlock it, or run `voisu setup`",
                Self::ToolMissing => "the secret-tool helper is not installed",
            })
    }
}

/// The situation that triggered a plaintext-fallback warning. Storing to the
/// file and reading from it are different acts with different wording, and
/// reading plaintext while the keyring is actually available is a migration
/// nudge, not a keyring failure.
enum FallbackNotice {
    Store(FallbackReason),
    Read(FallbackReason),
    ReadWhileKeyringAvailable,
}

/// The default desktop keyring retry budget: one immediate attempt then short
/// backoffs, ≈4.25s total, absorbing an edge-case slow activation without ever
/// blocking daemon startup (the load is lazy, at first use — ticket 06). Only an
/// `Unavailable` (activating) result is retried; a `Locked` collection is not,
/// since retries cannot unlock it.
const KEYRING_RETRY_BACKOFF: [Duration; 3] = [
    Duration::from_millis(250),
    Duration::from_secs(1),
    Duration::from_secs(3),
];

/// The retry backoff, overridable to a flat per-step delay (0 = instant) via
/// `VOISU_TEST_KEYRING_RETRY_MS` so a test can exercise the budget without real
/// waits.
fn keyring_retry_backoff() -> Vec<Duration> {
    if let Some(ms) = std::env::var("VOISU_TEST_KEYRING_RETRY_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
    {
        return vec![Duration::from_millis(ms); KEYRING_RETRY_BACKOFF.len()];
    }
    KEYRING_RETRY_BACKOFF.to_vec()
}

/// The per-Recording lookup retry budget: two short backoffs, ≈0.35s total. It is
/// deliberately far smaller than the store budget because the lookup runs on the
/// dictation hot path — a healthy lookup succeeds on the first attempt and pays
/// nothing, a transient Secret-Service denial recovers after the first backoff,
/// and a persistent denial surfaces the loud failure sub-second rather than
/// hanging the activation.
const LOOKUP_RETRY_BACKOFF: [Duration; 2] =
    [Duration::from_millis(100), Duration::from_millis(250)];

/// The lookup retry backoff, overridable to a flat per-step delay (0 = instant)
/// via the same `VOISU_TEST_KEYRING_RETRY_MS` seam the store path uses.
fn lookup_retry_backoff() -> Vec<Duration> {
    if let Some(ms) = std::env::var("VOISU_TEST_KEYRING_RETRY_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
    {
        return vec![Duration::from_millis(ms); LOOKUP_RETRY_BACKOFF.len()];
    }
    LOOKUP_RETRY_BACKOFF.to_vec()
}

/// How long a successfully-loaded credential is reused from the session cache
/// before the keyring is consulted again. Chosen to comfortably outlast any
/// transient Secret-Service hiccup (seconds) while bounding how long a
/// mid-session key rotation can be served stale to a few minutes — the daemon
/// re-reads the keyring once the entry expires. See docs/adr/
/// (2026-07-20) for why staleness is bounded by a TTL rather than by a
/// per-Recording 401/403 signal.
const CREDENTIAL_CACHE_TTL: Duration = Duration::from_secs(300);

/// The credential-cache TTL, overridable via `VOISU_TEST_CREDENTIAL_CACHE_TTL_MS`
/// (0 = never cache, so every load re-reads) so tests can exercise the cache and
/// its expiry without real waits.
fn credential_cache_ttl() -> Duration {
    if let Some(ms) = std::env::var("VOISU_TEST_CREDENTIAL_CACHE_TTL_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
    {
        return Duration::from_millis(ms);
    }
    CREDENTIAL_CACHE_TTL
}

/// One cached credential and the instant it was stored, for TTL expiry.
struct CachedCredential {
    credential: Credential,
    stored: Instant,
}

/// A session-scoped, in-process credential cache: at most one entry per provider,
/// each stamped with its load time so it expires after a bounded TTL. It lets a
/// credential that was successfully loaded earlier in the daemon's life survive a
/// later transient Secret-Service denial without re-shelling to `secret-tool` —
/// the reported failure mode (a warm daemon hitting one mid-session lookup
/// hiccup). The cache lives only in process memory: it is never written to disk
/// or logged, and `Credential` has no `Debug`, so a value cannot leak through it.
struct CredentialCache {
    /// One slot per provider, indexed by [`CredentialCache::slot`].
    slots: Mutex<[Option<CachedCredential>; 2]>,
}

impl CredentialCache {
    const fn new() -> Self {
        Self {
            slots: Mutex::new([None, None]),
        }
    }

    fn slot(provider: Provider) -> usize {
        match provider {
            Provider::Deepgram => 0,
            Provider::Groq => 1,
        }
    }

    /// Returns a fresh cached credential, or `None` when absent or expired. An
    /// expired entry is dropped in passing so a stale credential is never served.
    fn get(&self, provider: Provider, ttl: Duration) -> Option<Credential> {
        let mut slots = self.slots.lock().ok()?;
        let slot = &mut slots[Self::slot(provider)];
        match slot {
            Some(entry) if entry.stored.elapsed() < ttl => Some(entry.credential.clone()),
            Some(_) => {
                *slot = None;
                None
            }
            None => None,
        }
    }

    fn put(&self, provider: Provider, credential: Credential) {
        if let Ok(mut slots) = self.slots.lock() {
            slots[Self::slot(provider)] = Some(CachedCredential {
                credential,
                stored: Instant::now(),
            });
        }
    }

    /// Drops a provider's entry so the next load re-reads the keyring. Kept for
    /// on-demand eviction (e.g. a future provider auth-rejection hook); today the
    /// TTL is the sole staleness bound.
    #[allow(dead_code)]
    fn invalidate(&self, provider: Provider) {
        if let Ok(mut slots) = self.slots.lock() {
            slots[Self::slot(provider)] = None;
        }
    }
}

/// The daemon-process-wide credential cache. `SecretToolStore` is a unit struct
/// re-created at each call site, so the cache must be process-global to persist
/// across a session's Recordings.
static CREDENTIAL_CACHE: CredentialCache = CredentialCache::new();

fn credential_cache() -> &'static CredentialCache {
    &CREDENTIAL_CACHE
}

/// Serves a provider credential from the session cache when a fresh entry exists,
/// otherwise runs `load`, caches a successful result, and returns it. Only a
/// successful load is cached — a failed load never poisons the cache — so a
/// transient denial neither serves nor stores a bad value.
fn resolve_with_cache(
    provider: Provider,
    cache: &CredentialCache,
    ttl: Duration,
    load: impl FnOnce() -> Result<Credential, BoundaryError>,
) -> Result<Credential, BoundaryError> {
    if let Some(credential) = cache.get(provider, ttl) {
        return Ok(credential);
    }
    let credential = load()?;
    cache.put(provider, credential.clone());
    Ok(credential)
}

/// One attempt at the desktop Secret Service store.
enum StoreStep {
    Stored,
    Retry(FallbackReason),
    Stop(FallbackReason),
}

/// One attempt at the desktop Secret Service lookup. A transient D-Bus/ksecretd
/// denial (a nonzero exit WITH a stderr diagnostic) is `Retry`: it is
/// indistinguishable by output from a genuinely locked collection, so a small
/// bounded retry lets a momentary hiccup recover within the single load while a
/// persistent denial still exhausts the budget and surfaces the loud failure.
/// A clean no-match (nonzero exit, EMPTY stderr) is the OPPOSITE of the store
/// path: it is definitive absence, never retried, so the common unconfigured-key
/// and file-fallback reads stay on the fast path.
enum LoadStep {
    Found(Credential),
    /// The service is reachable but holds no such credential — definitive, so
    /// the caller consults the fallback file rather than retrying.
    Missing,
    /// A transient denial that a short bounded retry may clear; the reason is the
    /// terminal classification used once the budget is exhausted.
    Retry(FallbackReason),
    Stop(FallbackReason),
}

impl SecretStore for SecretToolStore {
    fn replace(&mut self, provider: Provider, credential: Credential) -> Result<(), BoundaryError> {
        match store_primary(provider, &credential) {
            Ok(()) => {
                // The keyring now holds the key, so migrate it out of the
                // plaintext fallback: drop that provider's line (deleting the
                // file when empty) so a later locked-at-boot window can never
                // silently serve a stale plaintext key. A failed prune must
                // not report a completed migration — so it is loud and an
                // error — with wording taken straight from `remove`'s own
                // classification (the single place the file is read, so this
                // relay can never disagree with it): "the copy survived" only
                // when the provider's line was verified on disk, "could not
                // verify" when its presence is unknowable.
                match FileSecretStore::at_default().remove(provider) {
                    Ok(_) => Ok(()),
                    Err(RemoveError::TargetPresent(_)) => {
                        warn_plaintext_prune_failed(PlaintextPruneFailure::CopySurvived);
                        Err(BoundaryError::new(
                            BoundaryKind::SecretStorage,
                            "plaintext prune failed after a successful keyring store",
                        )
                        .with_public_message(
                            "the key was stored in your keyring, but the old plaintext \
                             copy could not be removed and would still be used if the \
                             keyring is locked — delete the credentials file next to \
                             config.toml, then re-run `voisu doctor`",
                        ))
                    }
                    Err(RemoveError::Unverifiable(_)) => {
                        warn_plaintext_prune_failed(PlaintextPruneFailure::Unverifiable);
                        Err(BoundaryError::new(
                            BoundaryKind::SecretStorage,
                            "plaintext prune unverifiable after a successful keyring store",
                        )
                        .with_public_message(
                            "the key was stored in your keyring, but Voisu could not \
                             verify whether an old plaintext copy remains — check for a \
                             credentials file next to config.toml, then re-run \
                             `voisu doctor`",
                        ))
                    }
                }
            }
            Err(reason) => {
                warn_fallback(FallbackNotice::Store(reason));
                FileSecretStore::at_default().store(provider, &credential)
            }
        }
    }

    fn load(&mut self, provider: Provider) -> Result<Credential, BoundaryError> {
        // The env override wins over any stored key AND over the cache, preserving
        // the historic development/headless path; it is cheap to read so it is
        // never cached.
        if let Some(credential) = std::env::var_os(provider.environment_variable()) {
            return Credential::new(credential.to_string_lossy().into_owned());
        }
        // Serve a still-fresh credential from the session cache, so a later
        // transient Secret-Service denial never re-reaches secret-tool. Only a
        // successful load is cached; failures fall through and surface loudly.
        resolve_with_cache(provider, credential_cache(), credential_cache_ttl(), || {
            let fallback = FileSecretStore::at_default();
            match load_primary(provider) {
                LoadPrimary::Found(credential) => Ok(credential),
                // Keyring reachable but no key: a prior headless fallback write may
                // still hold it — reading that plaintext while the keyring is
                // available is a migration nudge, not a keyring failure.
                LoadPrimary::Missing => match fallback.read(provider)? {
                    Some(credential) => {
                        warn_fallback(FallbackNotice::ReadWhileKeyringAvailable);
                        Ok(credential)
                    }
                    None => Err(BoundaryError::new(
                        BoundaryKind::SecretStorage,
                        "no stored credential for provider",
                    )),
                },
                // Keyring unreachable: only warn about reading the file when we
                // actually read one; otherwise surface the keyring's real problem.
                LoadPrimary::Failed(reason) => match fallback.read(provider)? {
                    Some(credential) => {
                        warn_fallback(FallbackNotice::Read(reason));
                        Ok(credential)
                    }
                    None => Err(reason.load_error()),
                },
            }
        })
    }

    fn diagnose(&mut self, provider: Provider) -> KeyDiagnosis {
        // Mirror `load`: any PRESENT env variable is authoritative at runtime,
        // so a present-but-malformed value (empty, stray newline) must be
        // diagnosed as the broken override it is — never silently skipped in
        // favour of the keyring/file key it shadows.
        if let Some(value) = std::env::var_os(provider.environment_variable()) {
            return match Credential::new(value.to_string_lossy().into_owned()) {
                Ok(credential) => KeyDiagnosis::Found {
                    location: KeyLocation::EnvOverride,
                    credential,
                },
                Err(_) => KeyDiagnosis::EnvOverrideInvalid,
            };
        }
        let fallback = FileSecretStore::at_default();
        match load_primary(provider) {
            LoadPrimary::Found(credential) => KeyDiagnosis::Found {
                location: KeyLocation::Keyring,
                credential,
            },
            LoadPrimary::Missing => match fallback.read(provider) {
                Ok(Some(credential)) => KeyDiagnosis::Found {
                    location: KeyLocation::PlaintextFile,
                    credential,
                },
                _ => KeyDiagnosis::Absent,
            },
            LoadPrimary::Failed(reason) => match fallback.read(provider) {
                Ok(Some(credential)) => KeyDiagnosis::Found {
                    location: KeyLocation::PlaintextFile,
                    credential,
                },
                _ => match reason {
                    FallbackReason::Locked => KeyDiagnosis::Locked,
                    FallbackReason::ToolMissing => KeyDiagnosis::ToolMissing,
                    FallbackReason::Unavailable => KeyDiagnosis::Unavailable,
                },
            },
        }
    }
}

/// The outcome of the primary (desktop Secret Service) load after its retry
/// budget.
enum LoadPrimary {
    Found(Credential),
    Missing,
    Failed(FallbackReason),
}

/// The controlled seam value, if the test harness set one.
fn secret_seam_mode() -> Option<String> {
    std::env::var_os("VOISU_TEST_SECRET_STORE").map(|value| value.to_string_lossy().into_owned())
}

/// Stores to the primary with a bounded retry, honoring the test seam. `Ok`
/// means stored; `Err(reason)` means the caller should fall back to the file.
fn store_primary(provider: Provider, credential: &Credential) -> Result<(), FallbackReason> {
    if let Some(mode) = secret_seam_mode() {
        return match mode.as_str() {
            "available" => Ok(()),
            "denied" | "locked" => Err(FallbackReason::Locked),
            _ => Err(FallbackReason::Unavailable),
        };
    }
    let mut backoff = keyring_retry_backoff().into_iter();
    loop {
        match secret_tool_store(provider, credential) {
            StoreStep::Stored => return Ok(()),
            StoreStep::Stop(reason) => return Err(reason),
            StoreStep::Retry(reason) => match backoff.next() {
                Some(delay) => std::thread::sleep(delay),
                None => return Err(reason),
            },
        }
    }
}

/// Loads from the primary with a bounded retry, honoring the test seam.
fn load_primary(provider: Provider) -> LoadPrimary {
    if let Some(mode) = secret_seam_mode() {
        if mode == "available" {
            let name = match provider {
                Provider::Groq => "VOISU_TEST_STORED_GROQ_CREDENTIAL",
                Provider::Deepgram => "VOISU_TEST_STORED_DEEPGRAM_CREDENTIAL",
            };
            return match std::env::var(name).ok().and_then(|value| Credential::new(value).ok()) {
                Some(credential) => LoadPrimary::Found(credential),
                None => LoadPrimary::Missing,
            };
        }
        return match mode.as_str() {
            "denied" | "locked" => LoadPrimary::Failed(FallbackReason::Locked),
            _ => LoadPrimary::Failed(FallbackReason::Unavailable),
        };
    }
    let mut backoff = lookup_retry_backoff().into_iter();
    loop {
        match secret_tool_load(provider) {
            LoadStep::Found(credential) => return LoadPrimary::Found(credential),
            LoadStep::Missing => return LoadPrimary::Missing,
            LoadStep::Stop(reason) => return LoadPrimary::Failed(reason),
            // A transient denial: retry within the small budget, then fall back to
            // the terminal classification once it is exhausted.
            LoadStep::Retry(reason) => match backoff.next() {
                Some(delay) => std::thread::sleep(delay),
                None => return LoadPrimary::Failed(reason),
            },
        }
    }
}

/// One real `secret-tool store`. An empty stderr on failure reads as the service
/// still activating (retryable); a diagnostic on stderr, a timed-out prompt, or
/// invalid data read as a locked/denied collection (not retryable).
fn secret_tool_store(provider: Provider, credential: &Credential) -> StoreStep {
    match run_restricted(
        "secret-tool",
        &[
            "store",
            "--label=Voisu cloud credential",
            "voisu-provider",
            provider.secret_service_value(),
        ],
        Some(credential.expose_to_boundary().as_bytes()),
        false,
    ) {
        Ok(outcome) if outcome.success => StoreStep::Stored,
        // A nonzero exit with no diagnostic reads as the service still
        // activating — the one edge worth a bounded retry (ticket 06).
        Ok(outcome) if outcome.stderr.is_empty() => StoreStep::Retry(FallbackReason::Unavailable),
        Ok(_) => StoreStep::Stop(FallbackReason::Locked),
        Err(ProcessError::Unavailable) => StoreStep::Stop(FallbackReason::ToolMissing),
        Err(ProcessError::TimedOut) => StoreStep::Stop(FallbackReason::Locked),
        // A crashed or otherwise anomalous child is not retried — retrying only
        // reproduces the crash and would blow the bounded budget.
        Err(_) => StoreStep::Stop(FallbackReason::Unavailable),
    }
}

/// One real `secret-tool lookup`. A clean no-match (nonzero exit, empty stderr)
/// is `Missing`; a stderr diagnostic or a timed-out prompt is a locked/denied
/// collection; a spawn failure is the service being unavailable.
fn secret_tool_load(provider: Provider) -> LoadStep {
    match run_restricted(
        "secret-tool",
        &["lookup", "voisu-provider", provider.secret_service_value()],
        None,
        true,
    ) {
        Ok(outcome) if outcome.success => match String::from_utf8(outcome.stdout) {
            Ok(value) => match Credential::new(value.trim_end().to_owned()) {
                Ok(credential) => LoadStep::Found(credential),
                Err(_) => LoadStep::Missing,
            },
            Err(_) => LoadStep::Stop(FallbackReason::Locked),
        },
        Ok(outcome) if outcome.stderr.is_empty() => LoadStep::Missing,
        // A nonzero exit WITH a stderr diagnostic is the transient-denial shape:
        // a momentary D-Bus/ksecretd hiccup looks identical to a genuinely locked
        // collection here, so a short bounded retry lets the hiccup recover while
        // a real lock still exhausts the budget and stays loud.
        Ok(_) => LoadStep::Retry(FallbackReason::Locked),
        Err(ProcessError::Unavailable) => LoadStep::Stop(FallbackReason::ToolMissing),
        // A timeout already consumed the full process deadline; retrying would
        // multiply the hot-path latency, so it stays terminal.
        Err(ProcessError::TimedOut) => LoadStep::Stop(FallbackReason::Locked),
        // A crashed/anomalous child is not retried (see `secret_tool_store`).
        Err(_) => LoadStep::Stop(FallbackReason::Unavailable),
    }
}

/// Prints the loud, one-time-per-process fallback warning with wording that
/// matches what actually happened — storing to the file, reading from it because
/// the keyring is down, or reading plaintext while the keyring is up (a migration
/// nudge). Naming the file and the remedy is the whole point: gh's *silent*
/// keyring fallback is the named anti-pattern we refuse to repeat.
fn warn_fallback(notice: FallbackNotice) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static WARNED: AtomicBool = AtomicBool::new(false);
    if WARNED.swap(true, Ordering::Relaxed) {
        return;
    }
    let path = FileSecretStore::at_default().path().display().to_string();
    match notice {
        FallbackNotice::Store(reason) => eprintln!(
            "voisu: WARNING — {}. Storing the API key in a 0600 file at {} instead \
             (less secure than the keyring). To fix: {}.",
            reason.detail(),
            path,
            reason.remedy()
        ),
        FallbackNotice::Read(reason) => eprintln!(
            "voisu: WARNING — {}. Reading the API key from the 0600 file at {} \
             (less secure than the keyring). To fix: {}.",
            reason.detail(),
            path,
            reason.remedy()
        ),
        FallbackNotice::ReadWhileKeyringAvailable => eprintln!(
            "voisu: WARNING — reading the API key from the 0600 file at {} even though \
             your keyring is available. Run `voisu setup` to migrate it into the keyring.",
            path
        ),
    }
}

/// What is actually known when the post-store plaintext prune fails: the copy
/// is demonstrably still on disk, or its existence could not be checked at
/// all. The wording must never claim more than what was observed.
enum PlaintextPruneFailure {
    CopySurvived,
    Unverifiable,
}

/// The loud notice for a plaintext prune that failed after a successful
/// keyring store. It shares the fallback warnings' channel (stderr, naming
/// the file and the remedy) but not their once-per-process gate: this is a
/// distinct, rarer situation that must never be swallowed because an ordinary
/// fallback warning fired first.
fn warn_plaintext_prune_failed(failure: PlaintextPruneFailure) {
    let path = FileSecretStore::at_default().path().display().to_string();
    match failure {
        PlaintextPruneFailure::CopySurvived => eprintln!(
            "voisu: WARNING — the key is stored in your keyring, but the old plaintext copy at \
             {path} could not be removed. If the keyring is ever locked at start, that stale key \
             would be used. Delete {path}, then re-run `voisu doctor`."
        ),
        PlaintextPruneFailure::Unverifiable => eprintln!(
            "voisu: WARNING — the key is stored in your keyring, but Voisu could not verify \
             whether an old plaintext copy remains at {path}. If one does and the keyring is \
             ever locked at start, that stale key would be used. Check for and delete {path}, \
             then re-run `voisu doctor`."
        ),
    }
}

pub struct ProviderHttpClient;

/// A credentialed provider request with no response body retained. The next
/// provider adapter can supply its own endpoint while reusing this process and
/// environment boundary.
pub struct ProviderHttpRequest {
    pub url: &'static str,
    pub authorization_scheme: &'static str,
}

impl ProviderHttpClient {
    /// Runs the shared authenticated provider request boundary and returns its
    /// HTTP status together with whether the response carried a `Retry-After`
    /// header. Future Groq transcription can reuse this async boundary without
    /// inheriting credentials or curl configuration from the CLI.
    pub async fn authenticated_status(
        &self,
        credential: Credential,
        request: ProviderHttpRequest,
    ) -> Result<AuthProbe, BoundaryError> {
        tokio::task::spawn_blocking(move || authenticated_status(credential, request))
            .await
            .map_err(|_| BoundaryError::new(BoundaryKind::ProviderAuthentication, "provider request task failed"))?
    }

    /// The endpoint used for the cheapest authenticated round trip per provider.
    fn probe_request(provider: Provider) -> ProviderHttpRequest {
        match provider {
            Provider::Groq => ProviderHttpRequest {
                url: "https://api.groq.com/openai/v1/models",
                authorization_scheme: "Bearer",
            },
            Provider::Deepgram => ProviderHttpRequest {
                url: "https://api.deepgram.com/v1/projects",
                authorization_scheme: "Token",
            },
        }
    }

    /// Performs a live credential round trip and classifies the outcome. A
    /// transport failure (curl missing, timeout, connection refused) is a
    /// transient `Unreachable`, never a wrong-key verdict. Tests bypass the
    /// network via `VOISU_TEST_AUTH_{GROQ,DEEPGRAM}` (see [`controlled_key_status`]).
    pub async fn check(&self, provider: Provider, credential: Credential) -> ProviderKeyStatus {
        let controlled = match provider {
            Provider::Groq => std::env::var_os("VOISU_TEST_AUTH_GROQ"),
            Provider::Deepgram => std::env::var_os("VOISU_TEST_AUTH_DEEPGRAM"),
        };
        if let Some(mode) = controlled {
            return controlled_key_status(&mode.to_string_lossy());
        }
        match self
            .authenticated_status(credential, Self::probe_request(provider))
            .await
        {
            Ok(probe) => ProviderKeyStatus::classify(probe.status, probe.retry_after),
            Err(_) => ProviderKeyStatus::Unreachable,
        }
    }

    /// Verifies a credential, mapping a non-valid classification onto a
    /// `BoundaryError` whose public message is the same actionable headline
    /// every other surface shows.
    pub async fn verify(&self, provider: Provider, credential: Credential) -> Result<(), BoundaryError> {
        match self.check(provider, credential).await {
            ProviderKeyStatus::Valid => Ok(()),
            status => Err(BoundaryError::new(
                BoundaryKind::ProviderAuthentication,
                "provider credential round trip did not authenticate",
            )
            .with_public_message(status.headline())),
        }
    }
}

impl ProviderAuthenticator for ProviderHttpClient {
    fn verify(&mut self, provider: Provider, credential: Credential) -> BoundaryFuture<'_, ()> {
        Box::pin(async move { ProviderHttpClient.verify(provider, credential).await })
    }
}

/// The HTTP status of an authenticated probe plus whether a `Retry-After`
/// header accompanied it, which distinguishes a transient rate limit from a
/// spent quota on a bare 429.
pub struct AuthProbe {
    pub status: u16,
    pub retry_after: bool,
}

/// Maps a `VOISU_TEST_AUTH_*` seam value onto a classification so tests exercise
/// every branch without touching the network. `authorized` stays the historic
/// success token; `denied` stays the historic rejection (a wrong key).
fn controlled_key_status(mode: &str) -> ProviderKeyStatus {
    match mode {
        "authorized" | "valid" | "200" => ProviderKeyStatus::Valid,
        "denied" | "invalid" | "401" | "403" => ProviderKeyStatus::InvalidKey,
        "ratelimited" | "429-retry" => ProviderKeyStatus::RateLimited,
        "quota" | "429" => ProviderKeyStatus::QuotaExhausted,
        "unreachable" | "500" | "502" | "503" => ProviderKeyStatus::Unreachable,
        other => match other.parse::<u16>() {
            Ok(status) => ProviderKeyStatus::classify(status, false),
            Err(_) => ProviderKeyStatus::Unreachable,
        },
    }
}

fn authenticated_status(
    credential: Credential,
    request: ProviderHttpRequest,
) -> Result<AuthProbe, BoundaryError> {
    let credential = curl_config_escape(credential.expose_to_boundary());
    let config = format!(
        "url = \"{}\"\nheader = \"Authorization: {} {credential}\"\n",
        request.url, request.authorization_scheme,
    );
    // `--fail` is deliberately omitted: it makes curl exit non-zero on a 4xx/5xx
    // and swallows the status, collapsing 401/403/429/5xx into one opaque error.
    // Without it curl completes with exit 0 and writes the code, so the caller
    // can classify. A non-zero exit now means a genuine transport failure.
    let outcome = run_restricted(
        "curl",
        &[
            "-q",
            "--config",
            "-",
            "--silent",
            "--show-error",
            "--output",
            "/dev/null",
            "--write-out",
            "%{http_code}\t%header{retry-after}",
            "--max-time",
            "2",
        ],
        Some(config.as_bytes()),
        true,
    )
    .map_err(provider_authentication_error)?;
    if !outcome.success {
        return Err(BoundaryError::new(
            BoundaryKind::ProviderAuthentication,
            "provider request did not complete",
        ));
    }
    let rendered = std::str::from_utf8(&outcome.stdout)
        .map_err(|_| {
            BoundaryError::new(BoundaryKind::ProviderAuthentication, "provider returned no HTTP status")
        })?;
    parse_auth_probe(rendered).ok_or_else(|| {
        BoundaryError::new(BoundaryKind::ProviderAuthentication, "provider returned no HTTP status")
    })
}

/// Parses curl's `%{http_code}\t%header{retry-after}` write-out. The status is
/// the first tab-separated field; `Retry-After` is present when the second
/// field is non-empty and is a real value (older curl that lacks `%header{}`
/// writes the literal token, which is treated as absent).
fn parse_auth_probe(rendered: &str) -> Option<AuthProbe> {
    let line = rendered.trim_end_matches(['\r', '\n']);
    let mut fields = line.splitn(2, '\t');
    let status = fields.next()?.trim().parse::<u16>().ok()?;
    let retry_after = fields
        .next()
        .map(str::trim)
        .is_some_and(|value| !value.is_empty() && !value.starts_with('%'));
    Some(AuthProbe { status, retry_after })
}

fn curl_config_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn provider_authentication_error(error: ProcessError) -> BoundaryError {
    let detail = match error {
        ProcessError::Unavailable => "curl unavailable",
        ProcessError::Input => "curl rejected credential input",
        ProcessError::TimedOut => "curl deadline elapsed",
        ProcessError::Wait | ProcessError::Output => "curl execution failed",
    };
    BoundaryError::new(BoundaryKind::ProviderAuthentication, detail)
}

/// Resolve the current display session from the live environment. Detection is
/// pure logic in `voisu-core`; this only reads the environment for it.
fn current_session() -> SessionResolution {
    let wayland_display = std::env::var("WAYLAND_DISPLAY").ok();
    let x11_display = std::env::var("DISPLAY").ok();
    let session_type = std::env::var("XDG_SESSION_TYPE").ok();
    resolve_session(
        wayland_display.as_deref(),
        x11_display.as_deref(),
        session_type.as_deref(),
    )
}

/// The first executable of `program` found on `PATH`, honoring `PATH` order so
/// the resolved path is the one a spawned helper would actually run.
fn first_on_path(program: &str) -> Option<PathBuf> {
    resolve_on_path(&std::env::var_os("PATH")?, program)
}

/// Pure `PATH` resolution over an injected `PATH` value, so it is testable
/// without mutating the process environment.
fn resolve_on_path(path: &std::ffi::OsStr, program: &str) -> Option<PathBuf> {
    std::env::split_paths(path)
        .map(|dir| dir.join(program))
        .find(|candidate| {
            fs::metadata(candidate)
                .map(|meta| meta.is_file() && meta.mode() & 0o111 != 0)
                .unwrap_or(false)
        })
}

/// The host package manager, identified by which of the known binaries appears
/// on `PATH`. Detected only to print the correct install command — never run.
fn detect_package_manager() -> Option<PackageManager> {
    PACKAGE_MANAGERS
        .into_iter()
        .find(|manager| first_on_path(manager.probe_binary()).is_some())
}

/// A desktop label for the Session value column, e.g. `X11 (Cinnamon)`.
fn session_value(resolution: SessionResolution) -> String {
    let session = match resolution.session {
        SessionKind::Wayland => "Wayland",
        SessionKind::X11 if resolution.xwayland_fallback => "X11 (XWayland)",
        SessionKind::X11 => "X11",
        SessionKind::Unknown => "unknown",
    };
    match std::env::var("XDG_CURRENT_DESKTOP")
        .ok()
        .filter(|value| !value.is_empty())
    {
        Some(desktop) => format!("{session} ({desktop})"),
        None => session.to_owned(),
    }
}

/// Whether the systemd `--user` manager environment advertises `key` with a
/// non-empty value.
fn manager_env_has(show_environment: &str, key: &str) -> bool {
    let prefix = format!("{key}=");
    show_environment
        .lines()
        .filter_map(|line| line.strip_prefix(&prefix))
        .any(|value| !value.is_empty())
}

/// A doctor diagnosis for the systemd-user-service delivery gap: the daemon runs
/// under the `--user` manager, and if that manager never imported the graphical
/// session's display variables, Delivery cannot reach the X/Wayland server even
/// though the interactive CLI can. Returns a WARN only when the manager clearly
/// lacks this session's display variable; on an undetermined session, an
/// unreachable manager, or a manager that already has it, no row is produced.
fn service_display_env_finding() -> Option<ReadinessFinding> {
    // The variables the daemon's clipboard helper needs for THIS session. The
    // display endpoint depends on the session; XAUTHORITY is additionally
    // required whenever this CLI has a non-default one, since without it an X11
    // helper (xclip, or an XWayland fallback) cannot authenticate to the server.
    let mut needed: Vec<&str> = match current_session().session {
        SessionKind::Wayland => vec!["WAYLAND_DISPLAY"],
        SessionKind::X11 => vec!["DISPLAY"],
        SessionKind::Unknown => return None,
    };
    if std::env::var("XAUTHORITY").is_ok_and(|value| !value.is_empty()) {
        needed.push("XAUTHORITY");
    }
    let outcome = run_restricted("systemctl", &["--user", "show-environment"], None, true).ok()?;
    if !outcome.success {
        return None;
    }
    let show_environment = String::from_utf8_lossy(&outcome.stdout);
    let missing: Vec<&str> = needed
        .into_iter()
        .filter(|key| !manager_env_has(&show_environment, key))
        .collect();
    if missing.is_empty() {
        return None;
    }
    let names = missing.join(", ");
    Some(
        readiness(
            ReadinessCapability::ServiceEnvironment,
            ReadinessStatus::Warn,
            &format!(
                "the systemd --user manager is missing {names}, so Delivery from the daemon \
                 cannot reach or authenticate to the display; run `voisu service restart` from \
                 your graphical session (or `systemctl --user import-environment {}`)",
                missing.join(" ")
            ),
        )
        .with_value("missing display env"),
    )
}

/// The Session check: which display server this login is running. Both Wayland
/// and X11 are fully supported, so a cleanly detected session passes; only a
/// session that cannot be determined warns.
fn session_finding() -> ReadinessFinding {
    let resolution = current_session();
    let value = session_value(resolution);
    match resolution.session {
        SessionKind::Wayland | SessionKind::X11 => ReadinessFinding::new(
            ReadinessCapability::Session,
            ReadinessStatus::Pass,
            "display session detected; the matching clipboard backend is selected at runtime",
        )
        .with_value(value),
        SessionKind::Unknown => readiness(
            ReadinessCapability::Session,
            ReadinessStatus::Warn,
            "could not determine the display session; clipboard Delivery will try Wayland then X11. Log in to a graphical session (Wayland or X11)",
        )
        .with_value(value),
    }
}

/// Parse the PipeWire version `pw-record --help` reports in its
/// `Compiled with libpipewire X.Y.Z` banner. Best-effort: absent on some builds.
fn pw_record_version() -> Option<String> {
    let outcome = run_restricted("pw-record", &["--help"], None, true).ok()?;
    let text = String::from_utf8_lossy(&outcome.stdout).into_owned()
        + &String::from_utf8_lossy(&outcome.stderr);
    let marker = "libpipewire ";
    let start = text.find(marker)? + marker.len();
    let version: String = text[start..]
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    (!version.is_empty()).then_some(version)
}

/// The install command for `pw-record` (the recorder), whose package name
/// differs per distribution. Printed, never run.
fn pw_record_install_command() -> String {
    match detect_package_manager() {
        Some(PackageManager::Apt) => "sudo apt install pipewire-bin".to_owned(),
        Some(PackageManager::Dnf) => "sudo dnf install pipewire-utils".to_owned(),
        Some(PackageManager::Pacman) => "sudo pacman -S pipewire".to_owned(),
        Some(PackageManager::Zypper) => "sudo zypper install pipewire-tools".to_owned(),
        None => "install pipewire (pw-record) with your package manager".to_owned(),
    }
}

/// The PipeWire check. `pw-record` absent is a hard FAIL naming the package —
/// Voisu cannot capture without it, and a responding pw-cli must not mask that.
/// Otherwise the status comes from whether the PipeWire core answers, and the
/// value column carries the detected version and the capture path (`--raw`
/// headerless PCM, or the WAV-container fallback for PipeWire < 1.1).
fn pipewire_finding() -> ReadinessFinding {
    let mode = pw_record_capture_mode();
    let version = pw_record_version();
    if mode == PwRecordProbe::Unavailable {
        return ReadinessFinding::new(
            ReadinessCapability::PipeWire,
            ReadinessStatus::Fail,
            "pw-record is not available, so Voisu cannot capture audio",
        )
        .with_value("pw-record missing")
        .with_action(pw_record_install_command());
    }
    let responds = run_restricted("pw-cli", &["info", "0"], None, false)
        .is_ok_and(|outcome| outcome.success);
    let path = if mode == PwRecordProbe::Raw { "raw" } else { "WAV fallback" };
    let value = match version {
        Some(version) => format!("{version} ({path})"),
        None => format!("({path})"),
    };
    let detail = if mode == PwRecordProbe::Raw {
        "PipeWire core responds; pw-record --raw yields headerless PCM"
    } else {
        "PipeWire core responds; pw-record lacks --raw, so the WAV container is unwrapped to PCM"
    };
    if responds {
        ReadinessFinding::new(ReadinessCapability::PipeWire, ReadinessStatus::Pass, detail)
            .with_value(value)
    } else {
        ReadinessFinding::new(
            ReadinessCapability::PipeWire,
            ReadinessStatus::Fail,
            "PipeWire core does not respond",
        )
        .with_value(value)
        .with_action("start PipeWire and WirePlumber")
    }
}

fn controlled_readiness(value: &str) -> Vec<ReadinessFinding> {
    // Host-independent findings so the doctor-output golden test is stable
    // everywhere: no real probes, no package-manager detection.
    let mut findings = vec![
        readiness(ReadinessCapability::Session, ReadinessStatus::Pass, "display session detected")
            .with_value("Wayland (KDE)"),
        readiness(ReadinessCapability::PipeWire, ReadinessStatus::Pass, "PipeWire core responds")
            .with_value("1.4.11 (raw)"),
        readiness(ReadinessCapability::Microphone, ReadinessStatus::Pass, "default source available"),
        readiness(ReadinessCapability::Portals, ReadinessStatus::Pass, "desktop portal responds"),
        readiness(ReadinessCapability::Clipboard, ReadinessStatus::Pass, "clipboard roundtrip succeeds"),
        readiness(ReadinessCapability::SecretStorage, ReadinessStatus::Pass, "Secret Service responds"),
        daemon_finding(),
    ];
    if value == "pass" {
        return findings;
    }
    for override_value in value.split(',') {
        let Some((capability, status)) = override_value.split_once('=') else { continue };
        let (status, detail, action) = match status {
            "warn" => (ReadinessStatus::Warn, "needs attention; see remediation", None),
            "fail" => (
                ReadinessStatus::Fail,
                "not available; see remediation",
                Some("run the printed remediation command"),
            ),
            _ => continue,
        };
        if let Some(finding) = findings.iter_mut().find(|finding| {
            matches!(
                (capability, finding.capability),
                ("session", ReadinessCapability::Session)
                    | ("pipewire", ReadinessCapability::PipeWire)
                    | ("microphone", ReadinessCapability::Microphone)
                    | ("portals", ReadinessCapability::Portals)
                    | ("clipboard", ReadinessCapability::Clipboard)
                    | ("secret-storage", ReadinessCapability::SecretStorage)
                    | ("daemon", ReadinessCapability::Daemon)
            )
        }) {
            finding.status = status;
            finding.detail = detail.to_owned();
            finding.action = action.map(str::to_owned);
        }
    }
    // The Service-env row is appended by the real inspector only when a problem
    // is detected, so it is not in the base list. A `service-env=warn` override
    // synthesizes it here to exercise its formatting and diagnosis hermetically.
    for override_value in value.split(',') {
        if let Some(("service-env", "warn")) = override_value.split_once('=') {
            findings.push(
                readiness(
                    ReadinessCapability::ServiceEnvironment,
                    ReadinessStatus::Warn,
                    "the systemd --user manager is missing WAYLAND_DISPLAY, XAUTHORITY, so Delivery from the daemon cannot reach or authenticate to the display; run `voisu service restart`",
                )
                .with_value("missing display env"),
            );
        }
    }
    findings
}

fn microphone_finding() -> ReadinessFinding {
    match run_restricted("wpctl", &["inspect", "@DEFAULT_AUDIO_SOURCE@"], None, true) {
        Ok(outcome) if outcome.success => readiness(
            ReadinessCapability::Microphone,
            ReadinessStatus::Pass,
            "default source available",
        ),
        // WARN carries no action line (that is reserved for FAIL); the
        // remediation lives in the reasoning, shown under --verbose.
        Ok(_) => readiness(
            ReadinessCapability::Microphone,
            ReadinessStatus::Warn,
            "no default microphone is set; connect one and set it as the default source",
        ),
        Err(_) => ReadinessFinding::new(
            ReadinessCapability::Microphone,
            ReadinessStatus::Fail,
            "WirePlumber is unavailable",
        )
        .with_action("start PipeWire and WirePlumber"),
    }
}

/// Hand-written clipboard wrappers (from a workaround guide) that precede the
/// packaged tools on `PATH`. On a Wayland login they silently reroute the
/// Wayland clipboard through the wrong backend and break it. Detected as a
/// `wl-copy`/`wl-paste` that resolves under `$HOME` rather than a system bin;
/// each is reported by its exact path so remediation removes only what shadows.
fn shadowing_clipboard_wrappers() -> Vec<PathBuf> {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return Vec::new();
    };
    ["wl-copy", "wl-paste"]
        .into_iter()
        .filter_map(first_on_path)
        .filter(|winner| winner.starts_with(&home))
        .collect()
}

/// POSIX single-quote a path for a copy-pasteable shell command, quoting only
/// when the path contains characters a shell would interpret.
fn shell_quote(path: &Path) -> String {
    let text = path.to_string_lossy();
    let safe = !text.is_empty()
        && text
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "._-/".contains(character));
    if safe {
        text.into_owned()
    } else {
        format!("'{}'", text.replace('\'', r"'\''"))
    }
}

/// The outcome of round-tripping the clipboard through one backend tool.
enum ClipboardProbe {
    /// The round trip worked and the prior clipboard was restored (or there was
    /// nothing to restore).
    WorkedRestored,
    /// The round trip worked but the prior clipboard could not be restored.
    WorkedNotRestored,
    /// The tool binary is not installed (its write could not even be spawned).
    ToolMissing,
    /// The tool ran but the round trip failed — no reachable display or the
    /// selection never took.
    Failed,
}

/// Round-trip the clipboard through one tool. The probe is WRITTEN first, which
/// also establishes selection ownership, so a read-back succeeds even on a
/// previously empty or ownerless clipboard (`xclip -out` errors with no owner).
/// The prior value is preserved only when the initial read genuinely returned
/// one; a failed initial read is treated as "empty", never as "unavailable".
fn probe_clipboard_roundtrip(tool: ClipboardTool) -> ClipboardProbe {
    let (read_program, read_arguments) = tool.read_command();
    let (write_program, write_arguments) = tool.write_command();

    let original = match run_restricted(read_program, read_arguments, None, true) {
        Ok(outcome) if outcome.success => Some(outcome.stdout),
        _ => None,
    };

    let probe = format!("voisu-readiness-{}", std::process::id());
    match run_restricted_serving(write_program, write_arguments, Some(probe.as_bytes())) {
        Ok(outcome) if outcome.success => {}
        // A spawn failure is the only definitive "tool is not installed" signal.
        Err(ProcessError::Unavailable) => return ClipboardProbe::ToolMissing,
        _ => return ClipboardProbe::Failed,
    }

    let observed = run_restricted(read_program, read_arguments, None, true)
        .ok()
        .filter(|outcome| outcome.success)
        .map(|outcome| outcome.stdout == probe.as_bytes())
        .unwrap_or(false);
    if !observed {
        return ClipboardProbe::Failed;
    }

    // Restore the prior value only if there genuinely was one; writing an empty
    // string back would install an empty clipboard owner where none existed.
    match original {
        Some(original) => {
            let restored = run_restricted_serving(write_program, write_arguments, Some(&original))
                .is_ok_and(|outcome| outcome.success);
            if restored {
                ClipboardProbe::WorkedRestored
            } else {
                ClipboardProbe::WorkedNotRestored
            }
        }
        None => ClipboardProbe::WorkedRestored,
    }
}

/// The Clipboard check. It round-trips through the backend that matches the
/// detected session (`wl-copy`/`wl-paste` on Wayland, `xclip` on X11; an Unknown
/// session tries each in turn), and on failure prescribes the exact install
/// command for the host package manager.
fn clipboard_finding() -> ReadinessFinding {
    let resolution = current_session();
    // A shadowing wrapper is a Wayland-only hazard (harmless on X11). Per the
    // terseness contract the remediation lives in the reasoning (--verbose),
    // naming only the exact shadowing paths, shell-quoted.
    if resolution.session == SessionKind::Wayland {
        let shadows = shadowing_clipboard_wrappers();
        if !shadows.is_empty() {
            let names = shadows
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(" and ");
            let removal = shadows
                .iter()
                .map(|path| shell_quote(path))
                .collect::<Vec<_>>()
                .join(" ");
            return ReadinessFinding::new(
                ReadinessCapability::Clipboard,
                ReadinessStatus::Warn,
                format!(
                    "{names} shadow the packaged wl-clipboard on PATH and reroute the Wayland \
                     clipboard through the wrong backend; remove with: rm {removal}"
                ),
            )
            .with_value("shadowed wrapper");
        }
    }

    let candidates = clipboard_candidates(resolution.session);
    let mut a_tool_was_present = false;
    for tool in candidates {
        match probe_clipboard_roundtrip(*tool) {
            ClipboardProbe::WorkedRestored => {
                return readiness(
                    ReadinessCapability::Clipboard,
                    ReadinessStatus::Pass,
                    "clipboard roundtrip succeeds and the prior clipboard was restored",
                );
            }
            ClipboardProbe::WorkedNotRestored => {
                return readiness(
                    ReadinessCapability::Clipboard,
                    ReadinessStatus::Warn,
                    "clipboard roundtrip succeeds but the prior clipboard could not be restored",
                );
            }
            // Present-but-broken and missing both continue to the next
            // candidate; only the final message distinguishes them.
            ClipboardProbe::Failed => {
                a_tool_was_present = true;
            }
            ClipboardProbe::ToolMissing => {}
        }
    }

    let primary = candidates
        .first()
        .copied()
        .unwrap_or(ClipboardTool::WlClipboard);
    let detail = if a_tool_was_present {
        "the clipboard tool ran but the roundtrip failed — no reachable display or selection owner"
    } else {
        "no clipboard backend is installed for this session"
    };
    ReadinessFinding::new(ReadinessCapability::Clipboard, ReadinessStatus::Fail, detail)
        .with_action(install_instruction(
            detect_package_manager(),
            primary.install_package(),
        ))
}

fn secret_service_finding() -> ReadinessFinding {
    // Probe a nonexistent attribute. On a healthy, unlocked keyring this exits
    // without a match and without diagnostics: reaching the service cleanly is
    // the readiness signal, not whether a credential was found. Real secret-tool
    // reports a no-match with a nonzero exit and empty stdout/stderr, while a
    // D-Bus/service failure or a locked keyring prints an error to stderr.
    let probe = std::process::id().to_string();
    match run_restricted("secret-tool", &["lookup", "voisu-doctor-probe", &probe], None, false) {
        Ok(outcome) if outcome.success || outcome.stderr.is_empty() => readiness(
            ReadinessCapability::SecretStorage,
            ReadinessStatus::Pass,
            "Secret Service is reachable",
        ),
        Ok(_) => readiness(
            ReadinessCapability::SecretStorage,
            ReadinessStatus::Warn,
            "Secret Service reported an error; unlock the keyring or log in to the desktop session",
        ),
        Err(_) => ReadinessFinding::new(
            ReadinessCapability::SecretStorage,
            ReadinessStatus::Fail,
            "Secret Service is unavailable",
        )
        .with_action("start or unlock the desktop keyring"),
    }
}

/// The Portals readiness check. A portal that does not answer at all fails
/// closed; a portal that answers but exposes no `GlobalShortcuts` interface
/// warns with the Hyprland remediation, because plain wlroots portals implement
/// no GlobalShortcuts and there is no desktop dialog to bind the Trigger Key —
/// the daemon can never receive an activation there until the user installs
/// xdg-desktop-portal-hyprland and declares the bind in hyprland.conf. Only a
/// portal that answers AND exposes GlobalShortcuts passes.
fn portals_finding() -> ReadinessFinding {
    let portal_up = run_restricted(
        "busctl",
        &["--user", "--no-pager", "status", PORTAL_BUS_NAME],
        None,
        false,
    )
    .is_ok_and(|outcome| outcome.success);
    if !portal_up {
        return ReadinessFinding::new(
            ReadinessCapability::Portals,
            ReadinessStatus::Fail,
            "the desktop portal does not respond, so the Trigger Key cannot bind",
        )
        .with_action("start xdg-desktop-portal in this desktop session");
    }
    if global_shortcuts_available() {
        readiness(
            ReadinessCapability::Portals,
            ReadinessStatus::Pass,
            "desktop portal responds",
        )
    } else {
        // Detection is kept; only the presentation is made terse. The full
        // reasoning moves to --verbose. On a desktop without portal
        // GlobalShortcuts (Cinnamon/X11, plain wlroots) the Trigger Key is bound
        // through a desktop Custom Shortcut running `voisu toggle`.
        // WARN carries no action line; the remediation (install the Hyprland
        // portal, or bind a desktop Custom Shortcut to `voisu toggle`) is in the
        // reasoning, shown under --verbose.
        readiness(
            ReadinessCapability::Portals,
            ReadinessStatus::Warn,
            "the desktop portal exposes no GlobalShortcuts interface, so Voisu cannot bind the Trigger Key itself; on Hyprland install xdg-desktop-portal-hyprland, and on Cinnamon/X11 bind a desktop Custom Shortcut to run: voisu toggle",
        )
    }
}

/// Whether `org.freedesktop.portal.GlobalShortcuts` is exposed on the desktop
/// portal. Reads its `version` property: the portal answers with the interface
/// version when it is implemented and fails when it is absent. This mirrors how
/// the codebase already talks to the portal over the session bus (via busctl),
/// staying in one subprocess convention rather than opening a second zbus edge
/// just for a probe.
fn global_shortcuts_available() -> bool {
    run_restricted(
        "busctl",
        &[
            "--user",
            "get-property",
            PORTAL_BUS_NAME,
            PORTAL_OBJECT_PATH,
            GLOBAL_SHORTCUTS_INTERFACE,
            "version",
        ],
        None,
        false,
    )
    .is_ok_and(|outcome| outcome.success)
}

fn daemon_finding() -> ReadinessFinding {
    if daemon_status_handshake().is_ok() {
        return readiness(
            ReadinessCapability::Daemon,
            ReadinessStatus::Pass,
            "status handshake succeeds",
        );
    }
    // A daemon that was simply never started reads differently from a unit
    // systemd tried to run and could not: when the unit is in the failed state
    // (e.g. a namespace/exec failure that never reaches our handshake), point
    // the user at the journal rather than telling them to "start" a unit that
    // is already failing to start.
    if service_reports_failed() {
        ReadinessFinding::new(
            ReadinessCapability::Daemon,
            ReadinessStatus::Fail,
            "the daemon did not answer the status handshake and systemctl --user reports voisu.service failed",
        )
        .with_action("journalctl --user -u voisu.service")
    } else {
        ReadinessFinding::new(
            ReadinessCapability::Daemon,
            ReadinessStatus::Fail,
            "the daemon did not answer the status handshake",
        )
        .with_action("start voisu-daemon and run voisu doctor again")
    }
}

/// Whether systemd reports `voisu.service` in the failed state. A dedicated test
/// seam (`VOISU_TEST_SERVICE_FAILED`) keeps the doctor daemon check hermetic —
/// tests never depend on the host's real unit state.
fn service_reports_failed() -> bool {
    if let Some(value) = std::env::var_os("VOISU_TEST_SERVICE_FAILED") {
        return matches!(value.to_string_lossy().trim(), "1" | "failed");
    }
    crate::service::service_is_failed()
}

fn daemon_status_handshake() -> Result<(), ()> {
    let path = socket_path().map_err(|_| ())?;
    let mut stream = UnixStream::connect(path).map_err(|_| ())?;
    // A single Instant budget bounds the whole handshake. A per-read timeout is
    // reset by every byte, so a peer trickling one byte per interval would hold
    // doctor forever; the accumulated response is also capped during reading so
    // an oversized frame can never be fully buffered before the cap is checked.
    let started = Instant::now();
    stream.set_write_timeout(Some(PROCESS_DEADLINE)).map_err(|_| ())?;
    serde_json::to_writer(&mut stream, &Request { version: PROTOCOL_VERSION, command: DaemonCommand::Status })
        .map_err(|_| ())?;
    stream.write_all(b"\n").map_err(|_| ())?;
    let response = read_bounded_frame(&mut stream, started)?;
    let envelope: VersionEnvelope = serde_json::from_str(&response).map_err(|_| ())?;
    let response: Response = serde_json::from_str(&response).map_err(|_| ())?;
    (envelope.version == PROTOCOL_VERSION && response.ok && response.state.is_some())
        .then_some(())
        .ok_or(())
}

fn read_bounded_frame(stream: &mut UnixStream, started: Instant) -> Result<String, ()> {
    let mut response = Vec::new();
    let mut buffer = [0_u8; 1024];
    loop {
        let remaining = PROCESS_DEADLINE
            .checked_sub(started.elapsed())
            .filter(|remaining| !remaining.is_zero())
            .ok_or(())?;
        stream.set_read_timeout(Some(remaining)).map_err(|_| ())?;
        match stream.read(&mut buffer) {
            Ok(0) => return Err(()),
            Ok(read) => {
                // Reject before appending: a flooding peer must never force an
                // allocation beyond the response cap.
                if response.len() + read > MAX_DAEMON_RESPONSE_BYTES {
                    return Err(());
                }
                response.extend_from_slice(&buffer[..read]);
                if response.ends_with(b"\n") {
                    return String::from_utf8(response).map_err(|_| ());
                }
                if response.contains(&b'\n') {
                    return Err(());
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                return Err(());
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return Err(()),
        }
    }
}

fn readiness(capability: ReadinessCapability, status: ReadinessStatus, detail: &str) -> ReadinessFinding {
    ReadinessFinding::new(capability, status, detail)
}

struct ProcessOutcome {
    success: bool,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

enum ProcessError {
    Unavailable,
    Input,
    TimedOut,
    Wait,
    Output,
}

fn restricted_command(program: &str) -> Command {
    let mut command = Command::new(program);
    guard_external_child(&mut command);
    command.env_clear();
    if let Some(path) = std::env::var_os("PATH") {
        command.env("PATH", path);
    }
    for name in [
        "XDG_RUNTIME_DIR",
        "DBUS_SESSION_BUS_ADDRESS",
        "WAYLAND_DISPLAY",
        "XDG_SESSION_TYPE",
        "HYPRLAND_INSTANCE_SIGNATURE",
        // X11 helpers (xclip, and any tool that talks to the X server) need
        // DISPLAY to find the server and XAUTHORITY to authenticate to it.
        // Without these, a spawned X11 helper can never reach the display —
        // which is why the field clipboard wrappers had to restore them by
        // hand. Forwarding XAUTHORITY widens what a helper can read (it names a
        // file holding X credentials), so this line is reviewed as
        // security-relevant and gated on a real host.
        "DISPLAY",
        "XAUTHORITY",
    ] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    command
}

fn run_restricted(
    program: &str,
    arguments: &[&str],
    input: Option<&[u8]>,
    capture_stdout: bool,
) -> Result<ProcessOutcome, ProcessError> {
    run_restricted_with_deadline(program, arguments, input, capture_stdout, PROCESS_DEADLINE, None)
}

pub(crate) fn run_restricted_stdout(program: &str, arguments: &[&str]) -> Option<Vec<u8>> {
    run_restricted(program, arguments, None, true)
        .ok()
        .filter(|outcome| outcome.success)
        .map(|outcome| outcome.stdout)
}

/// Runs a helper whose SUCCESS mode is to fork a descendant that keeps
/// serving after the parent exits — real `wl-copy` serves the clipboard this
/// way. The descendant inherits the parent's pipes, so capturing output would
/// read the healthy case as a pipe held past the deadline; both streams are
/// discarded and only the parent's own exit status is observed.
fn run_restricted_serving(
    program: &str,
    arguments: &[&str],
    input: Option<&[u8]>,
) -> Result<ProcessOutcome, ProcessError> {
    run_restricted_serving_within(program, arguments, input, PROCESS_DEADLINE)
}

/// As [`run_restricted_serving`], but bounded by an explicit deadline so a
/// shared budget can span several candidate backends (see `clipboard_write`).
fn run_restricted_serving_within(
    program: &str,
    arguments: &[&str],
    input: Option<&[u8]>,
    deadline: Duration,
) -> Result<ProcessOutcome, ProcessError> {
    let started = Instant::now();
    let mut command = restricted_command(program);
    command
        .args(arguments)
        .stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = command.spawn().map_err(|_| ProcessError::Unavailable)?;
    let writer = match input {
        Some(input) => {
            let input = input.to_vec();
            let mut stdin = child.stdin.take().ok_or(ProcessError::Input)?;
            Some(thread::spawn(move || {
                let result = stdin.write_all(&input);
                drop(stdin);
                result
            }))
        }
        None => None,
    };
    let status = wait_for_child(&mut child, started, deadline, None);
    let writer = writer.map(|handle| bounded_join(handle, started, &mut child, deadline));
    let status = status?;
    if let Some(writer) = writer {
        match writer {
            Ok(Ok(())) => {}
            Err(ProcessError::TimedOut) => return Err(ProcessError::TimedOut),
            _ => return Err(ProcessError::Input),
        }
    }
    Ok(ProcessOutcome { success: status.success(), stdout: Vec::new(), stderr: Vec::new() })
}

fn run_restricted_with_deadline(
    program: &str,
    arguments: &[&str],
    input: Option<&[u8]>,
    capture_stdout: bool,
    deadline: Duration,
    cancel: Option<&CancelRegistry>,
) -> Result<ProcessOutcome, ProcessError> {
    // Fail fast without spawning when the operation is already cancelled.
    if cancel.is_some_and(CancelRegistry::is_cancelled) {
        return Err(ProcessError::TimedOut);
    }
    let started = Instant::now();
    let mut command = restricted_command(program);
    command
        .args(arguments)
        .stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(if capture_stdout { Stdio::piped() } else { Stdio::null() })
        .stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|_| ProcessError::Unavailable)?;
    // The whole-operation deadline starts before spawn and covers startup, the
    // stdin write, pipe drains, and wait. The write runs on its own thread so
    // the polling loop can kill an overdue child, which breaks the pipe and
    // unblocks the writer.
    let writer = match input {
        Some(input) => {
            let input = input.to_vec();
            let mut stdin = child.stdin.take().ok_or(ProcessError::Input)?;
            Some(thread::spawn(move || {
                let result = stdin.write_all(&input);
                drop(stdin);
                result
            }))
        }
        None => None,
    };
    let stdout_reader = child.stdout.take().map(|mut stdout| {
        thread::spawn(move || read_capped(&mut stdout, MAX_RETAINED_STDOUT_BYTES))
    });
    let stderr_reader = child.stderr.take().map(|mut stderr| {
        thread::spawn(move || read_capped(&mut stderr, MAX_RETAINED_STDERR_BYTES))
    });
    // Every helper thread join is bounded by the same Instant budget on every
    // path: a descendant of the child can inherit and hold the pipes open past
    // the child's own exit, which would otherwise block a bare join() forever
    // (or, on the error path, silently leave detached threads blocked).
    // Collect every helper-thread result FIRST, then decide the outcome: an
    // early return between joins would silently detach a later thread while it
    // may still be blocked on a descendant-held pipe.
    let status = wait_for_child(&mut child, started, deadline, cancel);
    let writer = writer.map(|handle| bounded_join(handle, started, &mut child, deadline));
    let stdout_joined = stdout_reader.map(|handle| bounded_join(handle, started, &mut child, deadline));
    let stderr_joined = stderr_reader.map(|handle| bounded_join(handle, started, &mut child, deadline));
    let stdout = pipe_bytes(stdout_joined)?;
    let stderr = pipe_bytes(stderr_joined)?;
    let status = status?;
    if let Some(writer) = writer {
        match writer {
            Ok(Ok(())) => {}
            Err(ProcessError::TimedOut) => return Err(ProcessError::TimedOut),
            _ => return Err(ProcessError::Input),
        }
    }
    Ok(ProcessOutcome { success: status.success(), stdout, stderr })
}

/// Joins a helper thread under the remaining process budget. On budget
/// exhaustion the overdue child is killed and the thread is deliberately
/// detached — it can never be forced to finish while a descendant holds the
/// pipe — and the caller receives the timeout error.
fn bounded_join<T: Send + 'static>(
    handle: thread::JoinHandle<T>,
    started: Instant,
    child: &mut Child,
    deadline: Duration,
) -> Result<T, ProcessError> {
    while !handle.is_finished() {
        if started.elapsed() >= deadline {
            let _ = child.kill();
            reap_briefly(child);
            drop(handle);
            return Err(ProcessError::TimedOut);
        }
        thread::sleep(PROCESS_POLL);
    }
    handle.join().map_err(|_| ProcessError::Output)
}

/// Best-effort reap of a killed child under a small extra budget so no zombie
/// is left behind; if it still has not been collected, give up rather than
/// block the caller further.
fn reap_briefly(child: &mut Child) {
    let reap_started = Instant::now();
    while reap_started.elapsed() < Duration::from_millis(250) {
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => return,
            Ok(None) => thread::sleep(PROCESS_POLL),
        }
    }
}

fn pipe_bytes(
    joined: Option<Result<std::io::Result<Vec<u8>>, ProcessError>>,
) -> Result<Vec<u8>, ProcessError> {
    match joined {
        Some(result) => result?.map_err(|_| ProcessError::Output),
        None => Ok(Vec::new()),
    }
}

/// Drains a pipe to EOF so the child never blocks on a full buffer, but
/// retains only the first `cap` bytes: a noisy child cannot force unbounded
/// memory growth inside the deadline window.
fn read_capped(source: &mut impl Read, cap: usize) -> std::io::Result<Vec<u8>> {
    let mut retained = Vec::new();
    let mut buffer = [0_u8; 1024];
    loop {
        match source.read(&mut buffer) {
            Ok(0) => return Ok(retained),
            Ok(read) => {
                let room = cap.saturating_sub(retained.len());
                retained.extend_from_slice(&buffer[..read.min(room)]);
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
}

fn wait_for_child(
    child: &mut Child,
    started: Instant,
    deadline: Duration,
    cancel: Option<&CancelRegistry>,
) -> Result<std::process::ExitStatus, ProcessError> {
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) => {}
            Err(_) => {
                // The child may still be live even though its status cannot be
                // read; kill and best-effort reap before surfacing the error.
                let _ = child.kill();
                reap_briefly(child);
                return Err(ProcessError::Wait);
            }
        }
        // Cancellation is observed by the loop that owns the Child handle:
        // killing through the handle is pid-reuse-safe because this loop is
        // also the only reaper. Latency is at most one poll tick.
        if cancel.is_some_and(CancelRegistry::is_cancelled)
            || started.elapsed() >= deadline
        {
            let _ = child.kill();
            reap_briefly(child);
            return Err(ProcessError::TimedOut);
        }
        thread::sleep(PROCESS_POLL);
    }
}

pub struct PipeWireCapture {
    reaper: ProviderReaper,
}

impl PipeWireCapture {
    pub fn new(reaper: ProviderReaper) -> Self {
        Self { reaper }
    }
}

struct CaptureReaderState {
    chunks: VecDeque<AudioChunk>,
    received_bytes: usize,
    eof: bool,
    error: Option<String>,
}

/// The capture mode the host's `pw-record` supports.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PwRecordProbe {
    /// `pw-record` could not be run at all (missing/broken).
    Unavailable,
    /// `--raw` is understood: headerless PCM on stdout (PipeWire >= 1.1, Fedora).
    Raw,
    /// `--raw` is absent (PipeWire 1.0.5, Ubuntu 24.04): `pw-record` wraps the
    /// PCM in a WAV container that must be unwrapped.
    Wav,
}

/// Probe the host `pw-record` once, caching for the process lifetime. `--raw`
/// is a *newer* PipeWire option — absent on 1.0.5, what Ubuntu 24.04 LTS ships
/// — so passing it there makes `pw-record` reject the whole invocation. A
/// version-number comparison was rejected as fragile across distro backports;
/// this parses `pw-record --help` for an exact `--raw` option token.
/// `VOISU_TEST_PW_RECORD_RAW` forces the answer for hermetic tests.
fn pw_record_capture_mode() -> PwRecordProbe {
    static MODE: OnceLock<PwRecordProbe> = OnceLock::new();
    *MODE.get_or_init(|| {
        if let Some(forced) = std::env::var_os("VOISU_TEST_PW_RECORD_RAW") {
            match forced.to_string_lossy().trim() {
                "0" | "wav" => return PwRecordProbe::Wav,
                "unavailable" => return PwRecordProbe::Unavailable,
                // `probe` exercises the real `pw-record --help` parse below
                // against a fake pw-record; anything else forces the raw path.
                "probe" => {}
                _ => return PwRecordProbe::Raw,
            }
        }
        // `--help` may print to either stream and exit nonzero on some builds;
        // inspect both regardless of exit status. A spawn failure (Err) means
        // pw-record cannot be run at all.
        match run_restricted("pw-record", &["--help"], None, true) {
            Ok(outcome) => {
                if help_advertises_raw(&outcome.stdout) || help_advertises_raw(&outcome.stderr) {
                    PwRecordProbe::Raw
                } else {
                    PwRecordProbe::Wav
                }
            }
            Err(_) => PwRecordProbe::Unavailable,
        }
    })
}

/// True only when the help text lists `--raw` as an exact option token. Splitting
/// on option separators (whitespace, `,`, `=`) rejects near-matches like
/// `--raw-file` and `--rawmode` that a substring search would accept.
fn help_advertises_raw(help: &[u8]) -> bool {
    String::from_utf8_lossy(help)
        .split(|character: char| character.is_whitespace() || character == ',' || character == '=')
        .any(|token| token == "--raw")
}

/// Strips the RIFF/WAVE framing from `pw-record` output when the tool lacks
/// `--raw` and therefore emits a WAV container. It buffers only the leading
/// header while walking the chunk chain to the `data` payload (validating the
/// format on the way), then passes the PCM through unchanged — so the existing
/// chunk reader stays oblivious to the container. A header that never resolves
/// within the retained-stdout ceiling, or whose format is wrong, is surfaced as
/// a read error and becomes a capture boundary error rather than wrong-format
/// audio reaching a provider.
struct WavHeaderStripper<R: Read> {
    inner: R,
    scan: Vec<u8>,
    pending: Vec<u8>,
    pending_pos: usize,
    header_done: bool,
}

impl<R: Read> WavHeaderStripper<R> {
    fn new(inner: R) -> Self {
        Self { inner, scan: Vec::new(), pending: Vec::new(), pending_pos: 0, header_done: false }
    }
}

impl<R: Read> Read for WavHeaderStripper<R> {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        loop {
            if self.pending_pos < self.pending.len() {
                let available = self.pending.len() - self.pending_pos;
                let take = available.min(out.len());
                out[..take]
                    .copy_from_slice(&self.pending[self.pending_pos..self.pending_pos + take]);
                self.pending_pos += take;
                if self.pending_pos == self.pending.len() {
                    self.pending.clear();
                    self.pending_pos = 0;
                }
                return Ok(take);
            }
            if self.header_done {
                return self.inner.read(out);
            }
            let mut buffer = [0_u8; PCM_CHUNK_BYTES];
            let read = self.inner.read(&mut buffer)?;
            if read == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "pw-record WAV stream ended before its data chunk",
                ));
            }
            self.scan.extend_from_slice(&buffer[..read]);
            match scan_wav_pcm(&self.scan) {
                WavScan::Incomplete => {
                    if self.scan.len() > MAX_RETAINED_STDOUT_BYTES {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "pw-record WAV header did not resolve within the bounded prefix",
                        ));
                    }
                }
                WavScan::Invalid(reason) => {
                    return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, reason));
                }
                WavScan::DataAt(offset) => {
                    self.pending = self.scan.split_off(offset);
                    self.scan = Vec::new();
                    self.pending_pos = 0;
                    self.header_done = true;
                }
            }
        }
    }
}

impl AudioCapture for PipeWireCapture {
    fn begin(&mut self, _recording_id: u64) -> Result<Box<dyn ActiveCapture>, BoundaryError> {
        // `--raw` yields headerless PCM directly (the Fedora path); without it
        // pw-record wraps the same PCM in a WAV container that WavHeaderStripper
        // unwraps below. The remaining flags are identical on both paths. An
        // Unavailable probe still takes the WAV path — the spawn below fails
        // cleanly if pw-record is truly missing.
        let raw_supported = pw_record_capture_mode() == PwRecordProbe::Raw;
        let mut command = restricted_command("pw-record");
        if raw_supported {
            command.arg("--raw");
        }
        command.args(["--rate", "16000", "--channels", "1", "--format", "s16"]);
        if let Some(target) = std::env::var_os("VOISU_PIPEWIRE_TARGET") {
            command.arg("--target").arg(target);
        }
        command
            .arg("-")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let state = Arc::new(Mutex::new(CaptureReaderState {
            chunks: VecDeque::new(),
            received_bytes: 0,
            eof: false,
            error: None,
        }));
        let reader_state = Arc::clone(&state);
        // pw-record MUST be spawned from the reader thread, never from the
        // caller: `guard_external_child` arms PR_SET_PDEATHSIG, and the kernel
        // delivers that signal when the FORKING THREAD exits, not the process.
        // The caller runs on a transient Tokio blocking-pool thread that is
        // reaped after ~10 s idle, which SIGKILLed every Recording longer than
        // that. The reader thread lives until the capture ends, so parent-death
        // delivery degrades to exactly the daemon-death contract intended.
        let (handoff_tx, handoff_rx) =
            std::sync::mpsc::channel::<Result<(Child, std::process::ChildStderr), &'static str>>();
        let reader = thread::spawn(move || {
            let mut child = match command.spawn() {
                Ok(child) => child,
                Err(_) => {
                    let _ = handoff_tx.send(Err("pw-record unavailable"));
                    return;
                }
            };
            let stdout = child.stdout.take();
            let stderr = child.stderr.take();
            let (Some(stdout), Some(stderr)) = (stdout, stderr) else {
                let _ = child.kill();
                let _ = child.wait();
                let _ = handoff_tx.send(Err("pw-record stdout unavailable"));
                return;
            };
            // Headerless PCM (`--raw`) is read straight through; a WAV container
            // (no `--raw`) is unwrapped to its PCM payload first.
            let mut stdout: Box<dyn Read + Send> = if raw_supported {
                Box::new(stdout)
            } else {
                Box::new(WavHeaderStripper::new(stdout))
            };
            if let Err(returned) = handoff_tx.send(Ok((child, stderr))) {
                // begin() is blocked on recv, so this only happens if it
                // panicked; reclaim the child rather than leaking it.
                if let Ok((mut child, _)) = returned.0 {
                    let _ = child.kill();
                    let _ = child.wait();
                }
                return;
            }
            let mut buffer = vec![0_u8; PCM_CHUNK_BYTES];
            loop {
                match stdout.read(&mut buffer) {
                    Ok(0) => {
                        reader_state.lock().unwrap().eof = true;
                        return;
                    }
                    Ok(read) => {
                        let mut state = reader_state.lock().unwrap();
                        state.received_bytes = state.received_bytes.saturating_add(read);
                        if state.received_bytes <= MAX_RECORDING_BYTES {
                            state.chunks.push_back(AudioChunk(buffer[..read].to_vec()));
                        } else if state.error.is_none() {
                            state.error = Some("Recording exceeded the bounded audio buffer".to_owned());
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(error) => {
                        let mut state = reader_state.lock().unwrap();
                        // A WAV-container format/boundary problem carries a
                        // specific, actionable message; anything else is the
                        // generic read failure.
                        state.error = Some(match error.kind() {
                            std::io::ErrorKind::InvalidData
                            | std::io::ErrorKind::UnexpectedEof => error.to_string(),
                            _ => "pw-record audio read failed".to_owned(),
                        });
                        state.eof = true;
                        return;
                    }
                }
            }
        });
        let (child, mut stderr) = handoff_rx
            .recv()
            .map_err(|_| BoundaryError::new(BoundaryKind::Capture, "pw-record unavailable"))?
            .map_err(|message| BoundaryError::new(BoundaryKind::Capture, message))?;
        let stderr_reader = thread::spawn(move || {
            read_capped(&mut stderr, MAX_RETAINED_STDERR_BYTES).unwrap_or_default()
        });
        let deadline =
            resolve_recording_deadline(std::env::var("VOISU_RECORDING_DEADLINE_MS").ok());
        Ok(Box::new(PipeWireActiveCapture {
            child: Some(child),
            state,
            reader: Some(reader),
            stderr_reader: Some(stderr_reader),
            cleanup: None,
            reaper: self.reaper.clone(),
            pcm: Vec::new(),
            started: Instant::now(),
            deadline,
        }))
    }
}

/// Default ceiling on a single Recording before the Recording Deadline stops
/// it. Recordings routinely run past two minutes (the provider chunking path
/// exists for exactly those), so the default must be generous; a stuck or
/// forgotten Recording is still bounded. `VOISU_RECORDING_DEADLINE_MS`
/// overrides it, and a zero override falls back to this default.
const DEFAULT_RECORDING_DEADLINE: Duration = Duration::from_secs(600);

/// Resolve the Recording Deadline from the raw `VOISU_RECORDING_DEADLINE_MS`
/// value. A parseable, non-zero millisecond count wins; anything else — absent,
/// unparseable, or zero — uses [`DEFAULT_RECORDING_DEADLINE`].
fn resolve_recording_deadline(raw: Option<String>) -> Duration {
    raw.and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_millis)
        .filter(|value| !value.is_zero())
        .unwrap_or(DEFAULT_RECORDING_DEADLINE)
}

struct PipeWireActiveCapture {
    child: Option<Child>,
    state: Arc<Mutex<CaptureReaderState>>,
    reader: Option<thread::JoinHandle<()>>,
    stderr_reader: Option<thread::JoinHandle<Vec<u8>>>,
    cleanup: Option<tokio::task::JoinHandle<Result<Vec<u8>, BoundaryError>>>,
    reaper: ProviderReaper,
    pcm: Vec<u8>,
    started: Instant,
    deadline: Duration,
}

impl PipeWireActiveCapture {
    fn drain_chunks(&mut self) {
        let mut state = self.state.lock().unwrap();
        while let Some(chunk) = state.chunks.pop_front() {
            self.pcm.extend_from_slice(&chunk.0);
        }
    }

    async fn stop_child(&mut self, graceful: bool) -> Result<Vec<u8>, BoundaryError> {
        if self.cleanup.is_none() {
            let child = self.child.take().ok_or_else(|| {
                BoundaryError::new(BoundaryKind::Capture, "pw-record already finalized")
            })?;
            let reader = self.reader.take();
            let stderr_reader = self.stderr_reader.take();
            self.cleanup = Some(tokio::task::spawn_blocking(move || {
                stop_child_blocking(child, reader, stderr_reader, graceful)
            }));
        }
        let result = self.cleanup.as_mut().expect("capture cleanup is present").await;
        self.cleanup.take();
        result
            .map_err(|_| {
                BoundaryError::new(BoundaryKind::Capture, "pw-record cleanup task failed")
            })?
    }

    fn validate_audio(&self) -> Result<(), BoundaryError> {
        if self.pcm.is_empty() {
            return Err(BoundaryError::new(
                BoundaryKind::EmptyRecording,
                "pw-record returned no audio frames",
            ));
        }
        if self.pcm.len() < MIN_RECORDING_BYTES {
            return Err(BoundaryError::new(
                BoundaryKind::TooShortRecording,
                format!("Recording contained {} PCM bytes", self.pcm.len()),
            ));
        }
        let audible = self.pcm.chunks_exact(2).any(|sample| {
            i16::from_le_bytes([sample[0], sample[1]]).unsigned_abs() > 32
        });
        if !audible {
            return Err(BoundaryError::new(
                BoundaryKind::SilentRecording,
                "Recording peak amplitude did not exceed the silence floor",
            ));
        }
        Ok(())
    }
}

fn stop_child_blocking(
    mut child: Child,
    reader: Option<thread::JoinHandle<()>>,
    stderr_reader: Option<thread::JoinHandle<Vec<u8>>>,
    graceful: bool,
) -> Result<Vec<u8>, BoundaryError> {
    // A tool that already exited before the stop failed on its own; only a
    // process that was still capturing when interrupted may exit nonzero.
    let exited_before_stop = matches!(child.try_wait(), Ok(Some(_)));
    if graceful {
        if let Ok(pid) = child.id().try_into() {
            unsafe {
                libc::kill(pid, libc::SIGINT);
            }
        }
    } else {
        let _ = child.kill();
    }
    let stopped = Instant::now();
    let status = wait_for_child(&mut child, stopped, PROCESS_DEADLINE, None);
    let reader = reader.map(|handle| bounded_join(handle, stopped, &mut child, PROCESS_DEADLINE));
    let stderr = stderr_reader
        .map(|handle| bounded_join(handle, stopped, &mut child, PROCESS_DEADLINE));
    if !matches!(reader, None | Some(Ok(()))) {
        return Err(BoundaryError::new(
            BoundaryKind::Capture,
            "pw-record audio drain failed",
        ));
    }
    let stderr = match stderr {
        Some(Ok(bytes)) => bytes,
        None => Vec::new(),
        Some(Err(_)) => {
            return Err(BoundaryError::new(
                BoundaryKind::Capture,
                "pw-record diagnostic drain failed",
            ));
        }
    };
    let status = status.map_err(|error| capture_process_error(error, &stderr))?;
    let expected_signal = if graceful { libc::SIGINT } else { libc::SIGKILL };
    // Real pw-record catches SIGINT and exits nonzero with no diagnostics
    // rather than dying by the signal; that silent nonzero exit is its
    // normal interrupted shape, not a failure. Anything with diagnostics,
    // or that had already died before the interrupt, stays rejected.
    let interrupted_cleanly = graceful && !exited_before_stop && stderr.is_empty();
    if !status.success()
        && status.signal() != Some(expected_signal)
        && !interrupted_cleanly
    {
        return Err(BoundaryError::new(
            BoundaryKind::Capture,
            process_diagnostic("pw-record failed", &stderr),
        ));
    }
    Ok(stderr)
}

impl ActiveCapture for PipeWireActiveCapture {
    fn next_chunk(&mut self) -> BoundaryFuture<'_, Option<AudioChunk>> {
        Box::pin(async move {
            loop {
                if self.started.elapsed() >= self.deadline {
                    return Err(BoundaryError::new(
                        BoundaryKind::RecordingDeadline,
                        "configured Recording Deadline elapsed",
                    ));
                }
                let next = {
                    let mut state = self.state.lock().unwrap();
                    if let Some(error) = state.error.clone() {
                        return Err(BoundaryError::new(BoundaryKind::Capture, error));
                    }
                    (state.chunks.pop_front(), state.eof)
                };
                match next {
                    (Some(chunk), _) => {
                        self.pcm.extend_from_slice(&chunk.0);
                        return Ok(Some(chunk));
                    }
                    (None, true) => return Ok(None),
                    (None, false) => tokio::time::sleep(PROCESS_POLL).await,
                }
            }
        })
    }

    fn finish(&mut self) -> BoundaryFuture<'_, CapturedAudio> {
        Box::pin(async move {
            self.stop_child(true).await?;
            self.drain_chunks();
            if let Some(error) = self.state.lock().unwrap().error.clone() {
                return Err(BoundaryError::new(BoundaryKind::Capture, error));
            }
            self.validate_audio()?;
            Ok(CapturedAudio::new(std::mem::take(&mut self.pcm)))
        })
    }

    fn abort(mut self: Box<Self>) -> BoundaryFuture<'static, ()> {
        Box::pin(async move {
            self.stop_child(false).await?;
            Ok(())
        })
    }
}

impl Drop for PipeWireActiveCapture {
    fn drop(&mut self) {
        if let Some(cleanup) = self.cleanup.take() {
            // An outer abort deadline may drop stop_child after it transferred
            // pw-record and both reader handles to spawn_blocking. Retain that
            // task in the actor-owned supervisor; every workflow drains it before
            // its acknowledgement permits the next Recording.
            self.reaper.adopt_capture(cleanup);
        } else if let Some(child) = self.child.take() {
            // stop_child never ran: capture_pump panicked or was cancelled while
            // still owning a live pw-record. Killing under reap_briefly's 250 ms
            // and then dropping the child and both reader handles would let a
            // slow-exiting child — or a descendant holding the pipe — outlive
            // Drop while the reaper looks empty, so supervise_recording could
            // permit Idle mid-cleanup. Hand the raw child and reader handles to
            // the reaper's bounded kill/reap instead; every Idle-permitting path
            // drains it before its acknowledgement releases the next Recording.
            self.reaper.adopt_capture_blocking(
                child,
                self.reader.take(),
                self.stderr_reader.take(),
            );
        }
    }
}

fn capture_process_error(error: ProcessError, stderr: &[u8]) -> BoundaryError {
    let detail = match error {
        ProcessError::Unavailable => "pw-record unavailable".to_owned(),
        ProcessError::TimedOut => "pw-record cleanup deadline elapsed".to_owned(),
        ProcessError::Input | ProcessError::Wait | ProcessError::Output => {
            process_diagnostic("pw-record execution failed", stderr)
        }
    };
    BoundaryError::new(BoundaryKind::Capture, detail)
}

fn process_diagnostic(prefix: &str, stderr: &[u8]) -> String {
    let detail = String::from_utf8_lossy(stderr);
    let detail = detail.trim();
    if detail.is_empty() {
        prefix.to_owned()
    } else {
        format!("{prefix}: {detail}")
    }
}

pub struct GroqProvider {
    reaper: ProviderReaper,
    prompt: Option<String>,
}

impl GroqProvider {
    /// Builds a Groq provider whose streams share the actor-owned `reaper`, so a
    /// stream dropped mid-abort hands its curl reap to the supervisor the actor
    /// drains before Idle.
    pub fn new(reaper: ProviderReaper) -> Self {
        Self { reaper, prompt: None }
    }

    /// Builds a provider with a Recording-start dictionary snapshot. Supplying
    /// the prompt keeps every Groq request for that Recording on the same
    /// glossary as its Deepgram stream.
    pub fn with_prompt(reaper: ProviderReaper, prompt: String) -> Self {
        Self {
            reaper,
            prompt: Some(prompt),
        }
    }
}

impl TranscriptProvider for GroqProvider {
    fn start(&mut self, _recording_id: u64) -> Result<Box<dyn ProviderStream>, BoundaryError> {
        let credential = SecretStore::load(&mut SecretToolStore, Provider::Groq)?;
        let endpoint = std::env::var("VOISU_GROQ_TRANSCRIPTION_URL")
            .unwrap_or_else(|_| "https://api.groq.com/openai/v1/audio/transcriptions".to_owned());
        if !provider_endpoint_is_secure(&endpoint) {
            return Err(BoundaryError::new(
                BoundaryKind::Provider,
                "Groq transcription endpoint must use HTTPS except on loopback",
            ));
        }
        Ok(Box::new(GroqStream {
            credential,
            endpoint,
            params: GroqRequestParams::from_config(
                self.prompt
                    .clone()
                    .unwrap_or_else(crate::dictionary::whisper_prompt),
            ),
            buffer: Vec::new(),
            streamed_bytes: 0,
            chunks: VecDeque::new(),
            cancel: CancelRegistry::new(),
            reaper: self.reaper.clone(),
        }))
    }
}

fn provider_endpoint_is_secure(endpoint: &str) -> bool {
    if endpoint.contains(['\n', '\r']) {
        return false;
    }
    if endpoint.starts_with("https://") {
        return true;
    }
    let Some(remainder) = endpoint.strip_prefix("http://") else {
        return false;
    };
    authority_is_loopback(remainder.split('/').next().unwrap_or_default())
}

fn authority_is_loopback(authority: &str) -> bool {
    let authority = authority.to_ascii_lowercase();
    authority == "localhost"
        || authority.starts_with("localhost:")
        || authority == "127.0.0.1"
        || authority.starts_with("127.0.0.1:")
        || authority == "[::1]"
        || authority.starts_with("[::1]:")
}

/// The per-Recording Groq/Whisper request tuning built once at stream start:
/// the model, the transcription language, and the vocabulary prompt. Cloned
/// into every chunk request so all requests for a Recording share one glossary.
#[derive(Clone)]
struct GroqRequestParams {
    model: String,
    language: String,
    prompt: String,
}

impl GroqRequestParams {
    /// Resolves the request tuning from config and a resolved dictionary prompt:
    /// model from `VOISU_GROQ_MODEL` (default `whisper-large-v3`), language from
    /// `VOISU_GROQ_LANGUAGE` (default `en`), and the Whisper vocabulary prompt.
    fn from_config(prompt: String) -> Self {
        let model = std::env::var("VOISU_GROQ_MODEL")
            .unwrap_or_else(|_| "whisper-large-v3".to_owned());
        let language = std::env::var("VOISU_GROQ_LANGUAGE").unwrap_or_else(|_| "en".to_owned());
        Self {
            model,
            language,
            prompt,
        }
    }
}

/// Whether the Groq stream should pre-stream chunks yet. Recordings at or below
/// the full-audio limit never pre-stream — they take one full-audio request at
/// finalize; only once a Recording grows past the limit does chunking begin.
fn groq_prestream_active(total_received_bytes: usize) -> bool {
    total_received_bytes > GROQ_FULL_AUDIO_MAX_BYTES
}

/// Plans the finalize Groq request(s) over a `len`-byte finalized buffer. A
/// buffer at or below the full-audio limit is one full-audio request; a buffer
/// past the limit (for example when a capture backlog appended at Stop pushes it
/// over) is split into 60 s windows with a 4 s overlap so no single request is
/// oversized and the word-overlap dedup can stitch the seams.
// A one-element Vec<Range> IS the intent: the whole capture as a single chunk.
#[allow(clippy::single_range_in_vec_init)]
fn plan_finalize_chunks(len: usize) -> Vec<std::ops::Range<usize>> {
    if len <= GROQ_FULL_AUDIO_MAX_BYTES {
        return vec![0..len];
    }
    let step = GROQ_CHUNK_BYTES - GROQ_CHUNK_OVERLAP_BYTES;
    let mut ranges = Vec::new();
    let mut start = 0;
    while start < len {
        let end = (start + GROQ_CHUNK_BYTES).min(len);
        ranges.push(start..end);
        if end == len {
            break;
        }
        start += step;
    }
    ranges
}

struct GroqStream {
    credential: Credential,
    endpoint: String,
    params: GroqRequestParams,
    buffer: Vec<u8>,
    streamed_bytes: usize,
    chunks: VecDeque<tokio::task::JoinHandle<Result<String, BoundaryError>>>,
    /// Per-Recording cancellation flag observed by each in-flight curl
    /// request's owning bounded wait. Because each Recording gets its own
    /// stream and flag, cancelling one Recording can never touch the next
    /// one's requests, and stale results die with their aborted stream.
    cancel: Arc<CancelRegistry>,
    /// Actor-owned supervisor that adopts this stream's chunk tasks if the
    /// stream is dropped mid-abort, so their curl reap is retained and awaited
    /// rather than detached.
    reaper: ProviderReaper,
}

/// A retained provider-stream cleanup: a future that awaits an adopted chunk
/// task deque until every chunk task — and therefore every nested
/// `spawn_blocking` curl reap those tasks own — has completed.
type ReapTask = std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>;

/// Actor-owned supervisor that keeps capture and provider-stream cleanup alive
/// and awaitable. When an adapter is dropped mid-abort — for example the abort
/// deadline elapsed and Tokio dropped the future that owned it — the adapter
/// hands its still-live cleanup task here. Adoption is SYNCHRONOUS: it retains
/// the raw handles inside a
/// future without spawning and without touching `Handle::try_current()`, so a
/// stream dropped from any thread — including during runtime teardown — always
/// lands its cleanup in this supervisor. The retained cleanup AWAITS each task
/// (never `abort()`, which would drop a nested `spawn_blocking` handle and
/// detach the still-running process cleanup before the child is reaped).
/// Drains are serialized: a concurrent drain waits for the in-flight one and
/// then re-checks, so it can never observe an empty supervisor while another
/// drain still holds unfinished cleanup. Each workflow task drains this
/// supervisor under an explicit bound after its streams have dropped and before
/// it acknowledges completion to the actor — the acknowledgement that alone
/// permits Idle — and the daemon drains it again after the actor has joined,
/// before the runtime is torn down.
#[derive(Clone, Default)]
pub struct ProviderReaper {
    tasks: Arc<std::sync::Mutex<Vec<ReapTask>>>,
    /// Serializes `drain` calls. While one drain temporarily holds cleanup
    /// futures out of `tasks`, a concurrent drain must wait here instead of
    /// reading `tasks` as empty and reporting a completed drain over live work.
    serial: Arc<tokio::sync::Mutex<()>>,
}

impl ProviderReaper {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adopts the still-live chunk tasks of a dropped stream. Cancellation MUST
    /// already be signalled so each task's owning bounded wait observes the
    /// flag, kills and reaps its curl child, and returns. Synchronous and
    /// runtime-free: the handles are wrapped in a future and retained; only a
    /// later `drain` polls them, so adopting from a non-runtime thread or a
    /// shutting-down runtime can never detach or abort the cleanup.
    /// Retains one cleanup future. Called from `Drop` (capture and
    /// provider-stream adoption), so it must never unwind: a poisoned lock is
    /// recovered rather than `expect`ed. The lock is only ever held for a
    /// push/take/len, so its guarded state stays consistent under recovery.
    fn retain(&self, task: ReapTask) {
        let mut guard = match self.tasks.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.push(task);
    }

    fn adopt<T: Send + 'static>(&self, mut chunks: VecDeque<tokio::task::JoinHandle<T>>) {
        if chunks.is_empty() {
            return;
        }
        self.retain(Box::pin(async move {
            while let Some(chunk) = chunks.pop_front() {
                let _ = chunk.await;
            }
        }));
    }

    fn adopt_capture(
        &self,
        cleanup: tokio::task::JoinHandle<Result<Vec<u8>, BoundaryError>>,
    ) {
        self.adopt(VecDeque::from([cleanup]));
    }

    /// Adopts a pre-stop capture whose `stop_child` never ran: the raw pw-record
    /// child and reader threads are still live. A dedicated OS thread performs
    /// `stop_child_blocking`'s bounded kill/reap/join off any async worker, and
    /// the retained future awaits its completion signal, so a drain blocks the
    /// Idle transition until the child and both reader threads are actually gone
    /// — not merely `reap_briefly`'s 250 ms. Runtime-free and non-panicking: no
    /// `spawn_blocking` and no `Handle::try_current`, so this still lands its
    /// cleanup when `Drop` runs on a non-runtime teardown thread. The thread's
    /// own bounds (`wait_for_child` and the two `bounded_join`s under
    /// `PROCESS_DEADLINE`) guarantee it signals, so the drain terminates.
    fn adopt_capture_blocking(
        &self,
        child: Child,
        reader: Option<thread::JoinHandle<()>>,
        stderr_reader: Option<thread::JoinHandle<Vec<u8>>>,
    ) {
        let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();
        thread::spawn(move || {
            // Abandoned Recording: SIGKILL (graceful = false), mirroring the
            // prior `child.kill()`. The classification result is irrelevant to a
            // dropped capture and is discarded; only the reap matters.
            let _ = stop_child_blocking(child, reader, stderr_reader, false);
            let _ = done_tx.send(());
        });
        self.retain(Box::pin(async move {
            let _ = done_rx.await;
        }));
    }

    /// Number of cleanup futures currently retained and not being drained.
    /// Test-observability only — production callers gate on `drain` /
    /// `drain_to_completion`, never on this count, because a cleanup being
    /// awaited by an in-flight `drain` is not counted.
    #[cfg(test)]
    fn pending(&self) -> usize {
        self.tasks
            .lock()
            .expect("provider reaper mutex poisoned")
            .len()
    }

    /// Awaits every retained cleanup future, bounded by `within`. Returns
    /// `true` when the supervisor fully drained, re-checking for cleanup adopted
    /// while draining. On timeout it puts every unfinished future back — so
    /// cleanup is retained, never detached — and returns `false`. Serialized
    /// with every other drain.
    /// Drains to completion in bounded passes, returning only once the
    /// supervisor is empty. A single bounded `drain` that times out RETAINS the
    /// unfinished cleanup — but a caller about to tear down the runtime would
    /// then drop the supervisor and detach that cleanup after all, so teardown
    /// paths must use this instead and keep draining. Each retained cleanup is
    /// internally bounded (capture and provider waits kill and reap their child
    /// within their own poll bounds), so this terminates; the service unit's
    /// explicit TimeoutStopSec is the external last-resort backstop.
    pub async fn drain_to_completion(&self, pass: Duration) {
        while !self.drain(pass).await {
            // Guaranteed-completion callers gate the Idle transition on this
            // drain; a failed stderr write must not panic it (`eprintln!` does).
            let _ = writeln!(std::io::stderr(), "provider cleanup still draining");
        }
    }

    pub async fn drain(&self, within: Duration) -> bool {
        let _serial = self.serial.lock().await;
        let deadline = tokio::time::Instant::now() + within;
        loop {
            let mut batch: Vec<ReapTask> = {
                let mut guard = self.tasks.lock().expect("provider reaper mutex poisoned");
                std::mem::take(&mut *guard)
            };
            if batch.is_empty() {
                return true;
            }
            while let Some(mut task) = batch.pop() {
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                if tokio::time::timeout(remaining, &mut task).await.is_err() {
                    let mut guard =
                        self.tasks.lock().expect("provider reaper mutex poisoned");
                    guard.push(task);
                    guard.append(&mut batch);
                    return false;
                }
            }
        }
    }
}

impl Drop for GroqStream {
    fn drop(&mut self) {
        // Signal cancellation FIRST so each in-flight curl request's owning
        // bounded wait kills and reaps its child, then hand the still-live chunk
        // tasks to the actor-owned reaper. Never abort them here: aborting the
        // outer task drops its nested `spawn_blocking` handle and detaches the
        // curl kill/reap, which is exactly the window that let Idle be published
        // over live blocking work.
        self.cancel.cancel();
        self.reaper.adopt(std::mem::take(&mut self.chunks));
    }
}

impl ProviderStream for GroqStream {
    fn provider(&self) -> Provider {
        Provider::Groq
    }

    fn send_audio(&mut self, chunk: AudioChunk) -> BoundaryFuture<'_, ()> {
        Box::pin(async move {
            self.streamed_bytes = self.streamed_bytes.saturating_add(chunk.0.len());
            self.buffer.extend_from_slice(&chunk.0);
            // A Recording at or below the full-audio limit never pre-streams: it
            // is transcribed as one full-audio request at finalize. Only once it
            // grows past the limit do we cut 60 s chunks with a 4 s overlap.
            if groq_prestream_active(self.streamed_bytes) {
                while self.buffer.len() >= GROQ_CHUNK_BYTES {
                    let pcm = self.buffer[..GROQ_CHUNK_BYTES].to_vec();
                    self.buffer = self.buffer
                        [GROQ_CHUNK_BYTES - GROQ_CHUNK_OVERLAP_BYTES..]
                        .to_vec();
                    let credential = self.credential.clone();
                    let endpoint = self.endpoint.clone();
                    let params = self.params.clone();
                    let cancel = Arc::clone(&self.cancel);
                    self.chunks.push_back(tokio::spawn(async move {
                        ProviderHttpClient
                            .transcribe_groq_chunk(credential, endpoint, params, pcm, cancel)
                            .await
                    }));
                }
            }
            Ok(())
        })
    }

    fn abort(mut self: Box<Self>) -> BoundaryFuture<'static, ()> {
        Box::pin(async move {
            // Cancel the in-flight curl children first: each owning bounded
            // wait observes the flag within one poll tick and kills through
            // its own Child handle. Aborting the tasks alone would detach
            // already-running blocking requests, letting work from the failed
            // Recording overlap the next one.
            self.cancel.cancel();
            while let Some(chunk) = self.chunks.front_mut() {
                let _ = chunk.await;
                self.chunks.pop_front();
            }
            Ok(())
        })
    }

    fn complete(&mut self, audio: CapturedAudio) -> BoundaryFuture<'_, SourceTranscript> {
        Box::pin(async move {
            let pcm = audio.pcm_s16le_mono_16khz();
            if self.streamed_bytes > pcm.len() {
                return Err(BoundaryError::new(
                    BoundaryKind::Provider,
                    "Groq stream exceeded the finalized Recording",
                ));
            }
            self.buffer.extend_from_slice(&pcm[self.streamed_bytes..]);
            // A finalize request needs issuing when nothing was pre-streamed, or
            // when the retained overlap tail carries fresh audio past the last
            // pre-streamed chunk. Its handle MUST live in `self.chunks` so a
            // Provider Deadline that drops this future still leaves `abort` /
            // `Drop` / the `ProviderReaper` owning — and killing — its curl
            // child, exactly as pre-streamed chunks are owned. Awaiting the
            // request inline here would detach that curl on cancellation.
            let needs_finalize =
                self.chunks.is_empty() || self.buffer.len() > GROQ_CHUNK_OVERLAP_BYTES;
            if needs_finalize {
                let buffer = std::mem::take(&mut self.buffer);
                // Re-evaluate the full-audio gate against the FINALIZED length:
                // a capture backlog appended at Stop can push a Recording past
                // the 120 s limit even when nothing crossed it during streaming,
                // in which case it must be chunked, not sent as one request.
                for range in plan_finalize_chunks(buffer.len()) {
                    let pcm = buffer[range].to_vec();
                    let credential = self.credential.clone();
                    let endpoint = self.endpoint.clone();
                    let params = self.params.clone();
                    let cancel = Arc::clone(&self.cancel);
                    self.chunks.push_back(tokio::spawn(async move {
                        ProviderHttpClient
                            .transcribe_groq_chunk(credential, endpoint, params, pcm, cancel)
                            .await
                    }));
                }
            }
            let mut transcripts = Vec::new();
            while let Some(chunk) = self.chunks.front_mut() {
                // Keep the handle in `self.chunks` for the await so a Provider
                // Deadline that drops this future still leaves `Drop` an owned
                // task to adopt. Once the await resolves, pop it BEFORE the `?`
                // error propagation: a completed handle left behind would be
                // polled a second time by the reaper's adopt closure and panic
                // ("JoinHandle polled after completion").
                let joined = chunk.await;
                self.chunks.pop_front();
                let transcript = joined.map_err(|_| {
                    BoundaryError::new(BoundaryKind::Provider, "Groq chunk task failed")
                })??;
                transcripts.push(transcript);
            }
            let text = merge_chunk_transcripts(transcripts);
            Ok(SourceTranscript {
                provider: Provider::Groq,
                text,
            })
        })
    }
}

pub struct DeepgramProvider {
    reaper: ProviderReaper,
    /// nova-3 `keyterm` boosting terms, repeated as query params on the
    /// streaming URL. Ticket 04's shared dictionary is wired in here by the
    /// driver at merge; until then the list defaults to empty.
    keyterms: Vec<String>,
}

impl DeepgramProvider {
    /// Builds a Deepgram provider whose streams share the actor-owned `reaper`,
    /// so a stream dropped mid-abort hands its websocket I/O task to the
    /// supervisor the actor drains before Idle. No keyterm boosting.
    pub fn new(reaper: ProviderReaper) -> Self {
        Self::with_keyterms(reaper, Vec::new())
    }

    /// Same as [`DeepgramProvider::new`] but with nova-3 `keyterm` boosting
    /// terms appended to every streaming connection URL.
    pub fn with_keyterms(reaper: ProviderReaper, keyterms: Vec<String>) -> Self {
        Self { reaper, keyterms }
    }
}

impl TranscriptProvider for DeepgramProvider {
    fn start(&mut self, _recording_id: u64) -> Result<Box<dyn ProviderStream>, BoundaryError> {
        let credential = SecretStore::load(&mut SecretToolStore, Provider::Deepgram)?;
        let base = std::env::var("VOISU_DEEPGRAM_TRANSCRIPTION_URL")
            .unwrap_or_else(|_| "wss://api.deepgram.com/v1/listen".to_owned());
        let url = deepgram_streaming_url(&base, &self.keyterms)?;
        Ok(Box::new(DeepgramStream::connect(
            url,
            credential,
            DEEPGRAM_KEEPALIVE_INTERVAL,
            DEEPGRAM_CLOSE_GRACE,
            self.reaper.clone(),
        )))
    }
}

/// Fixed query params for the Deepgram nova-3 real-time streaming connection:
/// raw s16le/16kHz/mono PCM in, interim results on (finals are filtered by the
/// accumulator), smart formatting, and explicit endpointing/utterance-end
/// tuning for dictation pauses.
const DEEPGRAM_STREAMING_PARAMS: &[(&str, &str)] = &[
    ("model", "nova-3"),
    ("encoding", "linear16"),
    ("sample_rate", "16000"),
    ("channels", "1"),
    ("interim_results", "true"),
    ("smart_format", "true"),
    ("punctuate", "true"),
    ("endpointing", "300"),
    ("utterance_end_ms", "1000"),
];

/// Builds the streaming websocket URL from a base endpoint. `https`/`http`
/// bases are rewritten to `wss`/`ws` so the existing endpoint override env var
/// keeps working; plaintext `ws` is allowed only on loopback, mirroring the
/// HTTPS policy of the batch endpoints.
fn deepgram_streaming_url(base: &str, keyterms: &[String]) -> Result<String, BoundaryError> {
    if base.contains(['\n', '\r']) {
        return Err(BoundaryError::new(
            BoundaryKind::Provider,
            "Deepgram streaming endpoint must use WSS except on loopback",
        ));
    }
    let normalized = if let Some(rest) = base.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = base.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        base.to_owned()
    };
    let (plaintext, remainder) = if let Some(rest) = normalized.strip_prefix("wss://") {
        (false, rest)
    } else if let Some(rest) = normalized.strip_prefix("ws://") {
        (true, rest)
    } else {
        return Err(BoundaryError::new(
            BoundaryKind::Provider,
            "Deepgram streaming endpoint must use WSS except on loopback",
        ));
    };
    let authority = remainder.split('/').next().unwrap_or_default();
    // Reject userinfo outright: `ws://127.0.0.1:80@attacker.example/…` has a
    // loopback-LOOKING authority prefix but its HOST is attacker.example, and
    // loopback-checking the raw authority string would send the Token header
    // there over plaintext. Deepgram auth travels in the Authorization header,
    // so no legitimate endpoint carries userinfo.
    if authority.is_empty() || authority.contains('@') {
        return Err(BoundaryError::new(
            BoundaryKind::Provider,
            "Deepgram streaming endpoint authority is invalid",
        ));
    }
    if plaintext && !authority_is_loopback(authority) {
        return Err(BoundaryError::new(
            BoundaryKind::Provider,
            "Deepgram streaming endpoint must use WSS except on loopback",
        ));
    }
    let mut url = normalized;
    let mut separator = if url.contains('?') { '&' } else { '?' };
    for (name, value) in DEEPGRAM_STREAMING_PARAMS {
        url.push(separator);
        url.push_str(name);
        url.push('=');
        url.push_str(value);
        separator = '&';
    }
    for keyterm in keyterms {
        let keyterm = keyterm.trim();
        if keyterm.is_empty() {
            continue;
        }
        url.push(separator);
        url.push_str("keyterm=");
        url.push_str(&percent_encode_query(keyterm));
        separator = '&';
    }
    Ok(url)
}

/// Percent-encodes a query component: RFC 3986 unreserved characters pass
/// through, everything else (including spaces) becomes `%XX`.
fn percent_encode_query(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(byte as char);
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

/// Assembles the Recording's Transcript from Deepgram streaming `Results`
/// messages. ONLY `is_final: true` segments are accumulated — interim results
/// for a time window are superseded by later messages and must never reach the
/// Transcript — so this can never blindly concatenate revisions of the same
/// audio the way the removed per-chunk batch path did.
#[derive(Default)]
struct TranscriptAccumulator {
    segments: Vec<String>,
}

impl TranscriptAccumulator {
    fn ingest(&mut self, message: &serde_json::Value) {
        if message.get("type").and_then(serde_json::Value::as_str) != Some("Results") {
            return;
        }
        if message.get("is_final").and_then(serde_json::Value::as_bool) != Some(true) {
            return;
        }
        let Some(text) = message
            .pointer("/channel/alternatives/0/transcript")
            .and_then(serde_json::Value::as_str)
        else {
            return;
        };
        let text = text.trim();
        if !text.is_empty() {
            self.segments.push(text.to_owned());
        }
    }

    fn text(&self) -> String {
        self.segments.join(" ")
    }
}

/// Frames the stream owner hands to the websocket I/O task: raw PCM goes out
/// as binary frames, `Finalize`/`CloseStream` as JSON text frames.
enum DeepgramOutbound {
    Audio(Vec<u8>),
    Finalize,
    CloseStream,
}

type DeepgramSocket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

struct DeepgramStream {
    /// `None` once `complete()` has taken the sender; dropping it lets the I/O
    /// task observe end-of-outbound and settle.
    outbound: Option<tokio::sync::mpsc::UnboundedSender<DeepgramOutbound>>,
    streamed_bytes: usize,
    /// The single long-lived websocket I/O task, kept in a deque so `Drop`
    /// hands it to the actor-owned `ProviderReaper` through the same adoption
    /// contract the curl chunk tasks used (await, never abort).
    io_tasks: VecDeque<tokio::task::JoinHandle<Result<(), BoundaryError>>>,
    /// Filled by the I/O task as finalized `Results` arrive.
    transcript: Arc<Mutex<TranscriptAccumulator>>,
    /// Per-Recording cancellation flag polled by the I/O task on a bounded
    /// tick, mirroring the poll-bound discipline of the subprocess waits.
    cancel: Arc<CancelRegistry>,
    /// Awaitable companion to `cancel`: `abort()`/`Drop` notify it so the I/O
    /// task wakes immediately instead of waiting out a backoff sleep or poll
    /// tick — the abort path must not stretch the Processing window.
    shutdown: Arc<tokio::sync::Notify>,
    /// Actor-owned supervisor that adopts the I/O task if the stream is
    /// dropped mid-abort, so the websocket teardown is retained and awaited
    /// rather than detached.
    reaper: ProviderReaper,
}

impl DeepgramStream {
    /// Spawns the websocket I/O task for one Recording. Must be called on the
    /// runtime; connect failures surface later, through `send_audio` (closed
    /// channel) or `complete()`/`abort()` (the task's stored error).
    fn connect(
        url: String,
        credential: Credential,
        keepalive: Duration,
        close_grace: Duration,
        reaper: ProviderReaper,
    ) -> Self {
        let (outbound_tx, outbound_rx) = tokio::sync::mpsc::unbounded_channel();
        let transcript = Arc::new(Mutex::new(TranscriptAccumulator::default()));
        let cancel = CancelRegistry::new();
        let shutdown = Arc::new(tokio::sync::Notify::new());
        let io_task = tokio::spawn(deepgram_ws_task(
            url,
            credential,
            outbound_rx,
            Arc::clone(&transcript),
            Arc::clone(&cancel),
            Arc::clone(&shutdown),
            keepalive,
            close_grace,
        ));
        Self {
            outbound: Some(outbound_tx),
            streamed_bytes: 0,
            io_tasks: VecDeque::from([io_task]),
            transcript,
            cancel,
            shutdown,
            reaper,
        }
    }
}

impl Drop for DeepgramStream {
    fn drop(&mut self) {
        // See `Drop for GroqStream`: cancel first, then adopt (await, never
        // abort) so the websocket I/O task finishes its teardown before the
        // reaper task completes and Idle becomes observable.
        self.cancel.cancel();
        self.shutdown.notify_waiters();
        self.reaper.adopt(std::mem::take(&mut self.io_tasks));
    }
}

impl ProviderStream for DeepgramStream {
    fn provider(&self) -> Provider {
        Provider::Deepgram
    }

    fn send_audio(&mut self, chunk: AudioChunk) -> BoundaryFuture<'_, ()> {
        Box::pin(async move {
            self.streamed_bytes = self.streamed_bytes.saturating_add(chunk.0.len());
            let outbound = self.outbound.as_ref().ok_or_else(|| {
                BoundaryError::new(BoundaryKind::Provider, "Deepgram stream already completed")
            })?;
            // A closed channel means the I/O task already failed. Do NOT fail
            // here: `ProviderCoordinator::stream_audio` propagates send errors
            // and would fail the whole Recording, while the bound decision is
            // that the parallel Groq stream carries it. The stored I/O-task
            // error surfaces visibly through `complete()` instead.
            let _ = outbound.send(DeepgramOutbound::Audio(chunk.0));
            Ok(())
        })
    }

    fn abort(mut self: Box<Self>) -> BoundaryFuture<'static, ()> {
        Box::pin(async move {
            // Signal cancellation first: the I/O task wakes on the shutdown
            // notification (the flag backstops a pre-poll race), closes the
            // websocket, and returns. Await it — never abort — through the
            // same front/pop discipline as the chunk tasks, so a drop
            // mid-await leaves the handle for the reaper. If the task had
            // ALREADY stored a provider failure (server Error, exhausted
            // dials) before this Recording was aborted for an unrelated
            // reason, surface it through abort's error channel rather than
            // discarding it — send_audio deliberately hides the closed
            // channel, so this is the failure's only remaining exit.
            // (Limitation: without voisu-core changes this reaches the
            // recovery-abort diagnostics, not the per-provider history.)
            self.cancel.cancel();
            self.shutdown.notify_waiters();
            let mut stored_failure = Ok(());
            while let Some(io_task) = self.io_tasks.front_mut() {
                let joined = io_task.await;
                self.io_tasks.pop_front();
                if let Ok(Err(error)) = joined {
                    stored_failure = Err(error);
                }
            }
            stored_failure
        })
    }

    fn complete(&mut self, audio: CapturedAudio) -> BoundaryFuture<'_, SourceTranscript> {
        Box::pin(async move {
            let pcm = audio.pcm_s16le_mono_16khz();
            if self.streamed_bytes > pcm.len() {
                return Err(BoundaryError::new(
                    BoundaryKind::Provider,
                    "Deepgram stream exceeded the finalized Recording",
                ));
            }
            if let Some(outbound) = self.outbound.take() {
                // Top up with any un-streamed tail, flush server-side buffers,
                // and end the stream gracefully. A closed channel here means
                // the I/O task already ended; its stored result carries the
                // error, so failed sends are deliberately ignored.
                let tail = &pcm[self.streamed_bytes..];
                if !tail.is_empty() {
                    let _ = outbound.send(DeepgramOutbound::Audio(tail.to_vec()));
                }
                let _ = outbound.send(DeepgramOutbound::Finalize);
                let _ = outbound.send(DeepgramOutbound::CloseStream);
            }
            // Await the I/O task WITHOUT removing it from `self.io_tasks`. If
            // this completion future is dropped mid-await (e.g. the Provider
            // Deadline elapses and the coordinator moves to `abort()`), the
            // handle must still be in the deque so the gated `abort()` awaits
            // the websocket teardown before Idle is observable.
            while let Some(io_task) = self.io_tasks.front_mut() {
                let joined = io_task.await;
                self.io_tasks.pop_front();
                joined.map_err(|_| {
                    BoundaryError::new(BoundaryKind::Provider, "Deepgram streaming task failed")
                })??;
            }
            let text = self
                .transcript
                .lock()
                .expect("Deepgram transcript accumulator mutex poisoned")
                .text();
            Ok(SourceTranscript {
                provider: Provider::Deepgram,
                text,
            })
        })
    }
}

/// How one websocket connection ended, as seen by the per-connection driver.
enum DeepgramConnectionEnd {
    /// The stream ended on purpose: `CloseStream` acknowledged, outbound side
    /// dropped, or cancellation observed. The I/O task is done.
    Finished,
    /// The connection dropped mid-Recording; the I/O task may redial within
    /// the bounded reconnect budget.
    Lost,
}

/// The long-lived websocket I/O task: one per Recording, owning the Deepgram
/// connection end to end. Slots into the existing `ProviderReaper` adoption
/// contract as a single `JoinHandle`. A connection lost mid-Recording is
/// redialed at most `DEEPGRAM_RECONNECT_ATTEMPTS` times (audio already in
/// flight during the drop is lost — the parallel Groq stream covers the gap);
/// past the budget the error is stored here and surfaces through `complete()`.
#[allow(clippy::too_many_arguments)] // WS plumbing carries the full session context; fate tied to the Deepgram keep/delete decision
async fn deepgram_ws_task(
    url: String,
    credential: Credential,
    outbound: tokio::sync::mpsc::UnboundedReceiver<DeepgramOutbound>,
    transcript: Arc<Mutex<TranscriptAccumulator>>,
    cancel: Arc<CancelRegistry>,
    shutdown: Arc<tokio::sync::Notify>,
    keepalive: Duration,
    close_grace: Duration,
) -> Result<(), BoundaryError> {
    // Arm the shutdown wakeup before any other await so an abort lands
    // immediately at whichever await point the session loop is parked on —
    // a backoff sleep or in-flight dial must not stretch the abort. The
    // cancellation flag backstops a notify that fires before this task's
    // first poll. Dropping the session future mid-await only drops an
    // in-process socket — nothing external is left to reap.
    let shutdown_notified = shutdown.notified();
    tokio::pin!(shutdown_notified);
    let sessions = deepgram_ws_sessions(
        url,
        credential,
        outbound,
        transcript,
        Arc::clone(&cancel),
        keepalive,
        close_grace,
    );
    tokio::pin!(sessions);
    if cancel.is_cancelled() {
        return Ok(());
    }
    tokio::select! {
        result = &mut sessions => result,
        _ = &mut shutdown_notified => Ok(()),
    }
}

/// The reconnect-bounded connection loop driven by [`deepgram_ws_task`].
async fn deepgram_ws_sessions(
    url: String,
    credential: Credential,
    mut outbound: tokio::sync::mpsc::UnboundedReceiver<DeepgramOutbound>,
    transcript: Arc<Mutex<TranscriptAccumulator>>,
    cancel: Arc<CancelRegistry>,
    keepalive: Duration,
    close_grace: Duration,
) -> Result<(), BoundaryError> {
    let mut reconnects_left = DEEPGRAM_RECONNECT_ATTEMPTS;
    let mut pending: Option<DeepgramOutbound> = None;
    // Set once any audio frame has been accepted by any socket: from then on
    // a lost connection is unrecoverable (see DEEPGRAM_RECONNECT_ATTEMPTS).
    let mut audio_delivered = false;
    loop {
        if cancel.is_cancelled() {
            return Ok(());
        }
        let socket = match deepgram_ws_connect(&url, &credential, &cancel).await {
            Ok(socket) => socket,
            Err(error) => {
                if cancel.is_cancelled() {
                    // Aborted while dialing: nothing was connected, nothing to
                    // reap — finish instead of burning the reconnect budget.
                    return Ok(());
                }
                if reconnects_left == 0 {
                    return Err(error);
                }
                reconnects_left -= 1;
                tokio::time::sleep(DEEPGRAM_RECONNECT_BACKOFF).await;
                continue;
            }
        };
        match drive_deepgram_connection(
            socket,
            &mut outbound,
            &mut pending,
            &mut audio_delivered,
            &transcript,
            &cancel,
            keepalive,
            close_grace,
        )
        .await?
        {
            DeepgramConnectionEnd::Finished => return Ok(()),
            DeepgramConnectionEnd::Lost => {
                if audio_delivered {
                    // Audio accepted by the dropped socket but not yet
                    // finalized cannot be replayed: redialing and continuing
                    // would return a plausible Transcript with a silent gap.
                    // Fail visibly; the parallel Groq stream carries the
                    // Recording (PRD §3.3).
                    return Err(BoundaryError::new(
                        BoundaryKind::Provider,
                        "Deepgram streaming connection lost",
                    ));
                }
                if reconnects_left == 0 {
                    return Err(BoundaryError::new(
                        BoundaryKind::Provider,
                        "Deepgram streaming connection lost",
                    ));
                }
                reconnects_left -= 1;
                tokio::time::sleep(DEEPGRAM_RECONNECT_BACKOFF).await;
            }
        }
    }
}

/// Installs the process-level rustls CryptoProvider (ring). The
/// `rustls-tls-webpki-roots` feature of tokio-tungstenite does not select a
/// crypto backend, and rustls panics on the first TLS handshake when none is
/// installed. Idempotent: a second call leaves the installed provider in place.
pub fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// Dials the streaming endpoint with the `Authorization: Token` header scheme
/// the batch path already used. The whole handshake is bounded by
/// `DEEPGRAM_CONNECT_DEADLINE` and observes cancellation on the poll tick, so
/// an abort never waits on a slow DNS/TLS dial: dropping the in-process
/// connect future cancels it without leaving anything to reap.
async fn deepgram_ws_connect(
    url: &str,
    credential: &Credential,
    cancel: &CancelRegistry,
) -> Result<DeepgramSocket, BoundaryError> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    let mut request = url.into_client_request().map_err(|_| {
        BoundaryError::new(BoundaryKind::Provider, "Deepgram streaming URL is invalid")
    })?;
    let token = format!("Token {}", credential.expose_to_boundary());
    request.headers_mut().insert(
        tokio_tungstenite::tungstenite::http::header::AUTHORIZATION,
        token.parse().map_err(|_| {
            BoundaryError::new(BoundaryKind::Provider, "Deepgram credential is not header-safe")
        })?,
    );
    let connect = tokio_tungstenite::connect_async(request);
    tokio::pin!(connect);
    let deadline = tokio::time::sleep(DEEPGRAM_CONNECT_DEADLINE);
    tokio::pin!(deadline);
    let mut ticks = tokio::time::interval(DEEPGRAM_CANCEL_POLL);
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            result = &mut connect => {
                let (socket, _response) = result.map_err(|_| {
                    BoundaryError::new(
                        BoundaryKind::Provider,
                        "Deepgram websocket connect failed",
                    )
                })?;
                return Ok(socket);
            }
            _ = &mut deadline => {
                return Err(BoundaryError::new(
                    BoundaryKind::Provider,
                    "Deepgram websocket connect deadline elapsed",
                ));
            }
            _ = ticks.tick() => {
                if cancel.is_cancelled() {
                    return Err(BoundaryError::new(
                        BoundaryKind::Provider,
                        "Deepgram websocket connect cancelled",
                    ));
                }
            }
        }
    }
}

/// Drives one websocket connection: forwards outbound audio/control frames,
/// ingests inbound `Results` into the accumulator, sends `KeepAlive` during
/// outbound gaps, and observes cancellation on a bounded tick. Marks
/// `audio_delivered` once any audio frame is accepted by the socket. Returns
/// `Err` for fatal failures (server-reported errors, malformed frames, an
/// unconfirmed CloseStream); transport drops return
/// `DeepgramConnectionEnd::Lost` and the caller decides whether a redial is
/// safe. A drain only Finishes when the server confirmed CloseStream with its
/// terminal summary `Metadata` before closing — Deepgram's contract is to
/// process remaining audio, return final results plus summary metadata, then
/// terminate; anything less may be a truncated Transcript.
#[allow(clippy::too_many_arguments)] // WS plumbing carries the full session context; fate tied to the Deepgram keep/delete decision
async fn drive_deepgram_connection(
    socket: DeepgramSocket,
    outbound: &mut tokio::sync::mpsc::UnboundedReceiver<DeepgramOutbound>,
    pending: &mut Option<DeepgramOutbound>,
    audio_delivered: &mut bool,
    transcript: &Arc<Mutex<TranscriptAccumulator>>,
    cancel: &CancelRegistry,
    keepalive: Duration,
    close_grace: Duration,
) -> Result<DeepgramConnectionEnd, BoundaryError> {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    let (mut sink, mut stream) = socket.split();
    let mut ticks = tokio::time::interval(DEEPGRAM_CANCEL_POLL);
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_sent = tokio::time::Instant::now();
    // Once `CloseStream` is out, stop consuming outbound frames and only
    // drain inbound until the server flushes final `Results`, confirms with
    // its terminal `Metadata`, and closes — bounded by the close grace.
    let mut draining_deadline: Option<tokio::time::Instant> = None;
    let mut terminal_metadata_seen = false;
    // A frame that failed to send on the previous connection is retried first
    // (only reachable before any audio was delivered — see the caller).
    if let Some(frame) = pending.take() {
        let is_audio = matches!(frame, DeepgramOutbound::Audio(_));
        let draining = matches!(frame, DeepgramOutbound::CloseStream);
        if sink.send(deepgram_ws_frame(&frame)).await.is_err() {
            *pending = Some(frame);
            return Ok(DeepgramConnectionEnd::Lost);
        }
        if is_audio {
            *audio_delivered = true;
        }
        last_sent = tokio::time::Instant::now();
        if draining {
            draining_deadline = Some(last_sent + close_grace);
        }
    }
    loop {
        tokio::select! {
            frame = outbound.recv(), if draining_deadline.is_none() => {
                let Some(frame) = frame else {
                    // Stream owner dropped without `complete()` (abort/Drop
                    // path): close this connection out and finish.
                    let _ = sink.send(Message::Close(None)).await;
                    return Ok(DeepgramConnectionEnd::Finished);
                };
                let is_audio = matches!(frame, DeepgramOutbound::Audio(_));
                let draining = matches!(frame, DeepgramOutbound::CloseStream);
                if sink.send(deepgram_ws_frame(&frame)).await.is_err() {
                    *pending = Some(frame);
                    return Ok(DeepgramConnectionEnd::Lost);
                }
                if is_audio {
                    *audio_delivered = true;
                }
                last_sent = tokio::time::Instant::now();
                if draining {
                    draining_deadline = Some(last_sent + close_grace);
                }
            }
            incoming = stream.next() => match incoming {
                Some(Ok(Message::Text(text))) => {
                    let kind = ingest_deepgram_message(transcript, &text)?;
                    if draining_deadline.is_some()
                        && matches!(kind, DeepgramMessageKind::Metadata)
                    {
                        terminal_metadata_seen = true;
                    }
                }
                Some(Ok(Message::Close(_))) | None => {
                    if draining_deadline.is_none() {
                        return Ok(DeepgramConnectionEnd::Lost);
                    }
                    if terminal_metadata_seen {
                        return Ok(DeepgramConnectionEnd::Finished);
                    }
                    // Closed after CloseStream but WITHOUT the terminal
                    // summary Metadata: the server-side flush is unconfirmed
                    // and the accumulated Transcript may be truncated.
                    return Err(BoundaryError::new(
                        BoundaryKind::Provider,
                        "Deepgram closed without confirming CloseStream",
                    ));
                }
                Some(Err(_)) => {
                    if draining_deadline.is_some() {
                        // The transport died between CloseStream and the
                        // server's flush: the final Results may be missing.
                        // Returning the partial accumulator here would
                        // silently truncate the Transcript — fail visibly and
                        // let the parallel Groq stream carry the Recording.
                        return Err(BoundaryError::new(
                            BoundaryKind::Provider,
                            "Deepgram streaming connection lost",
                        ));
                    }
                    return Ok(DeepgramConnectionEnd::Lost);
                }
                Some(Ok(_)) => {}
            },
            _ = ticks.tick() => {
                if cancel.is_cancelled() {
                    let _ = sink.send(Message::Close(None)).await;
                    return Ok(DeepgramConnectionEnd::Finished);
                }
                if let Some(deadline) = draining_deadline {
                    if tokio::time::Instant::now() >= deadline {
                        // Deepgram never confirmed CloseStream within the
                        // grace: the accumulated prefix would be a plausible
                        // but truncated Transcript, well inside the Provider
                        // Deadline — fail visibly instead.
                        let _ = sink.send(Message::Close(None)).await;
                        return Err(BoundaryError::new(
                            BoundaryKind::Provider,
                            "Deepgram did not confirm CloseStream within the close grace",
                        ));
                    }
                } else if last_sent.elapsed() >= keepalive {
                    if sink
                        .send(Message::Text(r#"{"type":"KeepAlive"}"#.to_owned()))
                        .await
                        .is_err()
                    {
                        return Ok(DeepgramConnectionEnd::Lost);
                    }
                    last_sent = tokio::time::Instant::now();
                }
            }
        }
    }
}

fn deepgram_ws_frame(frame: &DeepgramOutbound) -> tokio_tungstenite::tungstenite::Message {
    use tokio_tungstenite::tungstenite::Message;

    match frame {
        DeepgramOutbound::Audio(bytes) => Message::Binary(bytes.clone()),
        DeepgramOutbound::Finalize => Message::Text(r#"{"type":"Finalize"}"#.to_owned()),
        DeepgramOutbound::CloseStream => Message::Text(r#"{"type":"CloseStream"}"#.to_owned()),
    }
}

/// What one inbound text frame turned out to be, for the caller's
/// drain-confirmation tracking.
enum DeepgramMessageKind {
    /// The summary `Metadata` message — terminal when it follows CloseStream.
    Metadata,
    Other,
}

/// Parses one inbound text frame. `Results` feed the accumulator; a server
/// `Error` message, a frame that is not JSON, and a `Results` frame missing
/// its `is_final` marker or (when finalized) its transcript text are all
/// fatal — silently skipping them would truncate the Transcript without a
/// trace. Unknown-but-well-formed message types stay tolerated so server-side
/// schema ADDITIONS never break the Recording; interim shape drift is UI-only
/// and equally tolerated.
fn ingest_deepgram_message(
    transcript: &Arc<Mutex<TranscriptAccumulator>>,
    text: &str,
) -> Result<DeepgramMessageKind, BoundaryError> {
    let Ok(message) = serde_json::from_str::<serde_json::Value>(text) else {
        return Err(BoundaryError::new(
            BoundaryKind::Provider,
            "Deepgram sent a malformed streaming message",
        ));
    };
    match message.get("type").and_then(serde_json::Value::as_str) {
        Some("Error") => Err(BoundaryError::new(
            BoundaryKind::Provider,
            "Deepgram reported a streaming error",
        )),
        Some("Results") => {
            let Some(is_final) = message.get("is_final").and_then(serde_json::Value::as_bool)
            else {
                return Err(BoundaryError::new(
                    BoundaryKind::Provider,
                    "Deepgram sent a malformed streaming message",
                ));
            };
            if is_final
                && message
                    .pointer("/channel/alternatives/0/transcript")
                    .and_then(serde_json::Value::as_str)
                    .is_none()
            {
                return Err(BoundaryError::new(
                    BoundaryKind::Provider,
                    "Deepgram sent a malformed streaming message",
                ));
            }
            transcript
                .lock()
                .expect("Deepgram transcript accumulator mutex poisoned")
                .ingest(&message);
            Ok(DeepgramMessageKind::Other)
        }
        Some("Metadata") => Ok(DeepgramMessageKind::Metadata),
        _ => Ok(DeepgramMessageKind::Other),
    }
}

pub struct MergeResultValidator {
    pipeline: TranscriptDecisionPipeline<GroqReconciliationModel>,
}

impl MergeResultValidator {
    pub fn new() -> Self {
        Self {
            pipeline: TranscriptDecisionPipeline::new(
                GroqReconciliationModel,
                RECONCILIATION_DEADLINE,
            ),
        }
    }
}

impl Default for MergeResultValidator {
    fn default() -> Self {
        Self::new()
    }
}

impl TranscriptValidator for MergeResultValidator {
    fn validate(
        &mut self,
        sources: Vec<SourceTranscript>,
    ) -> BoundaryFuture<'_, TranscriptDecision> {
        self.pipeline.validate(sources)
    }
}

struct GroqReconciliationModel;

impl ReconciliationModel for GroqReconciliationModel {
    fn request(
        &mut self,
        kind: ReconciliationKind,
        sources: Vec<SourceTranscript>,
        candidate: Option<MergeResult>,
        cancel: Arc<CancelRegistry>,
    ) -> BoundaryFuture<'_, MergeResult> {
        Box::pin(async move {
            // The whole operation — including the potentially slow synchronous
            // Secret Service lookup — runs inside ONE owned blocking task, so
            // it never blocks the async thread and the pipeline can cancel it
            // as a unit. curl observes the cancel flag through its bounded
            // wait: on cancellation the child is killed and reaped by the same
            // loop that owns its handle, and this future completes only after
            // that cleanup, keeping the reap ordered before any fallback
            // becomes observable. The post-lookup check guarantees no curl is
            // spawned once the deadline has already cancelled the request.
            tokio::task::spawn_blocking(move || {
                let credential = SecretStore::load(&mut SecretToolStore, Provider::Groq)?;
                if cancel.is_cancelled() {
                    return Err(BoundaryError::new(
                        BoundaryKind::Validation,
                        "reconciliation request cancelled",
                    ));
                }
                request_groq_reconciliation(credential, kind, sources, candidate, &cancel)
            })
            .await
            .map_err(|_| {
                BoundaryError::new(BoundaryKind::Validation, "reconciliation request task failed")
            })?
        })
    }
}

fn request_groq_reconciliation(
    credential: Credential,
    kind: ReconciliationKind,
    sources: Vec<SourceTranscript>,
    candidate: Option<MergeResult>,
    cancel: &CancelRegistry,
) -> Result<MergeResult, BoundaryError> {
    let endpoint = std::env::var("VOISU_GROQ_RECONCILIATION_URL")
        .unwrap_or_else(|_| "https://api.groq.com/openai/v1/chat/completions".to_owned());
    if !provider_endpoint_is_secure(&endpoint) {
        return Err(BoundaryError::new(
            BoundaryKind::Validation,
            "Groq reconciliation endpoint must use HTTPS except on loopback",
        ));
    }
    let model = std::env::var("VOISU_GROQ_RECONCILIATION_MODEL")
        .unwrap_or_else(|_| "llama-3.3-70b-versatile".to_owned());
    if model.trim().is_empty() || model.contains(['\n', '\r']) {
        return Err(BoundaryError::new(
            BoundaryKind::Validation,
            "invalid Groq reconciliation model",
        ));
    }
    let source_text = sources
        .iter()
        .map(|source| format!("{}: {}", source.provider.cli_label(), source.text))
        .collect::<Vec<_>>()
        .join("\n");
    let task = match (kind, candidate) {
        (ReconciliationKind::Reconcile, _) => format!(
            "Reconcile these Source Transcripts. Return only the faithful final Transcript, with no labels, explanation, or added content.\n{source_text}"
        ),
        (ReconciliationKind::Repair, Some(candidate)) => format!(
            "Repair this unsafe candidate using only the Source Transcripts. Return only the faithful final Transcript, with no labels, explanation, or added content.\nCandidate: {}\n{source_text}",
            candidate.0
        ),
        (ReconciliationKind::Repair, None) => {
            return Err(BoundaryError::new(
                BoundaryKind::Validation,
                "reconciliation recovery omitted its candidate",
            ));
        }
    };
    let body = serde_json::json!({
        "model": model,
        "temperature": 0,
        "messages": [
            {
                "role": "system",
                "content": "You are Voisu's Transcript reconciliation model. Preserve spoken meaning and never add commentary, prompt text, or facts."
            },
            { "role": "user", "content": task }
        ]
    })
    .to_string();
    let config = format!(
        "url = \"{}\"\nheader = \"Authorization: Bearer {}\"\nheader = \"Content-Type: application/json\"\ndata = \"{}\"\n",
        curl_config_escape(&endpoint),
        curl_config_escape(credential.expose_to_boundary()),
        curl_config_escape(&body),
    );
    let outcome = run_restricted_with_deadline(
        "curl",
        &[
            "-q",
            "--config",
            "-",
            "--fail",
            "--silent",
            "--show-error",
            "--max-time",
            "2",
        ],
        Some(config.as_bytes()),
        true,
        RECONCILIATION_PROCESS_DEADLINE,
        Some(cancel),
    )
    .map_err(|error| match error {
        ProcessError::TimedOut => {
            BoundaryError::new(BoundaryKind::Validation, "reconciliation request deadline elapsed")
        }
        _ => BoundaryError::new(
            BoundaryKind::Validation,
            "Groq reconciliation request unavailable or failed",
        ),
    })?;
    if !outcome.success {
        return Err(BoundaryError::new(
            BoundaryKind::Validation,
            "Groq rejected the reconciliation request",
        ));
    }
    let response: serde_json::Value = serde_json::from_slice(&outcome.stdout).map_err(|_| {
        BoundaryError::new(BoundaryKind::Validation, "Groq reconciliation returned malformed JSON")
    })?;
    response
        .pointer("/choices/0/message/content")
        .and_then(|text| text.as_str())
        .map(|text| MergeResult(text.to_owned()))
        .ok_or_else(|| {
            BoundaryError::new(BoundaryKind::Validation, "Groq reconciliation omitted text")
        })
}

pub trait ClipboardBoundary: Send {
    fn preserve(&mut self, transcript: &Transcript) -> BoundaryFuture<'_, ()>;
}

pub trait DirectDeliverySession: Send {
    fn deliver_text(&mut self, text: &str) -> BoundaryFuture<'_, ()>;
}

pub trait RemoteDesktopPortal: Send {
    fn connect(&mut self) -> BoundaryFuture<'_, Box<dyn DirectDeliverySession>>;
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
                        &(
                            "Voisu",
                            0_u32,
                            "",
                            "Voisu",
                            body,
                            actions,
                            hints,
                            5_000_i32,
                        ),
                    )
                    .await
                    .map_err(|_| {
                        BoundaryError::new(
                            BoundaryKind::Delivery,
                            "desktop notification failed",
                        )
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
pub const FOCUS_GUARD_NOTIFICATION: &str =
    "focus changed — transcript preserved on the clipboard";

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
                eprintln!("focus guard: {FOCUS_GUARD_FALLBACK_REASON}; preserving Transcript on clipboard");
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
    session: Option<Box<dyn DirectDeliverySession>>,
    setup: Option<tokio::task::JoinHandle<Result<Box<dyn DirectDeliverySession>, BoundaryError>>>,
    setup_failure: Option<String>,
    setup_failure_terminal: bool,
    setup_retry_after: Option<Instant>,
    background_setup: bool,
}

const REMOTE_DESKTOP_RETRY_BACKOFF: Duration = Duration::from_secs(30);

impl PortalClipboardDelivery {
    pub fn with_boundaries(
        clipboard: Box<dyn ClipboardBoundary>,
        portal: Box<dyn RemoteDesktopPortal>,
    ) -> Self {
        Self {
            clipboard,
            portal,
            session: None,
            setup: None,
            setup_failure: None,
            setup_failure_terminal: false,
            setup_retry_after: None,
            background_setup: false,
        }
    }

    pub fn clipboard_only() -> Self {
        Self::with_boundaries(Box::new(WlClipboard), Box::new(DisabledRemoteDesktopPortal))
    }
}

impl DeliveryAdapter for PortalClipboardDelivery {
    fn deliver(&mut self, transcript: Transcript) -> BoundaryFuture<'_, DeliveryOutcome> {
        Box::pin(async move {
            // Clipboard preservation is the recoverability guarantee.
            // Compositor submission is never reported unless this succeeds.
            self.clipboard.preserve(&transcript).await?;

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

fn terminal_remote_desktop_failure(reason: &str) -> bool {
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
                .map_err(|_| {
                    BoundaryError::new(BoundaryKind::Delivery, "clipboard task failed")
                })?;
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

const REMOTE_DESKTOP_INTERFACE: &str = "org.freedesktop.portal.RemoteDesktop";
const KEYBOARD_DEVICE: u32 = 1;
const PERSIST_UNTIL_REVOKED: u32 = 2;
const MAX_RESTORE_TOKEN_BYTES: u64 = 4 * 1024;
static REMOTE_DESKTOP_TOKEN: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(1);
static RESTORE_TOKEN_TEMP: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(1);

fn restore_token_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("VOISU_REMOTE_DESKTOP_TOKEN_FILE") {
        let path = PathBuf::from(path);
        return path.is_absolute().then_some(path);
    }
    let state_root = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
                .map(|home| home.join(".local/state"))
        })?;
    Some(state_root.join("voisu/remote-desktop.restore-token"))
}

fn private_restore_token_file(path: &Path) -> Option<File> {
    let metadata = fs::symlink_metadata(path).ok()?;
    if !metadata.file_type().is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
        || metadata.len() > MAX_RESTORE_TOKEN_BYTES
    {
        return None;
    }
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .ok()
}

fn load_restore_token() -> Option<String> {
    let path = restore_token_path()?;
    let file = private_restore_token_file(&path)?;
    let mut token = String::new();
    file.take(MAX_RESTORE_TOKEN_BYTES + 1)
        .read_to_string(&mut token)
        .ok()?;
    (!token.is_empty() && token.len() as u64 <= MAX_RESTORE_TOKEN_BYTES).then_some(token)
}

fn persist_restore_token(token: &str) -> bool {
    if token.is_empty() || token.len() as u64 > MAX_RESTORE_TOKEN_BYTES {
        return false;
    }
    let Some(path) = restore_token_path() else {
        return false;
    };
    let Some(parent) = path.parent() else {
        return false;
    };
    if fs::create_dir_all(parent).is_err()
        || fs::set_permissions(parent, fs::Permissions::from_mode(0o700)).is_err()
    {
        return false;
    }
    let Ok(parent_metadata) = fs::symlink_metadata(parent) else {
        return false;
    };
    if !parent_metadata.file_type().is_dir()
        || parent_metadata.uid() != unsafe { libc::geteuid() }
        || parent_metadata.mode() & 0o077 != 0
    {
        return false;
    }
    let sequence = RESTORE_TOKEN_TEMP.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let temp = parent.join(format!(
        ".remote-desktop.restore-token.{}.{}.tmp",
        std::process::id(),
        sequence
    ));
    let written = (|| -> std::io::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&temp)?;
        file.write_all(token.as_bytes())?;
        file.sync_all()?;
        fs::rename(&temp, &path)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        Ok(())
    })();
    if written.is_err() {
        let _ = fs::remove_file(&temp);
        return false;
    }
    true
}

fn clear_restore_token() {
    if let Some(path) = restore_token_path() {
        let _ = fs::remove_file(path);
    }
}

pub struct FedoraRemoteDesktopPortal;

struct DisabledRemoteDesktopPortal;

impl RemoteDesktopPortal for DisabledRemoteDesktopPortal {
    fn connect(&mut self) -> BoundaryFuture<'_, Box<dyn DirectDeliverySession>> {
        Box::pin(async {
            Err(BoundaryError::new(
                BoundaryKind::Delivery,
                "direct Delivery disabled for this run",
            ))
        })
    }
}

impl RemoteDesktopPortal for FedoraRemoteDesktopPortal {
    fn connect(&mut self) -> BoundaryFuture<'_, Box<dyn DirectDeliverySession>> {
        Box::pin(async move {
            use std::sync::atomic::Ordering;
            use zbus::zvariant::Value;

            let connection = zbus::Connection::session().await.map_err(|_| {
                BoundaryError::new(BoundaryKind::Delivery, "RemoteDesktop portal unavailable")
            })?;
            let portal = zbus::Proxy::new(
                &connection,
                PORTAL_BUS_NAME,
                PORTAL_OBJECT_PATH,
                REMOTE_DESKTOP_INTERFACE,
            )
            .await
            .map_err(|_| {
                BoundaryError::new(BoundaryKind::Delivery, "RemoteDesktop portal unavailable")
            })?;

            let unique = REMOTE_DESKTOP_TOKEN.fetch_add(1, Ordering::Relaxed);
            let prefix = format!("voisu_delivery_{}_{}", std::process::id(), unique);
            let session_token = format!("{prefix}_session");
            let session_path = format!(
                "/org/freedesktop/portal/desktop/session/{}/{session_token}",
                escaped_sender(&connection).map_err(|_| BoundaryError::new(
                    BoundaryKind::Delivery,
                    "RemoteDesktop portal unavailable",
                ))?
            );
            let create_options: std::collections::HashMap<&str, Value<'_>> =
                std::collections::HashMap::from([
                    ("handle_token", Value::from(format!("{prefix}_create"))),
                    ("session_handle_token", Value::from(session_token.as_str())),
                ]);
            let create_results = portal_request(
                &connection,
                &portal,
                BoundaryKind::Delivery,
                "CreateSession",
                &(create_options,),
                PORTAL_SESSION_DEADLINE,
            )
            .await
            .map_err(classify_remote_desktop_failure)?;
            let session_path = session_handle_from(&create_results).unwrap_or(session_path);
            let session_object: zbus::zvariant::OwnedObjectPath =
                zbus::zvariant::ObjectPath::try_from(session_path.as_str())
                    .map_err(|_| BoundaryError::new(BoundaryKind::Delivery, "permission denied"))?
                    .into();
            let session_proxy = zbus::Proxy::new(
                &connection,
                PORTAL_BUS_NAME,
                session_path.clone(),
                PORTAL_SESSION_INTERFACE,
            )
            .await
            .map_err(|_| BoundaryError::new(BoundaryKind::Delivery, "permission denied"))?;
            let closures = session_proxy.receive_signal("Closed").await.map_err(|_| {
                BoundaryError::new(BoundaryKind::Delivery, "permission denied")
            })?;

            let restore_token = load_restore_token();
            let mut select_options: std::collections::HashMap<&str, Value<'_>> =
                std::collections::HashMap::from([
                    ("handle_token", Value::from(format!("{prefix}_select"))),
                    ("types", Value::from(KEYBOARD_DEVICE)),
                    ("persist_mode", Value::from(PERSIST_UNTIL_REVOKED)),
                ]);
            if let Some(token) = restore_token.as_deref() {
                select_options.insert("restore_token", Value::from(token));
            }
            if let Err(error) = portal_request(
                &connection,
                &portal,
                BoundaryKind::Delivery,
                "SelectDevices",
                &(session_object.clone(), select_options),
                PORTAL_BIND_DEADLINE,
            )
            .await
            {
                return Err(fail_and_close(&connection, session_path.as_str(), error).await);
            }

            let start_options: std::collections::HashMap<&str, Value<'_>> =
                std::collections::HashMap::from([(
                    "handle_token",
                    Value::from(format!("{prefix}_start")),
                )]);
            let started = match portal_request(
                &connection,
                &portal,
                BoundaryKind::Delivery,
                "Start",
                &(session_object.clone(), "", start_options),
                PORTAL_BIND_DEADLINE,
            )
            .await
            {
                Ok(results) => results,
                Err(error) => {
                    return Err(fail_and_close(&connection, session_path.as_str(), error).await);
                }
            };
            if let Some(token) = started
                .get("restore_token")
                .and_then(|value| value.downcast_ref::<zbus::zvariant::Str<'_>>().ok())
            {
                let _ = persist_restore_token(token.as_str());
            } else if restore_token.is_some() {
                // Restore tokens are single-use. If Start did not rotate the
                // supplied token, retaining it would guarantee a stale retry.
                clear_restore_token();
            }
            let devices = started
                .get("devices")
                .and_then(|value| value.downcast_ref::<u32>().ok())
                .unwrap_or(0);
            if devices & KEYBOARD_DEVICE == 0 {
                close_portal_session(&connection, session_path.as_str()).await;
                return Err(BoundaryError::new(
                    BoundaryKind::Delivery,
                    "keyboard permission unavailable",
                ));
            }

            let options: std::collections::HashMap<&str, Value<'_>> =
                std::collections::HashMap::new();
            let reply = portal
                .call_method("ConnectToEIS", &(session_object.clone(), options))
                .await
                .map_err(|_| {
                    BoundaryError::new(BoundaryKind::Delivery, "libei connection unavailable")
                })?;
            let fd: zbus::zvariant::OwnedFd = reply.body().deserialize().map_err(|_| {
                BoundaryError::new(BoundaryKind::Delivery, "libei connection unavailable")
            })?;
            let fd: std::os::fd::OwnedFd = fd.into();
            let sender_result = tokio::task::spawn_blocking(move || {
                NativeEiSender::connect(fd.into_raw_fd())
            })
            .await
            .map_err(|_| {
                BoundaryError::new(BoundaryKind::Delivery, "libei connection unavailable")
            })?;
            let sender = match sender_result {
                Ok(sender) => sender,
                Err(error) => {
                    close_portal_session(&connection, session_path.as_str()).await;
                    return Err(error);
                }
            };

            Ok(Box::new(FedoraDirectDeliverySession {
                connection,
                session_path: session_object,
                closures,
                sender: Some(sender),
            }) as Box<dyn DirectDeliverySession>)
        })
    }
}

fn classify_remote_desktop_failure(error: BoundaryError) -> BoundaryError {
    let reason = if error.diagnostic().contains("denied or cancelled") {
        "permission denied"
    } else {
        "RemoteDesktop portal unavailable"
    };
    BoundaryError::new(BoundaryKind::Delivery, reason)
}

async fn fail_and_close(
    connection: &zbus::Connection,
    session_path: &str,
    error: BoundaryError,
) -> BoundaryError {
    close_portal_session(connection, session_path).await;
    let error = classify_remote_desktop_failure(error);
    if terminal_remote_desktop_failure(error.diagnostic()) {
        clear_restore_token();
    }
    error
}

struct FedoraDirectDeliverySession {
    connection: zbus::Connection,
    session_path: zbus::zvariant::OwnedObjectPath,
    closures: zbus::proxy::SignalStream<'static>,
    sender: Option<NativeEiSender>,
}

impl DirectDeliverySession for FedoraDirectDeliverySession {
    fn deliver_text(&mut self, text: &str) -> BoundaryFuture<'_, ()> {
        let text = text.to_owned();
        Box::pin(async move {
            use zbus::export::ordered_stream::OrderedStreamExt;
            if matches!(
                tokio::time::timeout(Duration::from_millis(1), self.closures.next()).await,
                Ok(Some(_))
            ) {
                clear_restore_token();
                return Err(BoundaryError::new(BoundaryKind::Delivery, "permission revoked"));
            }
            let mut sender = self.sender.take().ok_or_else(|| {
                BoundaryError::new(BoundaryKind::Delivery, "libei disconnected")
            })?;
            let (returned, result) = tokio::task::spawn_blocking(move || {
                let result = sender.deliver(&text);
                (sender, result)
            })
            .await
            .map_err(|_| BoundaryError::new(BoundaryKind::Delivery, "libei disconnected"))?;
            self.sender = Some(returned);
            result
        })
    }
}

impl Drop for FedoraDirectDeliverySession {
    fn drop(&mut self) {
        let connection = self.connection.clone();
        let path = self.session_path.to_string();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move { close_portal_session(&connection, &path).await });
        }
    }
}

#[repr(C)]
struct EiContext {
    _private: [u8; 0],
}
#[repr(C)]
struct EiEvent {
    _private: [u8; 0],
}
#[repr(C)]
struct EiSeat {
    _private: [u8; 0],
}
#[repr(C)]
struct EiDevice {
    _private: [u8; 0],
}
#[repr(C)]
struct EiPing {
    _private: [u8; 0],
}
#[repr(C)]
struct EiKeymap {
    _private: [u8; 0],
}

type SeatBindCapabilities = unsafe extern "C" fn(*mut EiSeat, ...);

// libei protocol constants, verbatim from libei.h (verified against the
// libei-devel 1.5.0 header shipped for the host runtime):
//   enum ei_event_type { EI_EVENT_CONNECT = 1, EI_EVENT_DISCONNECT,
//     EI_EVENT_SEAT_ADDED, EI_EVENT_SEAT_REMOVED, EI_EVENT_DEVICE_ADDED,
//     EI_EVENT_DEVICE_REMOVED, EI_EVENT_DEVICE_PAUSED, EI_EVENT_DEVICE_RESUMED,
//     ..., EI_EVENT_PONG = 90, ... }
//   enum ei_device_capability { EI_DEVICE_CAP_POINTER = (1 << 0), ...,
//     EI_DEVICE_CAP_BUTTON = (1 << 5) }
const EI_EVENT_DISCONNECT: libc::c_int = 2;
const EI_EVENT_SEAT_ADDED: libc::c_int = 3;
const EI_EVENT_SEAT_REMOVED: libc::c_int = 4;
const EI_EVENT_DEVICE_ADDED: libc::c_int = 5;
const EI_EVENT_DEVICE_REMOVED: libc::c_int = 6;
const EI_EVENT_DEVICE_PAUSED: libc::c_int = 7;
const EI_EVENT_DEVICE_RESUMED: libc::c_int = 8;
const EI_EVENT_PONG: libc::c_int = 90;
// The text capability follows the header's bitmask progression
// (EI_DEVICE_CAP_BUTTON = 1 << 5 is the last capability in 1.5); it ships with
// the libei release that provides ei_device_text_utf8_with_length, which
// EiApi::load requires before this constant is ever used.
const EI_CAP_KEYBOARD: libc::c_int = 1 << 2;
const EI_CAP_TEXT: libc::c_int = 1 << 6;
const EI_EVENT_KEYBOARD_MODIFIERS: libc::c_int = 9;

/// Binds the text capability on a seat through the variadic
/// `ei_seat_bind_capabilities`. The header requires the capability list to be
/// "terminated by ``NULL``" — a pointer-width sentinel. Terminating with an
/// integer (e.g. `-1_i32`) is undefined behavior on ABIs where int and pointer
/// widths differ, so the sentinel is passed as an explicit null pointer.
fn bind_capability(api: &EiApi, seat: *mut EiSeat, capability: libc::c_int) {
    // SAFETY: `seat` is a live pointer obtained from the event currently being
    // processed; the variadic call passes one capability (promoted to int, as
    // C callers do) followed by the documented NULL terminator.
    unsafe {
        (api.seat_bind_capabilities)(seat, capability, std::ptr::null_mut::<libc::c_void>())
    };
}

/// What the connect loop should do in response to one libei event, decided by
/// the pure [`EiDeviceLink`] state machine so the protocol handling is
/// testable without a native EIS server.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EiLinkDirective {
    /// Bind the text capability on the seat carried by this event.
    BindCapability,
    /// Adopt the device carried by this event as the delivery device.
    AdoptDevice,
    /// Nothing to do for this event.
    Continue,
    /// The link is unusable; fail with this reason.
    Fail(&'static str),
}

/// One libei event as seen by the pure state machines. `ours` marks whether
/// the event's device (or ping) is the one this sender adopted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EiLinkEvent {
    SeatAddedWithText,
    SeatAddedWithKeyboard,
    SeatRemoved { ours: bool },
    DeviceAddedWithText,
    DeviceAddedWithKeyboard,
    DeviceResumed { ours: bool },
    DevicePaused { ours: bool },
    DeviceRemoved { ours: bool },
    Disconnect,
    Pong { ours: bool },
    KeyboardGroup { ours: bool, group: u32 },
    Other,
}

/// The device readiness state machine for a libei sender link.
///
/// libei semantics (libei.h): EI_EVENT_DEVICE_ADDED only announces the device;
/// events sent before EI_EVENT_DEVICE_RESUMED ("The client may send events")
/// are not permitted, and after EI_EVENT_DEVICE_PAUSED "any events sent from
/// this device will be discarded until the next resume". A removed device or
/// seat, or a disconnect, invalidates the link entirely.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct EiDeviceLink {
    adopted: bool,
    resumed: bool,
    keyboard_group: u32,
}

impl EiDeviceLink {
    fn observe(&mut self, event: EiLinkEvent) -> EiLinkDirective {
        match event {
            EiLinkEvent::SeatAddedWithText | EiLinkEvent::SeatAddedWithKeyboard => {
                EiLinkDirective::BindCapability
            }
            EiLinkEvent::DeviceAddedWithText | EiLinkEvent::DeviceAddedWithKeyboard
                if !self.adopted =>
            {
                self.adopted = true;
                EiLinkDirective::AdoptDevice
            }
            EiLinkEvent::DeviceResumed { ours: true } => {
                self.resumed = true;
                EiLinkDirective::Continue
            }
            EiLinkEvent::DevicePaused { ours: true } => {
                self.resumed = false;
                EiLinkDirective::Continue
            }
            EiLinkEvent::DeviceRemoved { ours: true } | EiLinkEvent::SeatRemoved { ours: true } => {
                self.adopted = false;
                self.resumed = false;
                EiLinkDirective::Fail("libei disconnected")
            }
            EiLinkEvent::Disconnect => {
                self.adopted = false;
                self.resumed = false;
                EiLinkDirective::Fail("libei disconnected")
            }
            EiLinkEvent::KeyboardGroup { ours: true, group } => {
                self.keyboard_group = group;
                EiLinkDirective::Continue
            }
            _ => EiLinkDirective::Continue,
        }
    }

    /// The device may emulate events only when it is adopted AND resumed.
    fn ready(&self) -> bool {
        self.adopted && self.resumed
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EiDeliveryMode {
    Text,
    KeyboardPaste,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct KeyboardPasteKeys {
    control: u32,
    paste: u32,
}

fn resolve_keyboard_paste_keys(
    keymap_text: String,
    group: u32,
) -> Result<KeyboardPasteKeys, BoundaryError> {
    use xkbcommon::xkb;

    let context = xkb::Context::new(xkb::CONTEXT_NO_ENVIRONMENT_NAMES);
    let keymap = xkb::Keymap::new_from_string(
        &context,
        keymap_text,
        xkb::KEYMAP_FORMAT_TEXT_V1,
        xkb::KEYMAP_COMPILE_NO_FLAGS,
    )
    .ok_or_else(|| {
        BoundaryError::new(BoundaryKind::Delivery, "active keyboard layout unavailable")
    })?;
    let find = |target: xkb::Keysym| -> Option<u32> {
        (keymap.min_keycode().raw()..=keymap.max_keycode().raw()).find_map(|raw| {
            let key = xkb::Keycode::new(raw);
            (keymap.key_get_syms_by_level(key, group, 0) == [target])
                .then(|| raw.checked_sub(8))
                .flatten()
        })
    };
    let control = find(xkb::Keysym::from(xkb::keysyms::KEY_Control_L))
        .or_else(|| find(xkb::Keysym::from(xkb::keysyms::KEY_Control_R)))
        .ok_or_else(|| {
            BoundaryError::new(BoundaryKind::Delivery, "active keyboard layout unavailable")
        })?;
    let paste = find(xkb::Keysym::from(xkb::keysyms::KEY_v)).ok_or_else(|| {
        BoundaryError::new(BoundaryKind::Delivery, "active keyboard layout unavailable")
    })?;
    Ok(KeyboardPasteKeys { control, paste })
}

fn libei_text_buffer(text: &str) -> Result<CString, BoundaryError> {
    CString::new(text).map_err(|_| {
        BoundaryError::new(
            BoundaryKind::Delivery,
            "Transcript contains an unsupported NUL character",
        )
    })
}

/// Confirmation state for one delivery roundtrip.
///
/// libei semantics (libei.h, ei_ping): "If the client is disconnected before
/// the roundtrip is complete, libei will emulate a @ref EI_EVENT_PONG event
/// before @ref EI_EVENT_DISCONNECT." A matching PONG therefore proves nothing
/// on its own — the already-queued events behind it must be drained, and a
/// queued DISCONNECT (or loss of the delivery device) converts the synthetic
/// PONG into a failure. Only a matching PONG followed by an exhausted event
/// queue confirms the delivery.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct EiDeliveryConfirmation {
    pong_matched: bool,
    failure: Option<&'static str>,
}

impl EiDeliveryConfirmation {
    fn observe(&mut self, event: EiLinkEvent) {
        if self.failure.is_some() {
            return;
        }
        match event {
            EiLinkEvent::Pong { ours: true } => self.pong_matched = true,
            EiLinkEvent::Disconnect => {
                self.failure = Some("libei disconnected during compositor submission");
            }
            EiLinkEvent::DeviceRemoved { ours: true }
            | EiLinkEvent::SeatRemoved { ours: true } => {
                self.failure = Some("libei disconnected");
            }
            EiLinkEvent::DevicePaused { ours: true } if !self.pong_matched => {
                // The pause discards not-yet-processed events; a pong that has
                // not arrived yet cannot vouch for events sent before it.
                self.failure = Some("compositor rejected libei submission");
            }
            _ => {}
        }
    }

    /// The verdict once the currently queued events are exhausted: a failure
    /// wins over a matched pong (the synthetic-PONG-before-DISCONNECT case);
    /// a matched pong with a clean queue confirms; otherwise keep waiting.
    fn verdict(&self) -> Option<Result<(), &'static str>> {
        if let Some(reason) = self.failure {
            return Some(Err(reason));
        }
        if self.pong_matched {
            return Some(Ok(()));
        }
        None
    }
}

struct EiApi {
    library: *mut libc::c_void,
    new_sender: unsafe extern "C" fn(*mut libc::c_void) -> *mut EiContext,
    configure_name: unsafe extern "C" fn(*mut EiContext, *const libc::c_char),
    setup_backend_fd: unsafe extern "C" fn(*mut EiContext, libc::c_int) -> libc::c_int,
    get_fd: unsafe extern "C" fn(*mut EiContext) -> libc::c_int,
    dispatch: unsafe extern "C" fn(*mut EiContext),
    get_event: unsafe extern "C" fn(*mut EiContext) -> *mut EiEvent,
    event_type: unsafe extern "C" fn(*mut EiEvent) -> libc::c_int,
    event_device: unsafe extern "C" fn(*mut EiEvent) -> *mut EiDevice,
    event_seat: unsafe extern "C" fn(*mut EiEvent) -> *mut EiSeat,
    event_unref: unsafe extern "C" fn(*mut EiEvent) -> *mut EiEvent,
    seat_has_capability: unsafe extern "C" fn(*mut EiSeat, libc::c_int) -> bool,
    seat_bind_capabilities: SeatBindCapabilities,
    device_has_capability: unsafe extern "C" fn(*mut EiDevice, libc::c_int) -> bool,
    device_keyboard_get_keymap: unsafe extern "C" fn(*mut EiDevice) -> *mut EiKeymap,
    keymap_get_fd: unsafe extern "C" fn(*mut EiKeymap) -> libc::c_int,
    keymap_get_size: unsafe extern "C" fn(*mut EiKeymap) -> usize,
    device_ref: unsafe extern "C" fn(*mut EiDevice) -> *mut EiDevice,
    device_unref: unsafe extern "C" fn(*mut EiDevice) -> *mut EiDevice,
    start_emulating: unsafe extern "C" fn(*mut EiDevice, u32),
    keyboard_key: unsafe extern "C" fn(*mut EiDevice, u32, bool),
    text_utf8: Option<unsafe extern "C" fn(*mut EiDevice, *const libc::c_char, usize)>,
    frame: unsafe extern "C" fn(*mut EiDevice, u64),
    stop_emulating: unsafe extern "C" fn(*mut EiDevice),
    now: unsafe extern "C" fn(*mut EiContext) -> u64,
    new_ping: unsafe extern "C" fn(*mut EiContext) -> *mut EiPing,
    ping: unsafe extern "C" fn(*mut EiPing),
    ping_get_id: unsafe extern "C" fn(*mut EiPing) -> u64,
    ping_unref: unsafe extern "C" fn(*mut EiPing) -> *mut EiPing,
    event_pong_get_ping: unsafe extern "C" fn(*mut EiEvent) -> *mut EiPing,
    event_keyboard_get_xkb_group: unsafe extern "C" fn(*mut EiEvent) -> u32,
    disconnect: unsafe extern "C" fn(*mut EiContext),
    context_unref: unsafe extern "C" fn(*mut EiContext) -> *mut EiContext,
}

// The loaded libei objects are owned exclusively by one spawn_blocking task at
// a time. No pointer is ever accessed concurrently.
unsafe impl Send for EiApi {}

impl EiApi {
    fn load() -> Result<Self, BoundaryError> {
        unsafe fn symbol<T: Copy>(
            library: *mut libc::c_void,
            name: &'static [u8],
        ) -> Result<T, BoundaryError> {
            // SAFETY: every name is NUL-terminated and each T below is the exact
            // C ABI function-pointer type documented by libei.
            let pointer = unsafe { libc::dlsym(library, name.as_ptr().cast()) };
            if pointer.is_null() {
                return Err(BoundaryError::new(
                    BoundaryKind::Delivery,
                    "libei capability unavailable",
                ));
            }
            // SAFETY: function pointers and dlsym pointers have pointer size on
            // the supported Fedora target; T is Copy and contains no references.
            Ok(unsafe { std::mem::transmute_copy(&pointer) })
        }

        unsafe fn optional_symbol<T: Copy>(library: *mut libc::c_void, name: &'static [u8]) -> Option<T> {
            let pointer = unsafe { libc::dlsym(library, name.as_ptr().cast()) };
            (!pointer.is_null()).then(|| unsafe { std::mem::transmute_copy(&pointer) })
        }

        // Load by SONAME so the build does not require libei-devel or a linker
        // symlink; Fedora's portal stack provides the runtime library.
        let library = unsafe { libc::dlopen(c"libei.so.1".as_ptr(), libc::RTLD_NOW) };
        if library.is_null() {
            return Err(BoundaryError::new(
                BoundaryKind::Delivery,
                "libei connection unavailable",
            ));
        }
        let loaded = unsafe {
            (|| {
                Ok(Self {
                    library,
                    new_sender: symbol(library, b"ei_new_sender\0")?,
                    configure_name: symbol(library, b"ei_configure_name\0")?,
                    setup_backend_fd: symbol(library, b"ei_setup_backend_fd\0")?,
                    get_fd: symbol(library, b"ei_get_fd\0")?,
                    dispatch: symbol(library, b"ei_dispatch\0")?,
                    get_event: symbol(library, b"ei_get_event\0")?,
                    event_type: symbol(library, b"ei_event_get_type\0")?,
                    event_device: symbol(library, b"ei_event_get_device\0")?,
                    event_seat: symbol(library, b"ei_event_get_seat\0")?,
                    event_unref: symbol(library, b"ei_event_unref\0")?,
                    seat_has_capability: symbol(library, b"ei_seat_has_capability\0")?,
                    seat_bind_capabilities: symbol(library, b"ei_seat_bind_capabilities\0")?,
                    device_has_capability: symbol(library, b"ei_device_has_capability\0")?,
                    device_keyboard_get_keymap: symbol(
                        library,
                        b"ei_device_keyboard_get_keymap\0",
                    )?,
                    keymap_get_fd: symbol(library, b"ei_keymap_get_fd\0")?,
                    keymap_get_size: symbol(library, b"ei_keymap_get_size\0")?,
                    device_ref: symbol(library, b"ei_device_ref\0")?,
                    device_unref: symbol(library, b"ei_device_unref\0")?,
                    start_emulating: symbol(library, b"ei_device_start_emulating\0")?,
                    keyboard_key: symbol(library, b"ei_device_keyboard_key\0")?,
                    text_utf8: optional_symbol(
                        library,
                        b"ei_device_text_utf8_with_length\0",
                    ),
                    frame: symbol(library, b"ei_device_frame\0")?,
                    stop_emulating: symbol(library, b"ei_device_stop_emulating\0")?,
                    now: symbol(library, b"ei_now\0")?,
                    new_ping: symbol(library, b"ei_new_ping\0")?,
                    ping: symbol(library, b"ei_ping\0")?,
                    ping_get_id: symbol(library, b"ei_ping_get_id\0")?,
                    ping_unref: symbol(library, b"ei_ping_unref\0")?,
                    event_pong_get_ping: symbol(library, b"ei_event_pong_get_ping\0")?,
                    event_keyboard_get_xkb_group: symbol(
                        library,
                        b"ei_event_keyboard_get_xkb_group\0",
                    )?,
                    disconnect: symbol(library, b"ei_disconnect\0")?,
                    context_unref: symbol(library, b"ei_unref\0")?,
                })
            })()
        };
        if loaded.is_err() {
            unsafe { libc::dlclose(library) };
        }
        loaded
    }
}

impl Drop for EiApi {
    fn drop(&mut self) {
        unsafe { libc::dlclose(self.library) };
    }
}

fn keyboard_keymap_text(api: &EiApi, device: *mut EiDevice) -> Result<String, BoundaryError> {
    let keymap = unsafe { (api.device_keyboard_get_keymap)(device) };
    if keymap.is_null() {
        return Err(BoundaryError::new(
            BoundaryKind::Delivery,
            "active keyboard layout unavailable",
        ));
    }
    let size = unsafe { (api.keymap_get_size)(keymap) };
    if size == 0 || size > 1024 * 1024 {
        return Err(BoundaryError::new(
            BoundaryKind::Delivery,
            "active keyboard layout unavailable",
        ));
    }
    let fd = unsafe { (api.keymap_get_fd)(keymap) };
    let owned_fd = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
    if owned_fd < 0 {
        return Err(BoundaryError::new(
            BoundaryKind::Delivery,
            "active keyboard layout unavailable",
        ));
    }
    // The dup owns the descriptor so every exit closes it.
    let file = unsafe { File::from_raw_fd(owned_fd) };
    read_keymap_fd(file.as_raw_fd(), size)
}

/// Reads a compiled XKB keymap of `size` bytes from an EIS keymap descriptor.
///
/// The keymap descriptor is a shared open file description whose offset is NOT
/// guaranteed to sit at the start: a compositor that populates the backing
/// memfd with `write()` leaves the offset at the end, and `F_DUPFD_CLOEXEC`
/// shares that offset rather than resetting it — as does any earlier read of
/// the same keymap. Reading through the ordinary file cursor therefore yielded
/// zero bytes, and libxkbcommon rejected the resulting empty string
/// (`[XKB-822] Failed to parse input xkb string`), stranding every Delivery on
/// the clipboard fallback. `pread` reads from absolute offset 0, so it neither
/// depends on nor mutates the shared offset. This mirrors the mmap-based
/// consumption the wl_keyboard/EIS keymap convention expects.
fn read_keymap_fd(fd: libc::c_int, size: usize) -> Result<String, BoundaryError> {
    let unavailable =
        || BoundaryError::new(BoundaryKind::Delivery, "active keyboard layout unavailable");
    let mut bytes = vec![0u8; size];
    let mut filled = 0usize;
    while filled < size {
        // SAFETY: `fd` is live for the call and the destination is an owned
        // buffer with at least `size - filled` bytes remaining at `filled`.
        let read = unsafe {
            libc::pread(
                fd,
                bytes[filled..].as_mut_ptr().cast(),
                size - filled,
                filled as libc::off_t,
            )
        };
        if read < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(unavailable());
        }
        if read == 0 {
            // Short descriptor: the advertised size overstates the payload.
            break;
        }
        filled += read as usize;
    }
    bytes.truncate(filled);
    // The convention advertises the keymap size including its terminating NUL,
    // which the compiler must not see.
    if bytes.last() == Some(&0) {
        bytes.pop();
    }
    String::from_utf8(bytes).map_err(|_| unavailable())
}

struct NativeEiSender {
    api: EiApi,
    context: *mut EiContext,
    device: *mut EiDevice,
    link: EiDeviceLink,
    mode: EiDeliveryMode,
    sequence: u32,
}

unsafe impl Send for NativeEiSender {}

/// RAII cleanup for a libei context (and an adopted device reference) while a
/// connection attempt is in flight: every early exit — poll failure, protocol
/// failure, deadline — disconnects and releases the native objects. Disarmed
/// exactly once, on successful handoff into [`NativeEiSender`].
struct EiConnectGuard<'a> {
    api: &'a EiApi,
    context: *mut EiContext,
    device: *mut EiDevice,
    armed: bool,
}

impl Drop for EiConnectGuard<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // SAFETY: the guard exclusively owns these pointers until disarmed;
        // device (when adopted) holds the reference taken via device_ref.
        unsafe {
            if !self.device.is_null() {
                (self.api.device_unref)(self.device);
            }
            (self.api.disconnect)(self.context);
            (self.api.context_unref)(self.context);
        }
    }
}

impl NativeEiSender {
    fn connect(fd: libc::c_int) -> Result<Self, BoundaryError> {
        let api = match EiApi::load() {
            Ok(api) => api,
            Err(error) => {
                unsafe { libc::close(fd) };
                return Err(error);
            }
        };
        let context = unsafe { (api.new_sender)(std::ptr::null_mut()) };
        if context.is_null() {
            unsafe { libc::close(fd) };
            return Err(BoundaryError::new(
                BoundaryKind::Delivery,
                "libei connection unavailable",
            ));
        }
        unsafe { (api.configure_name)(context, c"Voisu Delivery".as_ptr()) };
        if unsafe { (api.setup_backend_fd)(context, fd) } != 0 {
            unsafe { (api.context_unref)(context) };
            return Err(BoundaryError::new(
                BoundaryKind::Delivery,
                "libei connection unavailable",
            ));
        }

        let mut guard = EiConnectGuard {
            api: &api,
            context,
            device: std::ptr::null_mut(),
            armed: true,
        };
        let mut link = EiDeviceLink::default();
        let mut mode = None;
        let deadline = Instant::now() + LIBEI_DELIVERY_DEADLINE;
        // The device may emulate only after EI_EVENT_DEVICE_RESUMED; an added
        // device alone is not ready (libei.h).
        while Instant::now() < deadline && !link.ready() {
            poll_libei(guard.api, context, deadline)?;
            loop {
                let event = unsafe { (guard.api.get_event)(context) };
                if event.is_null() {
                    break;
                }
                let event_type = unsafe { (guard.api.event_type)(event) };
                // Pointers are read before the event is released.
                let mut event_seat: *mut EiSeat = std::ptr::null_mut();
                let mut event_device: *mut EiDevice = std::ptr::null_mut();
                let link_event = match event_type {
                    EI_EVENT_SEAT_ADDED => {
                        event_seat = unsafe { (guard.api.event_seat)(event) };
                        if guard.api.text_utf8.is_some()
                            && !event_seat.is_null()
                            && unsafe { (guard.api.seat_has_capability)(event_seat, EI_CAP_TEXT) }
                        {
                            EiLinkEvent::SeatAddedWithText
                        } else if !event_seat.is_null()
                            && unsafe {
                                (guard.api.seat_has_capability)(event_seat, EI_CAP_KEYBOARD)
                            }
                        {
                            EiLinkEvent::SeatAddedWithKeyboard
                        } else {
                            EiLinkEvent::Other
                        }
                    }
                    EI_EVENT_SEAT_REMOVED => EiLinkEvent::SeatRemoved {
                        // Conservative: once a device is adopted, any seat
                        // removal invalidates the link and falls back.
                        ours: !guard.device.is_null(),
                    },
                    EI_EVENT_DEVICE_ADDED => {
                        event_device = unsafe { (guard.api.event_device)(event) };
                        if guard.api.text_utf8.is_some()
                            && !event_device.is_null()
                            && unsafe {
                                (guard.api.device_has_capability)(event_device, EI_CAP_TEXT)
                            }
                        {
                            EiLinkEvent::DeviceAddedWithText
                        } else if !event_device.is_null()
                            && unsafe {
                                (guard.api.device_has_capability)(event_device, EI_CAP_KEYBOARD)
                            }
                        {
                            EiLinkEvent::DeviceAddedWithKeyboard
                        } else {
                            EiLinkEvent::Other
                        }
                    }
                    EI_EVENT_DEVICE_RESUMED => EiLinkEvent::DeviceResumed {
                        ours: Self::event_is_for_device(&api, event, guard.device),
                    },
                    EI_EVENT_DEVICE_PAUSED => EiLinkEvent::DevicePaused {
                        ours: Self::event_is_for_device(&api, event, guard.device),
                    },
                    EI_EVENT_DEVICE_REMOVED => EiLinkEvent::DeviceRemoved {
                        ours: Self::event_is_for_device(&api, event, guard.device),
                    },
                    EI_EVENT_KEYBOARD_MODIFIERS => EiLinkEvent::KeyboardGroup {
                        ours: Self::event_is_for_device(&api, event, guard.device),
                        group: unsafe { (guard.api.event_keyboard_get_xkb_group)(event) },
                    },
                    EI_EVENT_DISCONNECT => EiLinkEvent::Disconnect,
                    _ => EiLinkEvent::Other,
                };
                let directive = link.observe(link_event);
                match directive {
                    EiLinkDirective::BindCapability => {
                        let capability = if link_event == EiLinkEvent::SeatAddedWithText {
                            EI_CAP_TEXT
                        } else {
                            EI_CAP_KEYBOARD
                        };
                        bind_capability(&api, event_seat, capability);
                    }
                    EiLinkDirective::AdoptDevice => {
                        guard.device = unsafe { (guard.api.device_ref)(event_device) };
                        if guard.device.is_null() {
                            unsafe { (guard.api.event_unref)(event) };
                            return Err(BoundaryError::new(
                                BoundaryKind::Delivery,
                                "libei connection unavailable",
                            ));
                        }
                        mode = Some(if link_event == EiLinkEvent::DeviceAddedWithText {
                            EiDeliveryMode::Text
                        } else {
                            EiDeliveryMode::KeyboardPaste
                        });
                    }
                    EiLinkDirective::Continue => {}
                    EiLinkDirective::Fail(_) => {}
                }
                unsafe { (guard.api.event_unref)(event) };
                if let EiLinkDirective::Fail(reason) = directive {
                    return Err(BoundaryError::new(BoundaryKind::Delivery, reason));
                }
            }
        }
        if !link.ready() {
            return Err(BoundaryError::new(
                BoundaryKind::Delivery,
                "text or keyboard capability unavailable",
            ));
        }
        let mode = mode.ok_or_else(|| {
            BoundaryError::new(
                BoundaryKind::Delivery,
                "text or keyboard capability unavailable",
            )
        })?;
        guard.armed = false;
        let device = guard.device;
        drop(guard);
        Ok(Self {
            api,
            context,
            device,
            link,
            mode,
            sequence: 0,
        })
    }

    /// Whether this event's device is the adopted delivery device (pointer
    /// identity; libei keeps one struct per device for the context lifetime).
    fn event_is_for_device(api: &EiApi, event: *mut EiEvent, device: *mut EiDevice) -> bool {
        if device.is_null() {
            return false;
        }
        (unsafe { (api.event_device)(event) }) == device
    }

    /// Translates one already-fetched event for the delivery loops; pointers
    /// are read before the caller releases the event.
    fn classify_delivery_event(
        &self,
        event: *mut EiEvent,
        event_type: libc::c_int,
        expected_ping: u64,
    ) -> EiLinkEvent {
        match event_type {
            EI_EVENT_PONG => {
                let ping = unsafe { (self.api.event_pong_get_ping)(event) };
                let ours =
                    !ping.is_null() && unsafe { (self.api.ping_get_id)(ping) } == expected_ping;
                EiLinkEvent::Pong { ours }
            }
            EI_EVENT_DISCONNECT => EiLinkEvent::Disconnect,
            EI_EVENT_DEVICE_RESUMED => EiLinkEvent::DeviceResumed {
                ours: Self::event_is_for_device(&self.api, event, self.device),
            },
            EI_EVENT_DEVICE_PAUSED => EiLinkEvent::DevicePaused {
                ours: Self::event_is_for_device(&self.api, event, self.device),
            },
            EI_EVENT_DEVICE_REMOVED => EiLinkEvent::DeviceRemoved {
                ours: Self::event_is_for_device(&self.api, event, self.device),
            },
            EI_EVENT_KEYBOARD_MODIFIERS => EiLinkEvent::KeyboardGroup {
                ours: Self::event_is_for_device(&self.api, event, self.device),
                group: unsafe { (self.api.event_keyboard_get_xkb_group)(event) },
            },
            EI_EVENT_SEAT_REMOVED => EiLinkEvent::SeatRemoved { ours: true },
            _ => EiLinkEvent::Other,
        }
    }

    /// Absorbs every event already queued on the context without blocking,
    /// updating the device link. Called before emitting so a pause, removal,
    /// revocation, or disconnect that arrived between deliveries is honored.
    fn absorb_pending_state(&mut self) -> Result<(), BoundaryError> {
        unsafe { (self.api.dispatch)(self.context) };
        loop {
            let event = unsafe { (self.api.get_event)(self.context) };
            if event.is_null() {
                return Ok(());
            }
            let event_type = unsafe { (self.api.event_type)(event) };
            let link_event = self.classify_delivery_event(event, event_type, 0);
            let directive = self.link.observe(link_event);
            unsafe { (self.api.event_unref)(event) };
            if let EiLinkDirective::Fail(reason) = directive {
                return Err(BoundaryError::new(BoundaryKind::Delivery, reason));
            }
        }
    }

    fn deliver(&mut self, text: &str) -> Result<(), BoundaryError> {
        self.absorb_pending_state()?;
        if !self.link.ready() {
            return Err(BoundaryError::new(BoundaryKind::Delivery, "libei disconnected"));
        }
        let text = match self.mode {
            EiDeliveryMode::Text => Some(libei_text_buffer(text)?),
            EiDeliveryMode::KeyboardPaste => None,
        };
        let paste_keys = match self.mode {
            EiDeliveryMode::Text => None,
            EiDeliveryMode::KeyboardPaste => Some(resolve_keyboard_paste_keys(
                keyboard_keymap_text(&self.api, self.device)?,
                self.link.keyboard_group,
            )?),
        };
        self.sequence = self.sequence.wrapping_add(1).max(1);
        unsafe {
            (self.api.start_emulating)(self.device, self.sequence);
            match (self.mode, text.as_ref(), paste_keys) {
                (EiDeliveryMode::Text, Some(text), _) => {
                    let text_utf8 = self.api.text_utf8.expect("TEXT mode requires the TEXT symbol");
                    text_utf8(self.device, text.as_ptr(), text.as_bytes().len());
                    (self.api.frame)(self.device, (self.api.now)(self.context));
                }
                (EiDeliveryMode::KeyboardPaste, _, Some(keys)) => {
                    // The Transcript is already on the clipboard. Submit the
                    // focused application's paste shortcut using the active
                    // EIS keymap, with one frame per key transition as libei
                    // requires.
                    for (key, pressed) in [
                        (keys.control, true),
                        (keys.paste, true),
                        (keys.paste, false),
                        (keys.control, false),
                    ] {
                        (self.api.keyboard_key)(self.device, key, pressed);
                        (self.api.frame)(self.device, (self.api.now)(self.context));
                    }
                }
                _ => unreachable!("Delivery mode inputs are complete"),
            }
            (self.api.stop_emulating)(self.device);
        }
        let ping = unsafe { (self.api.new_ping)(self.context) };
        if ping.is_null() {
            return Err(BoundaryError::new(
                BoundaryKind::Delivery,
                "compositor rejected libei submission",
            ));
        }
        let expected_ping = unsafe { (self.api.ping_get_id)(ping) };
        unsafe {
            (self.api.ping)(ping);
            (self.api.ping_unref)(ping);
        }
        let mut confirmation = EiDeliveryConfirmation::default();
        let deadline = Instant::now() + LIBEI_DELIVERY_DEADLINE;
        while Instant::now() < deadline {
            poll_libei(&self.api, self.context, deadline)?;
            loop {
                let event = unsafe { (self.api.get_event)(self.context) };
                if event.is_null() {
                    break;
                }
                let event_type = unsafe { (self.api.event_type)(event) };
                let link_event = self.classify_delivery_event(event, event_type, expected_ping);
                // Both machines observe: the link so pauses/removals persist
                // beyond this delivery, the confirmation for the verdict.
                let _ = self.link.observe(link_event);
                confirmation.observe(link_event);
                unsafe { (self.api.event_unref)(event) };
            }
            // Only judge once the queued events are exhausted: libei emits a
            // synthetic PONG before EI_EVENT_DISCONNECT, so a matched pong
            // must not win against a disconnect queued right behind it.
            if let Some(verdict) = confirmation.verdict() {
                return verdict
                    .map_err(|reason| BoundaryError::new(BoundaryKind::Delivery, reason));
            }
        }
        Err(BoundaryError::new(
            BoundaryKind::Delivery,
            "compositor rejected libei submission",
        ))
    }
}

fn poll_libei(
    api: &EiApi,
    context: *mut EiContext,
    deadline: Instant,
) -> Result<(), BoundaryError> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    let millis = remaining.as_millis().clamp(1, 100) as libc::c_int;
    let mut pollfd = libc::pollfd {
        fd: unsafe { (api.get_fd)(context) },
        events: libc::POLLIN,
        revents: 0,
    };
    let result = unsafe { libc::poll(&mut pollfd, 1, millis) };
    if result < 0 {
        return Err(BoundaryError::new(
            BoundaryKind::Delivery,
            "libei disconnected",
        ));
    }
    unsafe { (api.dispatch)(context) };
    Ok(())
}

impl Drop for NativeEiSender {
    fn drop(&mut self) {
        unsafe {
            (self.api.device_unref)(self.device);
            (self.api.disconnect)(self.context);
            (self.api.context_unref)(self.context);
        }
    }
}

impl Default for PortalClipboardDelivery {
    fn default() -> Self {
        Self {
            clipboard: Box::new(WlClipboard),
            portal: Box::new(FedoraRemoteDesktopPortal),
            session: None,
            setup: Some(spawn_remote_desktop_setup()),
            setup_failure: None,
            setup_failure_terminal: false,
            setup_retry_after: None,
            background_setup: true,
        }
    }
}

fn spawn_remote_desktop_setup(
) -> tokio::task::JoinHandle<Result<Box<dyn DirectDeliverySession>, BoundaryError>> {
    tokio::spawn(async {
        let mut portal = FedoraRemoteDesktopPortal;
        portal.connect().await
    })
}

impl ProviderHttpClient {
    async fn transcribe_groq_chunk(
        &self,
        credential: Credential,
        endpoint: String,
        params: GroqRequestParams,
        pcm: Vec<u8>,
        cancel: Arc<CancelRegistry>,
    ) -> Result<String, BoundaryError> {
        tokio::task::spawn_blocking(move || {
            request_groq_chunk(credential, endpoint, &params, pcm, &cancel)
        })
        .await
        .map_err(|_| BoundaryError::new(BoundaryKind::Provider, "Groq request task failed"))?
    }
}

/// Builds the curl `--config` body for a Groq transcription request: the audio
/// form part plus the accuracy gains — `model`, `language`, `temperature=0`,
/// `response_format`, and (when non-empty) the vocabulary `prompt`. Rejects a
/// model or language carrying control characters; a control-character-bearing
/// prompt is defensively stripped rather than rejected. Kept pure and separate
/// from the request so the request shape is testable without a network call.
fn build_groq_curl_config(
    endpoint: &str,
    credential: &Credential,
    file_path: &str,
    params: &GroqRequestParams,
) -> Result<String, BoundaryError> {
    if params.model.is_empty() || params.model.contains(['\n', '\r']) {
        return Err(BoundaryError::new(BoundaryKind::Provider, "invalid Groq model"));
    }
    if params.language.contains(['\n', '\r']) {
        return Err(BoundaryError::new(BoundaryKind::Provider, "invalid Groq language"));
    }
    let endpoint = curl_config_escape(endpoint);
    let credential = curl_config_escape(credential.expose_to_boundary());
    let path = curl_config_escape(file_path);
    let model = curl_config_escape(&params.model);
    let mut config = format!(
        "url = \"{endpoint}\"\nheader = \"Authorization: Bearer {credential}\"\nform = \"file=@{path};filename=recording.flac;type=audio/flac\"\nform = \"model={model}\"\nform = \"response_format=json\"\nform = \"temperature=0\"\n"
    );
    if !params.language.is_empty() {
        let language = curl_config_escape(&params.language);
        config.push_str(&format!("form = \"language={language}\"\n"));
    }
    let prompt: String = params.prompt.chars().filter(|c| *c != '\n' && *c != '\r').collect();
    if !prompt.is_empty() {
        let prompt = curl_config_escape(&prompt);
        config.push_str(&format!("form = \"prompt={prompt}\"\n"));
    }
    Ok(config)
}

fn request_groq_chunk(
    credential: Credential,
    endpoint: String,
    params: &GroqRequestParams,
    pcm: Vec<u8>,
    cancel: &CancelRegistry,
) -> Result<String, BoundaryError> {
    let mut file = tempfile::Builder::new()
        .prefix("voisu-recording-")
        .suffix(".flac")
        .tempfile()
        .map_err(|_| BoundaryError::new(BoundaryKind::Provider, "temporary audio file unavailable"))?;
    let flac = flac_from_pcm(&pcm)?;
    file.write_all(&flac)
        .and_then(|()| file.flush())
        .map_err(|_| BoundaryError::new(BoundaryKind::Provider, "temporary audio write failed"))?;
    let config = build_groq_curl_config(&endpoint, &credential, &file.path().to_string_lossy(), params)?;
    let outcome = run_restricted_with_deadline(
        "curl",
        &[
            "-q",
            "--config",
            "-",
            "--fail",
            "--silent",
            "--show-error",
            "--max-time",
            "15",
        ],
        Some(config.as_bytes()),
        true,
        PROVIDER_PROCESS_DEADLINE,
        Some(cancel),
    )
    .map_err(|error| match error {
        ProcessError::TimedOut => {
            BoundaryError::new(BoundaryKind::Provider, "Groq Provider Deadline elapsed")
        }
        _ => BoundaryError::new(BoundaryKind::Provider, "Groq request unavailable or failed"),
    })?;
    if !outcome.success {
        return Err(BoundaryError::new(
            BoundaryKind::Provider,
            "Groq rejected the audio request",
        ));
    }
    let response: serde_json::Value = serde_json::from_slice(&outcome.stdout).map_err(|_| {
        BoundaryError::new(BoundaryKind::Provider, "Groq returned malformed JSON")
    })?;
    response
        .get("text")
        .and_then(|text| text.as_str())
        .map(str::to_owned)
        .ok_or_else(|| BoundaryError::new(BoundaryKind::Provider, "Groq response omitted text"))
}

fn merge_chunk_transcripts(transcripts: Vec<String>) -> String {
    let mut merged: Vec<String> = Vec::new();
    for transcript in transcripts {
        let words: Vec<String> = transcript
            .split_whitespace()
            .map(str::to_owned)
            .collect();
        let overlap = (1..=merged.len().min(words.len()).min(GROQ_MERGE_OVERLAP_WORDS))
            .rev()
            .find(|count| merged[merged.len() - count..] == words[..*count])
            .unwrap_or(0);
        merged.extend(words.into_iter().skip(overlap));
    }
    merged.join(" ")
}

fn flac_from_pcm(pcm: &[u8]) -> Result<Vec<u8>, BoundaryError> {
    use flacenc::bitsink::ByteSink;
    use flacenc::component::BitRepr;
    use flacenc::config::Encoder;
    use flacenc::error::Verify;
    use flacenc::source::MemSource;

    let mut chunks = pcm.chunks_exact(2);
    let samples: Vec<i32> = chunks
        .by_ref()
        .map(|bytes| i16::from_le_bytes([bytes[0], bytes[1]]) as i32)
        .collect();
    if !chunks.remainder().is_empty() {
        return Err(BoundaryError::new(
            BoundaryKind::Provider,
            "Recording PCM length is invalid",
        ));
    }
    let config = Encoder::default().into_verified().map_err(|_| {
        BoundaryError::new(BoundaryKind::Provider, "FLAC encoder configuration is invalid")
    })?;
    let source = MemSource::from_samples(&samples, 1, 16, 16_000);
    let stream = flacenc::encode_with_fixed_block_size(&config, source, config.block_size).map_err(
        |_| BoundaryError::new(BoundaryKind::Provider, "Recording FLAC encode failed"),
    )?;
    let mut sink = ByteSink::new();
    stream.write(&mut sink).map_err(|_| {
        BoundaryError::new(BoundaryKind::Provider, "Recording FLAC output failed")
    })?;
    Ok(sink.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    use voisu_core::{ProviderCoordinator, ProviderStreams};

    #[test]
    fn manager_env_missing_display_is_detected_from_show_environment() {
        let present = "LANG=en_US.UTF-8\nWAYLAND_DISPLAY=wayland-0\nDISPLAY=:0\n";
        assert!(manager_env_has(present, "WAYLAND_DISPLAY"));
        assert!(manager_env_has(present, "DISPLAY"));
        // Absent, and set-but-empty, both read as missing.
        let missing = "LANG=en_US.UTF-8\nDISPLAY=\n";
        assert!(!manager_env_has(missing, "WAYLAND_DISPLAY"));
        assert!(!manager_env_has(missing, "DISPLAY"));
    }

    #[test]
    fn pw_help_token_detects_raw_support() {
        assert!(help_advertises_raw(
            b"  -a, --raw     RAW mode (no header)\n  -h, --help\n"
        ));
        // An `=`-attached value form still exposes the exact option token.
        assert!(help_advertises_raw(b"      --raw=MODE   raw capture\n"));
        // PipeWire 1.0.5-era help never mentions --raw.
        assert!(!help_advertises_raw(
            b"  -R, --remote  Remote daemon\n  -h, --help\n"
        ));
    }

    #[test]
    fn pw_help_rejects_raw_near_matches() {
        // A different option that merely starts with the same letters must not
        // be mistaken for --raw support.
        assert!(!help_advertises_raw(b"      --raw-file FILE   write raw to FILE\n"));
        assert!(!help_advertises_raw(b"      --rawmode        legacy\n"));
        // Substring inside another word must not match either.
        assert!(!help_advertises_raw(b"  see the xyz--rawabc note\n"));
    }

    fn canonical_wav_with_payload(payload: &[u8]) -> Vec<u8> {
        let mut stream = Vec::new();
        stream.extend_from_slice(b"RIFF");
        stream.extend_from_slice(&u32::MAX.to_le_bytes());
        stream.extend_from_slice(b"WAVE");
        stream.extend_from_slice(b"fmt ");
        stream.extend_from_slice(&16u32.to_le_bytes());
        stream.extend_from_slice(&1u16.to_le_bytes());
        stream.extend_from_slice(&1u16.to_le_bytes());
        stream.extend_from_slice(&16_000u32.to_le_bytes());
        stream.extend_from_slice(&32_000u32.to_le_bytes());
        stream.extend_from_slice(&2u16.to_le_bytes());
        stream.extend_from_slice(&16u16.to_le_bytes());
        stream.extend_from_slice(b"data");
        stream.extend_from_slice(&u32::MAX.to_le_bytes());
        stream.extend_from_slice(payload);
        stream
    }

    #[test]
    fn wav_stripper_yields_only_the_pcm_payload() {
        let payload: Vec<u8> = (0..500u16).flat_map(|n| n.to_le_bytes()).collect();
        let stream = canonical_wav_with_payload(&payload);
        let mut stripper = WavHeaderStripper::new(std::io::Cursor::new(stream));
        let mut recovered = Vec::new();
        std::io::Read::read_to_end(&mut stripper, &mut recovered).unwrap();
        assert_eq!(recovered, payload);
    }

    #[test]
    fn wav_stripper_handles_a_header_split_across_reads() {
        // A reader that hands back one byte at a time must still resolve the
        // header and recover the exact payload.
        struct OneByteAtATime(std::io::Cursor<Vec<u8>>);
        impl Read for OneByteAtATime {
            fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
                if out.is_empty() {
                    return Ok(0);
                }
                self.0.read(&mut out[..1])
            }
        }
        let payload = b"the-pcm-body".to_vec();
        let stream = canonical_wav_with_payload(&payload);
        let mut stripper =
            WavHeaderStripper::new(OneByteAtATime(std::io::Cursor::new(stream)));
        let mut recovered = Vec::new();
        std::io::Read::read_to_end(&mut stripper, &mut recovered).unwrap();
        assert_eq!(recovered, payload);
    }

    #[test]
    fn wav_stripper_reports_a_wrong_format_as_a_read_error() {
        let mut stream = Vec::new();
        stream.extend_from_slice(b"RIFF");
        stream.extend_from_slice(&u32::MAX.to_le_bytes());
        stream.extend_from_slice(b"WAVE");
        stream.extend_from_slice(b"fmt ");
        stream.extend_from_slice(&16u32.to_le_bytes());
        stream.extend_from_slice(&1u16.to_le_bytes());
        stream.extend_from_slice(&2u16.to_le_bytes()); // stereo — wrong
        stream.extend_from_slice(&48_000u32.to_le_bytes()); // 48 kHz — wrong
        stream.extend_from_slice(&192_000u32.to_le_bytes());
        stream.extend_from_slice(&4u16.to_le_bytes());
        stream.extend_from_slice(&16u16.to_le_bytes());
        stream.extend_from_slice(b"data");
        stream.extend_from_slice(&u32::MAX.to_le_bytes());
        stream.extend_from_slice(&[0_u8; 64]);
        let mut stripper = WavHeaderStripper::new(std::io::Cursor::new(stream));
        let mut sink = Vec::new();
        let error = std::io::Read::read_to_end(&mut stripper, &mut sink).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn path_resolution_honors_path_order_for_shadowing_wrappers() {
        let home = tempfile::tempdir().unwrap();
        let system = tempfile::tempdir().unwrap();
        // A hand-written wrapper in a home dir that precedes the system dir.
        let wrapper = home.path().join("wl-copy");
        fs::write(&wrapper, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).unwrap();
        let packaged = system.path().join("wl-copy");
        fs::write(&packaged, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&packaged, fs::Permissions::from_mode(0o755)).unwrap();

        let path = std::env::join_paths([home.path(), system.path()]).unwrap();
        let winner = resolve_on_path(&path, "wl-copy").unwrap();
        assert_eq!(winner, wrapper);
        assert!(winner.starts_with(home.path()));
    }

    #[test]
    fn shortcut_session_token_is_stable_across_bind_cycles() {
        // Regression: the session_handle_token names a persistent kglobalaccel
        // component in xdg-desktop-portal-kde, so it MUST be identical across
        // separate bind cycles and independent of process identity — otherwise
        // KWin has no stored binding and re-prompts on every daemon start,
        // leaking an orphaned [token_voisu_session_<pid>] config section. The
        // broken code embedded the PID (`voisu_session_{pid}`).
        let first = shortcut_bind_tokens();
        let second = shortcut_bind_tokens();
        assert_eq!(first.session, second.session);
        assert_eq!(first.session, "voisu_session");
        assert!(
            !first.session.contains(&std::process::id().to_string()),
            "session_handle_token must not embed the PID"
        );

        // The request handle_tokens are a distinct mechanism identifying
        // in-flight Request objects and must stay unique per daemon process.
        let pid = std::process::id().to_string();
        assert!(first.create.contains(&pid));
        assert!(first.bind.contains(&pid));
    }

    #[test]
    fn crypto_provider_installs_and_is_idempotent() {
        install_crypto_provider();
        assert!(rustls::crypto::CryptoProvider::get_default().is_some());
        install_crypto_provider();
        assert!(rustls::crypto::CryptoProvider::get_default().is_some());
    }

    fn credential(value: &str) -> Credential {
        Credential::new(value.to_owned()).expect("test credential is valid")
    }

    #[test]
    fn session_cache_serves_a_second_load_without_re_invoking_the_store() {
        // The observed failure mode: a warm daemon whose credential was loaded on
        // an earlier Recording hits a transient denial on a later one. A cache hit
        // must serve that later load without re-reaching secret-tool at all.
        let cache = CredentialCache::new();
        let ttl = Duration::from_secs(300);
        let calls = std::cell::Cell::new(0_usize);
        let first = resolve_with_cache(Provider::Groq, &cache, ttl, || {
            calls.set(calls.get() + 1);
            Ok(credential("cached-secret"))
        })
        .unwrap();
        let second = resolve_with_cache(Provider::Groq, &cache, ttl, || {
            calls.set(calls.get() + 1);
            Ok(credential("cached-secret"))
        })
        .unwrap();
        assert_eq!(first.expose_to_boundary(), "cached-secret");
        assert_eq!(second.expose_to_boundary(), "cached-secret");
        assert_eq!(calls.get(), 1, "the second load must be served from the cache");
    }

    #[test]
    fn session_cache_re_reads_after_the_ttl_expires() {
        // A zero TTL means every entry is already stale, so a rotated key is never
        // served past its bound — each load re-reads.
        let cache = CredentialCache::new();
        let ttl = Duration::from_millis(0);
        let calls = std::cell::Cell::new(0_usize);
        for _ in 0..2 {
            resolve_with_cache(Provider::Groq, &cache, ttl, || {
                calls.set(calls.get() + 1);
                Ok(credential("fresh-secret"))
            })
            .unwrap();
        }
        assert_eq!(calls.get(), 2, "an expired entry must be re-read, never served stale");
    }

    #[test]
    fn session_cache_invalidation_forces_a_re_read() {
        let cache = CredentialCache::new();
        let ttl = Duration::from_secs(300);
        let calls = std::cell::Cell::new(0_usize);
        resolve_with_cache(Provider::Groq, &cache, ttl, || {
            calls.set(calls.get() + 1);
            Ok(credential("secret"))
        })
        .unwrap();
        cache.invalidate(Provider::Groq);
        resolve_with_cache(Provider::Groq, &cache, ttl, || {
            calls.set(calls.get() + 1);
            Ok(credential("secret"))
        })
        .unwrap();
        assert_eq!(calls.get(), 2, "invalidation must drop the cached entry");
    }

    #[test]
    fn session_cache_keys_each_provider_independently() {
        let cache = CredentialCache::new();
        let ttl = Duration::from_secs(300);
        let groq = resolve_with_cache(Provider::Groq, &cache, ttl, || Ok(credential("groq-key"))).unwrap();
        let deepgram =
            resolve_with_cache(Provider::Deepgram, &cache, ttl, || Ok(credential("deepgram-key"))).unwrap();
        assert_eq!(groq.expose_to_boundary(), "groq-key");
        assert_eq!(deepgram.expose_to_boundary(), "deepgram-key", "one provider must not read another's slot");
    }

    #[test]
    fn session_cache_does_not_store_a_failed_load() {
        // A failed load must not poison the cache: the next attempt must retry the
        // store, not serve a cached error.
        let cache = CredentialCache::new();
        let ttl = Duration::from_secs(300);
        let first = resolve_with_cache(Provider::Groq, &cache, ttl, || {
            Err(BoundaryError::new(BoundaryKind::SecretStorage, "transient"))
        });
        assert!(first.is_err());
        let second = resolve_with_cache(Provider::Groq, &cache, ttl, || Ok(credential("recovered"))).unwrap();
        assert_eq!(second.expose_to_boundary(), "recovered", "a failure must not be cached");
    }

    /// Fixed Groq request tuning for stream constructor tests: deterministic and
    /// independent of the host's environment and user dictionary.
    fn test_groq_params() -> GroqRequestParams {
        GroqRequestParams {
            model: "whisper-large-v3".to_owned(),
            language: "en".to_owned(),
            prompt: "Groq, Tokio".to_owned(),
        }
    }

    #[test]
    fn groq_stays_full_audio_at_or_below_the_limit_and_chunks_above() {
        // A Recording at exactly the full-audio limit still takes one full-audio
        // request; only once it grows past the limit does pre-streaming begin.
        assert!(!groq_prestream_active(GROQ_FULL_AUDIO_MAX_BYTES));
        assert!(!groq_prestream_active(GROQ_FULL_AUDIO_MAX_BYTES - 1));
        assert!(groq_prestream_active(GROQ_FULL_AUDIO_MAX_BYTES + 1));
    }

    #[test]
    fn finalize_is_one_full_audio_request_at_or_below_the_limit() {
        assert_eq!(plan_finalize_chunks(1_000), vec![0..1_000]);
        assert_eq!(
            plan_finalize_chunks(GROQ_FULL_AUDIO_MAX_BYTES),
            vec![0..GROQ_FULL_AUDIO_MAX_BYTES]
        );
    }

    #[test]
    fn finalize_chunks_a_backlog_inflated_recording_past_the_limit() {
        // 130 s finalized (e.g. a large capture backlog appended at Stop pushed a
        // Recording that streamed under 120 s past the limit): it must be chunked
        // into 60 s windows with a 4 s overlap, not one oversized request.
        let len = GROQ_FULL_AUDIO_MAX_BYTES + 16_000 * 2 * 10;
        let ranges = plan_finalize_chunks(len);
        assert!(ranges.len() >= 2, "past the limit finalize is chunked, not one request");
        assert_eq!(ranges[0], 0..GROQ_CHUNK_BYTES);
        assert_eq!(ranges[1].start, GROQ_CHUNK_BYTES - GROQ_CHUNK_OVERLAP_BYTES);
        assert_eq!(ranges.last().unwrap().end, len, "the last window ends at the recording end");
        for range in &ranges[..ranges.len() - 1] {
            assert_eq!(range.end - range.start, GROQ_CHUNK_BYTES, "non-final windows are full chunks");
        }
    }

    #[test]
    fn groq_chunk_geometry_is_sixty_second_windows_with_a_four_second_overlap() {
        assert_eq!(GROQ_CHUNK_BYTES, 16_000 * 2 * 60);
        assert_eq!(GROQ_CHUNK_OVERLAP_BYTES, 16_000 * 2 * 4);
        assert_eq!(GROQ_FULL_AUDIO_MAX_BYTES, 16_000 * 2 * 120);
    }

    #[test]
    fn groq_curl_config_carries_the_accuracy_gains() {
        let credential = Credential::new("secret-token".to_owned()).unwrap();
        let params = GroqRequestParams {
            model: "whisper-large-v3".to_owned(),
            language: "en".to_owned(),
            prompt: "Tokio, serde, SELinux".to_owned(),
        };
        let config =
            build_groq_curl_config("https://api.groq.com/v1", &credential, "/tmp/rec.wav", &params)
                .expect("valid config");
        assert!(config.contains("form = \"model=whisper-large-v3\""));
        assert!(config.contains("form = \"language=en\""));
        assert!(config.contains("form = \"temperature=0\""));
        assert!(config.contains("form = \"prompt=Tokio, serde, SELinux\""));
        assert!(config.contains("form = \"response_format=json\""));
        assert!(config.contains("Authorization: Bearer secret-token"));
    }

    #[test]
    fn groq_recording_payload_is_valid_flac() {
        let pcm = vec![0_u8; 16_000 * 2];
        let flac = flac_from_pcm(&pcm).expect("one second Recording encodes");

        assert_eq!(&flac[..4], b"fLaC");
        assert!(flac.len() < pcm.len(), "silence compresses below raw PCM");
    }

    #[test]
    fn groq_curl_config_uploads_a_flac_recording() {
        let credential = Credential::new("secret-token".to_owned()).unwrap();
        let config = build_groq_curl_config(
            "https://api.groq.com/v1",
            &credential,
            "/tmp/rec.flac",
            &test_groq_params(),
        )
        .expect("valid config");

        assert!(config.contains(
            "form = \"file=@/tmp/rec.flac;filename=recording.flac;type=audio/flac\""
        ));
        assert!(!config.contains("recording.wav"));
        assert!(!config.contains("audio/wav"));
    }

    #[test]
    fn groq_curl_config_omits_an_empty_prompt_and_language() {
        let credential = Credential::new("secret-token".to_owned()).unwrap();
        let params = GroqRequestParams {
            model: "whisper-large-v3".to_owned(),
            language: String::new(),
            prompt: String::new(),
        };
        let config =
            build_groq_curl_config("https://api.groq.com/v1", &credential, "/tmp/rec.wav", &params)
                .expect("valid config");
        assert!(!config.contains("prompt="));
        assert!(!config.contains("language="));
        // temperature is unconditional.
        assert!(config.contains("form = \"temperature=0\""));
    }

    #[test]
    fn groq_curl_config_rejects_a_control_character_model() {
        let credential = Credential::new("secret-token".to_owned()).unwrap();
        let params = GroqRequestParams {
            model: "bad\nmodel".to_owned(),
            language: "en".to_owned(),
            prompt: String::new(),
        };
        let error =
            build_groq_curl_config("https://api.groq.com/v1", &credential, "/tmp/rec.wav", &params)
                .unwrap_err();
        assert_eq!(error.diagnostic(), "invalid Groq model");
    }

    #[test]
    fn merge_dedupes_an_overlap_wider_than_the_old_window() {
        // A ~30-word seam overlap — wider than the previous 24-word window —
        // must be collapsed, not duplicated, at the 4 s chunk boundary.
        let first: Vec<String> = (0..40).map(|i| format!("w{i}")).collect();
        let second: Vec<String> = (10..60).map(|i| format!("w{i}")).collect();
        let merged =
            merge_chunk_transcripts(vec![first.join(" "), second.join(" ")]);
        let expected: Vec<String> = (0..60).map(|i| format!("w{i}")).collect();
        assert_eq!(merged, expected.join(" "), "the 30-word overlap is deduped");
    }

    #[tokio::test]
    async fn dropped_capture_retains_blocking_cleanup_until_reaper_drain() {
        let reaper = ProviderReaper::new();
        let entered = Arc::new(AtomicBool::new(false));
        let cleanup_done = Arc::new(AtomicBool::new(false));
        let (release, release_rx) = std::sync::mpsc::channel::<()>();
        let entered_task = Arc::clone(&entered);
        let cleanup_done_task = Arc::clone(&cleanup_done);
        let cleanup = tokio::task::spawn_blocking(move || {
            entered_task.store(true, Ordering::SeqCst);
            let _ = release_rx.recv();
            cleanup_done_task.store(true, Ordering::SeqCst);
            Ok(Vec::new())
        });
        wait_for(&entered).await;

        let capture = PipeWireActiveCapture {
            child: None,
            state: Arc::new(Mutex::new(CaptureReaderState {
                chunks: VecDeque::new(),
                received_bytes: 0,
                eof: true,
                error: None,
            })),
            reader: None,
            stderr_reader: None,
            cleanup: Some(cleanup),
            reaper: reaper.clone(),
            pcm: Vec::new(),
            started: Instant::now(),
            deadline: DEFAULT_RECORDING_DEADLINE,
        };

        // This is the state produced when an outer abort deadline drops
        // stop_child while its spawn_blocking cleanup still owns pw-record.
        drop(capture);
        assert_eq!(
            reaper.pending(),
            1,
            "capture cleanup must be retained instead of detached"
        );
        assert!(
            !cleanup_done.load(Ordering::SeqCst),
            "cleanup must still be live before the actor drains the reaper"
        );

        let _ = release.send(());
        assert!(
            reaper.drain(Duration::from_secs(2)).await,
            "the retained capture cleanup must drain before Idle"
        );
        assert!(
            cleanup_done.load(Ordering::SeqCst),
            "draining must await the blocking capture cleanup"
        );
    }

    #[tokio::test]
    async fn dropped_capture_before_stop_retains_child_cleanup_until_reaper_drain() {
        // capture_pump can panic or be cancelled while still owning a live
        // pw-record before stop_child ever runs: child is Some, cleanup is None.
        // Drop must not merely kill-and-forget under reap_briefly's 250 ms — a
        // slow-exiting child would then outlive Drop while the reaper looks empty
        // and Idle is permitted mid-cleanup. It must hand a bounded kill/reap to
        // the reaper so the workflow drains it before acknowledging completion.
        let reaper = ProviderReaper::new();
        let child = Command::new("sleep")
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn a killable stand-in child");
        let pid: i32 = child.id().try_into().expect("child pid fits in pid_t");

        let capture = PipeWireActiveCapture {
            child: Some(child),
            state: Arc::new(Mutex::new(CaptureReaderState {
                chunks: VecDeque::new(),
                received_bytes: 0,
                eof: true,
                error: None,
            })),
            reader: None,
            stderr_reader: None,
            cleanup: None,
            reaper: reaper.clone(),
            pcm: Vec::new(),
            started: Instant::now(),
            deadline: DEFAULT_RECORDING_DEADLINE,
        };

        drop(capture);
        assert_eq!(
            reaper.pending(),
            1,
            "a pre-stop capture drop must retain its child cleanup in the reaper"
        );

        assert!(
            reaper.drain(Duration::from_secs(4)).await,
            "the retained pre-stop capture cleanup must drain before Idle"
        );
        // Draining awaited the bounded kill/reap, so the child is gone (reaped,
        // not a lingering zombie) — kill(pid, 0) can no longer find it.
        let alive = unsafe { libc::kill(pid, 0) } == 0;
        assert!(
            !alive,
            "draining must have killed and reaped the abandoned pw-record child"
        );
    }

    #[tokio::test]
    async fn a_dropped_finalize_request_is_owned_by_the_reaper_not_detached() {
        // The single full-audio request of a short Recording is issued at
        // finalize. If the Provider Deadline drops complete() while that request
        // is in flight, its curl child must be OWNED — handed to the
        // ProviderReaper by Drop — never detached. A local server that accepts
        // and never answers keeps the finalize request in flight.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let mut held = Vec::new();
            for stream in listener.incoming() {
                match stream {
                    Ok(connection) => held.push(connection),
                    Err(_) => break,
                }
            }
        });

        let reaper = ProviderReaper::new();
        let mut stream = GroqStream {
            credential: Credential::new("controlled-credential".to_owned()).unwrap(),
            endpoint: format!("http://{address}/audio/transcriptions"),
            params: test_groq_params(),
            buffer: Vec::new(),
            streamed_bytes: 0,
            chunks: VecDeque::new(),
            cancel: CancelRegistry::new(),
            reaper: reaper.clone(),
        };

        // Drive complete() far enough to issue the finalize request, then drop it
        // exactly as the Provider Deadline would.
        {
            let completion = stream.complete(CapturedAudio::empty());
            assert!(
                tokio::time::timeout(Duration::from_millis(750), completion)
                    .await
                    .is_err(),
                "the hanging finalize request must not complete on its own"
            );
        }
        // Dropping the stream must adopt the still-live finalize task into the
        // reaper (cancel-first, then adopt), not detach its curl child. With the
        // finalize request awaited inline this count is zero.
        drop(stream);
        assert_eq!(
            reaper.pending(),
            1,
            "the finalize request handle must be owned by the reaper, not detached"
        );
        // Draining cancels the request and reaps its curl child.
        reaper.drain_to_completion(Duration::from_secs(5)).await;
    }

    /// A probe chunk task shaped like a real provider chunk: the outer async
    /// task awaits an inner `spawn_blocking` request. The blocking closure — the
    /// one holding a live curl child in production — waits for cancellation, then
    /// performs a kill-and-reap that the test releases explicitly, so the test
    /// can observe cleanup ownership at the exact instant the coordinator's error
    /// surfaces. `entered` proves the blocking task actually started; `reap_done`
    /// is the reap-completion latch that is set only after the child is reaped.
    struct BlockingChunkProbe {
        entered: Arc<AtomicBool>,
        reap_done: Arc<AtomicBool>,
        release: std::sync::mpsc::Sender<()>,
    }

    fn spawn_blocking_backed_chunk(
        cancel: Arc<CancelRegistry>,
    ) -> (
        tokio::task::JoinHandle<Result<String, BoundaryError>>,
        BlockingChunkProbe,
    ) {
        let entered = Arc::new(AtomicBool::new(false));
        let reap_done = Arc::new(AtomicBool::new(false));
        let (release, release_rx) = std::sync::mpsc::channel::<()>();
        let entered_task = Arc::clone(&entered);
        let reap_done_task = Arc::clone(&reap_done);
        let handle = tokio::spawn(async move {
            tokio::task::spawn_blocking(move || {
                entered_task.store(true, Ordering::SeqCst);
                // Mirror an in-flight curl request owned by this blocking task:
                // run until the owning bounded wait observes cancellation.
                while !cancel.is_cancelled() {
                    std::thread::sleep(Duration::from_millis(1));
                }
                // Cancellation observed. The kill-and-reap of the child is gated
                // by the test so the reap deliberately outlasts the abort
                // deadline, forcing the coordinator down its timeout path while
                // the blocking work is still live.
                let _ = release_rx.recv();
                reap_done_task.store(true, Ordering::SeqCst);
                Err(BoundaryError::new(BoundaryKind::Provider, "request cancelled"))
            })
            .await
            .map_err(|_| BoundaryError::new(BoundaryKind::Provider, "request task failed"))?
        });
        (
            handle,
            BlockingChunkProbe {
                entered,
                reap_done,
                release,
            },
        )
    }

    #[tokio::test]
    async fn provider_abort_deadline_retains_and_reaps_blocking_work_before_idle() {
        let reaper = ProviderReaper::new();
        let credential = Credential::new("controlled-credential".to_owned()).unwrap();
        let deepgram_cancel = CancelRegistry::new();
        let groq_cancel = CancelRegistry::new();
        let (deepgram_chunk, deepgram_probe) =
            spawn_blocking_backed_chunk(Arc::clone(&deepgram_cancel));
        let (groq_chunk, groq_probe) = spawn_blocking_backed_chunk(Arc::clone(&groq_cancel));
        // Shape the Deepgram websocket I/O task like a real one whose teardown
        // still owns nested blocking work when cancellation fires.
        let deepgram_io_task = tokio::spawn(async move {
            deepgram_chunk
                .await
                .map_err(|_| {
                    BoundaryError::new(BoundaryKind::Provider, "Deepgram streaming task failed")
                })?
                .map(|_| ())
        });
        let (deepgram_outbound, _deepgram_outbound_rx) = tokio::sync::mpsc::unbounded_channel();
        let streams = ProviderStreams {
            deepgram: Box::new(DeepgramStream {
                outbound: Some(deepgram_outbound),
                streamed_bytes: 0,
                io_tasks: VecDeque::from([deepgram_io_task]),
                transcript: Arc::new(Mutex::new(TranscriptAccumulator::default())),
                cancel: deepgram_cancel,
                shutdown: Arc::new(tokio::sync::Notify::new()),
                reaper: reaper.clone(),
            }),
            groq: Box::new(GroqStream {
                credential,
                endpoint: "http://localhost/groq".to_owned(),
                params: test_groq_params(),
                buffer: Vec::new(),
                streamed_bytes: 0,
                chunks: VecDeque::from([groq_chunk]),
                cancel: groq_cancel,
                reaper: reaper.clone(),
            }),
        };

        // Both blocking requests must actually be executing inside spawn_blocking
        // before the deadline fires, so cleanup has real nested ownership to lose.
        wait_for(&deepgram_probe.entered).await;
        wait_for(&groq_probe.entered).await;

        let error =
            ProviderCoordinator::start(Duration::from_millis(10), Duration::from_millis(10), streams)
                .complete(CapturedAudio::empty())
                .await
                .unwrap_err();
        assert_eq!(error.diagnostic(), "provider deadline cleanup timed out");

        // The moment the coordinator's cleanup-timeout error surfaces, the
        // blocking curl work is still live (its reap has not been released).
        // Publishing Idle here without draining would strand that live work.
        assert!(
            !deepgram_probe.reap_done.load(Ordering::SeqCst),
            "Deepgram curl reap must still be in flight when cleanup times out"
        );
        assert!(
            !groq_probe.reap_done.load(Ordering::SeqCst),
            "Groq curl reap must still be in flight when cleanup times out"
        );
        // Cleanup ownership was RETAINED by the actor-owned supervisor rather
        // than aborted and detached: with the detach defect this count is zero.
        assert_eq!(
            reaper.pending(),
            2,
            "both dropped streams must hand their curl reap to the supervisor"
        );

        // Release the reaps and drain the supervisor, exactly as the actor does
        // before it publishes Idle. Draining must await the retained reaper tasks
        // until the nested blocking work has actually completed its reap.
        let _ = deepgram_probe.release.send(());
        let _ = groq_probe.release.send(());
        assert!(
            reaper.drain(Duration::from_secs(2)).await,
            "the supervisor must fully drain within the bound"
        );
        assert!(
            deepgram_probe.reap_done.load(Ordering::SeqCst),
            "draining must not return until the Deepgram blocking reap completed"
        );
        assert!(
            groq_probe.reap_done.load(Ordering::SeqCst),
            "draining must not return until the Groq blocking reap completed"
        );
        assert_eq!(reaper.pending(), 0, "a full drain must leave nothing retained");
    }

    #[tokio::test]
    async fn stream_dropped_without_a_runtime_still_retains_its_blocking_cleanup() {
        // Runtime teardown (and any non-runtime thread) can drop a provider
        // stream where Handle::try_current() fails. Adoption must be synchronous
        // and runtime-free: the cleanup is retained for a later drain, never
        // aborted — aborting would detach the nested spawn_blocking curl reap.
        let reaper = ProviderReaper::new();
        let cancel = CancelRegistry::new();
        let (chunk, probe) = spawn_blocking_backed_chunk(Arc::clone(&cancel));
        wait_for(&probe.entered).await;
        let stream = GroqStream {
            credential: Credential::new("controlled-credential".to_owned()).unwrap(),
            endpoint: "http://localhost/groq".to_owned(),
            params: test_groq_params(),
            buffer: Vec::new(),
            streamed_bytes: 0,
            chunks: VecDeque::from([chunk]),
            cancel,
            reaper: reaper.clone(),
        };
        std::thread::spawn(move || drop(stream))
            .join()
            .expect("dropping a stream off the runtime must not panic");
        assert_eq!(
            reaper.pending(),
            1,
            "a stream dropped without a runtime must still retain its cleanup"
        );
        let _ = probe.release.send(());
        assert!(
            reaper.drain(Duration::from_secs(2)).await,
            "the retained cleanup must drain once released"
        );
        assert!(
            probe.reap_done.load(Ordering::SeqCst),
            "draining must await the blocking reap to completion"
        );
    }

    #[tokio::test]
    async fn concurrent_drains_never_report_completion_over_live_cleanup() {
        // While one drain temporarily holds cleanup futures out of the
        // supervisor, a concurrent drain must serialize behind it — never
        // observe an empty supervisor and report a completed drain while the
        // blocking reap is still running.
        let reaper = ProviderReaper::new();
        let cancel = CancelRegistry::new();
        cancel.cancel();
        let (chunk, probe) = spawn_blocking_backed_chunk(Arc::clone(&cancel));
        wait_for(&probe.entered).await;
        let mut chunks = VecDeque::new();
        chunks.push_back(chunk);
        reaper.adopt(chunks);

        let first = tokio::spawn({
            let reaper = reaper.clone();
            async move { reaper.drain(Duration::from_secs(2)).await }
        });
        // Give the first drain time to take the cleanup batch out of the
        // supervisor before the concurrent drain starts.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let second = tokio::spawn({
            let reaper = reaper.clone();
            let reap_done = Arc::clone(&probe.reap_done);
            async move {
                let drained = reaper.drain(Duration::from_secs(2)).await;
                (drained, reap_done.load(Ordering::SeqCst))
            }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        let _ = probe.release.send(());

        assert!(
            first.await.expect("first drain must not panic"),
            "the first drain must complete once the reap is released"
        );
        let (second_drained, reap_done_when_second_returned) =
            second.await.expect("second drain must not panic");
        assert!(second_drained, "the concurrent drain must also complete");
        assert!(
            reap_done_when_second_returned,
            "a concurrent drain must not report completion while the blocking reap runs"
        );
    }

    #[tokio::test]
    async fn drain_to_completion_survives_pass_timeouts_without_detaching_cleanup() {
        // A teardown path whose single bounded drain times out would retain the
        // unfinished cleanup only to drop it with the runtime immediately
        // after. drain_to_completion must keep draining across pass timeouts
        // and return only once the blocking reap has actually completed.
        let reaper = ProviderReaper::new();
        let cancel = CancelRegistry::new();
        cancel.cancel();
        let (chunk, probe) = spawn_blocking_backed_chunk(Arc::clone(&cancel));
        wait_for(&probe.entered).await;
        reaper.adopt(VecDeque::from([chunk]));

        // Release the reap well after several 50ms passes have timed out.
        let release = probe.release.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            let _ = release.send(());
        });
        reaper.drain_to_completion(Duration::from_millis(50)).await;
        assert!(
            probe.reap_done.load(Ordering::SeqCst),
            "drain_to_completion must not return before the blocking reap completed"
        );
        assert_eq!(
            reaper.pending(),
            0,
            "a completed teardown drain must leave nothing retained"
        );
    }

    /// Spins until an `entered` latch is set, bounded so a genuine failure to
    /// enter spawn_blocking surfaces as a timeout rather than a hang.
    async fn wait_for(flag: &Arc<AtomicBool>) {
        tokio::time::timeout(Duration::from_secs(2), async {
            while !flag.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("probe blocking task must enter spawn_blocking");
    }

    #[test]
    fn transcript_accumulator_assembles_only_finalized_results_in_order() {
        let mut accumulator = TranscriptAccumulator::default();
        // Interim revision of a window that a later final supersedes.
        accumulator.ingest(&serde_json::json!({
            "type": "Results", "is_final": false,
            "channel": {"alternatives": [{"transcript": "the quick brown"}]}
        }));
        accumulator.ingest(&serde_json::json!({
            "type": "Results", "is_final": true,
            "channel": {"alternatives": [{"transcript": "the quick brown fox"}]}
        }));
        // Non-Results, whitespace-only finals, and shape drift are ignored.
        accumulator.ingest(&serde_json::json!({"type": "Metadata"}));
        accumulator.ingest(&serde_json::json!({
            "type": "Results", "is_final": true,
            "channel": {"alternatives": [{"transcript": "   "}]}
        }));
        accumulator.ingest(&serde_json::json!({"type": "Results", "is_final": true}));
        accumulator.ingest(&serde_json::json!({
            "type": "Results", "is_final": true, "speech_final": true,
            "channel": {"alternatives": [{"transcript": "jumps over."}]}
        }));
        assert_eq!(accumulator.text(), "the quick brown fox jumps over.");
    }

    #[test]
    fn deepgram_streaming_url_carries_nova3_params_and_repeated_encoded_keyterms() {
        let url = deepgram_streaming_url(
            "wss://api.deepgram.com/v1/listen",
            &["Voisu".to_owned(), "smart format".to_owned(), "  ".to_owned()],
        )
        .unwrap();
        assert!(url.starts_with("wss://api.deepgram.com/v1/listen?"), "{url}");
        for expected in [
            "model=nova-3",
            "encoding=linear16",
            "sample_rate=16000",
            "channels=1",
            "interim_results=true",
            "smart_format=true",
            "punctuate=true",
            "endpointing=300",
            "utterance_end_ms=1000",
        ] {
            assert!(url.contains(expected), "{url} is missing {expected}");
        }
        assert!(url.contains("keyterm=Voisu"), "{url}");
        assert!(url.contains("keyterm=smart%20format"), "{url}");
        assert_eq!(url.matches("keyterm=").count(), 2, "blank keyterms must be dropped: {url}");
    }

    #[test]
    fn deepgram_streaming_url_rewrites_http_schemes_and_requires_wss_off_loopback() {
        assert!(deepgram_streaming_url("https://api.deepgram.com/v1/listen", &[])
            .unwrap()
            .starts_with("wss://api.deepgram.com/v1/listen?"));
        assert!(deepgram_streaming_url("http://127.0.0.1:9999/v1/listen", &[])
            .unwrap()
            .starts_with("ws://127.0.0.1:9999/v1/listen?"));
        assert!(deepgram_streaming_url("ws://deepgram.test/v1/listen", &[]).is_err());
        assert!(deepgram_streaming_url("http://deepgram.test/v1/listen", &[]).is_err());
        // A base that already carries a query keeps it and appends with '&'.
        let url = deepgram_streaming_url("wss://host/listen?tier=custom", &[]).unwrap();
        assert!(url.contains("?tier=custom&model=nova-3"), "{url}");
    }

    async fn mock_deepgram_listener() -> (tokio::net::TcpListener, String) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("ws://{}/v1/listen", listener.local_addr().unwrap());
        (listener, base)
    }

    fn test_deepgram_stream(
        base: &str,
        keepalive: Duration,
        reaper: &ProviderReaper,
    ) -> DeepgramStream {
        test_deepgram_stream_with_grace(base, keepalive, DEEPGRAM_CLOSE_GRACE, reaper)
    }

    fn test_deepgram_stream_with_grace(
        base: &str,
        keepalive: Duration,
        close_grace: Duration,
        reaper: &ProviderReaper,
    ) -> DeepgramStream {
        DeepgramStream::connect(
            deepgram_streaming_url(base, &[]).unwrap(),
            Credential::new("controlled-credential".to_owned()).unwrap(),
            keepalive,
            close_grace,
            reaper.clone(),
        )
    }

    /// The terminal summary message Deepgram sends after `CloseStream`, before
    /// closing the connection.
    fn deepgram_metadata_frame() -> String {
        serde_json::json!({"type": "Metadata", "request_id": "mock-request"}).to_string()
    }

    fn deepgram_results_frame(transcript: &str, is_final: bool) -> String {
        serde_json::json!({
            "type": "Results",
            "is_final": is_final,
            "speech_final": is_final,
            "channel": {"alternatives": [{"transcript": transcript}]}
        })
        .to_string()
    }

    #[tokio::test]
    // The tungstenite accept_hdr callback's Err type is the crate's ~136-byte
    // http::Response — fixed by the third-party signature, not shrinkable here.
    #[allow(clippy::result_large_err)]
    async fn deepgram_streams_binary_audio_and_returns_only_finalized_segments() {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::handshake::server::{
            Request as WsRequest, Response as WsResponse,
        };
        use tokio_tungstenite::tungstenite::Message;

        let (listener, base) = mock_deepgram_listener().await;
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let handshake: Arc<Mutex<Option<(String, String)>>> = Arc::default();
            let capture = Arc::clone(&handshake);
            let mut socket = tokio_tungstenite::accept_hdr_async(
                tcp,
                move |request: &WsRequest, response: WsResponse| {
                    let authorization = request
                        .headers()
                        .get("authorization")
                        .and_then(|value| value.to_str().ok())
                        .unwrap_or_default()
                        .to_owned();
                    *capture.lock().unwrap() =
                        Some((request.uri().to_string(), authorization));
                    Ok(response)
                },
            )
            .await
            .unwrap();
            let mut audio: Vec<u8> = Vec::new();
            let mut control: Vec<String> = Vec::new();
            let mut interim_sent = false;
            while let Some(message) = socket.next().await {
                match message.unwrap() {
                    Message::Binary(bytes) => {
                        audio.extend_from_slice(&bytes);
                        if !interim_sent {
                            interim_sent = true;
                            socket
                                .send(Message::Text(deepgram_results_frame(
                                    "this interim revision must never reach the Transcript",
                                    false,
                                )))
                                .await
                                .unwrap();
                        }
                    }
                    Message::Text(text) => {
                        let closing = text.contains("CloseStream");
                        control.push(text);
                        if closing {
                            socket
                                .send(Message::Text(deepgram_results_frame("Hello world.", true)))
                                .await
                                .unwrap();
                            socket
                                .send(Message::Text(deepgram_results_frame(
                                    "Second segment.",
                                    true,
                                )))
                                .await
                                .unwrap();
                            socket
                                .send(Message::Text(deepgram_metadata_frame()))
                                .await
                                .unwrap();
                            let _ = socket.send(Message::Close(None)).await;
                            break;
                        }
                    }
                    Message::Close(_) => break,
                    _ => {}
                }
            }
            let captured = handshake.lock().unwrap().clone();
            (captured, audio, control)
        });

        let reaper = ProviderReaper::new();
        let mut stream = test_deepgram_stream(&base, Duration::from_secs(5), &reaper);
        let mut pcm = Vec::new();
        for chunk in [vec![1u8; 64], vec![2u8; 64]] {
            pcm.extend_from_slice(&chunk);
            stream.send_audio(AudioChunk(chunk)).await.unwrap();
        }
        // An un-streamed tail that complete() must top up before Finalize.
        pcm.extend_from_slice(&[3u8; 32]);
        let transcript = stream
            .complete(CapturedAudio::new(pcm.clone()))
            .await
            .unwrap();
        assert_eq!(transcript.provider, Provider::Deepgram);
        assert_eq!(transcript.text, "Hello world. Second segment.");

        let (captured, audio, control) = server.await.unwrap();
        let (uri, authorization) = captured.expect("handshake must be captured");
        assert_eq!(authorization, "Token controlled-credential");
        assert!(uri.contains("model=nova-3"), "{uri}");
        assert!(uri.contains("interim_results=true"), "{uri}");
        assert_eq!(audio, pcm, "every PCM byte must arrive as binary frames");
        assert!(
            control.iter().any(|text| text.contains("\"Finalize\"")),
            "{control:?}"
        );
        assert!(
            control.iter().any(|text| text.contains("\"CloseStream\"")),
            "{control:?}"
        );
        assert_eq!(reaper.pending(), 0);
    }

    #[tokio::test]
    async fn deepgram_connection_lost_mid_recording_fails_the_provider_visibly() {
        use futures_util::StreamExt;

        let (listener, base) = mock_deepgram_listener().await;
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(tcp).await.unwrap();
            // Take the first frame, then drop both the connection and the
            // listener so every redial within the bounded budget is refused.
            let _ = socket.next().await;
            drop(socket);
            drop(listener);
        });

        let reaper = ProviderReaper::new();
        let mut stream = test_deepgram_stream(&base, Duration::from_secs(5), &reaper);
        stream.send_audio(AudioChunk(vec![0u8; 32])).await.unwrap();
        server.await.unwrap();
        let error = stream
            .complete(CapturedAudio::new(vec![0u8; 32]))
            .await
            .unwrap_err();
        assert!(
            error.diagnostic().contains("Deepgram"),
            "a mid-Recording drop must surface a visible provider error, got {:?}",
            error.diagnostic()
        );
    }

    #[tokio::test]
    async fn deepgram_server_error_message_fails_the_provider_visibly() {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;

        let (listener, base) = mock_deepgram_listener().await;
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(tcp).await.unwrap();
            let _ = socket.next().await;
            socket
                .send(Message::Text(
                    r#"{"type":"Error","description":"rejected"}"#.to_owned(),
                ))
                .await
                .unwrap();
            // Hold the connection open; the client must fail on the Error
            // message itself, not on a transport drop.
            while let Some(Ok(message)) = socket.next().await {
                if matches!(message, Message::Close(_)) {
                    break;
                }
            }
        });

        let reaper = ProviderReaper::new();
        let mut stream = test_deepgram_stream(&base, Duration::from_secs(5), &reaper);
        stream.send_audio(AudioChunk(vec![0u8; 32])).await.unwrap();
        let error = stream
            .complete(CapturedAudio::new(vec![0u8; 32]))
            .await
            .unwrap_err();
        assert_eq!(error.diagnostic(), "Deepgram reported a streaming error");
        server.abort();
    }

    #[tokio::test]
    async fn deepgram_drop_after_delivered_audio_fails_visibly_without_redialing() {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;

        let (listener, base) = mock_deepgram_listener().await;
        let redialed = Arc::new(AtomicBool::new(false));
        let redialed_flag = Arc::clone(&redialed);
        let server = tokio::spawn(async move {
            // Finalize one segment for delivered audio, then drop abruptly.
            let (tcp, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(tcp).await.unwrap();
            let _ = socket.next().await;
            socket
                .send(Message::Text(deepgram_results_frame("schedule the", true)))
                .await
                .unwrap();
            drop(socket);
            // Keep listening: a redial WOULD succeed here, so a pass proves the
            // client refused to redial rather than that it couldn't.
            if tokio::time::timeout(Duration::from_secs(1), listener.accept())
                .await
                .is_ok()
            {
                redialed_flag.store(true, Ordering::SeqCst);
            }
        });

        let reaper = ProviderReaper::new();
        let mut stream = test_deepgram_stream(&base, Duration::from_secs(5), &reaper);
        stream.send_audio(AudioChunk(vec![7u8; 32])).await.unwrap();
        // Wait until the finalized segment was ingested, then give a
        // redial-and-continue implementation time to observe the drop and
        // redial BEFORE the Recording completes — the exact window where a
        // silent audio gap would hide.
        tokio::time::timeout(Duration::from_secs(2), async {
            while stream.transcript.lock().unwrap().text() != "schedule the" {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the finalized segment must be ingested");
        tokio::time::sleep(Duration::from_millis(600)).await;
        // Audio already accepted by the dropped socket cannot be replayed, so a
        // redial-and-continue would return a plausible Transcript with a silent
        // gap. The provider must fail visibly instead; Groq carries.
        let error = stream
            .complete(CapturedAudio::new(vec![7u8; 32]))
            .await
            .unwrap_err();
        assert!(
            error.diagnostic().contains("Deepgram"),
            "got {:?}",
            error.diagnostic()
        );
        let _ = server.await;
        assert!(
            !redialed.load(Ordering::SeqCst),
            "a drop after delivered audio must not be redialed"
        );
    }

    #[tokio::test]
    async fn deepgram_redials_a_failed_dial_before_any_audio_within_the_budget() {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;

        let (listener, base) = mock_deepgram_listener().await;
        let (second_up_tx, second_up_rx) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(async move {
            // First connection dies before any audio was delivered on it.
            let (tcp, _) = listener.accept().await.unwrap();
            let socket = tokio_tungstenite::accept_async(tcp).await.unwrap();
            drop(socket);
            // Second connection: the bounded redial carries the Recording.
            let (tcp, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(tcp).await.unwrap();
            let _ = second_up_tx.send(());
            while let Some(Ok(message)) = socket.next().await {
                if let Message::Text(text) = message {
                    if text.contains("CloseStream") {
                        socket
                            .send(Message::Text(deepgram_results_frame("after redial", true)))
                            .await
                            .unwrap();
                        socket
                            .send(Message::Text(deepgram_metadata_frame()))
                            .await
                            .unwrap();
                        let _ = socket.send(Message::Close(None)).await;
                        break;
                    }
                }
            }
        });

        let reaper = ProviderReaper::new();
        let mut stream = test_deepgram_stream(&base, Duration::from_secs(5), &reaper);
        // No audio has been handed to the first connection, so nothing can be
        // lost: the dial phase stays covered by the bounded reconnect budget.
        second_up_rx.await.unwrap();
        stream.send_audio(AudioChunk(vec![7u8; 32])).await.unwrap();
        let transcript = stream
            .complete(CapturedAudio::new(vec![7u8; 32]))
            .await
            .unwrap();
        assert_eq!(transcript.text, "after redial");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn deepgram_close_without_terminal_metadata_fails_visibly() {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;

        let (listener, base) = mock_deepgram_listener().await;
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(tcp).await.unwrap();
            while let Some(Ok(message)) = socket.next().await {
                if let Message::Text(text) = message {
                    if text.contains("CloseStream") {
                        // A final Results but NO terminal Metadata before the
                        // close: the server-side flush is unconfirmed, so the
                        // Transcript may be truncated.
                        socket
                            .send(Message::Text(deepgram_results_frame("truncated", true)))
                            .await
                            .unwrap();
                        let _ = socket.send(Message::Close(None)).await;
                        break;
                    }
                }
            }
        });

        let reaper = ProviderReaper::new();
        let mut stream = test_deepgram_stream(&base, Duration::from_secs(5), &reaper);
        stream.send_audio(AudioChunk(vec![5u8; 32])).await.unwrap();
        let error = stream
            .complete(CapturedAudio::new(vec![5u8; 32]))
            .await
            .unwrap_err();
        assert!(
            error.diagnostic().contains("Deepgram"),
            "a close without the terminal Metadata must fail visibly, got {:?}",
            error.diagnostic()
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn deepgram_unanswered_closestream_fails_visibly_at_the_close_grace() {
        use futures_util::StreamExt;
        use tokio_tungstenite::tungstenite::Message;

        let (listener, base) = mock_deepgram_listener().await;
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(tcp).await.unwrap();
            // Read everything, never answer CloseStream.
            while let Some(Ok(message)) = socket.next().await {
                if matches!(message, Message::Close(_)) {
                    break;
                }
            }
        });

        let reaper = ProviderReaper::new();
        let mut stream = test_deepgram_stream_with_grace(
            &base,
            Duration::from_secs(5),
            Duration::from_millis(200),
            &reaper,
        );
        stream.send_audio(AudioChunk(vec![4u8; 32])).await.unwrap();
        // Returning the partial accumulator here would deliver a plausible but
        // truncated Source Transcript inside the 14s Provider Deadline.
        let error = stream
            .complete(CapturedAudio::new(vec![4u8; 32]))
            .await
            .unwrap_err();
        assert!(
            error.diagnostic().contains("Deepgram"),
            "an unanswered CloseStream must fail visibly at the close grace, got {:?}",
            error.diagnostic()
        );
        drop(stream);
        let _ = reaper.drain(Duration::from_secs(2)).await;
        let _ = server.await;
    }

    #[test]
    fn deepgram_streaming_url_rejects_userinfo_in_the_authority() {
        // ws://127.0.0.1:80@attacker.example/… — the raw authority starts with
        // a loopback-looking userinfo but the HOST is attacker.example; the
        // Token header must never travel over that connection (let alone in
        // plaintext).
        assert!(
            deepgram_streaming_url("ws://127.0.0.1:80@attacker.example/listen", &[]).is_err()
        );
        assert!(
            deepgram_streaming_url("http://localhost@attacker.example/listen", &[]).is_err()
        );
        assert!(deepgram_streaming_url("wss://user@api.deepgram.com/v1/listen", &[]).is_err());
        assert!(deepgram_streaming_url("ws:///listen", &[]).is_err());
    }

    #[tokio::test]
    async fn deepgram_abort_surfaces_a_stored_streaming_failure() {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;

        let (listener, base) = mock_deepgram_listener().await;
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(tcp).await.unwrap();
            let _ = socket.next().await;
            socket
                .send(Message::Text(
                    r#"{"type":"Error","description":"rejected"}"#.to_owned(),
                ))
                .await
                .unwrap();
            while let Some(Ok(message)) = socket.next().await {
                if matches!(message, Message::Close(_)) {
                    break;
                }
            }
        });

        let reaper = ProviderReaper::new();
        let mut stream = test_deepgram_stream(&base, Duration::from_secs(5), &reaper);
        stream.send_audio(AudioChunk(vec![3u8; 32])).await.unwrap();
        // Wait until the I/O task stored the failure, mirroring a Recording
        // whose capture fails (or whose Recording Deadline fires) after
        // Deepgram already failed: abort(), not complete(), is what runs.
        tokio::time::timeout(Duration::from_secs(2), async {
            while !stream.io_tasks.front().unwrap().is_finished() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the streaming task must settle its failure");
        let error = Box::new(stream).abort().await.unwrap_err();
        assert_eq!(
            error.diagnostic(),
            "Deepgram reported a streaming error",
            "abort must surface the stored provider failure, not discard it"
        );
        server.abort();
    }

    #[test]
    fn malformed_deepgram_text_frames_fail_visibly() {
        let transcript = Arc::new(Mutex::new(TranscriptAccumulator::default()));
        // Not JSON at all.
        assert!(ingest_deepgram_message(&transcript, "not-json").is_err());
        // A Results frame with no is_final marker.
        assert!(ingest_deepgram_message(&transcript, r#"{"type":"Results"}"#).is_err());
        // A finalized Results frame whose transcript text is missing: skipping
        // it would silently truncate the Transcript.
        assert!(ingest_deepgram_message(
            &transcript,
            r#"{"type":"Results","is_final":true,"speech_final":true}"#
        )
        .is_err());
        // Unknown message types stay tolerated (server-side schema additions),
        // and interim shape drift is UI-only.
        assert!(ingest_deepgram_message(&transcript, r#"{"type":"SpeechStarted"}"#).is_ok());
        assert!(
            ingest_deepgram_message(&transcript, r#"{"type":"Results","is_final":false}"#)
                .is_ok()
        );
        assert_eq!(
            transcript.lock().unwrap().text(),
            "",
            "no rejected frame may leak text into the Transcript"
        );
    }

    #[tokio::test]
    async fn deepgram_malformed_final_results_fail_the_provider_visibly() {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;

        let (listener, base) = mock_deepgram_listener().await;
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(tcp).await.unwrap();
            let _ = socket.next().await;
            // A malformed finalized Results frame, followed by a perfectly
            // clean drain: only frame-level strictness can catch this.
            socket
                .send(Message::Text(
                    r#"{"type":"Results","is_final":true}"#.to_owned(),
                ))
                .await
                .unwrap();
            while let Some(Ok(message)) = socket.next().await {
                match message {
                    Message::Text(text) if text.contains("CloseStream") => {
                        let _ = socket.send(Message::Text(deepgram_metadata_frame())).await;
                        let _ = socket.send(Message::Close(None)).await;
                        break;
                    }
                    Message::Close(_) => break,
                    _ => {}
                }
            }
        });

        let reaper = ProviderReaper::new();
        let mut stream = test_deepgram_stream(&base, Duration::from_secs(5), &reaper);
        stream.send_audio(AudioChunk(vec![2u8; 32])).await.unwrap();
        let error = stream
            .complete(CapturedAudio::new(vec![2u8; 32]))
            .await
            .unwrap_err();
        assert!(
            error.diagnostic().contains("Deepgram"),
            "a malformed finalized Results frame must fail visibly, got {:?}",
            error.diagnostic()
        );
        let _ = server.await;
    }

    #[tokio::test]
    async fn deepgram_sends_keepalive_text_frames_during_outbound_gaps() {
        use futures_util::StreamExt;
        use tokio_tungstenite::tungstenite::Message;

        let (listener, base) = mock_deepgram_listener().await;
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(tcp).await.unwrap();
            loop {
                match socket.next().await {
                    Some(Ok(Message::Text(text))) if text.contains("KeepAlive") => return true,
                    Some(Ok(Message::Close(_))) | Some(Err(_)) | None => return false,
                    _ => {}
                }
            }
        });

        let reaper = ProviderReaper::new();
        let stream = test_deepgram_stream(&base, Duration::from_millis(50), &reaper);
        let keepalive_seen = tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .expect("the mock server must observe a frame within the bound")
            .unwrap();
        assert!(keepalive_seen, "an idle outbound side must emit KeepAlive");
        Box::new(stream).abort().await.unwrap();
    }

    #[tokio::test]
    async fn deepgram_stream_dropped_mid_abort_hands_the_streaming_task_to_the_reaper() {
        use futures_util::StreamExt;
        use tokio_tungstenite::tungstenite::Message;

        let (listener, base) = mock_deepgram_listener().await;
        let (connected_tx, connected_rx) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(tcp).await.unwrap();
            let _ = connected_tx.send(());
            while let Some(Ok(message)) = socket.next().await {
                if matches!(message, Message::Close(_)) {
                    break;
                }
            }
        });

        let reaper = ProviderReaper::new();
        let stream = test_deepgram_stream(&base, Duration::from_secs(5), &reaper);
        // Cancellation before the connection exists would end the I/O task
        // pre-connect; the drop under test must land on a live websocket.
        connected_rx.await.unwrap();
        drop(stream);
        assert_eq!(
            reaper.pending(),
            1,
            "a dropped stream must hand its websocket I/O task to the supervisor"
        );
        assert!(
            reaper.drain(Duration::from_secs(2)).await,
            "the retained I/O task must observe cancellation and drain"
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn deepgram_abort_awaits_the_streaming_task_and_leaves_nothing_retained() {
        use futures_util::StreamExt;
        use tokio_tungstenite::tungstenite::Message;

        let (listener, base) = mock_deepgram_listener().await;
        let (connected_tx, connected_rx) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(tcp).await.unwrap();
            let _ = connected_tx.send(());
            while let Some(Ok(message)) = socket.next().await {
                if matches!(message, Message::Close(_)) {
                    break;
                }
            }
        });

        let reaper = ProviderReaper::new();
        let mut stream = test_deepgram_stream(&base, Duration::from_secs(5), &reaper);
        stream.send_audio(AudioChunk(vec![9u8; 32])).await.unwrap();
        // The abort under test must land on a live websocket, not on an I/O
        // task that observed cancellation before it ever connected.
        connected_rx.await.unwrap();
        Box::new(stream).abort().await.unwrap();
        assert_eq!(
            reaper.pending(),
            0,
            "abort must await the I/O task itself, leaving nothing for the supervisor"
        );
        server.await.unwrap();
    }

    #[test]
    fn every_restricted_external_child_receives_the_parent_death_contract() {
        let mut child = restricted_command("python3");
        child.args([
            "-c",
            "import ctypes, signal, sys; value = ctypes.c_int(); result = ctypes.CDLL(None).prctl(2, ctypes.byref(value)); sys.exit(result != 0 or value.value != signal.SIGKILL)",
        ]);

        assert!(child.status().unwrap().success());
    }

    #[test]
    fn cancel_set_mid_wait_kills_the_owned_child_within_the_poll_bound() {
        let cancel = CancelRegistry::new();
        let registry = Arc::clone(&cancel);
        let canceller = thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            registry.cancel();
        });
        let started = Instant::now();
        let result = run_restricted_with_deadline(
            "sleep",
            &["5"],
            None,
            false,
            Duration::from_secs(4),
            Some(&cancel),
        );
        canceller.join().unwrap();
        assert!(matches!(result, Err(ProcessError::TimedOut)));
        assert!(
            started.elapsed() < Duration::from_millis(600),
            "a mid-wait cancel must kill within the poll bound, elapsed {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn already_cancelled_operations_fail_fast_without_spawning() {
        let cancel = CancelRegistry::new();
        cancel.cancel();
        let started = Instant::now();
        let result = run_restricted_with_deadline(
            "sleep",
            &["5"],
            None,
            false,
            Duration::from_secs(4),
            Some(&cancel),
        );
        assert!(matches!(result, Err(ProcessError::TimedOut)));
        assert!(
            started.elapsed() < Duration::from_millis(100),
            "an already-cancelled operation must not spawn, elapsed {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn libei_device_must_resume_and_stays_failed_after_removal() {
        let mut link = EiDeviceLink::default();
        assert_eq!(
            link.observe(EiLinkEvent::DeviceAddedWithKeyboard),
            EiLinkDirective::AdoptDevice
        );
        assert!(!link.ready(), "DEVICE_ADDED alone cannot accept events");
        link.observe(EiLinkEvent::DeviceResumed { ours: true });
        assert!(link.ready());
        link.observe(EiLinkEvent::DevicePaused { ours: true });
        assert!(!link.ready());
        link.observe(EiLinkEvent::DeviceResumed { ours: true });
        assert!(link.ready());
        assert_eq!(
            link.observe(EiLinkEvent::DeviceRemoved { ours: true }),
            EiLinkDirective::Fail("libei disconnected")
        );
        assert!(!link.ready());
    }

    #[test]
    fn libei_confirmation_drains_a_synthetic_pong_before_disconnect() {
        let mut confirmation = EiDeliveryConfirmation::default();
        confirmation.observe(EiLinkEvent::Pong { ours: false });
        assert_eq!(confirmation.verdict(), None);
        confirmation.observe(EiLinkEvent::Pong { ours: true });
        confirmation.observe(EiLinkEvent::Disconnect);
        assert_eq!(
            confirmation.verdict(),
            Some(Err("libei disconnected during compositor submission"))
        );
    }

    #[test]
    fn keyboard_paste_resolves_the_v_key_from_the_active_layout_group() {
        use xkbcommon::xkb;

        let context = xkb::Context::new(xkb::CONTEXT_NO_ENVIRONMENT_NAMES);
        let keymap = xkb::Keymap::new_from_names(
            &context,
            "",
            "",
            "us,us",
            ",dvorak",
            Some(String::new()),
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .unwrap();
        let text = keymap.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1);
        let us = resolve_keyboard_paste_keys(text.clone(), 0).unwrap();
        let dvorak = resolve_keyboard_paste_keys(text, 1).unwrap();

        assert_eq!(us.control, dvorak.control);
        assert_ne!(us.paste, dvorak.paste);
    }

    /// A compositor that populates the keymap memfd with `write()` leaves the
    /// shared offset at the end; reading through the file cursor then returned
    /// an empty keymap that libxkbcommon rejected, forcing the clipboard
    /// fallback. The read must not depend on the shared offset.
    #[test]
    fn keymap_fd_reads_the_whole_keymap_regardless_of_the_shared_file_offset() {
        use std::io::{Seek, SeekFrom, Write};
        use xkbcommon::xkb;

        let context = xkb::Context::new(xkb::CONTEXT_NO_ENVIRONMENT_NAMES);
        let source = xkb::Keymap::new_from_names(
            &context,
            "",
            "pc105",
            "us",
            "",
            Some(String::new()),
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .unwrap();
        let expected = source.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1);

        // Mirror the EIS handoff: keymap plus terminating NUL, size counting it.
        let mut payload = expected.clone().into_bytes();
        payload.push(0);
        let size = payload.len();

        // SAFETY: a fresh anonymous descriptor owned by this test.
        let raw = unsafe { libc::memfd_create(c"voisu-keymap-test".as_ptr(), libc::MFD_CLOEXEC) };
        assert!(raw >= 0, "memfd_create failed");
        let mut backing = unsafe { File::from_raw_fd(raw) };
        backing.write_all(&payload).unwrap();

        // The write left the offset at the end — the failing production case.
        assert_eq!(backing.stream_position().unwrap(), size as u64);
        let text = read_keymap_fd(backing.as_raw_fd(), size).unwrap();
        assert_eq!(text, expected);
        assert!(resolve_keyboard_paste_keys(text, 0).is_ok());

        // An offset already at the start stays correct, and the read leaves the
        // shared offset untouched for any later reader.
        backing.seek(SeekFrom::Start(0)).unwrap();
        assert_eq!(read_keymap_fd(backing.as_raw_fd(), size).unwrap(), expected);
        assert_eq!(backing.stream_position().unwrap(), 0);
    }

    #[test]
    fn recording_deadline_defaults_to_ten_minutes_and_survives_past_sixty_seconds() {
        // With no override the Recording Deadline must be generous enough that a
        // routine multi-minute Recording is never killed. Sixty seconds was the
        // old, wrong default that discarded audio before providers ever saw it.
        let default = resolve_recording_deadline(None);
        assert_eq!(default, Duration::from_secs(600));
        assert!(
            default > Duration::from_secs(60),
            "default Recording Deadline must not kill a >60s Recording"
        );

        // A parseable, non-zero override still wins; junk and zero fall back.
        assert_eq!(
            resolve_recording_deadline(Some("5000".to_owned())),
            Duration::from_millis(5000)
        );
        assert_eq!(resolve_recording_deadline(Some("0".to_owned())), default);
        assert_eq!(resolve_recording_deadline(Some("nonsense".to_owned())), default);
    }

    #[test]
    fn libei_text_buffer_is_nul_terminated_and_rejects_interior_nul() {
        let text = libei_text_buffer("Hello, दुनिया!").unwrap();
        assert_eq!(text.as_bytes_with_nul().last(), Some(&0));
        assert!(libei_text_buffer("unsafe\0tail").is_err());
    }
}
