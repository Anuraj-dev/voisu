//! Dev-only: run the speech-presence gate over WAV files and print the
//! decision per file. Not a user command.
//!
//! ```text
//! cargo run -p voisu-core --example speech_gate_wavs -- a.wav b.wav ...
//! ```
//!
//! Accepts 16-bit PCM mono WAV at any sample rate. Prints one tab-separated
//! line per file: `SPEECH|SILENT`, effective threshold, longest loud run in
//! frames, frame count, path. Exits 0 when every file could be read.

use std::process::ExitCode;

fn main() -> ExitCode {
    let mut failed = false;
    for path in std::env::args().skip(1) {
        match std::fs::read(&path)
            .map_err(|e| e.to_string())
            .and_then(|bytes| parse_wav(&bytes))
        {
            Ok((samples, rate)) => {
                let d = voisu_core::detect_speech(&samples, rate);
                println!(
                    "{}\t{:.5}\t{}\t{}\t{path}",
                    if d.has_speech { "SPEECH" } else { "SILENT" },
                    d.effective_threshold,
                    d.longest_run_frames,
                    d.frames
                );
            }
            Err(error) => {
                failed = true;
                eprintln!("ERROR\t{path}\t{error}");
            }
        }
    }
    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

/// Minimal RIFF walk: `fmt ` must be PCM s16 mono; `data` is clamped to the
/// file so a streamed-WAV placeholder size still reads.
fn parse_wav(bytes: &[u8]) -> Result<(Vec<i16>, u32), String> {
    if bytes.len() < 12 || &bytes[..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err("not a RIFF/WAVE file".into());
    }
    let (mut pos, mut rate) = (12, None);
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let size = u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into().unwrap()) as usize;
        let body = pos + 8;
        if id == b"fmt " {
            let fmt = bytes.get(body..body + 16).ok_or("short fmt chunk")?;
            let (tag, channels) = (le16(&fmt[0..2]), le16(&fmt[2..4]));
            let bits = le16(&fmt[14..16]);
            if (tag, channels, bits) != (1, 1, 16) {
                return Err(format!(
                    "need PCM mono 16-bit, got tag {tag} ch {channels} bits {bits}"
                ));
            }
            rate = Some(u32::from_le_bytes(fmt[4..8].try_into().unwrap()));
        } else if id == b"data" {
            let rate = rate.ok_or("data before fmt")?;
            let end = body.saturating_add(size).min(bytes.len());
            let samples = bytes[body..end]
                .as_chunks::<2>()
                .0
                .iter()
                .map(|s| i16::from_le_bytes(*s))
                .collect();
            return Ok((samples, rate));
        }
        pos = body.saturating_add(size).saturating_add(size & 1);
    }
    Err("no data chunk".into())
}

fn le16(b: &[u8]) -> u16 {
    u16::from_le_bytes([b[0], b[1]])
}
