//! Windows toast notifications for bypass start/stop events.
//!
//! Toasts are delivered through an AppUserModelID (AUMID) registered in HKCU so
//! Windows shows them under the app's own name instead of "Windows PowerShell"
//! — and so they appear at all (a registered AUMID is required for toast
//! delivery on Win10 1709+ when the app has no Start-menu shortcut). All calls
//! are best-effort: failures are logged and swallowed so a missing/blocked
//! notification system never affects the bypass itself.

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::process::CommandExt;

/// Our AppUserModelID. Stable so the registry entry is reused across runs.
const APP_ID: &str = "Flowseal.ZapretUI";
const APP_DISPLAY_NAME: &str = "Zapret UI";
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[link(name = "shell32")]
extern "system" {
    fn SetCurrentProcessExplicitAppUserModelID(app_id: *const u16) -> i32;
}

/// Register the AUMID (once, at startup) so toasts render under our app name.
/// Safe to call repeatedly; both steps are idempotent and non-fatal.
pub fn init() {
    // Register the AUMID display name in HKCU. Without this key Windows silently
    // drops toasts for a custom AppId that has no Start-menu shortcut.
    let key = format!("HKCU\\Software\\Classes\\AppUserModelId\\{APP_ID}");
    let result = std::process::Command::new("reg")
        .args([
            "add",
            &key,
            "/v",
            "DisplayName",
            "/t",
            "REG_SZ",
            "/d",
            APP_DISPLAY_NAME,
            "/f",
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .output();
    if let Err(e) = result {
        tracing::warn!("Failed to register notification AppUserModelID: {}", e);
    }

    let wide: Vec<u16> = OsStr::new(APP_ID).encode_wide().chain(Some(0)).collect();
    unsafe {
        SetCurrentProcessExplicitAppUserModelID(wide.as_ptr());
    }
}

/// Show a toast with `title` and `body`. Blocking and quick; call from a
/// blocking context (e.g. `tokio::task::spawn_blocking`) to keep it off the
/// async runtime.
pub fn show(title: &str, body: &str) {
    use tauri_winrt_notification::{Duration, Toast};

    let result = Toast::new(APP_ID)
        .title(title)
        .text1(body)
        .duration(Duration::Short)
        .show();
    if let Err(e) = result {
        tracing::warn!("Failed to show notification: {}", e);
    }
}
