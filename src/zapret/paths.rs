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
        path.join("bin").join("winws.exe").is_file() || path.join("winws.exe").is_file()
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

/// Resolve a Windows utility without searching the working directory, PATH or
/// environment variables. These callers can run elevated.
pub fn system_executable(name: &str) -> anyhow::Result<PathBuf> {
    if name.is_empty() || name.contains(['/', '\\', ':']) || name == "." || name == ".." {
        anyhow::bail!("Expected a system executable basename, got {name:?}");
    }
    #[cfg(windows)]
    {
        use std::ffi::OsString;
        use std::os::windows::ffi::OsStringExt;

        extern "system" {
            fn GetSystemDirectoryW(buffer: *mut u16, size: u32) -> u32;
        }
        let mut buffer = vec![0u16; 32_768];
        let len = unsafe { GetSystemDirectoryW(buffer.as_mut_ptr(), buffer.len() as u32) } as usize;
        if len == 0 || len >= buffer.len() {
            anyhow::bail!(
                "Failed to resolve the Windows system directory: {}",
                std::io::Error::last_os_error()
            );
        }
        Ok(PathBuf::from(OsString::from_wide(&buffer[..len])).join(name))
    }
    #[cfg(not(windows))]
    {
        anyhow::bail!("Windows system utilities are unavailable on this platform")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_directory_named_winws_is_not_an_installation() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("winws.exe")).unwrap();
        assert!(!is_valid_install_dir(tmp.path()));
    }

    #[test]
    fn system_executable_rejects_paths() {
        for name in ["", "..", "../sc.exe", r"C:\sc.exe", r"sub\sc.exe"] {
            assert!(system_executable(name).is_err());
        }
    }

    #[cfg(windows)]
    #[test]
    fn system_executable_resolves_an_absolute_system_path() {
        let exe = system_executable("sc.exe").unwrap();
        assert!(exe.is_absolute());
        assert!(exe.is_file());
        assert_eq!(exe.file_name().unwrap(), "sc.exe");
    }
}
