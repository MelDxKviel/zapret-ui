//! Shared application, UI and backend for Windows and macOS.
//! Platform-specific behavior is selected only at the adapter boundary.

pub mod app;
mod app_dirs;
pub mod config;
pub mod contracts;
mod download;
pub mod i18n;
pub mod log;
#[cfg_attr(target_os = "macos", path = "platform/macos/notify.rs")]
pub mod notify;
pub mod ports;
mod release_feed;
pub mod selfupdate;
#[cfg_attr(target_os = "macos", path = "platform/macos/single_instance.rs")]
pub mod single_instance;
pub mod state;
pub mod telegram;
pub mod tray;
#[cfg_attr(target_os = "macos", path = "platform/macos/winenv.rs")]
pub mod winenv;
#[cfg(windows)]
pub mod winicon;
pub mod zapret;
