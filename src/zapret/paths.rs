use std::path::{Path, PathBuf};
use directories::BaseDirs;

#[cfg(test)]
#[path = "../config.rs"]
pub mod config_stub;

#[cfg(test)]
use config_stub::AppConfig;

#[cfg(not(test))]
use crate::config::AppConfig;

/// Default installation directory under %APPDATA%\zapret-ui\zapret\
pub fn default_install_dir() -> Option<PathBuf> {
    BaseDirs::new().map(|base| base.config_dir().join("zapret-ui").join("zapret"))
}

/// Directory adjacent to the current executable: <exe_dir>\zapret\
pub fn adjacent_install_dir() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|parent| parent.join("zapret")))
}

/// Helper to check if a directory has a valid installation.
/// We check if `winws.exe` exists in `bin/winws.exe` or `winws.exe`.
pub fn is_valid_install_dir(path: &Path) -> bool {
    path.join("bin").join("winws.exe").exists() || path.join("winws.exe").exists()
}

/// Resolves the active installation directory based on the following precedence:
/// 1. User overridden path in config (if it points to a valid install)
/// 2. Adjacent to current executable (if it contains a valid install)
/// 3. Default path (if it contains a valid install)
/// 
/// If no valid installation is detected, returns the default path or overridden path if set.
pub fn resolve_install_dir() -> PathBuf {
    // 1. User overridden path in config
    let config = AppConfig::load();
    if let Some(config_path) = config.install_dir_override {
        if is_valid_install_dir(&config_path) {
            return config_path.to_path_buf();
        }
    }

    // 2. Adjacent to current executable
    if let Some(adj_path) = adjacent_install_dir() {
        if is_valid_install_dir(&adj_path) {
            return adj_path;
        }
    }

    // 3. Default path (if it exists and is valid)
    if let Some(def_path) = default_install_dir() {
        if is_valid_install_dir(&def_path) {
            return def_path;
        }
    }

    // Default fallback
    let config = AppConfig::load();
    if let Some(config_path) = config.install_dir_override {
        return config_path.to_path_buf();
    }

    default_install_dir().unwrap_or_else(|| PathBuf::from("zapret"))
}
