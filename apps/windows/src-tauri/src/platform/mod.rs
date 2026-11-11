//! Windows and macOS implementations of voisu-core's platform seams: the
//! secret store, microphone capture, the Trigger Key keyboard hook, delivery
//! into the focused app, and (macOS only) the privacy permissions.

pub mod capture;
pub mod delivery;
#[cfg(windows)]
pub mod hotkey;
#[cfg(target_os = "macos")]
#[path = "hotkey_macos.rs"]
pub mod hotkey;
pub mod permissions;
pub mod secrets;
