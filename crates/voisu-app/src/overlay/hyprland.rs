//! Which output Hyprland has focused, asked before the pill is mapped so the
//! surface can be bound there at its final margins. Hyprland animates a
//! mapped layer surface whose margins change, so learning the output from
//! the surface's own enter, after it is mapped, would slide the pill in.
//!
//! Other compositors (and a Hyprland whose socket doesn't answer in time)
//! get `None`, and the Overlay falls back to an unbound map.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// The longest the Overlay waits on Hyprland before mapping without it.
pub const IPC_DEADLINE: Duration = Duration::from_millis(50);

/// A `j/monitors` reply is a few KiB per output; past this it is not one.
const MAX_REPLY_BYTES: u64 = 1024 * 1024;

/// The connector name (`HDMI-A-1`) of the focused output in a Hyprland
/// `j/monitors` reply, or `None` if the reply isn't one or names none.
#[must_use]
pub fn focused_monitor(reply: &[u8]) -> Option<String> {
    let monitors: serde_json::Value = serde_json::from_slice(reply).ok()?;
    monitors
        .as_array()?
        .iter()
        .find(|monitor| monitor.get("focused").and_then(serde_json::Value::as_bool) == Some(true))?
        .get("name")?
        .as_str()
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
}

/// Asks the running Hyprland, if this session is one, for its focused
/// output, within [`IPC_DEADLINE`].
#[must_use]
pub fn focused_output() -> Option<String> {
    let instance = std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE")?;
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")?;
    let socket = PathBuf::from(runtime)
        .join("hypr")
        .join(instance)
        .join(".socket.sock");
    let deadline = Instant::now() + IPC_DEADLINE;
    let mut stream = UnixStream::connect(socket).ok()?;
    stream.set_write_timeout(Some(IPC_DEADLINE)).ok()?;
    stream.write_all(b"j/monitors").ok()?;
    let mut reply = Vec::new();
    let mut chunk = [0_u8; 8192];
    loop {
        let left = deadline.checked_duration_since(Instant::now())?;
        if left.is_zero() {
            return None;
        }
        stream.set_read_timeout(Some(left)).ok()?;
        match stream.read(&mut chunk).ok()? {
            0 => break,
            read => reply.extend_from_slice(&chunk[..read]),
        }
        if reply.len() as u64 > MAX_REPLY_BYTES {
            return None;
        }
    }
    focused_monitor(&reply)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_focused_monitor_is_the_one_marked_focused() {
        let reply = br#"[
            {"id": 0, "name": "eDP-1", "width": 1920, "height": 1200, "scale": 1.25, "focused": false},
            {"id": 1, "name": "HDMI-A-1", "width": 1920, "height": 1080, "scale": 1.00, "focused": true}
        ]"#;
        assert_eq!(focused_monitor(reply), Some("HDMI-A-1".to_owned()));
    }

    #[test]
    fn no_focused_monitor_or_a_reply_that_isnt_one_names_nothing() {
        assert_eq!(
            focused_monitor(br#"[{"name": "eDP-1", "focused": false}]"#),
            None
        );
        assert_eq!(focused_monitor(b"[]"), None);
        assert_eq!(focused_monitor(b"unknown request"), None);
        assert_eq!(
            focused_monitor(br#"{"name": "eDP-1", "focused": true}"#),
            None
        );
        assert_eq!(focused_monitor(br#"[{"focused": true}]"#), None);
        assert_eq!(focused_monitor(br#"[{"name": "", "focused": true}]"#), None);
        assert_eq!(focused_monitor(br#"[{"name": 3, "focused": true}]"#), None);
        assert_eq!(focused_monitor(b""), None);
    }
}
