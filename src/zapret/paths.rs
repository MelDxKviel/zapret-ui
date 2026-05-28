use std::path::{Path, PathBuf};
use directories::BaseDirs;

/// Default installation directory under %APPDATA%\zapret-ui\zapret\
pub fn default_install_dir() -> Option<PathBuf> {
    BaseDirs::new().map(|base| base.config_dir().join("zapret-ui").join("zapret"))
}

/// Machine-wide, admin-only directory used for **service-mode** binaries:
/// `%ProgramData%\zapret-ui\zapret`. The per-user `%APPDATA%` install dir is
/// writable by the (unprivileged) user, so pointing a `LocalSystem` service at
/// it would let that user swap `winws2.exe` and gain code execution as SYSTEM.
/// The elevated installer copies the install here and locks it down (see
/// `service.rs::prepare_protected_dir`) before registering the service.
pub fn service_install_dir() -> PathBuf {
    let program_data = std::env::var_os("ProgramData")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));
    program_data.join("zapret-ui").join("zapret")
}

/// Recognise a directory as a valid zapret2 install. The bundle (see
/// [`crate::zapret::winbundle`]) lays its Windows distribution out flat under
/// `zapret-winws/`, which the installer promotes to the install root — so a
/// healthy install has `winws2.exe` next to a `lua/` directory of Lua
/// strategy scripts. Both must be present: `winws2.exe` alone would start
/// but every preset that references `--lua-init=lua/…` would fail at runtime.
pub fn is_valid_install_dir(path: &Path) -> bool {
    path.join("winws2.exe").is_file() && path.join("lua").is_dir()
}
