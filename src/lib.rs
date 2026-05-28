// Cross-platform modules. Everything Windows-specific is partitioned below
// behind `#[cfg(target_os = "windows")]` so `cargo check` succeeds on Linux
// for the library — a precondition for a future cross-platform port.
pub mod contracts;
pub mod ports;
pub mod config;
pub mod i18n;
pub mod state;
pub mod log;
pub mod zapret;
pub mod selfupdate;

// Windows-only adapters — system tray, native toasts, single-instance via a
// named kernel mutex, the Win32 small-icon push, and Run-key autostart.
#[cfg(target_os = "windows")]
pub mod notify;
#[cfg(target_os = "windows")]
pub mod tray;
#[cfg(target_os = "windows")]
pub mod single_instance;
#[cfg(target_os = "windows")]
pub mod winicon;
#[cfg(target_os = "windows")]
pub mod winenv;
