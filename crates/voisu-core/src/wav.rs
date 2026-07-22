//! Locating the PCM payload inside a streamed WAV container.
//!
//! When `pw-record` runs without `--raw` (the flag is absent on PipeWire before
//! 1.1), it emits a RIFF/WAVE container on stdout instead of headerless PCM. A
//! streamed WAV cannot know its final length, so its RIFF and `data` size fields
//! carry placeholders and the chunk layout is not guaranteed canonical. The scan
//! therefore walks the chunk chain to find `data` rather than blind-skipping 44
//! bytes, and asserts the `fmt ` chunk really is s16 / 16 kHz / mono before any
//! payload is trusted — a wrong-format stream must fail the capture, not reach a
//! provider as if it were audio.
//!
//! The scan is incremental: it is fed a growing prefix of the stream and reports
//! whether it needs more bytes, where the payload begins, or that the stream is
//! not the expected format. That lets the capture reader strip the header from a
//! live byte stream without buffering the whole Recording.

/// The audio parameters every Recording must carry, matched against the WAV
/// `fmt ` chunk.
const EXPECTED_FORMAT_PCM: u16 = 1;
const EXPECTED_CHANNELS: u16 = 1;
const EXPECTED_SAMPLE_RATE: u32 = 16_000;
const EXPECTED_BITS_PER_SAMPLE: u16 = 16;

/// The outcome of scanning a prefix of a WAV stream.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WavScan {
    /// The prefix is a valid WAV so far but the `data` chunk (or a `fmt ` chunk
    /// still needed to validate it) has not arrived yet. Feed more bytes.
    Incomplete,
    /// The PCM payload begins at this byte offset. Everything before it is
    /// container framing to discard; everything from here on is s16le mono
    /// 16 kHz samples.
    DataAt(usize),
    /// The stream is not a WAV, or not the audio format Voisu requires. The
    /// capture must fail with a boundary error rather than ship these bytes.
    Invalid(&'static str),
}

/// Scan a prefix of a `pw-record` WAV stream for the start of the PCM payload.
///
/// The `data` size field is deliberately ignored: a streamed WAV writes a
/// placeholder there, and everything after the `data` chunk header is payload.
/// Sizes of *earlier* chunks (`fmt `, `LIST`, `fact`, …) are honored so they can
/// be skipped.
pub fn scan_wav_pcm(prefix: &[u8]) -> WavScan {
    // RIFF / WAVE preamble. Reject as soon as a byte contradicts it; wait if the
    // preamble is merely not fully arrived.
    if prefix.len() >= 4 && &prefix[0..4] != b"RIFF" {
        return WavScan::Invalid("capture stream is not a RIFF container");
    }
    if prefix.len() >= 12 && &prefix[8..12] != b"WAVE" {
        return WavScan::Invalid("capture stream is not a WAVE stream");
    }
    if prefix.len() < 12 {
        return WavScan::Incomplete;
    }

    let mut offset = 12usize;
    let mut fmt_validated = false;
    loop {
        // A chunk header is 4 bytes of id + 4 bytes of little-endian size.
        let Some(header_end) = offset.checked_add(8) else {
            return WavScan::Invalid("WAV chunk offset overflowed");
        };
        if prefix.len() < header_end {
            return WavScan::Incomplete;
        }
        let id = &prefix[offset..offset + 4];
        let size = u32::from_le_bytes([
            prefix[offset + 4],
            prefix[offset + 5],
            prefix[offset + 6],
            prefix[offset + 7],
        ]) as usize;

        if id == b"data" {
            if !fmt_validated {
                return WavScan::Invalid("WAV data chunk precedes its fmt chunk");
            }
            return WavScan::DataAt(header_end);
        }

        if id == b"fmt " {
            // Need the whole fmt body present to validate it.
            if size < 16 {
                return WavScan::Invalid("WAV fmt chunk is too short");
            }
            let Some(body_end) = header_end.checked_add(size) else {
                return WavScan::Invalid("WAV fmt chunk size overflowed");
            };
            if prefix.len() < header_end + 16 {
                return WavScan::Incomplete;
            }
            let body = &prefix[header_end..header_end + 16];
            let audio_format = u16::from_le_bytes([body[0], body[1]]);
            let channels = u16::from_le_bytes([body[2], body[3]]);
            let sample_rate =
                u32::from_le_bytes([body[4], body[5], body[6], body[7]]);
            let bits_per_sample = u16::from_le_bytes([body[14], body[15]]);
            if audio_format != EXPECTED_FORMAT_PCM
                || channels != EXPECTED_CHANNELS
                || sample_rate != EXPECTED_SAMPLE_RATE
                || bits_per_sample != EXPECTED_BITS_PER_SAMPLE
            {
                return WavScan::Invalid(
                    "WAV fmt chunk is not s16 / 16 kHz / mono PCM",
                );
            }
            fmt_validated = true;
            offset = advance_past_chunk(body_end, size);
            continue;
        }

        // An unrelated chunk (LIST, fact, …) before the payload: skip its body,
        // honoring the RIFF word-alignment pad on an odd size.
        let Some(body_end) = header_end.checked_add(size) else {
            return WavScan::Invalid("WAV chunk size overflowed");
        };
        offset = advance_past_chunk(body_end, size);
    }
}

