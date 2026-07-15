use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::process::ExitCode;
use std::time::Duration;

use voisu_core::{
    Command, PROTOCOL_VERSION, Request, Response, VersionEnvelope, socket_path,
};

const MAX_RESPONSE_BYTES: u64 = 16 * 1024;
const IO_DEADLINE: Duration = Duration::from_secs(2);

fn main() -> ExitCode {
    let command = match parse_command() {
        Ok(command) => command,
        Err(message) => return fail(2, &message),
    };
    let path = match socket_path() {
        Ok(path) => path,
        Err(message) => return fail(2, &message),
    };
    let mut stream = match UnixStream::connect(path) {
        Ok(stream) => stream,
        Err(_) => {
            println!("daemon unavailable");
            return ExitCode::from(3);
        }
    };

    if stream.set_read_timeout(Some(IO_DEADLINE)).is_err()
        || stream.set_write_timeout(Some(IO_DEADLINE)).is_err()
    {
        return fail(1, "failed to configure daemon connection deadline");
    }

    let request = Request {
        version: PROTOCOL_VERSION,
        command,
    };
    if serde_json::to_writer(&mut stream, &request).is_err() || stream.write_all(b"\n").is_err() {
        return fail(1, "failed to send command to daemon");
    }

    // Bounded, deadline-guarded read: never trust the daemon to send a
    // terminated frame within a finite size.
    let mut response = String::new();
    let mut limited = BufReader::new(stream).take(MAX_RESPONSE_BYTES + 1);
    if limited.read_line(&mut response).is_err() {
        return fail(1, "failed to read daemon response");
    }
    if response.len() as u64 > MAX_RESPONSE_BYTES || !response.ends_with('\n') {
        return fail(1, "daemon response frame is too large or incomplete");
    }

    // Envelope-first decode: reject a protocol mismatch before trusting the
    // rest of the payload to match this CLI's schema.
    let envelope: VersionEnvelope = match serde_json::from_str(&response) {
        Ok(envelope) => envelope,
        Err(_) => return fail(1, "daemon returned an invalid response"),
    };
    if envelope.version != PROTOCOL_VERSION {
        return fail(
            5,
            &format!(
                "IPC protocol mismatch: daemon uses {}, CLI uses {}",
                envelope.version, PROTOCOL_VERSION
            ),
        );
    }
    let response: Response = match serde_json::from_str(&response) {
        Ok(response) => response,
        Err(_) => return fail(1, "daemon returned an invalid response"),
    };
    if response.ok {
        println!("{}", response.message);
        ExitCode::SUCCESS
    } else {
        fail(4, &response.message)
    }
}

fn parse_command() -> Result<Command, String> {
    let mut arguments = std::env::args().skip(1);
    let command = match arguments.next().as_deref() {
        Some("start") => Command::Start,
        Some("stop") => Command::Stop,
        Some("toggle") => Command::Toggle,
        Some("status") => Command::Status,
        _ => return Err("usage: voisu <start|stop|toggle|status>".to_owned()),
    };
    if arguments.next().is_some() {
        return Err("usage: voisu <start|stop|toggle|status>".to_owned());
    }
    Ok(command)
}

fn fail(code: u8, message: &str) -> ExitCode {
    eprintln!("{message}");
    ExitCode::from(code)
}
