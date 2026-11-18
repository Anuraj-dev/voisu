//! Wire contract of the private Local speech-to-text worker (protocol v1).
//!
//! The daemon side (`voisu-app`) and the native worker (`voisu-local-asr`)
//! both take these from here, so the worker never depends on `voisu-app` and
//! the two sides cannot drift. Not re-exported at the crate root: the daemon
//! IPC has its own [`crate::PROTOCOL_VERSION`].

/// Worker protocol version carried as `"v"` in every control frame.
pub const PROTOCOL_VERSION: u32 = 1;

/// Signed 16-bit little-endian, mono, 16 kHz.
pub const PCM_SAMPLE_RATE_HZ: u32 = 16_000;
/// 600 seconds × 16_000 Hz × 2 bytes.
pub const MAX_PCM_BYTES: usize = 19_200_000;

/// 4-byte length prefix; at most 64 KiB JSON per control frame.
pub const MAX_JSON_FRAME_BYTES: usize = 64 * 1024;

/// Cap on the optional `initial_prompt` a Transcribe request carries. The
/// Dictionary builder stays far below it (about 224 bytes); this only bounds
/// what a frame may claim.
pub const MAX_PROMPT_BYTES: usize = 1024;

/// UTF-8 Transcript cap. Empty/silence is an explicit no-text outcome.
pub const MAX_TRANSCRIPT_BYTES: usize = 24 * 1024;

/// The one prompt rule, shared with the Python worker: non-empty after
/// trimming, at most [`MAX_PROMPT_BYTES`], and no control characters.
#[must_use]
pub fn is_valid_prompt(prompt: &str) -> bool {
    !prompt.trim().is_empty()
        && prompt.len() <= MAX_PROMPT_BYTES
        && !prompt.chars().any(char::is_control)
}