/// RIFF chunk bodies are word-aligned: an odd-length body is followed by one
/// pad byte before the next chunk header.
fn advance_past_chunk(body_end: usize, size: usize) -> usize {
    body_end.saturating_add(size & 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canonical_header(data_len: u32) -> Vec<u8> {
        let mut header = Vec::new();
        header.extend_from_slice(b"RIFF");
        header.extend_from_slice(&(36 + data_len).to_le_bytes());
        header.extend_from_slice(b"WAVE");
        header.extend_from_slice(b"fmt ");
        header.extend_from_slice(&16u32.to_le_bytes());
        header.extend_from_slice(&1u16.to_le_bytes()); // PCM
        header.extend_from_slice(&1u16.to_le_bytes()); // mono
        header.extend_from_slice(&16_000u32.to_le_bytes()); // 16 kHz
        header.extend_from_slice(&32_000u32.to_le_bytes()); // byte rate
        header.extend_from_slice(&2u16.to_le_bytes()); // block align
        header.extend_from_slice(&16u16.to_le_bytes()); // bits
        header.extend_from_slice(b"data");
        header.extend_from_slice(&data_len.to_le_bytes());
        header
    }

    #[test]
    fn canonical_44_byte_header_locates_payload_at_44() {
        let mut stream = canonical_header(8);
        stream.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(scan_wav_pcm(&stream), WavScan::DataAt(44));
    }

    #[test]
    fn placeholder_sizes_are_ignored_and_payload_is_still_found() {
        // A streamed WAV writes 0xFFFFFFFF (or 0) for the RIFF and data sizes.
        let mut header = Vec::new();
        header.extend_from_slice(b"RIFF");
        header.extend_from_slice(&u32::MAX.to_le_bytes());
        header.extend_from_slice(b"WAVE");
        header.extend_from_slice(b"fmt ");
        header.extend_from_slice(&16u32.to_le_bytes());
        header.extend_from_slice(&1u16.to_le_bytes());
        header.extend_from_slice(&1u16.to_le_bytes());
        header.extend_from_slice(&16_000u32.to_le_bytes());
        header.extend_from_slice(&32_000u32.to_le_bytes());
        header.extend_from_slice(&2u16.to_le_bytes());
        header.extend_from_slice(&16u16.to_le_bytes());
        header.extend_from_slice(b"data");
        header.extend_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(scan_wav_pcm(&header), WavScan::DataAt(44));
    }

    #[test]
    fn an_extra_chunk_before_data_is_skipped() {
        let mut header = Vec::new();
        header.extend_from_slice(b"RIFF");
        header.extend_from_slice(&u32::MAX.to_le_bytes());
        header.extend_from_slice(b"WAVE");
        header.extend_from_slice(b"fmt ");
        header.extend_from_slice(&16u32.to_le_bytes());
        header.extend_from_slice(&1u16.to_le_bytes());
        header.extend_from_slice(&1u16.to_le_bytes());
        header.extend_from_slice(&16_000u32.to_le_bytes());
        header.extend_from_slice(&32_000u32.to_le_bytes());
        header.extend_from_slice(&2u16.to_le_bytes());
        header.extend_from_slice(&16u16.to_le_bytes());
        // An odd-length LIST chunk exercises the word-alignment pad.
        header.extend_from_slice(b"LIST");
        header.extend_from_slice(&3u32.to_le_bytes());
        header.extend_from_slice(&[b'I', b'N', b'F']);
        header.push(0); // pad byte
        let list_end = header.len();
        header.extend_from_slice(b"data");
        header.extend_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(scan_wav_pcm(&header), WavScan::DataAt(list_end + 8));
    }

    #[test]
    fn a_partial_header_asks_for_more_bytes() {
        let stream = canonical_header(8);
        assert_eq!(scan_wav_pcm(&stream[..20]), WavScan::Incomplete);
        assert_eq!(scan_wav_pcm(&[]), WavScan::Incomplete);
    }

    #[test]
    fn a_non_riff_stream_is_rejected_immediately() {
        // Headerless PCM fed to the WAV scanner (garbage from its point of view)
        // must fail cleanly, not loop or panic.
        assert!(matches!(
            scan_wav_pcm(b"\x00\x01\x02\x03rest-of-pcm-audio-bytes"),
            WavScan::Invalid(_)
        ));
    }

    #[test]
    fn a_wrong_format_fmt_chunk_is_rejected() {
        let mut header = Vec::new();
        header.extend_from_slice(b"RIFF");
        header.extend_from_slice(&u32::MAX.to_le_bytes());
        header.extend_from_slice(b"WAVE");
        header.extend_from_slice(b"fmt ");
        header.extend_from_slice(&16u32.to_le_bytes());
        header.extend_from_slice(&1u16.to_le_bytes());
        header.extend_from_slice(&2u16.to_le_bytes()); // stereo — wrong
        header.extend_from_slice(&48_000u32.to_le_bytes()); // 48 kHz — wrong
        header.extend_from_slice(&192_000u32.to_le_bytes());
        header.extend_from_slice(&4u16.to_le_bytes());
        header.extend_from_slice(&16u16.to_le_bytes());
        header.extend_from_slice(b"data");
        header.extend_from_slice(&u32::MAX.to_le_bytes());
        assert!(matches!(scan_wav_pcm(&header), WavScan::Invalid(_)));
    }

    #[test]
    fn data_before_fmt_is_rejected() {
        let mut header = Vec::new();
        header.extend_from_slice(b"RIFF");
        header.extend_from_slice(&u32::MAX.to_le_bytes());
        header.extend_from_slice(b"WAVE");
        header.extend_from_slice(b"data");
        header.extend_from_slice(&u32::MAX.to_le_bytes());
        assert!(matches!(scan_wav_pcm(&header), WavScan::Invalid(_)));
    }
}
