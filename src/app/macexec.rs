pub(super) fn open_external(target: &str) {
    let _ = std::process::Command::new("/usr/bin/open")
        .arg(target)
        .spawn();
}
pub(super) fn relaunch_after_update() -> anyhow::Result<()> {
    std::process::Command::new(std::env::current_exe()?)
        .arg("--relaunch")
        .spawn()?;
    Ok(())
}
pub(super) fn copy_to_clipboard(text: &str) -> anyhow::Result<()> {
    use std::io::Write;
    let mut child = std::process::Command::new("/usr/bin/pbcopy")
        .stdin(std::process::Stdio::piped())
        .spawn()?;
    child.stdin.take().unwrap().write_all(text.as_bytes())?;
    if !child.wait()?.success() {
        anyhow::bail!("pbcopy failed");
    }
    Ok(())
}
pub(super) fn open_hosts_file() {
    let _ = std::process::Command::new("/usr/bin/open")
        .args(["-t", "/etc/hosts"])
        .spawn();
}
