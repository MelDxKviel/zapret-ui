// Cross-platform adapters. The bundle source, installer, strategy catalog,
// tester and version-comparison helper don't talk to any platform-specific
// API and are compiled on every target.
pub mod installer;
pub mod updater;
pub mod winbundle;
pub mod paths;
pub mod strategies;
pub mod tester;

// Windows-only adapters. `process` spawns winws2 through `CREATE_NO_WINDOW` +
// `CREATE_NEW_PROCESS_GROUP`; `service` talks to the SCM via `windows-service`;
// `elevation` checks the process token; `maintenance` issues a `taskkill` for
// the Discord cache clear. None of them have a Linux equivalent yet — a
// future nfqws2/systemd port lives in sibling modules.
#[cfg(target_os = "windows")]
pub mod process;
#[cfg(target_os = "windows")]
pub mod service;
#[cfg(target_os = "windows")]
pub mod elevation;
#[cfg(target_os = "windows")]
pub mod maintenance;
