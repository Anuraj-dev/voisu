//! Dev-only: run the Local speech check (Silero VAD) over WAV files and
//! print the decision per file. Not a user command.
//!
//! ```text
//! cargo run --release -p voisu-app --example local_vad_wavs -- a.wav b.wav ...
//! ```
//!
//! Accepts 16 kHz 16-bit PCM mono WAV. Prints one tab-separated line per
//! file: `SPEECH|SILENT`, peak probability, windows scored, gain, check time
//! in microseconds, raw full-scan peak (no gain, no early stop; for parity
//! with onnxruntime), path. Exits 0 when every file could be read.

use std::process::ExitCode;
use std::time::Instant;

use voisu_app::local_vad::{check_samples, window_probabilities};

fn main() -> ExitCode {
    let mut failed = false;
    for path in std::env::args().skip(1) {
        match std::fs::read(&path)
            .map_err(|e| e.to_string())
            .and_then(|bytes| parse_wav(&bytes))
        {
            Ok(samples) => {
                let started = Instant::now();
                let check = check_samples(&samples);
                let micros = started.elapsed().as_micros();
                let audio: Vec<f32> = samples.iter().map(|&s| f32::from(s) / 32_768.0).collect();
                let raw_peak = window_probabilities(&audio)
                    .into_iter()
                    .fold(0.0f32, f32::max);
                println!(
                    "{}\t{:.5}\t{}\t{:.3}\t{micros}\t{raw_peak:.5}\t{path}",
                    if check.has_speech { "SPEECH" } else { "SILENT" },
                    check.peak_probability,
                    check.windows_scored,
                    check.gain,
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

/// Minimal RIFF walk: `fmt ` must be 16 kHz PCM s16 mono; `data` is clamped
/// to the file so a streamed-WAV placeholder size still reads.
fn parse_wav(bytes: &[u8]) -> Result<Vec<i16>, String> {
    if bytes.len() < 12 || &bytes[..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err("not a RIFF/WAVE file".into());
    }
    let (mut pos, mut format_ok) = (12, false);
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let size = u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into().unwrap()) as usize;
        let body = pos + 8;
        if id == b"fmt " {
            let fmt = bytes.get(body..body + 16).ok_or("short fmt chunk")?;
            let (tag, channels, bits) = (le16(&fmt[0..2]), le16(&fmt[2..4]), le16(&fmt[14..16]));
            let rate = u32::from_le_bytes(fmt[4..8].try_into().unwrap());
            if (tag, channels, bits, rate) != (1, 1, 16, 16_000) {
                return Err(format!(
                    "need 16 kHz PCM mono 16-bit, got tag {tag} ch {channels} bits {bits} rate {rate}"
                ));
            }
            format_ok = true;
        } else if id == b"data" {
            if !format_ok {
                return Err("data before fmt".into());
            }
            let end = body.saturating_add(size).min(bytes.len());
            return Ok(bytes[body..end]
                .as_chunks::<2>()
                .0
                .iter()
                .map(|s| i16::from_le_bytes(*s))
                .collect());
        }
        pos = body.saturating_add(size).saturating_add(size & 1);
    }
    Err("no data chunk".into())
}

fn le16(b: &[u8]) -> u16 {
    u16::from_le_bytes([b[0], b[1]])
}
