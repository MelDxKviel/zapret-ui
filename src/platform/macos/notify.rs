use crate::zapret::macos_bundle::applescript_string;
pub fn init() {}
pub fn show(title: &str, body: &str) {
    let script = format!(
        "display notification {} with title {}",
        applescript_string(body),
        applescript_string(title)
    );
    if let Err(e) = std::process::Command::new("/usr/bin/osascript")
        .args(["-e", &script])
        .output()
    {
        tracing::debug!("Notification unavailable: {e}");
    }
}
