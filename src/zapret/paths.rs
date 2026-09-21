use std::path::{Path, PathBuf};

/// Machine-wide, admin-only directory used for **service-mode** binaries:
/// `%ProgramData%\zapret-ui\zapret`. The per-user `%APPDATA%` install dir is
/// writable by the (unprivileged) user, so pointing a `LocalSystem` service at
/// it would let that user swap `winws.exe` and gain code execution as SYSTEM.
/// The elevated installer copies the install here and locks it down (see
/// `service.rs::prepare_protected_dir`) before registering the service.
pub fn service_install_dir() -> PathBuf {
    let program_data = std::env::var_os("ProgramData")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));
    program_data.join("zapret-ui").join("zapret")
}

/// Machine-wide location for one-shot elevated helper result files. The helper
/// locks this directory down to Administrators/System write + Users read before
/// writing a result, so same-user processes cannot race a forged outcome in temp.
pub fn elevation_result_dir() -> PathBuf {
    let program_data = std::env::var_os("ProgramData")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));
    program_data.join("zapret-ui").join("elev-results")
}

/// Helper to check if a directory has a valid installation.
/// We check if `winws.exe` exists in `bin/winws.exe` or `winws.exe`.
pub fn is_valid_install_dir(path: &Path) -> bool {
    #[cfg(target_os = "macos")]
    {
        crate::zapret::macos_bundle::valid_payload(path)
    }
    #[cfg(not(target_os = "macos"))]
    {
        path.join("bin").join("winws.exe").exists() || path.join("winws.exe").exists()
    }
}

pub fn lists_dir(install_dir: &Path) -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        let _ = install_dir;
        crate::zapret::macos_bundle::user_data_dir()
            .unwrap_or_default()
            .join("lists")
    }
    #[cfg(not(target_os = "macos"))]
    {
        install_dir.join("lists")
    }
}
