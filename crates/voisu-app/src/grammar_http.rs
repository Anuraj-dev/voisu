//! Async Minimal Grammar HTTP transport (Smart Writing SW5 / specification §7.3).
//!
//! Owns one process-scoped [`reqwest::Client`] built **without** default TLS
//! features and with **Rustls + JSON** only. Per-request futures are polled by
//! the caller (Final Transform Gate / SW10); dropping a future must leave zero
//! per-request work on the real client stack.
//!
//! Out of scope here (other tickets):
//! - prompt / JSON Schema body mapping (SW6)
//! - credential preparation / reaper lane (SW7)
//! - gate wiring into `process_recording` (SW10)
//!
//! Forbidden on this path: curl, subprocesses, `spawn_blocking`, per-request
//! `tokio::spawn`, retry/backoff sleeps, and in-request credential load.

use std::fmt;
use std::sync::Once;
use std::time::Duration;

use crate::system::{endpoint_authority_is_allowed, parsed_host_is_loopback};

/// Exact production chat-completions endpoint
/// (`MINIMAL_GRAMMAR_ENDPOINT` in the constants companion).
pub const MINIMAL_GRAMMAR_ENDPOINT: &str = "https://api.groq.com/openai/v1/chat/completions";

/// Per-request wall budget for the grammar HTTP call
/// (`GRAMMAR_HTTP_REQUEST_DEADLINE` = 700 ms).
pub const GRAMMAR_HTTP_REQUEST_DEADLINE: Duration = Duration::from_millis(700);

/// Maximum accepted response body size in bytes
/// (`MAX_GRAMMAR_RESPONSE_BYTES` = 64 KiB).
pub const MAX_GRAMMAR_RESPONSE_BYTES: usize = 65_536;

/// Retries are forbidden (`MINIMAL_GRAMMAR_REQUEST_RETRIES` = 0).
pub const MINIMAL_GRAMMAR_REQUEST_RETRIES: u32 = 0;

// Compile-time lock: transport never gains a retry budget by accident.
const _: () = assert!(MINIMAL_GRAMMAR_REQUEST_RETRIES == 0);

/// Closed transport failure reasons for SW6/SW8/SW10 fallback mapping.
///
/// Secrets must never appear in these messages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrammarHttpError {
    /// `reqwest::Client` could not be constructed.
    ClientBuild,
    /// Endpoint rejected (non-HTTPS non-loopback, empty, or control characters).
    InvalidEndpoint,
    /// Request exceeded the configured deadline.
    Timeout,
    /// Connect / write / read / protocol failure before a usable status.
    Transport,
    /// Response status was not HTTP 200 (includes 429 / 5xx → local fallback).
    NonSuccessStatus { status: u16 },
    /// Body exceeded [`MAX_GRAMMAR_RESPONSE_BYTES`] (or the configured limit).
    BodyTooLarge { limit: usize },
}

impl fmt::Display for GrammarHttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ClientBuild => write!(f, "grammar HTTP client build failed"),
            Self::InvalidEndpoint => write!(f, "grammar HTTP endpoint is not allowed"),
            Self::Timeout => write!(f, "grammar HTTP request timed out"),
            Self::Transport => write!(f, "grammar HTTP transport error"),
            Self::NonSuccessStatus { status } => {
                write!(f, "grammar HTTP non-success status {status}")
            }
            Self::BodyTooLarge { limit } => {
                write!(f, "grammar HTTP response exceeded {limit} bytes")
            }
        }
    }
}

impl std::error::Error for GrammarHttpError {}

/// Successful transport outcome: HTTP 200 with a bounded body.
///
/// The body is still untrusted provider bytes; SW4/SW6 parse and validate it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrammarHttpSuccess {
    pub body: Vec<u8>,
}

/// Process-owned Minimal Grammar HTTP client.
///
/// Clone is cheap (`reqwest::Client` is `Arc`-backed). SW10 stores one instance
/// inside a ready capability and reuses it; the Final Transform Gate must not
/// construct a new client per Recording.
#[derive(Clone, Debug)]
pub struct GrammarHttpClient {
    client: reqwest::Client,
    endpoint: String,
    request_timeout: Duration,
    max_body_bytes: usize,
}

impl GrammarHttpClient {
    /// Production client: fixed HTTPS endpoint, 700 ms request budget, 64 KiB body.
    pub fn production() -> Result<Self, GrammarHttpError> {
        Self::build(
            MINIMAL_GRAMMAR_ENDPOINT.to_owned(),
            GRAMMAR_HTTP_REQUEST_DEADLINE,
            MAX_GRAMMAR_RESPONSE_BYTES,
        )
    }

    /// Inject a loopback (or other allowed) endpoint through the constructor.
    ///
    /// Tests use this instead of a general production environment override.
    pub fn with_endpoint(endpoint: impl Into<String>) -> Result<Self, GrammarHttpError> {
        Self::build(
            endpoint.into(),
            GRAMMAR_HTTP_REQUEST_DEADLINE,
            MAX_GRAMMAR_RESPONSE_BYTES,
        )
    }

