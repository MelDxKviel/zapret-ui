//! Windows zapret2 runtime distribution and data-only strategy catalog.
pub mod catalog;
pub mod installer;

pub use catalog::Zapret2Catalog;
pub use installer::Zapret2Installer;

use std::path::Path;

pub const RUNTIME_FILES: &[&str] = &[
    "bin/winws2.exe",
    "bin/cygwin1.dll",
    "bin/WinDivert.dll",
    "bin/WinDivert64.sys",
    "lua/zapret-lib.lua",
    "lua/zapret-antidpi.lua",
];

pub fn is_valid_install_dir(path: &Path) -> bool {
    [path.to_path_buf(), path.join("bin"), path.join("lua")]
        .iter()
        .all(|dir| {
            std::fs::symlink_metadata(dir).is_ok_and(|meta| meta.is_dir() && !is_link(&meta))
        })
        && RUNTIME_FILES.iter().all(|name| {
            std::fs::symlink_metadata(path.join(name))
                .is_ok_and(|meta| meta.is_file() && !is_link(&meta) && meta.len() > 0)
        })
}

pub(crate) fn is_link(meta: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    meta.file_type().is_symlink() || meta.file_attributes() & 0x400 != 0
}

/// Archive/data paths must be portable relative paths, never Win32 device names,
/// drive paths, alternate streams, or a path containing traversal components.
pub(crate) fn safe_relative(value: &str) -> bool {
    !value.is_empty()
        && !value.contains(['\\', ':', '\0', '?', '*', '<', '>', '"'])
        && value.split('/').all(|part| {
            !part.is_empty()
                && part != "."
                && part != ".."
                && !part.ends_with(['.', ' '])
                && !part.chars().any(|c| c.is_control())
                && !matches!(
                    part.split('.')
                        .next()
                        .unwrap_or("")
                        .to_ascii_uppercase()
                        .as_str(),
                    "CON"
                        | "PRN"
                        | "AUX"
                        | "NUL"
                        | "COM1"
                        | "COM2"
                        | "COM3"
                        | "COM4"
                        | "COM5"
                        | "COM6"
                        | "COM7"
                        | "COM8"
                        | "COM9"
                        | "LPT1"
                        | "LPT2"
                        | "LPT3"
                        | "LPT4"
                        | "LPT5"
                        | "LPT6"
                        | "LPT7"
                        | "LPT8"
                        | "LPT9"
                )
        })
}
