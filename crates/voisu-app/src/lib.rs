// BoundaryError carries rich failure diagnostics by design, and the system
// boundary returns it pervasively in Err position. Boxing/shrinking it is a
// cross-cutting change owned by the hardening-05 hygiene sweep, not the CI gate.
#![allow(clippy::result_large_err)]

pub mod config;
pub mod dictionary;
pub mod history_view;
pub mod feedback;
mod process;
pub mod overlay;
pub mod service;
pub mod system;