    /// Full constructor for tests that need a non-default timeout or body cap.
    pub fn with_config(
        endpoint: impl Into<String>,
        request_timeout: Duration,
        max_body_bytes: usize,
    ) -> Result<Self, GrammarHttpError> {
        Self::build(endpoint.into(), request_timeout, max_body_bytes)
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    pub fn request_timeout(&self) -> Duration {
        self.request_timeout
    }

    pub fn max_body_bytes(&self) -> usize {
        self.max_body_bytes
    }

    /// One non-streaming JSON POST. Returns only on HTTP 200 with a bounded body.
    ///
    /// The returned future is request-scoped and drop-safe: dropping it cancels
    /// the in-flight request on the real `reqwest` stack and must leave zero
    /// gate-owned residual request work. The bearer token is already-ready;
    /// this method never loads credentials.
    ///
    /// `body` is opaque transport JSON (SW6 owns prompt/schema mapping and
    /// builds the value). There is no retry, backoff sleep, `spawn_blocking`,
    /// curl, subprocess, or per-request `tokio::spawn` on this path.
    pub async fn post_json(
        &self,
        bearer_token: &str,
        body: &serde_json::Value,
    ) -> Result<GrammarHttpSuccess, GrammarHttpError> {
        // Single attempt only — MINIMAL_GRAMMAR_REQUEST_RETRIES is locked to 0.
        let response = self
            .client
            .post(&self.endpoint)
            .timeout(self.request_timeout)
            .header(
                reqwest::header::AUTHORIZATION,
                format!("Bearer {bearer_token}"),
            )
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(reqwest::header::ACCEPT, "application/json")
            .json(body)
            .send()
            .await
            .map_err(map_reqwest_error)?;

        Self::finish_response(response, self.max_body_bytes).await
    }

    /// Test-only: same transport path as [`Self::post_json`], but with a caller-
    /// owned body (used to prove mid-upload cancel with an explicit unsent
    /// remainder under test control).
    #[cfg(test)]
    async fn post_transport_body(
        &self,
        bearer_token: &str,
        body: reqwest::Body,
    ) -> Result<GrammarHttpSuccess, GrammarHttpError> {
        let response = self
            .client
            .post(&self.endpoint)
            .timeout(self.request_timeout)
            .header(
                reqwest::header::AUTHORIZATION,
                format!("Bearer {bearer_token}"),
            )
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(reqwest::header::ACCEPT, "application/json")
            .body(body)
            .send()
            .await
            .map_err(map_reqwest_error)?;

        Self::finish_response(response, self.max_body_bytes).await
    }

    async fn finish_response(
        response: reqwest::Response,
        max_body_bytes: usize,
    ) -> Result<GrammarHttpSuccess, GrammarHttpError> {
        let status = response.status();
        if status != reqwest::StatusCode::OK {
            // Drop the response without streaming the body; connection work ends.
            return Err(GrammarHttpError::NonSuccessStatus {
                status: status.as_u16(),
            });
        }

        let body = read_bounded_body(response, max_body_bytes).await?;
        Ok(GrammarHttpSuccess { body })
    }

    fn build(
        endpoint: String,
        request_timeout: Duration,
        max_body_bytes: usize,
    ) -> Result<Self, GrammarHttpError> {
        if !endpoint_is_allowed(&endpoint) {
            return Err(GrammarHttpError::InvalidEndpoint);
        }
        if request_timeout.is_zero() || max_body_bytes == 0 {
            return Err(GrammarHttpError::InvalidEndpoint);
        }
        ensure_rustls_ring_provider();
        let client = reqwest::Client::builder()
            // No default native-TLS stack: features are rustls-no-provider + json.
            .timeout(request_timeout)
            .connect_timeout(request_timeout)
            // Grammar is one optional request per eligible Recording; keep the
            // idle pool small. The pool may outlive a dropped request; per-request
            // work must not.
            .pool_max_idle_per_host(2)
            .build()
            .map_err(|_| GrammarHttpError::ClientBuild)?;
        Ok(Self {
            client,
            endpoint,
            request_timeout,
            max_body_bytes,
        })
    }
}

fn ensure_rustls_ring_provider() {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        // Align with the rest of voisu-app (rustls + ring). Ignore AlreadyInstalled
        // when another subsystem installed a provider first.
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// Production HTTPS always; plain HTTP only on loopback (test injection). The
/// URL is parsed so the loopback decision is the real host, never a prefix of
/// the raw authority — `http://localhost:8080@attacker.example/` is attacker
/// .example carrying userinfo, not loopback.
fn endpoint_is_allowed(endpoint: &str) -> bool {
    if endpoint.is_empty() || endpoint.contains(['\n', '\r', '\0']) {
        return false;
    }
    let Ok(url) = url::Url::parse(endpoint) else {
        return false;
    };
    endpoint_authority_is_allowed(&url)
        && match url.scheme() {
            "https" => true,
            "http" => parsed_host_is_loopback(&url),
            _ => false,
        }
}

fn map_reqwest_error(error: reqwest::Error) -> GrammarHttpError {
    if error.is_timeout() {
        GrammarHttpError::Timeout
    } else {
        GrammarHttpError::Transport
    }
}

async fn read_bounded_body(
    mut response: reqwest::Response,
    max_body_bytes: usize,
) -> Result<Vec<u8>, GrammarHttpError> {
    if let Some(content_length) = response.content_length() {
        if content_length as usize > max_body_bytes {
            // Drop response: cancel residual body download on the real stack.
            return Err(GrammarHttpError::BodyTooLarge {
                limit: max_body_bytes,
            });
        }
    }

    let mut body = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                let next = body.len().saturating_add(chunk.len());
                if next > max_body_bytes {
                    return Err(GrammarHttpError::BodyTooLarge {
                        limit: max_body_bytes,
                    });
                }
                body.extend_from_slice(&chunk);
            }
            Ok(None) => break,
            Err(error) => return Err(map_reqwest_error(error)),
        }
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::task::{Context, Poll};
    use std::time::Instant;

