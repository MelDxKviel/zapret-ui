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

/// Helper to check if a directory has a valid installation.
/// We check if `winws.exe` exists in `bin/winws.exe` or `winws.exe`.
pub fn is_valid_install_dir(path: &Path) -> bool {
    path.join("bin").join("winws.exe").exists() || path.join("winws.exe").exists()
}
