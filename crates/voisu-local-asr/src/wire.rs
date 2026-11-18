//! Worker side of the length-prefixed transport, matching
//! `tools/local-asr/voisu_asr/protocol.py`: a 4-byte little-endian length,
//! then a UTF-8 JSON object; PCM follows a Transcribe frame as raw bytes.

use std::io::{self, Read, Write};

use serde_json::Value;
use voisu_core::local_wire::{MAX_JSON_FRAME_BYTES, MAX_PCM_BYTES};

/// Longest request id the worker accepts, in UTF-8 bytes.
pub const MAX_ID_BYTES: usize = 128;
const MAX_MESSAGE_CHARS: usize = 320;

/// A protocol failure: a stable `code` plus a sanitized human message.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WireError {
    pub code: &'static str,
    pub message: String,
}

impl WireError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// Fill `buf`, or report a clean end of input (`Ok(false)`) when nothing at
/// all was read.
fn read_exact_or_eof(reader: &mut impl Read, buf: &mut [u8]) -> Result<bool, WireError> {
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]) {
            Ok(0) if filled == 0 => return Ok(false),
            Ok(0) => return Err(WireError::new("eof_mid_frame", "input ended in a frame")),
            Ok(read) => filled += read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(WireError::new("read_failed", error.to_string())),
        }
    }
    Ok(true)
}

/// Read one request frame; `Ok(None)` is a clean end of input.
///
/// # Errors
///
/// Any framing failure. The stream cannot be resynchronized afterwards.
pub fn read_frame(reader: &mut impl Read) -> Result<Option<Value>, WireError> {
    let mut header = [0u8; 4];
    if !read_exact_or_eof(reader, &mut header)? {
        return Ok(None);
    }
    let size = u32::from_le_bytes(header) as usize;
    if size == 0 || size > MAX_JSON_FRAME_BYTES {
        return Err(WireError::new(
            "frame_too_large",
            "JSON frame exceeds the 64 KiB limit",
        ));
    }
    let mut payload = vec![0u8; size];
    if !read_exact_or_eof(reader, &mut payload)? {
        return Err(WireError::new(
            "eof_mid_frame",
            "input ended before JSON frame",
        ));
    }
    let value: Value = serde_json::from_slice(&payload)
        .map_err(|_| WireError::new("invalid_json", "request frame is not valid UTF-8 JSON"))?;
    if !value.is_object() {
        return Err(WireError::new(
            "invalid_request",
            "request frame must be a JSON object",
        ));
    }
    Ok(Some(value))
}

/// The declared PCM length of a Transcribe frame, checked before any byte of
/// it is read.
///
/// # Errors
///
/// `invalid_pcm_bytes` for a missing, non-integer, empty, odd or over-cap size.
pub fn pcm_len(request: &Value) -> Result<usize, WireError> {
    let refused = || {
        WireError::new(
            "invalid_pcm_bytes",
            "PCM size is outside the bounded mono PCM16 range",
        )
    };
    let size = request
        .get("pcm_bytes")
        .and_then(Value::as_u64)
        .ok_or_else(refused)?;
    let size = usize::try_from(size).map_err(|_| refused())?;
    if size == 0 || size > MAX_PCM_BYTES || !size.is_multiple_of(2) {
        return Err(refused());
    }
    Ok(size)
}

/// Read exactly `size` PCM bytes as little-endian samples.
///
/// # Errors
///
/// `eof_mid_pcm` when input ends first.
pub fn read_pcm(reader: &mut impl Read, size: usize) -> Result<Vec<i16>, WireError> {
    let mut bytes = vec![0u8; size];
    match read_exact_or_eof(reader, &mut bytes) {
        Ok(true) => Ok(bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| i16::from_le_bytes(*pair))
            .collect()),
        Ok(false) | Err(_) => Err(WireError::new(
            "eof_mid_pcm",
            "input ended before PCM payload",
        )),
    }
}

/// The request id, when it is a non-empty bounded string.
pub fn request_id(request: &Value) -> Option<&str> {
    request
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty() && id.len() <= MAX_ID_BYTES)
}

/// Write one reply frame and flush it.
///
/// # Errors
///
/// The write failing, or a reply over the frame cap (a bug, since every reply
/// field is bounded).
pub fn write_frame(writer: &mut impl Write, value: &Value) -> io::Result<()> {
    let payload = serde_json::to_vec(value)?;
    if payload.len() > MAX_JSON_FRAME_BYTES {
        return Err(io::Error::other("reply exceeds the 64 KiB frame limit"));
    }
    let size = u32::try_from(payload.len()).map_err(io::Error::other)?;
    writer.write_all(&size.to_le_bytes())?;
    writer.write_all(&payload)?;
    writer.flush()
}

/// Control characters become spaces, whitespace collapses, at most 320
/// characters; an empty result is "runtime error".
#[must_use]
pub fn sanitize_message(message: &str) -> String {
    let spaced: String = message
        .chars()
        .map(|ch| {
            if ch <= '\u{1f}' || ch == '\u{7f}' {
                ' '
            } else {
                ch
            }
        })
        .collect();
    let collapsed = spaced.split_whitespace().collect::<Vec<_>>().join(" ");
    let bounded: String = collapsed.chars().take(MAX_MESSAGE_CHARS).collect();
    if bounded.is_empty() {
        "runtime error".to_owned()
    } else {
        bounded
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_strips_controls_and_bounds_length() {
        assert_eq!(sanitize_message(" a\n\tb\u{7f} c "), "a b c");
        assert_eq!(sanitize_message("\n"), "runtime error");
        assert_eq!(sanitize_message(&"x".repeat(400)).len(), 320);
    }

    #[test]
    fn pcm_len_refuses_odd_empty_and_over_cap_sizes() {
        for bad in [
            serde_json::json!(0),
            serde_json::json!(3),
            serde_json::json!(MAX_PCM_BYTES + 2),
            serde_json::json!(-2),
            serde_json::json!("32"),
            serde_json::json!(true),
        ] {
            let request = serde_json::json!({ "pcm_bytes": bad });
            assert_eq!(
                pcm_len(&request).unwrap_err().code,
                "invalid_pcm_bytes",
                "{bad}"
            );
        }
        assert_eq!(pcm_len(&serde_json::json!({ "pcm_bytes": 32 })), Ok(32));
    }

    #[test]
    fn frames_round_trip_and_eof_is_clean() {
        let mut buf = Vec::new();
        write_frame(&mut buf, &serde_json::json!({ "v": 1 })).unwrap();
        let mut reader = buf.as_slice();
        assert_eq!(
            read_frame(&mut reader).unwrap(),
            Some(serde_json::json!({ "v": 1 }))
        );
        assert_eq!(read_frame(&mut reader).unwrap(), None);
        let mut torn: &[u8] = &[9, 0];
        assert_eq!(read_frame(&mut torn).unwrap_err().code, "eof_mid_frame");
    }
}