    // Note: drop tests use Box::pin so cancelling owns the future (see assert_drop_cancels).

    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::sync::oneshot;
    use tokio::time::{sleep, timeout};

    /// Shared counters for §11.2 gate (1): residual per-request work must be 0.
    #[derive(Default)]
    struct ServerProbe {
        /// Handlers currently between accept-start and full exit.
        active_handlers: AtomicUsize,
        /// Total request lines fully observed.
        requests_seen: AtomicUsize,
        /// Bytes the server still read *after* the client drop barrier.
        bytes_after_drop: AtomicU64,
        /// Set when the server has entered the hang phase of the scenario.
        hang_entered: AtomicUsize,
        /// Request body bytes the Upload hang mode intentionally observed
        /// *at hang entry* (frozen; never updated by post-cancel drain).
        upload_body_bytes: AtomicU64,
        /// Declared Content-Length from the Upload hang request headers.
        upload_content_length: AtomicU64,
        /// Hang handlers that observed peer EOF/reset on the live socket
        /// (not exit by independently dropping the connection).
        client_eof_observed: AtomicUsize,
    }

    impl ServerProbe {
        fn active(&self) -> usize {
            self.active_handlers.load(Ordering::SeqCst)
        }

        fn requests(&self) -> usize {
            self.requests_seen.load(Ordering::SeqCst)
        }

        fn upload_body_bytes(&self) -> u64 {
            self.upload_body_bytes.load(Ordering::SeqCst)
        }

        fn upload_content_length(&self) -> u64 {
            self.upload_content_length.load(Ordering::SeqCst)
        }

        fn client_eof_observed(&self) -> usize {
            self.client_eof_observed.load(Ordering::SeqCst)
        }
    }

    #[derive(Clone, Copy)]
    enum HangMode {
        /// Mid-upload: partial body + stall (no further drain) until cancel.
        Upload,
        /// Fully read request; never send response headers.
        Wait,
        /// Send 200 headers + partial body; stall before finishing the body.
        PartialBody,
    }

    #[derive(Clone, Copy)]
    enum ReplyMode {
        OkJson(&'static str),
        Status(u16, &'static str),
        Oversize {
            declare_length: usize,
            send_bytes: usize,
        },
        Hang(HangMode),
    }

    async fn bind_loopback() -> (TcpListener, String) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let addr = listener.local_addr().expect("local addr");
        (
            listener,
            format!("http://{addr}/openai/v1/chat/completions"),
        )
    }

