pub mod config;
pub mod contracts;
pub mod i18n;
pub mod log;
#[cfg_attr(target_os = "macos", path = "platform/macos/notify.rs")]
pub mod notify;
pub mod ports;
pub mod selfupdate;
#[cfg_attr(target_os = "macos", path = "platform/macos/single_instance.rs")]
pub mod single_instance;
pub mod state;
pub mod tray;
#[cfg_attr(target_os = "macos", path = "platform/macos/winenv.rs")]
pub mod winenv;
pub mod zapret;
