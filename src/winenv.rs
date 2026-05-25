//! Small Windows environment helpers: the HKCU "Run" autostart entry and the
//! system light/dark preference. All calls are best-effort (failures logged and
//! swallowed) and shell out to `reg` to avoid pulling in a registry crate.

use std::os::windows::process::CommandExt;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "zapret-ui";
const PERSONALIZE_KEY: &str =
    r"HKCU\Software\Microsoft\Windows\CurrentVersion\Themes\Personalize";

/// Enable or disable launching this exe at user logon (HKCU Run key).
pub fn set_autostart(enable: bool) {
    let result = if enable {
        let exe = match std::env::current_exe() {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!("autostart: cannot resolve current exe: {e}");
                return;
            }
        };
        std::process::Command::new("reg")
            .args([
                "add", RUN_KEY, "/v", RUN_VALUE, "/t", "REG_SZ", "/d",
                &format!("\"{}\"", exe.display()), "/f",
            ])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
    } else {
        std::process::Command::new("reg")
            .args(["delete", RUN_KEY, "/v", RUN_VALUE, "/f"])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
    };
    if let Err(e) = result {
        tracing::warn!("autostart: failed to update Run key: {e}");
    }
}

/// True if Windows apps are currently using the dark theme. Reads
/// `AppsUseLightTheme` (0 = dark, 1 = light); defaults to dark when unreadable.
pub fn system_is_dark() -> bool {
    let out = std::process::Command::new("reg")
        .args(["query", PERSONALIZE_KEY, "/v", "AppsUseLightTheme"])
        .creation_flags(CREATE_NO_WINDOW)
        .output();
    match out {
        Ok(o) if o.status.success() => {
            let text = String::from_utf8_lossy(&o.stdout);
            // Line looks like: "    AppsUseLightTheme    REG_DWORD    0x0"
            for tok in text.split_whitespace().rev() {
                if let Some(hex) = tok.strip_prefix("0x") {
                    return u32::from_str_radix(hex, 16).map(|v| v == 0).unwrap_or(true);
                }
            }
            true
        }
        _ => true,
    }
}
