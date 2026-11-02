//! Where Voisu keeps its files on Windows. Settings roam with the profile
//! (`%APPDATA%\Voisu`); logs stay on this machine
//! (`%LOCALAPPDATA%\Voisu\logs`) and never leave it unless the user exports
//! them (ADR 0006).

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

/// `%LOCALAPPDATA%\Voisu\logs`.
pub fn log_dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join(APP_DIR)
        .join("logs")
}
