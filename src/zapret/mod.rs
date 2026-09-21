pub mod batparse;
#[cfg_attr(target_os = "macos", path = "macos/catalog.rs")]
pub mod catalog;
#[cfg(windows)]
pub mod elevation;
pub mod github;
pub mod installer;
pub mod macos_bundle;
pub mod maintenance;
pub mod paths;
#[cfg_attr(target_os = "macos", path = "macos/runtime.rs")]
pub mod process;
#[cfg_attr(target_os = "macos", path = "macos/service.rs")]
pub mod service;
#[cfg(windows)]
pub mod tcp;
pub mod tester;
pub mod updater;
