//! DPI tuning surface for zapret2.
//!
//! After the `bol-van/zapret2` migration this is much smaller than the
//! pre-zapret-2 "Maintenance" port was: the Flowseal-port concepts
//! `service.bat` exposed — game-filter port-range hack, `ipset-all.txt`
//! any/none/loaded swap, hosts-file merge — don't translate to zapret2's
//! flag model on Windows. What remains is hostlist housekeeping (refresh
//! the curated lists in `<install>/files/`) and the Discord cache clear
//! (Windows-side housekeeping, zapret-version-agnostic).
//!
//! The file is intentionally still named `maintenance.rs` so `git blame`
//! survives the rewrite; the publicly-exported type is now `ZapretDpiTuning`.

use std::path::PathBuf;

use anyhow::{Context, Result};
use reqwest::header::USER_AGENT;

use crate::contracts::{DiscordCacheResult, DpiTuningState, HostlistInfo};
use crate::ports::DpiTuning;

/// The Discord cache subfolders we delete under `%appdata%\discord`. Matches
/// what the pre-zapret-2 `service.bat` did (kept verbatim — these names are
/// stable across Discord versions).
const DISCORD_CACHE_DIRS: [&str; 3] = ["Cache", "Code Cache", "GPUCache"];

/// Hostlists we know how to refresh. The bundle ships `list-youtube.txt`
/// upstream; if we add curated lists later (e.g. a `list-discord.txt` we
/// maintain ourselves), they slot in here as `(filename, url)` pairs and
/// `update_hostlists()` will pick them up with no other code change.
const HOSTLIST_SOURCES: &[(&str, &str)] = &[
    (
        "list-youtube.txt",
        "https://raw.githubusercontent.com/bol-van/zapret-win-bundle/master/zapret-winws/files/list-youtube.txt",
    ),
];

pub struct ZapretDpiTuning {
    install_dir: PathBuf,
    client: reqwest::Client,
}

impl ZapretDpiTuning {
    pub fn new(install_dir: PathBuf, client: reqwest::Client) -> Self {
        Self { install_dir, client }
    }

    fn hostlist_path(&self, name: &str) -> PathBuf {
        self.install_dir.join("files").join(name)
    }

    /// Build a `HostlistInfo` for a given filename, reading mtime + line count
    /// off disk. Returns a row with `age_days = None` and `line_count = 0`
    /// when the file is absent — the UI shows that as "not present".
    fn inspect_hostlist(&self, name: &str) -> HostlistInfo {
        let path = self.hostlist_path(name);
        let line_count = std::fs::read_to_string(&path)
            .map(|c| c.lines().filter(|l| !l.trim().is_empty()).count() as u32)
            .unwrap_or(0);
        let age_days = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .map(|d| (d.as_secs() / 86_400) as u32);
        HostlistInfo { name: name.to_string(), age_days, line_count }
    }
}

#[async_trait::async_trait]
impl DpiTuning for ZapretDpiTuning {
    async fn status(&self) -> DpiTuningState {
        let hostlists = HOSTLIST_SOURCES
            .iter()
            .map(|(name, _)| self.inspect_hostlist(name))
            .collect();
        DpiTuningState { hostlists }
    }

    async fn update_hostlists(&self) -> Result<usize> {
        // Make sure the destination dir exists — on a fresh install it will,
        // but defensive in case the user manually deleted `files/`.
        let dir = self.install_dir.join("files");
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("Failed to create {dir:?}"))?;

        let mut written = 0usize;
        for (name, url) in HOSTLIST_SOURCES {
            let resp = self
                .client
                .get(*url)
                .header(USER_AGENT, "zapret-ui-hostlists")
                .send()
                .await
                .with_context(|| format!("Failed to fetch hostlist {name} from {url}"))?;
            if !resp.status().is_success() {
                return Err(anyhow::anyhow!(
                    "Hostlist {name} download returned HTTP {}",
                    resp.status()
                ));
            }
            let body = resp
                .text()
                .await
                .with_context(|| format!("Failed to read hostlist {name} body"))?;
            // Validate: hostlists must have at least one non-empty line or
            // we'd silently replace a usable list with an empty file.
            let has_entries = body.lines().any(|l| !l.trim().is_empty());
            if !has_entries {
                return Err(anyhow::anyhow!(
                    "Refused to write hostlist {name}: downloaded content has no entries"
                ));
            }
            std::fs::write(self.hostlist_path(name), body.as_bytes())
                .with_context(|| format!("Failed to write hostlist {name}"))?;
            written += 1;
        }
        Ok(written)
    }

    async fn clear_discord_cache(&self) -> Result<DiscordCacheResult> {
        // First try to close any running Discord.exe so the cache folders
        // aren't held open (taskkill /F /IM works on user processes without
        // elevation).
        let was_running = {
            let kill = std::process::Command::new("taskkill")
                .args(["/IM", "Discord.exe", "/F"])
                .output();
            // Exit code 0: at least one process was killed.
            // Exit code 128: no process matched (Discord wasn't running) —
            // not an error, just info.
            matches!(kill, Ok(out) if out.status.success())
        };

        let mut cleared = 0u32;
        let base = match std::env::var_os("APPDATA").map(PathBuf::from) {
            Some(p) => p.join("discord"),
            None => {
                return Ok(DiscordCacheResult {
                    discord_was_running: was_running,
                    cleared: 0,
                });
            }
        };

        for sub in &DISCORD_CACHE_DIRS {
            let path = base.join(sub);
            if path.exists() {
                match std::fs::remove_dir_all(&path) {
                    Ok(()) => cleared += 1,
                    Err(e) => {
                        tracing::warn!("Failed to clear Discord cache dir {path:?}: {e}");
                    }
                }
            }
        }

        Ok(DiscordCacheResult { discord_was_running: was_running, cleared })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn status_reports_absent_hostlists_with_no_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let client = reqwest::Client::new();
        let tuning = ZapretDpiTuning::new(tmp.path().to_path_buf(), client);
        let state = tuning.status().await;
        assert_eq!(state.hostlists.len(), HOSTLIST_SOURCES.len());
        for info in &state.hostlists {
            assert_eq!(info.line_count, 0);
            assert!(info.age_days.is_none());
        }
    }

    #[tokio::test]
    async fn status_reads_line_count_and_age() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("files")).unwrap();
        std::fs::write(
            tmp.path().join("files").join("list-youtube.txt"),
            b"youtube.com\nyoutu.be\n\n# comment kept as non-blank\n",
        ).unwrap();
        let client = reqwest::Client::new();
        let tuning = ZapretDpiTuning::new(tmp.path().to_path_buf(), client);
        let state = tuning.status().await;
        let info = state.hostlists.iter().find(|i| i.name == "list-youtube.txt").unwrap();
        assert_eq!(info.line_count, 3);
        assert!(info.age_days.is_some(), "age computable for a present file");
    }
}
