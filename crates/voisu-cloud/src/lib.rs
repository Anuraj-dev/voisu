//! Portable cloud pipeline: the Groq and Deepgram transcript providers,
//! Groq reconciliation and Intent Reconstruction, the provider auth probes,
//! the Developer Prompt Rendering and Minimal Grammar HTTP clients, and the
//! bounded subprocess runner they share.
//!
//! Platform concerns stay with the host: credential storage, capture, the
//! credential-preparation lane, and Delivery live in voisu-app (Linux) and
//! reach this crate only through [`CloudHost`].

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use voisu_core::{
    AudioChunk, BoundaryError, BoundaryFuture, BoundaryKind, CancelRegistry, CapturedAudio,
    Credential, IntentReconstructionAttempt, IntentReconstructionRequest, MergeResult,
    PreparedTranscriptDecision, Provider, ProviderAuthenticator, ProviderKeyStatus, ProviderStream,
    ProviderWordConfidences, ReconciliationKind, ReconciliationModel, SourceTranscript,
    TranscriptDecision, TranscriptDecisionPipeline, TranscriptProvider, TranscriptValidator,
};

use process::*;

pub mod dpr_cloud;
pub mod grammar_http;
pub mod process;

mod cleanup;
mod deepgram;
mod groq;
mod provider_http;
mod reconcile;

pub use cleanup::*;
pub use deepgram::*;
pub use groq::*;
pub use provider_http::*;
pub use reconcile::*;

pub const RECONCILIATION_DEADLINE: Duration = Duration::from_secs(3);

pub const INTENT_RECONSTRUCTION_DEADLINE: Duration = Duration::from_secs(5);

const PROVIDER_PROCESS_DEADLINE: Duration = Duration::from_secs(14);

const RECONCILIATION_PROCESS_DEADLINE: Duration = Duration::from_secs(2);

/// Default Groq chat-completions model for Transcript reconciliation.
/// Exact id only — no family-wide Qwen rule. Co-lands with the #98
/// `reasoning_effort: "none"` body field for this same exact id.
pub const DEFAULT_GROQ_RECONCILIATION_MODEL: &str = "qwen/qwen3.8-27b";

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

fn curl_config_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Raw-endpoint pre-parse gate shared by every endpoint transport policy.
/// Rejects, before any parsing:
/// - control characters the URL parser silently strips before validating
///   (`\n`, `\r`, `\t` — so `localho\tst` cannot gate as plain `localhost`),
///   plus `\0`, which has no place in an endpoint;
/// - any `\`: the url crate ends its userinfo/host scan at `\` for special
///   schemes while curl splits userinfo at the LAST `@` and accepts `\`
///   inside it, so `http://localhost:8080\@attacker.example/` gates as
///   loopback-without-userinfo yet curl connects to attacker.example;
/// - any `@` in the raw authority: userinfo of ANY shape is invalid on a
///   provider endpoint, and the EMPTY form (`http://@localhost/`) is
///   invisible to the parsed accessors — `username()` is empty, `password()`
///   is None, and the url crate drops the `@` from its serialization
///   entirely — so only the raw authority can see it.
pub(crate) fn endpoint_raw_string_is_allowed(endpoint: &str) -> bool {
    if endpoint.contains(['\n', '\r', '\t', '\0', '\\']) {
        return false;
    }
    let Some((_scheme, authority)) = endpoint.split_once("://") else {
        // No scheme-shaped authority; the parse and scheme gates decide.
        return true;
    };
    !authority
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default()
        .contains('@')
}

/// Shared endpoint-transport policy on the PARSED URL: the host must be
/// present, and any surviving userinfo (non-empty username or any password)
/// rejects — no legitimate provider endpoint authenticates through the URL
/// itself. The raw empty-`@` form is caught earlier by
/// `endpoint_raw_string_is_allowed`, which sees the string the way curl does.
pub(crate) fn endpoint_authority_is_allowed(url: &url::Url) -> bool {
    url.host_str().is_some_and(|host| !host.is_empty())
        && url.username().is_empty()
        && url.password().is_none()
}

/// Whether the URL's PARSED host is loopback: exactly `localhost`, an IPv4 in
/// 127.0.0.0/8, or ::1. The comparison is on the parsed host only — never a
/// prefix or suffix of the raw authority string — so `localhost.evil.test` is a
/// different host and `localhost:8080@evil.test` is evil.test with userinfo.
pub(crate) fn parsed_host_is_loopback(url: &url::Url) -> bool {
    match url.host() {
        Some(url::Host::Domain(domain)) => domain == "localhost",
        Some(url::Host::Ipv4(address)) => address.is_loopback(),
        Some(url::Host::Ipv6(address)) => address.is_loopback(),
        None => false,
    }
}

#[cfg(test)]
mod tests;
