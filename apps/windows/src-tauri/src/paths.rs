//! Where Voisu keeps its files. On Windows settings roam with the profile
//! (`%APPDATA%\Voisu`); logs stay on this machine
//! (`%LOCALAPPDATA%\Voisu\logs`). On macOS both live under
//! `~/Library/Application Support/Voisu`. Logs never leave the machine
//! unless the user exports them (ADR 0006).

use std::path::PathBuf;

const APP_DIR: &str = "Voisu";

/// `%APPDATA%\Voisu`.
pub fn config_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join(APP_DIR)
}

/// `%APPDATA%\Voisu\settings.json`.
pub fn settings_file() -> PathBuf {
    config_dir().join("settings.json")
}

/// `%APPDATA%\Voisu\star-prompt.json`: the local dictation count behind
/// the one-time star prompt.
pub fn star_prompt_file() -> PathBuf {
    config_dir().join("star-prompt.json")
}

/// `%LOCALAPPDATA%\Voisu\logs`.
pub fn log_dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join(APP_DIR)
        .join("logs")
}