    /// Controllable HTTP/1.1 loopback using the **real** TCP path (not a reqwest
    /// mock). Production-boundary cancel proof requires this stack.
    async fn spawn_http_server(
        listener: TcpListener,
        probe: Arc<ServerProbe>,
        reply: ReplyMode,
        mut drop_barrier: Option<oneshot::Receiver<()>>,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                let Ok((socket, _)) = listener.accept().await else {
                    break;
                };
                let probe = Arc::clone(&probe);
                let drop_rx = drop_barrier.take();
                tokio::spawn(async move {
                    handle_connection(socket, probe, reply, drop_rx).await;
                });
            }
        })
    }

    async fn handle_connection(
        mut socket: TcpStream,
        probe: Arc<ServerProbe>,
        reply: ReplyMode,
        drop_rx: Option<oneshot::Receiver<()>>,
    ) {
        probe.active_handlers.fetch_add(1, Ordering::SeqCst);
        // Ensure residual work is zero when the handler exits for any reason.
        struct ActiveGuard<'a>(&'a AtomicUsize);
        impl Drop for ActiveGuard<'_> {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::SeqCst);
            }
        }
        let _active = ActiveGuard(&probe.active_handlers);

        match reply {
            ReplyMode::Hang(HangMode::Upload) => {
                // Mid-upload proof (§11.2 / Sol r3):
                // 1) Read a verified partial body only (never drain-to-CL).
                // 2) STALL — no further reads — until the test signals cancel.
                //    body_seen is frozen at hang entry. Continued draining would
                //    make body_seen < CL vacuous (the rest may already have left
                //    the client into socket buffers).
                // 3) After cancel, keep the live socket open and exit only on
                //    peer EOF/reset so active_handlers==0 is client-caused.
                //
                // Client-side unsent remainder is proved by a gated stream body
                // in the mid-upload test (explicit remaining bytes under test
                // control). No SO_RCVBUF zero-window: that blocks client
                // teardown so the server never observes EOF/reset.
                let partial =
                    read_verified_partial_body(&mut socket, UPLOAD_HANG_MAX_BODY_OBSERVE).await;
                probe
                    .upload_body_bytes
                    .store(partial.body_bytes as u64, Ordering::SeqCst);
                probe
                    .upload_content_length
                    .store(partial.content_length as u64, Ordering::SeqCst);
                if partial.content_length > 0
                    && partial.body_bytes > 0
                    && partial.body_bytes < partial.content_length
                {
                    probe.hang_entered.fetch_add(1, Ordering::SeqCst);
                }
                // Stall: barrier only marks cancel epoch. Must not close the
                // socket here — observe peer terminal next.
                if let Some(rx) = drop_rx {
                    let _ = rx.await;
                }
                observe_peer_close_after_cancel(&mut socket, &probe).await;
            }
            ReplyMode::Hang(HangMode::Wait) => {
                let _ = read_http_request(&mut socket).await;
                probe.requests_seen.fetch_add(1, Ordering::SeqCst);
                probe.hang_entered.fetch_add(1, Ordering::SeqCst);
                // Hold the connection open until the client drops (EOF).
                drain_until_eof_counting_after_drop(&mut socket, &probe, drop_rx).await;
            }
            ReplyMode::Hang(HangMode::PartialBody) => {
                let _ = read_http_request(&mut socket).await;
                probe.requests_seen.fetch_add(1, Ordering::SeqCst);
                let header = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 65536\r\nConnection: close\r\n\r\n";
                if socket.write_all(header.as_bytes()).await.is_err() {
                    return;
                }
                // Partial body so the client is mid-response when we hang.
                let partial = vec![b'x'; 1024];
                if socket.write_all(&partial).await.is_err() {
                    return;
                }
                let _ = socket.flush().await;
                probe.hang_entered.fetch_add(1, Ordering::SeqCst);
                // Stay open until drop; further writes would be residual work if
                // they continued after the client cancelled — we do not write more.
                drain_until_eof_counting_after_drop(&mut socket, &probe, drop_rx).await;
            }
            ReplyMode::OkJson(body) => {
                let _ = read_http_request(&mut socket).await;
                probe.requests_seen.fetch_add(1, Ordering::SeqCst);
                write_response(&mut socket, 200, body).await;
            }
            ReplyMode::Status(code, body) => {
                let _ = read_http_request(&mut socket).await;
                probe.requests_seen.fetch_add(1, Ordering::SeqCst);
                write_response(&mut socket, code, body).await;
            }
            ReplyMode::Oversize {
                declare_length,
                send_bytes,
            } => {
                let _ = read_http_request(&mut socket).await;
                probe.requests_seen.fetch_add(1, Ordering::SeqCst);
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {declare_length}\r\nConnection: close\r\n\r\n"
                );
                if socket.write_all(header.as_bytes()).await.is_err() {
                    return;
                }
                let chunk = vec![b'y'; send_bytes.min(declare_length)];
                let _ = socket.write_all(&chunk).await;
            }
        }
    }

    /// Max body bytes the Upload hang intentionally observes (must stay ≪ request CL).
    const UPLOAD_HANG_MAX_BODY_OBSERVE: usize = 512;
    /// Declared Content-Length / gated-body total for the mid-upload cancel proof.
    /// First frame is small; the remainder is held under test control and never
    /// yielded before cancel — so unsent client payload is explicit, not inferred
    /// from socket buffers.
    const UPLOAD_HANG_BODY_TOTAL: usize = 256 * 1024;
    /// First gated body frame size (must be ≥ server partial observe cap so the
    /// hang can enter without needing further client frames).
    const UPLOAD_HANG_BODY_FIRST_FRAME: usize = 1024;

    struct PartialBodyObservation {
        body_bytes: usize,
        content_length: usize,
    }

    /// Read headers + at most `max_body_observe` body bytes. Never drains to
    /// Content-Length — hang entry must remain a partial-body observation.
    async fn read_verified_partial_body(
        socket: &mut TcpStream,
        max_body_observe: usize,
    ) -> PartialBodyObservation {
        let mut buf = Vec::with_capacity(4096);
        // Small chunks so a single read cannot swallow the whole body even if
        // the kernel buffer still holds more than max_body_observe.
        let mut tmp = [0u8; 256];
        loop {
            let n = match socket.read(&mut tmp).await {
                Ok(0) => break,
                Ok(n) => n,
                Err(_) => break,
            };
            buf.extend_from_slice(&tmp[..n]);
            if let Some(header_end) = find_header_end(&buf) {
                let content_length = parse_content_length(&buf[..header_end]).unwrap_or(0);
                let mut body_bytes = buf.len().saturating_sub(header_end);
                // Cap intentional observation strictly below Content-Length when known.
                let observe_cap = match content_length {
                    0 => max_body_observe,
                    cl => max_body_observe.min(cl.saturating_sub(1)),
                };
                while body_bytes < observe_cap {
                    let want = (observe_cap - body_bytes).min(tmp.len());
                    match socket.read(&mut tmp[..want]).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            buf.extend_from_slice(&tmp[..n]);
                            body_bytes += n;
                        }
                    }
                }
                return PartialBodyObservation {
                    body_bytes,
                    content_length,
                };
            }
            if buf.len() > 64 * 1024 {
                break;
            }
        }
        PartialBodyObservation {
            body_bytes: 0,
            content_length: 0,
        }
    }

    /// After client cancel, keep the live socket open and read until peer
    /// EOF/reset. Returning without this would drop the server socket first and
    /// would not prove cancellation caused quiescence.
    async fn observe_peer_close_after_cancel(socket: &mut TcpStream, probe: &ServerProbe) {
        let mut buf = [0u8; 4096];
        loop {
            match socket.read(&mut buf).await {
                Ok(0) | Err(_) => {
                    probe.client_eof_observed.fetch_add(1, Ordering::SeqCst);
                    break;
                }
                Ok(n) => {
                    // Kernel-buffered remainder that arrived with/before the close.
                    probe.bytes_after_drop.fetch_add(n as u64, Ordering::SeqCst);
                }
            }
        }
    }

    /// Keep reading the live peer until EOF/reset. The optional drop barrier
    /// only flips residual-byte accounting — it never closes the server socket
    /// or ends the handler. Exit therefore requires observing client cancel
    /// (or peer error), not independent server close.
    async fn drain_until_eof_counting_after_drop(
        socket: &mut TcpStream,
        probe: &ServerProbe,
        drop_rx: Option<oneshot::Receiver<()>>,
    ) {
        let mut drop_rx = drop_rx;
        let mut dropped = drop_rx.is_none();
        let mut buf = [0u8; 4096];
        loop {
            if !dropped {
                if let Some(rx) = drop_rx.as_mut() {
                    // Wait on either more socket data or the drop signal.
                    // Barrier alone never terminates this loop.
                    tokio::select! {
                        biased;
                        msg = rx => {
                            let _ = msg;
                            dropped = true;
                            drop_rx = None;
                        }
                        read = socket.read(&mut buf) => {
                            match read {
                                Ok(0) | Err(_) => {
                                    probe
                                        .client_eof_observed
                                        .fetch_add(1, Ordering::SeqCst);
                                    break;
                                }
                                Ok(n) => {
                                    // Still before client drop — not residual.
                                    let _ = n;
                                }
                            }
                        }
                    }
                }
            } else {
                // Reuse the post-cancel observation path so Wait / PartialBody
                // and Upload share the same EOF/reset proof.
                observe_peer_close_after_cancel(socket, probe).await;
                break;
            }
        }
    }

    async fn read_http_request(socket: &mut TcpStream) -> Vec<u8> {
        let mut buf = Vec::with_capacity(4096);
        let mut tmp = [0u8; 1024];
        // Read until header terminator, then Content-Length body if present.
        loop {
            let n = match socket.read(&mut tmp).await {
                Ok(0) => break,
                Ok(n) => n,
                Err(_) => break,
            };
            buf.extend_from_slice(&tmp[..n]);
            if let Some(header_end) = find_header_end(&buf) {
                let headers = &buf[..header_end];
                let content_length = parse_content_length(headers).unwrap_or(0);
                let body_start = header_end;
                while buf.len() < body_start + content_length {
                    let n = match socket.read(&mut tmp).await {
                        Ok(0) => break,
                        Ok(n) => n,
                        Err(_) => break,
                    };
                    buf.extend_from_slice(&tmp[..n]);
                }
                break;
            }
            if buf.len() > 1024 * 1024 {
                break;
            }
        }
        buf
    }

    fn find_header_end(buf: &[u8]) -> Option<usize> {
        buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4)
    }

    fn parse_content_length(headers: &[u8]) -> Option<usize> {
        let text = std::str::from_utf8(headers).ok()?;
        for line in text.split("\r\n") {
            let lower = line.to_ascii_lowercase();
            if let Some(rest) = lower.strip_prefix("content-length:") {
                return rest.trim().parse().ok();
            }
        }
        None
    }

    async fn write_response(socket: &mut TcpStream, status: u16, body: &str) {
        let reason = match status {
            200 => "OK",
            429 => "Too Many Requests",
            500 => "Internal Server Error",
            _ => "Error",
        };
        let response = format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = socket.write_all(response.as_bytes()).await;
        let _ = socket.flush().await;
    }

    fn sample_grammar_request_body() -> serde_json::Value {
        // Minimal transport payload shaped like the SW6 contract; mapping itself
        // is not implemented here — only that a strict-schema-style JSON POST works.
        json!({
            "model": "openai/gpt-oss-20b",
            "stream": false,
            "reasoning_effort": "low",
            "max_completion_tokens": 2048,
            "response_format": {
                "type": "json_schema",
                "json_schema": {
                    "name": "minimal_grammar_edits",
                    "strict": true,
                    "schema": {
                        "type": "object",
                        "additionalProperties": false,
                        "required": ["edits"],
                        "properties": {
                            "edits": { "type": "array" }
                        }
                    }
                }
            },
            "messages": [
                {
                    "role": "system",
                    "content": "emit only localized edits"
                },
                {
                    "role": "user",
                    "content": "hello world"
                }
            ]
        })
    }

    async fn wait_until(cond: impl Fn() -> bool, limit: Duration) {
        let start = Instant::now();
        while !cond() {
            if start.elapsed() > limit {
                panic!("condition not met within {limit:?}");
            }
            sleep(Duration::from_millis(5)).await;
        }
    }

    /// Poll a pinned future until `pred` is true, without completing it.
    async fn drive_until<F>(fut: &mut Pin<Box<F>>, pred: impl Fn() -> bool, limit: Duration)
    where
        F: Future,
    {
        let start = Instant::now();
        while !pred() {
            if start.elapsed() > limit {
                panic!("future did not progress to hang within {limit:?}");
            }
            std::future::poll_fn(|cx: &mut Context<'_>| {
                let _ = fut.as_mut().poll(cx);
                Poll::Ready(())
            })
            .await;
            sleep(Duration::from_millis(2)).await;
        }
    }

    #[test]
    fn production_endpoint_and_retry_constants_match_companion() {
        assert_eq!(
            MINIMAL_GRAMMAR_ENDPOINT,
            "https://api.groq.com/openai/v1/chat/completions"
        );
        assert_eq!(GRAMMAR_HTTP_REQUEST_DEADLINE, Duration::from_millis(700));
        assert_eq!(MAX_GRAMMAR_RESPONSE_BYTES, 65_536);
        assert_eq!(MINIMAL_GRAMMAR_REQUEST_RETRIES, 0);
    }

    #[test]
    fn production_client_builds_with_rustls_stack() {
        let client = GrammarHttpClient::production().expect("production client");
        assert_eq!(client.endpoint(), MINIMAL_GRAMMAR_ENDPOINT);
        assert_eq!(client.request_timeout(), GRAMMAR_HTTP_REQUEST_DEADLINE);
        assert_eq!(client.max_body_bytes(), MAX_GRAMMAR_RESPONSE_BYTES);
    }

    #[test]
    fn rejects_non_loopback_http_and_control_characters() {
        assert!(matches!(
            GrammarHttpClient::with_endpoint("http://example.com/v1"),
            Err(GrammarHttpError::InvalidEndpoint)
        ));
        assert!(matches!(
            GrammarHttpClient::with_endpoint("https://api.groq.com/openai/v1/chat/completions\n"),
            Err(GrammarHttpError::InvalidEndpoint)
        ));
        assert!(GrammarHttpClient::with_endpoint("http://127.0.0.1:9/x").is_ok());
        assert!(GrammarHttpClient::with_endpoint("http://localhost:9/x").is_ok());
        // The policy parses the URL: userinfo smuggling and lookalike suffixes
        // must fail even when the raw authority prefix looks trusted.
        assert!(matches!(
            GrammarHttpClient::with_endpoint("http://localhost:8080@attacker.example/v1"),
            Err(GrammarHttpError::InvalidEndpoint)
        ));
        assert!(matches!(
            GrammarHttpClient::with_endpoint("https://user:pass@api.groq.com@attacker.example/v1"),
            Err(GrammarHttpError::InvalidEndpoint)
        ));
        assert!(matches!(
            GrammarHttpClient::with_endpoint("http://localhost.attacker.example/v1"),
            Err(GrammarHttpError::InvalidEndpoint)
        ));
    }

    #[tokio::test]
    async fn post_json_accepts_http_200_bounded_body() {
        let probe = Arc::new(ServerProbe::default());
        let (listener, endpoint) = bind_loopback().await;
        let server = spawn_http_server(
            listener,
            Arc::clone(&probe),
            ReplyMode::OkJson(r#"{"choices":[{"message":{"content":"{\"edits\":[]}"}}]}"#),
            None,
        )
        .await;

        let client = GrammarHttpClient::with_endpoint(endpoint).unwrap();
        let result = client
            .post_json("already-ready-token", &sample_grammar_request_body())
            .await
            .expect("200 body");
        assert!(result.body.starts_with(b"{\"choices\""));
        assert_eq!(probe.requests(), 1);

        server.abort();
    }

    #[tokio::test]
    async fn post_json_rejects_non_200_without_retry() {
        let probe = Arc::new(ServerProbe::default());
        let (listener, endpoint) = bind_loopback().await;
        let server = spawn_http_server(
            listener,
            Arc::clone(&probe),
            ReplyMode::Status(429, r#"{"error":"rate_limited"}"#),
            None,
        )
        .await;

        let client = GrammarHttpClient::with_endpoint(endpoint).unwrap();
        let err = client
            .post_json("already-ready-token", &sample_grammar_request_body())
            .await
            .expect_err("429 must not succeed");
        assert_eq!(err, GrammarHttpError::NonSuccessStatus { status: 429 });
        // No retry: the loopback server saw exactly one request.
        assert_eq!(probe.requests(), 1);

        server.abort();
    }

    #[tokio::test]
    async fn post_json_rejects_declared_oversize_body() {
        let probe = Arc::new(ServerProbe::default());
        let (listener, endpoint) = bind_loopback().await;
        let server = spawn_http_server(
            listener,
            Arc::clone(&probe),
            ReplyMode::Oversize {
                declare_length: MAX_GRAMMAR_RESPONSE_BYTES + 1,
                send_bytes: 64,
            },
            None,
        )
        .await;

        let client = GrammarHttpClient::with_endpoint(endpoint).unwrap();
        let err = client
            .post_json("already-ready-token", &json!({}))
            .await
            .expect_err("oversize must fail");
        assert_eq!(
            err,
            GrammarHttpError::BodyTooLarge {
                limit: MAX_GRAMMAR_RESPONSE_BYTES
            }
        );

        server.abort();
    }

    #[tokio::test]
    async fn post_json_times_out_when_server_never_responds() {
        let probe = Arc::new(ServerProbe::default());
        let (listener, endpoint) = bind_loopback().await;
        let server = spawn_http_server(
            listener,
            Arc::clone(&probe),
            ReplyMode::Hang(HangMode::Wait),
            None,
        )
        .await;

        let client =
            GrammarHttpClient::with_config(endpoint, Duration::from_millis(80), 1024).unwrap();
        let err = client
            .post_json("already-ready-token", &json!({"ping": true}))
            .await
            .expect_err("must time out");
        assert_eq!(err, GrammarHttpError::Timeout);

        server.abort();
    }

    /// §11.2 gate (1): mid-wait drop on the **real** reqwest client stack.
    #[tokio::test]
    async fn drop_mid_wait_leaves_zero_per_request_work() {
        assert_drop_cancels(HangMode::Wait).await;
    }

    /// §11.2 gate (1): mid-response drop on the real client stack.
    #[tokio::test]
    async fn drop_mid_response_leaves_zero_per_request_work() {
        assert_drop_cancels(HangMode::PartialBody).await;
    }

    /// Gated body: yields one frame, then Pending forever. Exact size_hint sets
    /// Content-Length so the server can see a partial body while the client
    /// still owns unsent stream bytes under test control (Sol r3 mid-upload).
    struct GatedUploadBody {
        first: Option<bytes::Bytes>,
        total: u64,
        yielded: Arc<AtomicU64>,
    }

    impl http_body::Body for GatedUploadBody {
        type Data = bytes::Bytes;
        type Error = std::convert::Infallible;

        fn poll_frame(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
            if let Some(chunk) = self.first.take() {
                self.yielded.fetch_add(chunk.len() as u64, Ordering::SeqCst);
                return Poll::Ready(Some(Ok(http_body::Frame::data(chunk))));
            }
            // Remainder never yields — cancel must happen with unsent payload.
            Poll::Pending
        }

        fn is_end_stream(&self) -> bool {
            false
        }

        fn size_hint(&self) -> http_body::SizeHint {
            http_body::SizeHint::with_exact(self.total)
        }
    }

    /// §11.2 gate (1): mid-upload drop on the real client stack.
    ///
    /// Protocol (Sol r3):
    /// 1. Client streams a gated body (first frame only; remainder held).
    /// 2. Server reads a verified partial body, freezes body_seen, STALLS
    ///    (no further drain) until the test cancel barrier.
    /// 3. Cancel while client_yielded < total (explicit unsent remainder).
    /// 4. Server observes peer EOF/reset → active_handlers == 0.
    #[tokio::test]
    async fn drop_mid_upload_leaves_zero_per_request_work() {
        let probe = Arc::new(ServerProbe::default());
        let (listener, endpoint) = bind_loopback().await;
        let (drop_tx, drop_rx) = oneshot::channel();
        let server = spawn_http_server(
            listener,
            Arc::clone(&probe),
            ReplyMode::Hang(HangMode::Upload),
            Some(drop_rx),
        )
        .await;

        let client =
            GrammarHttpClient::with_config(endpoint, Duration::from_secs(30), 65_536).unwrap();

        let yielded = Arc::new(AtomicU64::new(0));
        let first = bytes::Bytes::from(vec![b'x'; UPLOAD_HANG_BODY_FIRST_FRAME]);
        let body = reqwest::Body::wrap(GatedUploadBody {
            first: Some(first),
            total: UPLOAD_HANG_BODY_TOTAL as u64,
            yielded: Arc::clone(&yielded),
        });

        // Box::pin so `drop(fut)` owns and tears down the request future.
        let mut fut = Box::pin(client.post_transport_body("already-ready-token", body));

        drive_until(
            &mut fut,
            || probe.hang_entered.load(Ordering::SeqCst) > 0,
            Duration::from_secs(3),
        )
        .await;

        assert!(
            probe.active() >= 1,
            "server must still own the in-flight request before drop"
        );

        // Cancel-epoch mid-upload invariants (before drop):
        // - server froze a non-empty partial body observation
        // - client still owns unsent stream bytes (yielded < total)
        let body_seen = probe.upload_body_bytes();
        let content_length = probe.upload_content_length();
        let client_yielded = yielded.load(Ordering::SeqCst);
        assert!(
            content_length > 0,
            "Upload hang must parse Content-Length from request headers"
        );
        assert_eq!(
            content_length as usize, UPLOAD_HANG_BODY_TOTAL,
            "gated body must declare the full Content-Length"
        );
        assert!(
            body_seen > 0 && body_seen < content_length,
            "server must observe a non-empty partial body before drop \
             (body_bytes={body_seen}, Content-Length={content_length})"
        );
        assert!(
            client_yielded > 0 && (client_yielded as usize) < UPLOAD_HANG_BODY_TOTAL,
            "client must still own unsent stream bytes at cancel epoch \
             (yielded={client_yielded}, total={UPLOAD_HANG_BODY_TOTAL})"
        );
        let client_remaining = (UPLOAD_HANG_BODY_TOTAL as u64).saturating_sub(client_yielded);
        assert!(
            client_remaining > 0,
            "explicit unsent remainder required for mid-upload cancel proof"
        );

        // Cancel the request future — the production gate's drop path.
        drop(fut);
        // Barrier marks cancel epoch only. Upload stalled without draining;
        // now observe peer EOF/reset (client-caused quiescence).
        let _ = drop_tx.send(());

        timeout(Duration::from_secs(3), async {
            wait_until(
                || probe.active() == 0 && probe.client_eof_observed() > 0,
                Duration::from_secs(3),
            )
            .await;
        })
        .await
        .expect("server handler must observe client EOF/reset and exit after drop");

        assert_eq!(
            probe.active(),
            0,
            "residual per-request handler work must be zero"
        );
        assert!(
            probe.client_eof_observed() > 0,
            "server must observe peer EOF/reset after cancel without closing first"
        );

        // Hang-entry observation stays frozen (no drain-to-CL during the stall).
        let body_seen_after = probe.upload_body_bytes();
        assert_eq!(
            body_seen_after, body_seen,
            "upload hang must not update body_seen after hang entry (no drain during stall)"
        );
        assert!(
            body_seen_after < content_length,
            "mid-upload hang entry must remain a partial body observation \
             (body_bytes={body_seen_after} < Content-Length={content_length})"
        );
        // Gated body never yields the remainder — cancel was mid-upload.
        assert!(
            (yielded.load(Ordering::SeqCst) as usize) < UPLOAD_HANG_BODY_TOTAL,
            "client must not have finished the gated body after cancel"
        );

        server.abort();
    }

    async fn assert_drop_cancels(mode: HangMode) {
        // Wait / PartialBody only — Upload has a dedicated gated-body proof.
        assert!(
            matches!(mode, HangMode::Wait | HangMode::PartialBody),
            "assert_drop_cancels is for mid-wait / mid-response only"
        );

        let probe = Arc::new(ServerProbe::default());
        let (listener, endpoint) = bind_loopback().await;
        let (drop_tx, drop_rx) = oneshot::channel();
        let server = spawn_http_server(
            listener,
            Arc::clone(&probe),
            ReplyMode::Hang(mode),
            Some(drop_rx),
        )
        .await;

        // Generous timeout so the hang is client-drop, not request timeout.
        let client =
            GrammarHttpClient::with_config(endpoint, Duration::from_secs(30), 65_536).unwrap();

        let body = json!({
            "model": "openai/gpt-oss-20b",
            "stream": false,
            "padding": "x".repeat(256 * 1024),
            "messages": [{"role": "user", "content": "hello"}]
        });

        // Box::pin so `drop(fut)` owns and tears down the request future.
        // (`tokio::pin!` shadows with `Pin<&mut _>`; dropping that Pin alone
        // would leave the owned future alive — not a real cancel.)
        let mut fut = Box::pin(client.post_json("already-ready-token", &body));

        drive_until(
            &mut fut,
            || probe.hang_entered.load(Ordering::SeqCst) > 0,
            Duration::from_secs(3),
        )
        .await;

        assert!(
            probe.active() >= 1,
            "server must still own the in-flight request before drop"
        );

        // Cancel the request future — the production gate's drop path.
        drop(fut);
        // Barrier marks residual-byte accounting; hang handlers keep the socket
        // open and exit only after observing peer EOF/reset.
        let _ = drop_tx.send(());

        // §11.2 / PRODUCTION_CANCEL_RESIDUAL_REQUEST_WORK_MAX = 0:
        // after the request future is dropped, the server-side handler for that
        // connection must reach terminal *because the client closed*, not because
        // the server independently returned and dropped its own socket.
        timeout(Duration::from_secs(3), async {
            wait_until(
                || probe.active() == 0 && probe.client_eof_observed() > 0,
                Duration::from_secs(3),
            )
            .await;
        })
        .await
        .expect("server handler must observe client EOF/reset and exit after drop");

        assert_eq!(
            probe.active(),
            0,
            "residual per-request handler work must be zero"
        );
        assert!(
            probe.client_eof_observed() > 0,
            "server must observe peer EOF/reset after cancel without closing first"
        );

        server.abort();
    }

    #[tokio::test]
    async fn client_is_reusable_across_sequential_requests() {
        let probe = Arc::new(ServerProbe::default());
        let (listener, endpoint) = bind_loopback().await;
        let server = spawn_http_server(
            listener,
            Arc::clone(&probe),
            ReplyMode::OkJson(r#"{"choices":[{"message":{"content":"{\"edits\":[]}"}}]}"#),
            None,
        )
        .await;

        let client = GrammarHttpClient::with_endpoint(endpoint).unwrap();
        for _ in 0..3 {
            client
                .post_json("already-ready-token", &sample_grammar_request_body())
                .await
                .expect("reusable client request");
        }
        assert_eq!(probe.requests(), 3);

        server.abort();
    }
}
