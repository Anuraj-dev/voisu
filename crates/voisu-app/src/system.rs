use std::collections::VecDeque;
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::{FromRawFd, IntoRawFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use voisu_core::{
    socket_path, ActiveCapture, AudioCapture, AudioChunk, BoundaryError, BoundaryFuture,
    BoundaryKind, CancelRegistry, CapturedAudio, Command as DaemonCommand, Credential,
    DeliveryAdapter, DeliveryOutcome, Provider,
    ProviderAuthenticator, ProviderStream, ReadinessCapability, ReadinessFinding,
    MergeResult, ReadinessInspector, ReadinessStatus, ReconciliationKind, ReconciliationModel,
    Request, Response, SecretStore, ShortcutPortal, ShortcutSession, SourceTranscript, Transcript,
    TranscriptDecision, TranscriptDecisionPipeline, TranscriptProvider, TranscriptValidator,
    TriggerKeyBinding, VersionEnvelope, PROTOCOL_VERSION,
};

use crate::process::guard_external_child;

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
const DEEPGRAM_CHUNK_BYTES: usize = 16_000 * 2;
const MAX_DEEPGRAM_IN_FLIGHT: usize = 3;

pub struct FedoraReadiness;

impl ReadinessInspector for FedoraReadiness {
    fn inspect(&mut self) -> Vec<ReadinessFinding> {
        if let Some(value) = std::env::var_os("VOISU_TEST_READINESS") {
            return controlled_readiness(&value.to_string_lossy());
        }
        vec![
            command_finding(
                ReadinessCapability::PipeWire,
                "pw-cli",
                &["info", "0"],
                "PipeWire core responds",
                "start PipeWire and WirePlumber",
            ),
            microphone_finding(),
            command_finding(
                ReadinessCapability::Portals,
                "busctl",
                &["--user", "--no-pager", "status", "org.freedesktop.portal.Desktop"],
                "desktop portal responds",
                "start xdg-desktop-portal in this desktop session",
            ),
            clipboard_finding(),
            secret_service_finding(),
            daemon_finding(),
        ]
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
/// activations (see docs/decisions.md). Every failure — no session bus, portal
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
            return Err(BoundaryError::new(kind, format!(
                "the desktop denied or cancelled the {method} request (response {code})"
            )));
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

            // Tokens are unique per daemon process; the daemon binds at most one
            // Global Shortcuts session per run.
            let unique = std::process::id();
            let session_token = format!("voisu_session_{unique}");
            let create_token = format!("voisu_create_{unique}");
            let bind_token = format!("voisu_bind_{unique}");
            let session_path = format!(
                "/org/freedesktop/portal/desktop/session/{}/{session_token}",
                escaped_sender(&connection)?
            );

            let create_options: std::collections::HashMap<&str, Value<'_>> =
                std::collections::HashMap::from([
                    ("handle_token", Value::from(create_token.as_str())),
                    ("session_handle_token", Value::from(session_token.as_str())),
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
    /// Best-effort, bounded `Session.Close`; runs at most once.
    async fn close(&mut self) {
        if std::mem::replace(&mut self.retired, true) {
            return;
        }
        close_portal_session(&self.connection, self.session_path.as_str()).await;
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
                        None => {
                            // The daemon's own bus connection ended; close what
                            // can still be closed and treat it as revocation.
                            self.close().await;
                            return Ok(ShortcutEvent::Revoked);
                        }
                    },
                    closed = self.closures.next() => {
                        // The desktop closed the session: permission revoked.
                        // Nothing is left to Close on the portal side.
                        let _ = closed;
                        self.retired = true;
                        return Ok(ShortcutEvent::Revoked);
                    }
                    owner_change = self.owner_changes.next() => {
                        let Some(message) = owner_change else {
                            self.close().await;
                            return Ok(ShortcutEvent::Revoked);
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

impl SecretStore for SecretToolStore {
    fn replace(&mut self, provider: Provider, credential: Credential) -> Result<(), BoundaryError> {
        if let Some(mode) = std::env::var_os("VOISU_TEST_SECRET_STORE") {
            return controlled_secret_store(&mode.to_string_lossy());
        }
        let outcome = run_restricted(
            "secret-tool",
            &["store", "--label=Voisu cloud credential", "voisu-provider", provider.secret_service_value()],
            Some(credential.expose_to_boundary().as_bytes()),
            false,
        )
        .map_err(secret_storage_error)?;
        if outcome.success {
            Ok(())
        } else {
            Err(BoundaryError::new(
                BoundaryKind::SecretStorage,
                "secret service denied credential storage",
            ))
        }
    }

    fn load(&mut self, provider: Provider) -> Result<Credential, BoundaryError> {
        if let Some(credential) = std::env::var_os(provider.environment_variable()) {
            return Credential::new(credential.to_string_lossy().into_owned());
        }
        if let Some(mode) = std::env::var_os("VOISU_TEST_SECRET_STORE") {
            if mode == "available" {
                let name = match provider {
                    Provider::Groq => "VOISU_TEST_STORED_GROQ_CREDENTIAL",
                    Provider::Deepgram => "VOISU_TEST_STORED_DEEPGRAM_CREDENTIAL",
                };
                return std::env::var(name)
                    .map_err(|_| BoundaryError::new(BoundaryKind::SecretStorage, "controlled credential missing"))
                    .and_then(Credential::new);
            }
            return controlled_secret_store(&mode.to_string_lossy()).and_then(|()| {
                Err(BoundaryError::new(BoundaryKind::SecretStorage, "controlled credential missing"))
            });
        }
        let outcome = run_restricted(
            "secret-tool",
            &["lookup", "voisu-provider", provider.secret_service_value()],
            None,
            true,
        )
        .map_err(secret_storage_error)?;
        if !outcome.success {
            return Err(BoundaryError::new(
                BoundaryKind::SecretStorage,
                "secret service lookup denied",
            ));
        }
        let credential = String::from_utf8(outcome.stdout).map_err(|_| {
            BoundaryError::new(BoundaryKind::SecretStorage, "secret service returned invalid data")
        })?;
        Credential::new(credential.trim_end().to_owned())
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
    /// Runs the shared authenticated provider request boundary and returns only
    /// its HTTP status. Future Groq transcription can reuse this async boundary
    /// without inheriting credentials or curl configuration from the CLI.
    pub async fn authenticated_status(
        &self,
        credential: Credential,
        request: ProviderHttpRequest,
    ) -> Result<u16, BoundaryError> {
        let result = tokio::task::spawn_blocking(move || authenticated_status(credential, request))
            .await
            .map_err(|_| BoundaryError::new(BoundaryKind::ProviderAuthentication, "provider request task failed"))?;
        result
    }

    pub async fn verify(&self, provider: Provider, credential: Credential) -> Result<(), BoundaryError> {
        let request = match provider {
            Provider::Groq => ProviderHttpRequest {
                url: "https://api.groq.com/openai/v1/models",
                authorization_scheme: "Bearer",
            },
            Provider::Deepgram => ProviderHttpRequest {
                url: "https://api.deepgram.com/v1/projects",
                authorization_scheme: "Token",
            },
        };
        let status = self.authenticated_status(credential, request).await?;
        if (200..300).contains(&status) {
            Ok(())
        } else {
            Err(BoundaryError::new(
                BoundaryKind::ProviderAuthentication,
                "provider returned a non-success HTTP status",
            ))
        }
    }
}

impl ProviderAuthenticator for ProviderHttpClient {
    fn verify(&mut self, provider: Provider, credential: Credential) -> BoundaryFuture<'_, ()> {
        Box::pin(async move {
            let controlled = match provider {
                Provider::Groq => std::env::var_os("VOISU_TEST_AUTH_GROQ"),
                Provider::Deepgram => std::env::var_os("VOISU_TEST_AUTH_DEEPGRAM"),
            };
            if let Some(result) = controlled {
                return if result == "authorized" {
                    Ok(())
                } else {
                    Err(BoundaryError::new(
                        BoundaryKind::ProviderAuthentication,
                        "controlled provider rejected credential",
                    ))
                };
            }
            ProviderHttpClient::verify(&ProviderHttpClient, provider, credential).await
        })
    }
}

fn authenticated_status(
    credential: Credential,
    request: ProviderHttpRequest,
) -> Result<u16, BoundaryError> {
    let credential = curl_config_escape(credential.expose_to_boundary());
    let config = format!(
        "url = \"{}\"\nheader = \"Authorization: {} {credential}\"\n",
        request.url, request.authorization_scheme,
    );
    let outcome = run_restricted(
        "curl",
        &[
            "-q",
            "--config",
            "-",
            "--fail",
            "--silent",
            "--show-error",
            "--output",
            "/dev/null",
            "--write-out",
            "%{http_code}",
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
            "provider rejected credential",
        ));
    }
    let status = std::str::from_utf8(&outcome.stdout)
        .ok()
        .and_then(|value| value.trim().parse::<u16>().ok())
        .ok_or_else(|| {
            BoundaryError::new(BoundaryKind::ProviderAuthentication, "provider returned no HTTP status")
        })?;
    Ok(status)
}

fn curl_config_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn secret_storage_error(error: ProcessError) -> BoundaryError {
    let detail = match error {
        ProcessError::Unavailable => "secret-tool unavailable",
        ProcessError::Input => "secret-tool rejected credential input",
        ProcessError::TimedOut => "secret-tool deadline elapsed",
        ProcessError::Wait | ProcessError::Output => "secret-tool execution failed",
    };
    BoundaryError::new(BoundaryKind::SecretStorage, detail)
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

fn controlled_readiness(value: &str) -> Vec<ReadinessFinding> {
    let mut findings = vec![
        readiness(ReadinessCapability::PipeWire, ReadinessStatus::Pass, "PipeWire core responds"),
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
        let (status, detail) = match status {
            "warn" => (ReadinessStatus::Warn, "needs attention; see remediation"),
            "fail" => (ReadinessStatus::Fail, "not available; see remediation"),
            _ => continue,
        };
        if let Some(finding) = findings.iter_mut().find(|finding| {
            matches!(
                (capability, finding.capability),
                ("pipewire", ReadinessCapability::PipeWire)
                    | ("microphone", ReadinessCapability::Microphone)
                    | ("portals", ReadinessCapability::Portals)
                    | ("clipboard", ReadinessCapability::Clipboard)
                    | ("secret-storage", ReadinessCapability::SecretStorage)
                    | ("daemon", ReadinessCapability::Daemon)
            )
        }) {
            finding.status = status;
            finding.detail = detail.to_owned();
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
        Ok(_) => readiness(
            ReadinessCapability::Microphone,
            ReadinessStatus::Warn,
            "no default microphone; connect one and set it as the default source",
        ),
        Err(_) => readiness(
            ReadinessCapability::Microphone,
            ReadinessStatus::Fail,
            "WirePlumber is unavailable; start PipeWire and WirePlumber",
        ),
    }
}

fn clipboard_finding() -> ReadinessFinding {
    let original = match run_restricted("wl-paste", &["--no-newline"], None, true) {
        Ok(outcome) if outcome.success => outcome.stdout,
        _ => return readiness(
            ReadinessCapability::Clipboard,
            ReadinessStatus::Fail,
            "cannot read the Wayland clipboard; run inside an active Wayland session",
        ),
    };
    let probe = format!("voisu-readiness-{}", std::process::id());
    let copied = run_restricted_serving("wl-copy", &["--"], Some(probe.as_bytes()))
        .is_ok_and(|outcome| outcome.success);
    let observed = run_restricted("wl-paste", &["--no-newline"], None, true)
        .ok()
        .filter(|outcome| outcome.success)
        .map(|outcome| outcome.stdout == probe.as_bytes())
        .unwrap_or(false);
    let restored = run_restricted_serving("wl-copy", &["--"], Some(&original))
        .is_ok_and(|outcome| outcome.success);
    match (copied && observed, restored) {
        (true, true) => readiness(
            ReadinessCapability::Clipboard,
            ReadinessStatus::Pass,
            "clipboard roundtrip succeeds and the prior clipboard was restored",
        ),
        (true, false) => readiness(
            ReadinessCapability::Clipboard,
            ReadinessStatus::Warn,
            "clipboard roundtrip succeeds but the prior clipboard could not be restored",
        ),
        _ => readiness(
            ReadinessCapability::Clipboard,
            ReadinessStatus::Fail,
            "clipboard roundtrip failed; install wl-clipboard and use an active Wayland session",
        ),
    }
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
        Err(_) => readiness(
            ReadinessCapability::SecretStorage,
            ReadinessStatus::Fail,
            "Secret Service is unavailable; start or unlock the desktop keyring",
        ),
    }
}

fn command_finding(
    capability: ReadinessCapability,
    command: &str,
    arguments: &[&str],
    pass_detail: &str,
    fail_detail: &str,
) -> ReadinessFinding {
    let available = run_restricted(command, arguments, None, false)
        .is_ok_and(|outcome| outcome.success);
    readiness(
        capability,
        if available { ReadinessStatus::Pass } else { ReadinessStatus::Fail },
        if available { pass_detail } else { fail_detail },
    )
}

fn daemon_finding() -> ReadinessFinding {
    let result = daemon_status_handshake();
    readiness(
        ReadinessCapability::Daemon,
        if result.is_ok() { ReadinessStatus::Pass } else { ReadinessStatus::Fail },
        if result.is_ok() {
            "status handshake succeeds"
        } else {
            "daemon status handshake failed; start voisu-daemon and run voisu doctor again"
        },
    )
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
    ReadinessFinding { capability, status, detail: detail.to_owned() }
}

fn controlled_secret_store(mode: &str) -> Result<(), BoundaryError> {
    if mode == "available" {
        Ok(())
    } else {
        Err(BoundaryError::new(BoundaryKind::SecretStorage, "controlled secret service denied access"))
    }
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
    let status = wait_for_child(&mut child, started, PROCESS_DEADLINE, None);
    let writer = writer.map(|handle| bounded_join(handle, started, &mut child, PROCESS_DEADLINE));
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

pub struct PipeWireCapture;

struct CaptureReaderState {
    chunks: VecDeque<AudioChunk>,
    received_bytes: usize,
    eof: bool,
    error: Option<String>,
}

impl AudioCapture for PipeWireCapture {
    fn begin(&mut self, _recording_id: u64) -> Result<Box<dyn ActiveCapture>, BoundaryError> {
        let mut command = restricted_command("pw-record");
        command.args([
            "--raw",
            "--rate",
            "16000",
            "--channels",
            "1",
            "--format",
            "s16",
        ]);
        if let Some(target) = std::env::var_os("VOISU_PIPEWIRE_TARGET") {
            command.arg("--target").arg(target);
        }
        command
            .arg("-")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().map_err(|_| {
            BoundaryError::new(BoundaryKind::Capture, "pw-record unavailable")
        })?;
        let mut stdout = child.stdout.take().ok_or_else(|| {
            BoundaryError::new(BoundaryKind::Capture, "pw-record stdout unavailable")
        })?;
        let mut stderr = child.stderr.take().ok_or_else(|| {
            BoundaryError::new(BoundaryKind::Capture, "pw-record stderr unavailable")
        })?;
        let state = Arc::new(Mutex::new(CaptureReaderState {
            chunks: VecDeque::new(),
            received_bytes: 0,
            eof: false,
            error: None,
        }));
        let reader_state = Arc::clone(&state);
        let reader = thread::spawn(move || {
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
                    Err(_) => {
                        let mut state = reader_state.lock().unwrap();
                        state.error = Some("pw-record audio read failed".to_owned());
                        state.eof = true;
                        return;
                    }
                }
            }
        });
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

    fn stop_child(&mut self, graceful: bool) -> Result<Vec<u8>, BoundaryError> {
        let mut child = self.child.take().ok_or_else(|| {
            BoundaryError::new(BoundaryKind::Capture, "pw-record already finalized")
        })?;
        // A tool that already exited before the stop failed on its own; only a
        // process that was still capturing when interrupted may exit nonzero.
        let exited_before_stop = matches!(child.try_wait(), Ok(Some(_)));
        if graceful {
            if let Some(pid) = child.id().try_into().ok() {
                unsafe {
                    libc::kill(pid, libc::SIGINT);
                }
            }
        } else {
            let _ = child.kill();
        }
        let stopped = Instant::now();
        let status = wait_for_child(&mut child, stopped, PROCESS_DEADLINE, None);
        let reader = self
            .reader
            .take()
            .map(|handle| bounded_join(handle, stopped, &mut child, PROCESS_DEADLINE));
        let stderr = self
            .stderr_reader
            .take()
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
            self.stop_child(true)?;
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
            self.stop_child(false)?;
            Ok(())
        })
    }
}

impl Drop for PipeWireActiveCapture {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            reap_briefly(&mut child);
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
}

impl GroqProvider {
    /// Builds a Groq provider whose streams share the actor-owned `reaper`, so a
    /// stream dropped mid-abort hands its curl reap to the supervisor the actor
    /// drains before Idle.
    pub fn new(reaper: ProviderReaper) -> Self {
        Self { reaper }
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
            params: GroqRequestParams::from_config(),
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
    let authority = remainder.split('/').next().unwrap_or_default().to_ascii_lowercase();
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
    /// Resolves the request tuning from config and the shared dictionary:
    /// model from `VOISU_GROQ_MODEL` (default `whisper-large-v3`), language from
    /// `VOISU_GROQ_LANGUAGE` (default `en`), and the Whisper vocabulary prompt.
    fn from_config() -> Self {
        let model = std::env::var("VOISU_GROQ_MODEL")
            .unwrap_or_else(|_| "whisper-large-v3".to_owned());
        let language = std::env::var("VOISU_GROQ_LANGUAGE").unwrap_or_else(|_| "en".to_owned());
        Self {
            model,
            language,
            prompt: crate::dictionary::whisper_prompt(),
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

/// Actor-owned supervisor that keeps every provider-stream cleanup alive and
/// awaitable. When a provider stream is dropped mid-abort — for example the
/// abort deadline elapsed and Tokio dropped the abort future that owned the
/// boxed stream — the stream signals cancellation and hands its still-live chunk
/// tasks here. Adoption is SYNCHRONOUS: it retains the raw handles inside a
/// future without spawning and without touching `Handle::try_current()`, so a
/// stream dropped from any thread — including during runtime teardown — always
/// lands its cleanup in this supervisor. The retained cleanup AWAITS each chunk
/// task (never `abort()`, which would drop the task's nested `spawn_blocking`
/// handle and detach the still-running curl before the child is reaped).
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
    fn adopt<T: Send + 'static>(&self, mut chunks: VecDeque<tokio::task::JoinHandle<T>>) {
        if chunks.is_empty() {
            return;
        }
        self.tasks
            .lock()
            .expect("provider reaper mutex poisoned")
            .push(Box::pin(async move {
                while let Some(chunk) = chunks.pop_front() {
                    let _ = chunk.await;
                }
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
    /// internally bounded (a cancelled curl wait kills and reaps its child
    /// within its own poll bound), so this terminates; the service unit's
    /// explicit TimeoutStopSec is the external last-resort backstop.
    pub async fn drain_to_completion(&self, pass: Duration) {
        while !self.drain(pass).await {
            eprintln!("provider cleanup still draining");
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
}

impl DeepgramProvider {
    /// Builds a Deepgram provider whose streams share the actor-owned `reaper`,
    /// so a stream dropped mid-abort hands its curl reap to the supervisor the
    /// actor drains before Idle.
    pub fn new(reaper: ProviderReaper) -> Self {
        Self { reaper }
    }
}

impl TranscriptProvider for DeepgramProvider {
    fn start(&mut self, _recording_id: u64) -> Result<Box<dyn ProviderStream>, BoundaryError> {
        let credential = SecretStore::load(&mut SecretToolStore, Provider::Deepgram)?;
        let endpoint = std::env::var("VOISU_DEEPGRAM_TRANSCRIPTION_URL").unwrap_or_else(|_| {
            "https://api.deepgram.com/v1/listen?model=nova-3&encoding=linear16&sample_rate=16000&channels=1"
                .to_owned()
        });
        if !provider_endpoint_is_secure(&endpoint) {
            return Err(BoundaryError::new(
                BoundaryKind::Provider,
                "Deepgram transcription endpoint must use HTTPS except on loopback",
            ));
        }
        Ok(Box::new(DeepgramStream {
            credential,
            endpoint,
            buffer: Vec::new(),
            streamed_bytes: 0,
            chunks: VecDeque::new(),
            permits: Arc::new(tokio::sync::Semaphore::new(MAX_DEEPGRAM_IN_FLIGHT)),
            cancel: CancelRegistry::new(),
            reaper: self.reaper.clone(),
        }))
    }
}

struct DeepgramStream {
    credential: Credential,
    endpoint: String,
    buffer: Vec<u8>,
    streamed_bytes: usize,
    chunks: VecDeque<tokio::task::JoinHandle<Result<String, BoundaryError>>>,
    permits: Arc<tokio::sync::Semaphore>,
    cancel: Arc<CancelRegistry>,
    /// Actor-owned supervisor that adopts this stream's chunk tasks if the
    /// stream is dropped mid-abort, so their curl reap is retained and awaited
    /// rather than detached.
    reaper: ProviderReaper,
}

impl Drop for DeepgramStream {
    fn drop(&mut self) {
        // See `Drop for GroqStream`: cancel first, then adopt (await, never
        // abort) so the nested `spawn_blocking` curl is reaped before the
        // reaper task completes and Idle becomes observable.
        self.cancel.cancel();
        self.reaper.adopt(std::mem::take(&mut self.chunks));
    }
}

impl ProviderStream for DeepgramStream {
    fn provider(&self) -> Provider {
        Provider::Deepgram
    }

    fn send_audio(&mut self, chunk: AudioChunk) -> BoundaryFuture<'_, ()> {
        Box::pin(async move {
            self.streamed_bytes = self.streamed_bytes.saturating_add(chunk.0.len());
            self.buffer.extend_from_slice(&chunk.0);
            while self.buffer.len() >= DEEPGRAM_CHUNK_BYTES {
                let pcm = self.buffer.drain(..DEEPGRAM_CHUNK_BYTES).collect();
                let credential = self.credential.clone();
                let endpoint = self.endpoint.clone();
                let cancel = Arc::clone(&self.cancel);
                let permits = Arc::clone(&self.permits);
                self.chunks.push_back(tokio::spawn(async move {
                    let _permit = permits.acquire_owned().await.map_err(|_| {
                        BoundaryError::new(BoundaryKind::Provider, "Deepgram request queue closed")
                    })?;
                    ProviderHttpClient
                        .transcribe_deepgram_chunk(credential, endpoint, pcm, cancel)
                        .await
                }));
            }
            Ok(())
        })
    }

    fn abort(mut self: Box<Self>) -> BoundaryFuture<'static, ()> {
        Box::pin(async move {
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
                    "Deepgram stream exceeded the finalized Recording",
                ));
            }
            self.buffer.extend_from_slice(&pcm[self.streamed_bytes..]);
            if !self.buffer.is_empty() || self.chunks.is_empty() {
                let credential = self.credential.clone();
                let endpoint = self.endpoint.clone();
                let tail = std::mem::take(&mut self.buffer);
                let cancel = Arc::clone(&self.cancel);
                let permits = Arc::clone(&self.permits);
                self.chunks.push_back(tokio::spawn(async move {
                    let _permit = permits.acquire_owned().await.map_err(|_| {
                        BoundaryError::new(BoundaryKind::Provider, "Deepgram request queue closed")
                    })?;
                    ProviderHttpClient
                        .transcribe_deepgram_chunk(credential, endpoint, tail, cancel)
                        .await
                }));
            }
            let mut transcripts = Vec::new();
            // Await the in-flight chunk WITHOUT removing it from `self.chunks`.
            // If this completion future is dropped mid-await (e.g. the Provider
            // Deadline elapses and the coordinator moves to `abort()`), the
            // chunk must still be in the deque so the gated `abort()` awaits and
            // reaps its curl child before Idle is observable. Popping it here
            // would detach that reap and race the Idle transition.
            while let Some(chunk) = self.chunks.front_mut() {
                match await_deepgram_chunk(chunk).await {
                    Ok(transcript) => {
                        self.chunks.pop_front();
                        transcripts.push(transcript);
                    }
                    Err(error) => {
                        // Cancel the siblings so their curl children are killed,
                        // then drop the already-awaited front handle (re-awaiting
                        // a completed JoinHandle panics) and await the rest so
                        // their reaps complete before this error surfaces. Each
                        // sibling is awaited through `front_mut()` and popped only
                        // AFTER its await completes: if the Provider Deadline drops
                        // this future mid-cleanup, the unfinished handles are still
                        // in the deque for the gated `abort()` to own and reap —
                        // draining first would detach them on drop.
                        self.cancel.cancel();
                        self.chunks.pop_front();
                        while let Some(chunk) = self.chunks.front_mut() {
                            let _ = chunk.await;
                            self.chunks.pop_front();
                        }
                        return Err(error);
                    }
                }
            }
            Ok(SourceTranscript {
                provider: Provider::Deepgram,
                text: concatenate_chunk_transcripts(transcripts),
            })
        })
    }
}

async fn await_deepgram_chunk(
    chunk: &mut tokio::task::JoinHandle<Result<String, BoundaryError>>,
) -> Result<String, BoundaryError> {
    chunk.await.map_err(|_| {
        BoundaryError::new(BoundaryKind::Provider, "Deepgram chunk task failed")
    })?
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

impl ClipboardBoundary for WlClipboard {
    fn preserve(&mut self, transcript: &Transcript) -> BoundaryFuture<'_, ()> {
        let text = transcript.0.clone();
        Box::pin(async move {
            let result = tokio::task::spawn_blocking(move || {
                run_restricted_serving("wl-copy", &[], Some(text.as_bytes()))
            })
            .await
            .map_err(|_| {
                BoundaryError::new(BoundaryKind::Delivery, "wl-copy task failed")
            })?;
            match result {
                Ok(outcome) if outcome.success => Ok(()),
                Ok(_outcome) => Err(BoundaryError::new(
                    BoundaryKind::Delivery,
                    "wl-copy rejected the Transcript",
                )),
                Err(ProcessError::TimedOut) => Err(BoundaryError::new(
                    BoundaryKind::Delivery,
                    "wl-copy deadline elapsed",
                )),
                Err(_) => Err(BoundaryError::new(
                    BoundaryKind::Delivery,
                    "wl-copy unavailable or failed",
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
                close_portal_session(&connection, session_path.as_str()).await;
                let error = classify_remote_desktop_failure(error);
                if terminal_remote_desktop_failure(error.diagnostic()) {
                    clear_restore_token();
                }
                return Err(error);
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
                    close_portal_session(&connection, session_path.as_str()).await;
                    let error = classify_remote_desktop_failure(error);
                    if terminal_remote_desktop_failure(error.diagnostic()) {
                        clear_restore_token();
                    }
                    return Err(error);
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
    let file = unsafe { File::from_raw_fd(owned_fd) };
    let mut bytes = Vec::with_capacity(size);
    file.take(size as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| {
            BoundaryError::new(BoundaryKind::Delivery, "active keyboard layout unavailable")
        })?;
    if bytes.last() == Some(&0) {
        bytes.pop();
    }
    String::from_utf8(bytes).map_err(|_| {
        BoundaryError::new(BoundaryKind::Delivery, "active keyboard layout unavailable")
    })
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
    let millis = remaining.as_millis().min(100).max(1) as libc::c_int;
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
    async fn transcribe_deepgram_chunk(
        &self,
        credential: Credential,
        endpoint: String,
        pcm: Vec<u8>,
        cancel: Arc<CancelRegistry>,
    ) -> Result<String, BoundaryError> {
        tokio::task::spawn_blocking(move || {
            request_deepgram_chunk(credential, endpoint, pcm, &cancel)
        })
        .await
        .map_err(|_| BoundaryError::new(BoundaryKind::Provider, "Deepgram request task failed"))?
    }

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

fn request_deepgram_chunk(
    credential: Credential,
    endpoint: String,
    pcm: Vec<u8>,
    cancel: &CancelRegistry,
) -> Result<String, BoundaryError> {
    let mut file = tempfile::Builder::new()
        .prefix("voisu-deepgram-")
        .suffix(".pcm")
        .tempfile()
        .map_err(|_| BoundaryError::new(BoundaryKind::Provider, "temporary audio file unavailable"))?;
    file.write_all(&pcm)
        .and_then(|()| file.flush())
        .map_err(|_| BoundaryError::new(BoundaryKind::Provider, "temporary audio write failed"))?;
    let endpoint = curl_config_escape(&endpoint);
    let credential = curl_config_escape(credential.expose_to_boundary());
    let path = curl_config_escape(&file.path().to_string_lossy());
    let config = format!(
        "url = \"{endpoint}\"\nheader = \"Authorization: Token {credential}\"\nheader = \"Content-Type: audio/raw\"\ndata-binary = \"@{path}\"\n"
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
            "15",
        ],
        Some(config.as_bytes()),
        true,
        PROVIDER_PROCESS_DEADLINE,
        Some(cancel),
    )
    .map_err(|error| match error {
        ProcessError::TimedOut => {
            BoundaryError::new(BoundaryKind::Provider, "Deepgram Provider Deadline elapsed")
        }
        _ => BoundaryError::new(
            BoundaryKind::Provider,
            "Deepgram request unavailable or failed",
        ),
    })?;
    if !outcome.success {
        return Err(BoundaryError::new(
            BoundaryKind::Provider,
            "Deepgram rejected the audio request",
        ));
    }
    let response: serde_json::Value = serde_json::from_slice(&outcome.stdout).map_err(|_| {
        BoundaryError::new(BoundaryKind::Provider, "Deepgram returned malformed JSON")
    })?;
    response
        .pointer("/results/channels/0/alternatives/0/transcript")
        .and_then(|text| text.as_str())
        .map(str::to_owned)
        .ok_or_else(|| {
            BoundaryError::new(BoundaryKind::Provider, "Deepgram response omitted text")
        })
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
        "url = \"{endpoint}\"\nheader = \"Authorization: Bearer {credential}\"\nform = \"file=@{path};filename=recording.wav;type=audio/wav\"\nform = \"model={model}\"\nform = \"response_format=json\"\nform = \"temperature=0\"\n"
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
        .suffix(".wav")
        .tempfile()
        .map_err(|_| BoundaryError::new(BoundaryKind::Provider, "temporary audio file unavailable"))?;
    let wav = wav_from_pcm(&pcm)?;
    file.write_all(&wav)
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

fn concatenate_chunk_transcripts(transcripts: Vec<String>) -> String {
    transcripts
        .into_iter()
        .flat_map(|transcript| {
            transcript
                .split_whitespace()
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn wav_from_pcm(pcm: &[u8]) -> Result<Vec<u8>, BoundaryError> {
    let data_len = u32::try_from(pcm.len()).map_err(|_| {
        BoundaryError::new(BoundaryKind::Provider, "Recording is too large for WAV")
    })?;
    let riff_len = data_len.checked_add(36).ok_or_else(|| {
        BoundaryError::new(BoundaryKind::Provider, "Recording WAV length overflow")
    })?;
    let mut wav = Vec::with_capacity(pcm.len() + 44);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&riff_len.to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16_u32.to_le_bytes());
    wav.extend_from_slice(&1_u16.to_le_bytes());
    wav.extend_from_slice(&1_u16.to_le_bytes());
    wav.extend_from_slice(&16_000_u32.to_le_bytes());
    wav.extend_from_slice(&32_000_u32.to_le_bytes());
    wav.extend_from_slice(&2_u16.to_le_bytes());
    wav.extend_from_slice(&16_u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    wav.extend_from_slice(pcm);
    Ok(wav)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    use voisu_core::{ProviderCoordinator, ProviderStreams};

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
        let streams = ProviderStreams {
            deepgram: Box::new(DeepgramStream {
                credential: credential.clone(),
                endpoint: "http://localhost/deepgram".to_owned(),
                buffer: Vec::new(),
                streamed_bytes: 0,
                chunks: VecDeque::from([deepgram_chunk]),
                permits: Arc::new(tokio::sync::Semaphore::new(MAX_DEEPGRAM_IN_FLIGHT)),
                cancel: deepgram_cancel,
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
    fn non_overlapping_deepgram_chunks_keep_a_repeated_boundary_word() {
        assert_eq!(
            concatenate_chunk_transcripts(vec!["that was very".to_owned(), "very good".to_owned()]),
            "that was very very good"
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
