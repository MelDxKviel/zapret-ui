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
    let netsh = crate::zapret::paths::system_executable("netsh.exe")?;

    // `dump` emits stable command tokens even on localized Windows. Query first:
    // an unelevated development build can use an already-enabled setting without
    // attempting a privileged SET.
    let mut query = tokio::process::Command::new(&netsh);
    query.args(["interface", "tcp", "dump"]);
    query.creation_flags(0x08000000);
    if let Ok(output) = query.output().await {
        if output.status.success()
            && timestamps_enabled_from_dump(&String::from_utf8_lossy(&output.stdout))
        {
            tracing::info!("TCP timestamps are already enabled");
            return Ok(());
        }
    }

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

#[cfg(any(windows, test))]
fn timestamps_enabled_from_dump(dump: &str) -> bool {
    dump.lines().any(|line| {
        let mut tokens = line.split_whitespace();
        matches!(tokens.next(), Some(token) if token.eq_ignore_ascii_case("set"))
            && matches!(tokens.next(), Some(token) if token.eq_ignore_ascii_case("global"))
            && tokens.any(|token| token.eq_ignore_ascii_case("timestamps=enabled"))
    })
}

#[cfg(not(windows))]
async fn enable_tcp_timestamps() -> anyhow::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_query_only_accepts_enabled_global_setting() {
        assert!(timestamps_enabled_from_dump(
            "# TCP configuration\r\nset global rss=enabled timestamps=enabled initialrto=1000\r\n"
        ));
        assert!(timestamps_enabled_from_dump(
            "SET GLOBAL TIMESTAMPS=ENABLED"
        ));
        assert!(!timestamps_enabled_from_dump(
            "set global timestamps=disabled"
        ));
        assert!(!timestamps_enabled_from_dump(
            "# set global timestamps=enabled"
        ));
        assert!(!timestamps_enabled_from_dump(
            "set supplemental timestamps=enabled"
        ));
    }
}
