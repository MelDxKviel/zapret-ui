//! Per-user GUI data root shared by settings, logs, downloads and instance state.
//! Core-owned and protected system paths stay in their respective adapters.
use std::path::PathBuf;

/// `%APPDATA%/zapret-ui` on Windows; `~/Library/Application Support/zapret-ui`
/// on macOS. Callers retain their own error/fallback behavior if unavailable.
pub fn root() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|base| base.config_dir().join("zapret-ui"))
}
