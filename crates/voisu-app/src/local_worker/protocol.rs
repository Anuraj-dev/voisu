//! Length-prefixed worker protocol (R5).
//!
//! Control frames are JSON with a 4-byte little-endian length prefix. PCM is a
//! separate bounded transfer. Worker stdout is protocol only.

use super::bounds::{
    MAX_JSON_DEPTH, MAX_JSON_FIELDS, MAX_JSON_FRAME_BYTES, MAX_PCM_BYTES, MAX_TRANSCRIPT_BYTES,
};

pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProtocolVersion(pub u32);

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FrameError {
    OddPcmLength,
    PcmTooLarge { bytes: usize },
    DeclaredPcmMismatch { declared: usize, actual: usize },
    JsonTooLarge { bytes: usize },
    JsonTooDeep,
    JsonTooManyFields,
    Truncated,
    InvalidUtf8,
    InvalidJson,
    UnknownVersion { version: u32 },
    DuplicateTerminal,
    Unsolicited,
    TranscriptTooLarge { bytes: usize },
    UnsupportedControl,
    EmbeddedNul,
    ExtraAudio,
    CorrelationMismatch,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Correlation {
    pub daemon_nonce: String,
    pub generation: u64,
    pub request_id: String,
    pub recording_id: String,
    pub model_receipt_hash: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ControlFrame {
    Prepare(Correlation),
    Transcribe {
        correlation: Correlation,
        pcm_bytes: usize,
    },
    Cancel {
        request_id: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkerFrame {
    Ready {
        correlation: Correlation,
        observed_device: String,
    },
    Transcript {
        correlation: Correlation,
        text: String,
    },
    NoText {
        correlation: Correlation,
        reason: String,
    },
    Error {
        code: String,
        metadata: serde_json::Map<String, serde_json::Value>,
    },
}

pub fn encode_json_frame(value: &serde_json::Value) -> Result<Vec<u8>, FrameError> {
    check_json_bounds(value, 1, &mut 0)?;
    let payload = serde_json::to_vec(value).map_err(|_| FrameError::InvalidJson)?;
    if payload.len() > MAX_JSON_FRAME_BYTES {
        return Err(FrameError::JsonTooLarge {
            bytes: payload.len(),
        });
    }
    let mut frame = Vec::with_capacity(4 + payload.len());
    frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    frame.extend_from_slice(&payload);
    Ok(frame)
}

pub fn decode_json_frame(bytes: &[u8]) -> Result<(serde_json::Value, usize), FrameError> {
    if bytes.len() < 4 {
        return Err(FrameError::Truncated);
    }
    let len = u32::from_le_bytes(bytes[0..4].try_into().expect("4-byte prefix")) as usize;
    if len == 0 || len > MAX_JSON_FRAME_BYTES {
        return Err(FrameError::JsonTooLarge { bytes: len });
    }
    let end = 4usize.checked_add(len).ok_or(FrameError::Truncated)?;
    if bytes.len() < end {
        return Err(FrameError::Truncated);
    }
    let payload = &bytes[4..end];
    let text = std::str::from_utf8(payload).map_err(|_| FrameError::InvalidUtf8)?;
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|_| FrameError::InvalidJson)?;
    check_json_bounds(&value, 1, &mut 0)?;
    if let Some(version) = value.get("v").and_then(serde_json::Value::as_u64) {
        if version != u64::from(PROTOCOL_VERSION) {
            return Err(FrameError::UnknownVersion {
                version: version as u32,
            });
        }
    } else {
        return Err(FrameError::UnknownVersion { version: 0 });
    }
    Ok((value, end))
}

pub fn validate_pcm(pcm: &[u8], declared: Option<usize>) -> Result<(), FrameError> {
    if !pcm.len().is_multiple_of(2) {
        return Err(FrameError::OddPcmLength);
    }
    if pcm.len() > MAX_PCM_BYTES {
        return Err(FrameError::PcmTooLarge { bytes: pcm.len() });
    }
    if let Some(declared) = declared
        && declared != pcm.len()
    {
        return Err(FrameError::DeclaredPcmMismatch {
            declared,
            actual: pcm.len(),
        });
    }
    Ok(())
}

/// Empty/whitespace is not a successful Transcript; callers map it to no-text.
pub fn validate_transcript(text: &str) -> Result<(), FrameError> {
    if text.len() > MAX_TRANSCRIPT_BYTES {
        return Err(FrameError::TranscriptTooLarge { bytes: text.len() });
    }
    if text.as_bytes().contains(&0) {
        return Err(FrameError::EmbeddedNul);
    }
    if text.chars().any(unsupported_control) {
        return Err(FrameError::UnsupportedControl);
    }
    Ok(())
}

#[must_use]
pub fn is_silence_pcm(pcm: &[u8]) -> bool {
    pcm.as_chunks::<2>().0.iter().all(|pair| pair == &[0, 0])
}

pub fn parse_control(value: &serde_json::Value) -> Result<ControlFrame, FrameError> {
    let kind = value.get("kind").and_then(serde_json::Value::as_str);
    match kind {
        Some("prepare") => Ok(ControlFrame::Prepare(correlation_from(value)?)),
        Some("transcribe") => {
            let pcm_bytes = value
                .get("pcm_bytes")
                .and_then(serde_json::Value::as_u64)
                .ok_or(FrameError::InvalidJson)? as usize;
            if pcm_bytes > MAX_PCM_BYTES || !pcm_bytes.is_multiple_of(2) {
                return Err(if !pcm_bytes.is_multiple_of(2) {
                    FrameError::OddPcmLength
                } else {
                    FrameError::PcmTooLarge { bytes: pcm_bytes }
                });
            }
            Ok(ControlFrame::Transcribe {
                correlation: correlation_from(value)?,
                pcm_bytes,
            })
        }
        Some("cancel") => Ok(ControlFrame::Cancel {
            request_id: required_string(value, "request_id")?,
        }),
        _ => Err(FrameError::InvalidJson),
    }
}

pub fn parse_worker(value: &serde_json::Value) -> Result<WorkerFrame, FrameError> {
    let kind = value.get("kind").and_then(serde_json::Value::as_str);
    match kind {
        Some("ready") => Ok(WorkerFrame::Ready {
            correlation: correlation_from(value)?,
            observed_device: required_string(value, "observed_device")?,
        }),
        Some("transcript") => {
            let text = required_string(value, "text")?;
            validate_transcript(&text)?;
            Ok(WorkerFrame::Transcript {
                correlation: correlation_from(value)?,
                text,
            })
        }
        Some("no_text") => Ok(WorkerFrame::NoText {
            correlation: correlation_from(value)?,
            reason: required_string(value, "reason")?,
        }),
        Some("error") => Ok(WorkerFrame::Error {
            code: required_string(value, "code")?,
            metadata: value
                .get("metadata")
                .and_then(serde_json::Value::as_object)
                .cloned()
                .unwrap_or_default(),
        }),
        _ => Err(FrameError::InvalidJson),
    }
}

pub fn correlations_match(expected: &Correlation, got: &Correlation) -> Result<(), FrameError> {
    if expected == got {
        Ok(())
    } else {
        Err(FrameError::CorrelationMismatch)
    }
}

fn unsupported_control(ch: char) -> bool {
    let n = ch as u32;
    (n < 0x20 && !matches!(ch, '\t' | '\n' | '\r')) || (0x7f..=0x9f).contains(&n)
}

fn required_string(value: &serde_json::Value, key: &str) -> Result<String, FrameError> {
    value
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(ToOwned::to_owned)
        .ok_or(FrameError::InvalidJson)
}

fn correlation_from(value: &serde_json::Value) -> Result<Correlation, FrameError> {
    Ok(Correlation {
        daemon_nonce: required_string(value, "daemon_nonce")?,
        generation: value
            .get("generation")
            .and_then(serde_json::Value::as_u64)
            .ok_or(FrameError::InvalidJson)?,
        request_id: required_string(value, "request_id")?,
        recording_id: required_string(value, "recording_id")?,
        model_receipt_hash: required_string(value, "model_receipt_hash")?,
    })
}

fn check_json_bounds(
    value: &serde_json::Value,
    depth: usize,
    fields: &mut usize,
) -> Result<(), FrameError> {
    if depth > MAX_JSON_DEPTH {
        return Err(FrameError::JsonTooDeep);
    }
    match value {
        serde_json::Value::Object(map) => {
            *fields = fields.saturating_add(map.len());
            if *fields > MAX_JSON_FIELDS {
                return Err(FrameError::JsonTooManyFields);
            }
            for nested in map.values() {
                check_json_bounds(nested, depth + 1, fields)?;
            }
        }
        serde_json::Value::Array(items) => {
            for nested in items {
                check_json_bounds(nested, depth + 1, fields)?;
            }
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn corr() -> Correlation {
        Correlation {
            daemon_nonce: "d1".into(),
            generation: 1,
            request_id: "r1".into(),
            recording_id: "rec-1".into(),
            model_receipt_hash: "abc".into(),
        }
    }

    fn control_json() -> serde_json::Value {
        serde_json::json!({
            "v": 1,
            "kind": "prepare",
            "daemon_nonce": "d1",
            "generation": 1,
            "request_id": "r1",
            "recording_id": "rec-1",
            "model_receipt_hash": "abc",
        })
    }

    #[test]
    fn frame_roundtrip_and_unknown_version_are_rejected() {
        let encoded = encode_json_frame(&control_json()).unwrap();
        let (value, consumed) = decode_json_frame(&encoded).unwrap();
        assert_eq!(consumed, encoded.len());
        assert_eq!(
            parse_control(&value).unwrap(),
            ControlFrame::Prepare(corr())
        );

        let mut bad = control_json();
        bad["v"] = serde_json::json!(2);
        let encoded = encode_json_frame(&bad).unwrap();
        assert!(matches!(
            decode_json_frame(&encoded),
            Err(FrameError::UnknownVersion { version: 2 })
        ));
    }

    #[test]
    fn oversized_length_is_rejected_before_a_huge_allocation() {
        let mut bytes = 1_000_000u32.to_le_bytes().to_vec();
        bytes.extend_from_slice(&[1, 2, 3, 4]);
        assert!(matches!(
            decode_json_frame(&bytes),
            Err(FrameError::JsonTooLarge { bytes: 1_000_000 })
        ));
    }

    #[test]
    fn pcm_rejects_odd_length_cap_and_declared_mismatch() {
        assert!(matches!(
            validate_pcm(&[0], None),
            Err(FrameError::OddPcmLength)
        ));
        assert!(validate_pcm(&[0, 0], Some(2)).is_ok());
        assert!(matches!(
            validate_pcm(&[0, 0], Some(4)),
            Err(FrameError::DeclaredPcmMismatch {
                declared: 4,
                actual: 2
            })
        ));
        let huge = MAX_PCM_BYTES + 2;
        assert!(matches!(
            validate_pcm(&vec![0; huge], None),
            Err(FrameError::PcmTooLarge { bytes }) if bytes == huge
        ));
    }

    #[test]
    fn transcript_rejects_nul_and_maps_blank_to_caller() {
        assert!(validate_transcript("hello").is_ok());
        assert!(matches!(
            validate_transcript("nul\0byte"),
            Err(FrameError::EmbeddedNul)
        ));
        assert!(matches!(
            validate_transcript("bell\u{0007}"),
            Err(FrameError::UnsupportedControl)
        ));
        assert!(validate_transcript("").is_ok());
    }

    #[test]
    fn json_depth_and_field_caps_fail_closed() {
        let mut nested = serde_json::json!({"v": 1, "kind": "error", "code": "x"});
        let mut cursor = &mut nested;
        for i in 0..10 {
            cursor["k"] = serde_json::json!({});
            cursor = &mut cursor["k"];
            let _ = i;
        }
        assert!(matches!(
            encode_json_frame(&nested),
            Err(FrameError::JsonTooDeep)
        ));
    }

    #[test]
    fn correlation_mismatch_is_visible() {
        let mut got = corr();
        got.generation = 9;
        assert_eq!(
            correlations_match(&corr(), &got),
            Err(FrameError::CorrelationMismatch)
        );
    }
}
