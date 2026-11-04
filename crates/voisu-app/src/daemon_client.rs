//! One bounded round trip to the daemon, returning data instead of printing.
//!
//! The CLI maps [`DaemonClientError`] to its historic output and exit codes; the
//! TUI maps it to words on screen. Neither behaviour lives here.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use voisu_core::{Command, PROTOCOL_VERSION, Request, Response, VersionEnvelope, socket_path};

use crate::system::DIAGNOSTIC_RESPONSE_DEADLINE;

/// The most response the CLI will buffer — per transport frame, and in total
/// across the pages of one diagnostic payload.
///
/// Derived from the retention policy rather than picked: every retained record
/// can hold two Source Transcripts and one final Transcript at the stored-text
/// clamp, so the retained transcript budget is `DEFAULT_MAX_RECORDS * 3 *
/// MAX_STORED_TEXT`. This allows a fourth clamp's worth per record for JSON
/// escaping and the record's fixed fields, plus a flat four mebibytes of slack
/// for provider-failure diagnostics, which the store does not clamp. Paging
/// removed the previous bound rather than raising it, leaving the CLI's memory
/// use answerable only to the 15 s diagnostic deadline — which on a local Unix
/// socket permits gigabytes.
const MAX_RESPONSE_BYTES: usize =
    voisu_core::DEFAULT_MAX_RECORDS * 4 * voisu_core::MAX_STORED_TEXT + 4 * 1024 * 1024;
const IO_DEADLINE: Duration = Duration::from_secs(2);
#[derive(Clone, Copy)]
enum DiagnosticResponseKind {
    History,
    Export,
}

/// Why a daemon round trip produced no response.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DaemonClientError {
    /// The socket path could not be resolved (the runtime directory is unsafe
    /// or missing). The CLI reports this message and exits 2.
    Socket(String),
    /// Nothing accepted the connection. The CLI prints `daemon unavailable`
    /// and exits 3.
    Unavailable,
    /// The round trip began and failed. The CLI reports `message` and exits
    /// with `code`.
    Failed { code: u8, message: String },
}

/// Sends one command to the daemon and returns its typed response.
pub fn send_command(command: Command) -> Result<Response, DaemonClientError> {
    let path = socket_path().map_err(DaemonClientError::Socket)?;
    let mut stream = UnixStream::connect(path).map_err(|_| DaemonClientError::Unavailable)?;

    if stream.set_write_timeout(Some(IO_DEADLINE)).is_err() {
        return Err(DaemonClientError::Failed {
            code: 1,
            message: "failed to configure daemon connection deadline".to_owned(),
        });
    }

    // A replay drives the same provider/validation boundaries as Stop, so it
    // shares the longer processing budget. `history` and `export` read the
    // bounded diagnostic store instead, which has its own budget.
    let response_deadline = match &command {
        Command::Stop | Command::Toggle | Command::Replay(_) => {
            crate::asr_mode::processing_response_deadline()
        }
        Command::History | Command::Export(_) => DIAGNOSTIC_RESPONSE_DEADLINE,
        _ => IO_DEADLINE,
    };
    let diagnostic_kind = match &command {
        Command::History => Some(DiagnosticResponseKind::History),
        Command::Export(_) => Some(DiagnosticResponseKind::Export),
        _ => None,
    };
    // Only a diagnostic command can outgrow one frame, and only this CLI can
    // reassemble pages, so paging is asked for per request rather than assumed.
    // A daemon that predates paging ignores the field and answers in one frame,
    // which `read_response` still accepts.
    let request = if diagnostic_kind.is_some() {
        Request::paged(command)
    } else {
        Request::new(command)
    };
    if serde_json::to_writer(&mut stream, &request).is_err() || stream.write_all(b"\n").is_err() {
        return Err(DaemonClientError::Failed {
            code: 1,
            message: "failed to send command to daemon".to_owned(),
        });
    }
    read_response(
        &mut BufReader::new(stream),
        response_deadline,
        diagnostic_kind,
    )
    .map_err(|(code, message)| DaemonClientError::Failed { code, message })
}

