//! Local, rotated log files (ADR 0006). Callers log statuses and
//! diagnostics only, never key material.

use std::path::Path;

use tracing_appender::rolling::{RollingFileAppender, Rotation};

/// Daily files, at most a week kept.
const MAX_LOG_FILES: usize = 7;

/// Starts file logging. Logging failure never stops the app.
///
/// Writes are synchronous: the app logs a few status lines, and the
/// updater ends the process with `exit(0)`, which a background writer would
/// cut short.
pub fn init(dir: &Path) {
    let appender = RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix("voisu")
        .filename_suffix("log")
        .max_log_files(MAX_LOG_FILES)
        .build(dir);
    let Ok(appender) = appender else { return };
    let subscriber = tracing_subscriber::fmt()
        .with_writer(appender)
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .try_init();
    if subscriber.is_err() {
        return;
    }
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        tracing::error!(%info, "panic");
        default_hook(info);
    }));
}
