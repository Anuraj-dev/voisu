use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::process::ExitCode;

use voisu_core::{Command, PROTOCOL_VERSION, Request, Response, socket_path};

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

    let request = Request {
        version: PROTOCOL_VERSION,
        command,
    };
    if serde_json::to_writer(&mut stream, &request).is_err() || stream.write_all(b"\n").is_err() {
        return fail(1, "failed to send command to daemon");
    }

    let mut response = String::new();
    if BufReader::new(stream).read_line(&mut response).is_err() {
        return fail(1, "failed to read daemon response");
    }
    let response: Response = match serde_json::from_str(&response) {
        Ok(response) => response,
        Err(_) => return fail(1, "daemon returned an invalid response"),
    };
    if response.version != PROTOCOL_VERSION {
        return fail(
            5,
            &format!(
                "IPC protocol mismatch: daemon uses {}, CLI uses {}",
                response.version, PROTOCOL_VERSION
            ),
        );
    }
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
