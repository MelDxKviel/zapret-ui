pub(super) fn open_external(target: &str) {
    let _ = std::process::Command::new("/usr/bin/open")
        .arg(target)
        .spawn();
}
/// `open` exits after LaunchServices accepts the request. Inspect its status
/// so a missing tg:// handler produces the same useful error as on Windows.
/// Arguments are passed directly, without interpreting link contents as shell.
pub(super) fn try_open_external(target: &str) -> bool {
    std::process::Command::new("/usr/bin/open")
        .arg(target)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
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
pub(super) fn open_hosts_file() -> anyhow::Result<()> {
    let status = std::process::Command::new("/usr/bin/open")
        .args(["-t", "/etc/hosts"])
        .status()?;
    anyhow::ensure!(
        status.success(),
        "Cannot open the hosts file in the text editor"
    );
    Ok(())
}
