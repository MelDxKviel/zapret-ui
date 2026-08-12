//! Windows TCP prerequisites mirrored from upstream `service.bat`.
//!
//! Every upstream strategy calls `service.bat status_zapret`, whose
//! `:tcp_enable` routine enables TCP timestamps before `winws.exe` starts.
//! Parsing the strategy command line directly used to skip that side effect.

use std::sync::atomic::{AtomicBool, Ordering};

static TCP_TIMESTAMPS_READY: AtomicBool = AtomicBool::new(false);
static TCP_TIMESTAMPS_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Idempotently enable the TCP timestamps prerequisite used by the upstream
/// strategy launcher. A successful call is cached for the lifetime of the app.
pub async fn ensure_tcp_timestamps_enabled() -> anyhow::Result<()> {
    if TCP_TIMESTAMPS_READY.load(Ordering::Acquire) {
        return Ok(());
    }

    let _guard = TCP_TIMESTAMPS_LOCK.lock().await;
    if TCP_TIMESTAMPS_READY.load(Ordering::Acquire) {
        return Ok(());
    }

    enable_tcp_timestamps().await?;
    TCP_TIMESTAMPS_READY.store(true, Ordering::Release);
    Ok(())
}

#[cfg(windows)]
async fn enable_tcp_timestamps() -> anyhow::Result<()> {
    use anyhow::Context;
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    use std::path::PathBuf;

    extern "system" {
        fn GetSystemDirectoryW(buffer: *mut u16, size: u32) -> u32;
    }

    // Resolve System32 through the Win32 API instead of PATH or environment
    // variables: the release binary is elevated, so executable search-order
    // hijacking here would otherwise become a privilege-escalation vector.
    let mut buffer = vec![0u16; 32_768];
    let len = unsafe { GetSystemDirectoryW(buffer.as_mut_ptr(), buffer.len() as u32) } as usize;
    if len == 0 || len >= buffer.len() {
        return Err(anyhow::anyhow!(
            "Failed to resolve the Windows system directory: {}",
            std::io::Error::last_os_error()
        ));
    }
    let netsh = PathBuf::from(OsString::from_wide(&buffer[..len])).join("netsh.exe");

    let mut command = tokio::process::Command::new(&netsh);
    command.args(["interface", "tcp", "set", "global", "timestamps=enabled"]);
    command.creation_flags(0x08000000); // CREATE_NO_WINDOW

    let output = command
        .output()
        .await
        .with_context(|| format!("Failed to launch {}", netsh.display()))?;
    if output.status.success() {
        tracing::info!("TCP timestamps are enabled (upstream zapret prerequisite)");
        return Ok(());
    }

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let detail = if !stderr.is_empty() {
        stderr
    } else if !stdout.is_empty() {
        stdout
    } else {
        format!("netsh exited with {}", output.status)
    };
    Err(anyhow::anyhow!(
        "Failed to enable TCP timestamps required by zapret: {detail}"
    ))
}

#[cfg(not(windows))]
async fn enable_tcp_timestamps() -> anyhow::Result<()> {
    Ok(())
}
