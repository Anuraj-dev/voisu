//! Windows implementations of voisu-core's platform seams: the secret
//! store, microphone capture and the Ctrl+Win keyboard hook. Delivery
//! arrives in W3.

pub mod capture;
#[cfg(windows)]
pub mod hotkey;
pub mod secrets;
