//! Native macOS desktop integration (the module name is kept for existing callers).
use std::process::Command;

fn xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
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
            let exe = xml(&std::env::current_exe()?.to_string_lossy());
            std::fs::write(
                path,
                format!(
                    r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict><key>Label</key><string>io.github.zapret-ui</string><key>ProgramArguments</key><array><string>{exe}</string></array><key>RunAtLoad</key><true/></dict></plist>"#
                ),
            )?;
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
