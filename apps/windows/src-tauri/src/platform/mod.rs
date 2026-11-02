//! Windows implementations of voisu-core's platform seams: the secret
//! store, microphone capture, the Ctrl+Win keyboard hook and delivery into
//! the focused app.

pub mod capture;
pub mod delivery;
#[cfg(windows)]
pub mod hotkey;
pub mod secrets;