fn read_response_frame(
    stream: &mut BufReader<UnixStream>,
    deadline: Duration,
    budget: usize,
) -> Result<String, String> {
    let started = Instant::now();
    let mut response = Vec::new();
    loop {
        let remaining = deadline
            .checked_sub(started.elapsed())
            .filter(|remaining| !remaining.is_zero())
            .ok_or_else(|| "daemon response deadline elapsed".to_owned())?;
        stream
            .get_ref()
            .set_read_timeout(Some(remaining))
            .map_err(|_| "failed to configure daemon connection deadline".to_owned())?;
        match stream.fill_buf() {
            Ok([]) => return Err("daemon response frame is incomplete".to_owned()),
            Ok(available) => {
                let frame_end = available.iter().position(|byte| *byte == b'\n');
                let consumed = frame_end.map_or(available.len(), |position| position + 1);
                response.extend_from_slice(&available[..consumed]);
                stream.consume(consumed);
                // Checked against what has been buffered SO FAR, so an oversized
                // frame is refused while it streams rather than after the CLI
                // has already allocated all of it.
                if response.len() > budget {
                    return Err("daemon response frame is too large".to_owned());
                }
                if frame_end.is_some() {
                    return String::from_utf8(response)
                        .map_err(|_| "daemon returned an invalid response".to_owned());
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                return Err("daemon response deadline elapsed".to_owned());
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return Err("failed to read daemon response".to_owned()),
        }
    }
}

fn read_response(
    stream: &mut BufReader<UnixStream>,
    deadline: Duration,
    diagnostic_kind: Option<DiagnosticResponseKind>,
) -> Result<Response, (u8, String)> {
    let started = Instant::now();
    let mut payload = String::new();
    let mut expected_sequence = 0;
    loop {
        let remaining = deadline
            .checked_sub(started.elapsed())
            .filter(|remaining| !remaining.is_zero())
            .ok_or_else(|| (1, "daemon response deadline elapsed".to_owned()))?;
        // The remaining budget, not a fresh one per frame: a paged payload is
        // one logical response, so the ceiling has to bound the accumulation
        // across pages, not just each page in isolation.
        let budget = MAX_RESPONSE_BYTES.saturating_sub(payload.len());
        let frame =
            read_response_frame(stream, remaining, budget).map_err(|message| (1, message))?;
        let envelope: VersionEnvelope = serde_json::from_str(&frame)
            .map_err(|_| (1, "daemon returned an invalid response".to_owned()))?;
        if envelope.version != PROTOCOL_VERSION {
            return Err((
                5,
                format!(
                    "IPC protocol mismatch: daemon uses {}, CLI uses {}",
                    envelope.version, PROTOCOL_VERSION
                ),
            ));
        }
        let mut response: Response = serde_json::from_str(&frame)
            .map_err(|_| (1, "daemon returned an invalid response".to_owned()))?;
        let Some(page) = response.diagnostic_page.take() else {
            if expected_sequence == 0 {
                return Ok(response);
            }
            return Err((
                1,
                "daemon returned an incomplete diagnostic response".to_owned(),
            ));
        };
        let Some(kind) = diagnostic_kind else {
            return Err((
                1,
                "daemon returned an unexpected diagnostic page".to_owned(),
            ));
        };
        if page.sequence != expected_sequence {
            return Err((
                1,
                "daemon returned diagnostic pages out of order".to_owned(),
            ));
        }
        expected_sequence += 1;
        payload.push_str(&page.payload);
        if !page.last {
            continue;
        }
        match kind {
            DiagnosticResponseKind::History => {
                response.history = Some(serde_json::from_str(&payload).map_err(|_| {
                    (
                        1,
                        "daemon returned an invalid diagnostic history".to_owned(),
                    )
                })?);
            }
            DiagnosticResponseKind::Export => {
                response.export =
                    Some(serde_json::from_str(&payload).map_err(|_| {
                        (1, "daemon returned an invalid diagnostic export".to_owned())
                    })?);
            }
        }
        return Ok(response);
    }
}
