//! Voisu's private Local speech-to-text worker.
//!
//! The daemon launches `voisu-local-asr --manifest <path>` in a sandbox and
//! speaks worker protocol v1 with it over stdin/stdout. Only this process
//! links whisper.cpp and ggml; the daemon never does.

pub mod echo;
pub mod engine;
pub mod manifest;
pub mod server;
pub mod window;
pub mod wire;
