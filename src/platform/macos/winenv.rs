//! Native macOS desktop integration (the module name is kept for existing callers).
use std::{io::Write, path::Path, process::Command};

/// Bring an already shown Slint window to the foreground, including a window
/// restored from the menu bar or a second app launch. Must run on the UI thread.
pub fn focus_window(window: &slint::Window) {
    use slint::winit_030::WinitWindowAccessor;
    window.set_minimized(false);
    window.with_winit_window(|native| native.focus_window());
}

fn xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn login_agent(exe: &Path) -> String {
    // LaunchServices preserves the app bundle identity at login (Dock icon,
    // menu bar and notifications). A development binary can still run directly.
    let bundle = exe
        .parent()
        .filter(|dir| dir.file_name().is_some_and(|name| name == "MacOS"))
        .and_then(Path::parent)
        .filter(|dir| dir.file_name().is_some_and(|name| name == "Contents"))
        .and_then(Path::parent)
        .filter(|dir| dir.extension().is_some_and(|ext| ext == "app"));
    let arguments = match bundle {
        Some(path) => format!(
            "<string>/usr/bin/open</string><string>-a</string><string>{}</string>",
            xml(&path.to_string_lossy())
        ),
        None => format!("<string>{}</string>", xml(&exe.to_string_lossy())),
    };
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>Label</key><string>io.github.zapret-ui</string>
<key>ProgramArguments</key><array>{arguments}</array>
<key>RunAtLoad</key><true/>
<key>LimitLoadToSessionType</key><string>Aqua</string>
</dict></plist>"#
    )
}

pub fn set_autostart(enable: bool) {
    let result = (|| -> anyhow::Result<()> {
        let home =
            directories::BaseDirs::new().ok_or_else(|| anyhow::anyhow!("Cannot resolve home"))?;
        let path = home
            .home_dir()
            .join("Library/LaunchAgents/io.github.zapret-ui.plist");
        if enable {
            std::fs::create_dir_all(path.parent().unwrap())?;
            let mut staged = tempfile::NamedTempFile::new_in(path.parent().unwrap())?;
            staged.write_all(login_agent(&std::env::current_exe()?).as_bytes())?;
            staged.as_file().sync_all()?;
            staged.persist(path)?;
        } else if path.exists() {
            std::fs::remove_file(path)?;
        }
        Ok(())
    })();
    if let Err(e) = result {
        tracing::warn!("Cannot update login LaunchAgent: {e:#}");
    }
}

pub fn system_is_dark() -> bool {
    Command::new("/usr/bin/defaults")
        .args(["read", "-g", "AppleInterfaceStyle"])
        .output()
        .is_ok_and(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).trim() == "Dark")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_login_uses_launch_services_and_escapes_paths() {
        let plist = login_agent(Path::new(
            "/Applications/Tools & Apps/Zapret UI.app/Contents/MacOS/zapret-ui",
        ));
        assert!(plist.contains("<string>/usr/bin/open</string><string>-a</string>"));
        assert!(plist.contains("<string>/Applications/Tools &amp; Apps/Zapret UI.app</string>"));
        assert!(!plist.contains("Contents/MacOS"));
        assert!(plist.contains("<key>LimitLoadToSessionType</key><string>Aqua</string>"));
    }

    #[test]
    fn unbundled_login_runs_binary_directly() {
        let plist = login_agent(Path::new("/Users/test/build/zapret-ui"));
        assert!(plist.contains("<array><string>/Users/test/build/zapret-ui</string></array>"));
        assert!(!plist.contains("/usr/bin/open"));
    }
}
