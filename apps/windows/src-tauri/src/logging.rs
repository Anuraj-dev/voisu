//! Local, rotated log files (ADR 0006). Callers log statuses and
//! diagnostics only, never key material.

use std::path::Path;

use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::{RollingFileAppender, Rotation};

/// Daily files, at most a week kept.
const MAX_LOG_FILES: usize = 7;

/// Starts file logging; the returned guard flushes on drop, so `run` holds
/// it for the process lifetime. Logging failure never stops the app.
pub fn init(dir: &Path) -> Option<WorkerGuard> {
    let appender = RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix("voisu")
        .filename_suffix("log")
        .max_log_files(MAX_LOG_FILES)
        .build(dir)
        .ok()?;
    let (writer, guard) = tracing_appender::non_blocking(appender);
    tracing_subscriber::fmt()
        .with_writer(writer)
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .try_init()
        .ok()?;
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        tracing::error!(%info, "panic");
        default_hook(info);
    }));
    Some(guard)
}
