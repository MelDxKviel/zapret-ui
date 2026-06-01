//! In-app port of the `service.bat` SETTINGS / UPDATES menu items.
//!
//! Covers the game filter, the ipset filter, "Update IPSet List" and "Update
//! Hosts File". Every operation touches files under the install dir (or, for the
//! hosts check, only *reads* the system hosts file), so none of them require
//! elevation — unlike the SCM service operations.

use anyhow::{Context, Result};
use reqwest::header::USER_AGENT;
use std::path::PathBuf;

use crate::contracts::{
    DiscordCacheResult, GameFilterMode, HostsCheck, IpsetMode, MaintenanceStatus,
};
use crate::ports::Maintenance;
use crate::zapret::batparse;

/// The Discord cache subfolders `service.bat` deletes under `%appdata%\discord`.
const DISCORD_CACHE_DIRS: [&str; 3] = ["Cache", "Code Cache", "GPUCache"];

/// The single placeholder entry `service.bat` writes for the ipset "none" mode.
const IPSET_PLACEHOLDER: &str = "203.0.113.113/32";

/// Source lists in the Flowseal repo (raw.githubusercontent.com — reachable even
/// when api.github.com is DPI-blocked, per `github.rs`).
const IPSET_URL: &str =
    "https://raw.githubusercontent.com/Flowseal/zapret-discord-youtube/refs/heads/main/.service/ipset-service.txt";
const HOSTS_URL: &str =
    "https://raw.githubusercontent.com/Flowseal/zapret-discord-youtube/refs/heads/main/.service/hosts";

pub struct ZapretMaintenance {
    install_dir: PathBuf,
    client: reqwest::Client,
}

impl ZapretMaintenance {
    pub fn new(install_dir: PathBuf, client: reqwest::Client) -> Self {
        Self {
            install_dir,
            client,
        }
    }

    fn ipset_path(&self) -> PathBuf {
        self.install_dir.join("lists").join("ipset-all.txt")
    }
    fn ipset_backup_path(&self) -> PathBuf {
        self.install_dir.join("lists").join("ipset-all.txt.backup")
    }
    fn game_flag_path(&self) -> PathBuf {
        self.install_dir.join("utils").join("game_filter.enabled")
    }

    /// Classify `ipset-all.txt` the way `service.bat:ipset_switch_status` does.
    fn read_ipset_mode(&self) -> IpsetMode {
        let content = match std::fs::read_to_string(self.ipset_path()) {
            Ok(c) => c,
            Err(_) => return IpsetMode::Unknown,
        };
        let non_empty: Vec<&str> = content
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect();
        if non_empty.is_empty() {
            IpsetMode::Any
        } else if non_empty.contains(&IPSET_PLACEHOLDER) {
            IpsetMode::None
        } else {
            IpsetMode::Loaded
        }
    }
}

#[async_trait::async_trait]
impl Maintenance for ZapretMaintenance {
    async fn status(&self) -> MaintenanceStatus {
        let game_filter = batparse::read_game_filter(&self.install_dir);
        let ipset_mode = self.read_ipset_mode();
        let ipset_lines = std::fs::read_to_string(self.ipset_path())
            .map(|c| c.lines().filter(|l| !l.trim().is_empty()).count() as u32)
            .unwrap_or(0);
        let ipset_age_days = std::fs::metadata(self.ipset_path())
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .map(|d| (d.as_secs() / 86_400) as u32);
        MaintenanceStatus {
            game_filter,
            ipset_mode,
            ipset_lines,
            ipset_age_days,
        }
    }

    async fn set_game_filter(&self, mode: GameFilterMode) -> Result<()> {
        let flag = self.game_flag_path();
        if let Some(parent) = flag.parent() {
            std::fs::create_dir_all(parent).context("creating utils dir")?;
        }
        match mode {
            // No flag file == disabled (matches service.bat).
            GameFilterMode::Disabled => {
                let _ = std::fs::remove_file(&flag);
            }
            other => {
                std::fs::write(&flag, format!("{}\n", other.slug()))
                    .context("writing game_filter.enabled")?;
            }
        }
        tracing::info!("Game filter set to {:?}", mode);
        Ok(())
    }

    async fn set_ipset_mode(&self, mode: IpsetMode) -> Result<()> {
        let path = self.ipset_path();
        let backup = self.ipset_backup_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).context("creating lists dir")?;
        }
        let current = self.read_ipset_mode();

        // Preserve a real list before overwriting it, so "Loaded" can be restored.
        if current == IpsetMode::Loaded && mode != IpsetMode::Loaded {
            std::fs::copy(&path, &backup).context("backing up ipset list")?;
        }

        match mode {
            IpsetMode::Any => {
                std::fs::write(&path, "").context("writing empty ipset list")?;
            }
            IpsetMode::None => {
                std::fs::write(&path, format!("{IPSET_PLACEHOLDER}\n"))
                    .context("writing ipset placeholder")?;
            }
            IpsetMode::Loaded => {
                if backup.exists() {
                    std::fs::copy(&backup, &path).context("restoring ipset list")?;
                } else if current != IpsetMode::Loaded {
                    anyhow::bail!("No saved IP list to restore — run \"Update IPSet list\" first");
                }
            }
            IpsetMode::Unknown => {
                anyhow::bail!("Cannot switch the IPSet filter: zapret is not installed");
            }
        }
        tracing::info!("IPSet filter set to {:?}", mode);
        Ok(())
    }

    async fn update_ipset_list(&self) -> Result<usize> {
        let path = self.ipset_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).context("creating lists dir")?;
        }
        tracing::info!("Updating ipset-all.txt from {}", IPSET_URL);
        let resp = self
            .client
            .get(IPSET_URL)
            .header(USER_AGENT, "zapret-ui")
            .send()
            .await
            .context("downloading ipset list (raw.githubusercontent.com unreachable)")?;
        if !resp.status().is_success() {
            anyhow::bail!("ipset download returned HTTP {}", resp.status());
        }
        let body = resp.text().await.context("reading ipset response")?;
        let count = body.lines().filter(|l| !l.trim().is_empty()).count();
        if count == 0 {
            anyhow::bail!("the downloaded ipset list was empty");
        }
        std::fs::write(&path, &body).context("writing ipset-all.txt")?;
        // A fresh download replaces the placeholder/empty file with the real
        // list; drop the stale backup so "Loaded" reflects this list.
        let _ = std::fs::remove_file(self.ipset_backup_path());
        tracing::info!("ipset-all.txt updated — {count} entries");
        Ok(count)
    }

    async fn update_hosts_file(&self) -> Result<HostsCheck> {
        let system_root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".to_string());
        let hosts_dir = PathBuf::from(system_root)
            .join("System32")
            .join("drivers")
            .join("etc");
        let hosts_path = hosts_dir.join("hosts");

        tracing::info!("Checking hosts file against {}", HOSTS_URL);
        let resp = self
            .client
            .get(HOSTS_URL)
            .header(USER_AGENT, "zapret-ui")
            .send()
            .await
            .context("downloading hosts file (raw.githubusercontent.com unreachable)")?;
        if !resp.status().is_success() {
            anyhow::bail!("hosts download returned HTTP {}", resp.status());
        }
        let repo = resp.text().await.context("reading hosts response")?;
        let repo_lines: Vec<&str> = repo
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect();
        let (first, last) = match (repo_lines.first(), repo_lines.last()) {
            (Some(f), Some(l)) => (*f, *l),
            _ => anyhow::bail!("the downloaded hosts file was empty"),
        };

        let system = std::fs::read_to_string(&hosts_path).unwrap_or_default();
        let up_to_date = system.contains(first) && system.contains(last);
        if up_to_date {
            tracing::info!("Hosts file is up to date");
        } else {
            tracing::warn!("Hosts file is out of date — review window available");
        }

        // Writing the system hosts file needs admin, so we hand the content back
        // to the UI for an in-app review/copy window instead of editing it here.
        Ok(HostsCheck {
            up_to_date,
            content: repo,
            hosts_path: hosts_path.display().to_string(),
            hosts_dir: hosts_dir.display().to_string(),
        })
    }

    async fn clear_discord_cache(&self) -> Result<DiscordCacheResult> {
        // Discord locks its cache while running, so close it first (matches
        // `service.bat`: `taskkill /IM Discord.exe /F`). taskkill returns a
        // non-zero exit code when no matching process is found — that's not an
        // error here, it just means Discord wasn't running.
        let discord_was_running = kill_discord();

        let cache_dir = PathBuf::from(
            std::env::var("APPDATA")
                .context("APPDATA is not set — cannot locate the Discord cache")?,
        )
        .join("discord");

        let mut cleared = 0u32;
        let mut failures = Vec::new();
        for sub in DISCORD_CACHE_DIRS {
            let dir = cache_dir.join(sub);
            if !dir.exists() {
                continue;
            }
            match std::fs::remove_dir_all(&dir) {
                Ok(_) => {
                    cleared += 1;
                    tracing::info!("Cleared Discord cache folder {}", dir.display());
                }
                Err(e) => {
                    // A single locked file shouldn't abort the whole operation;
                    // try the remaining cache dirs, then surface all failures.
                    tracing::warn!("Failed to delete {}: {}", dir.display(), e);
                    failures.push(format!("{}: {}", dir.display(), e));
                }
            }
        }
        tracing::info!(
            "Discord cache clear done — {} folder(s) removed (was running: {})",
            cleared,
            discord_was_running
        );
        if !failures.is_empty() {
            anyhow::bail!(
                "Discord cache partially cleared ({} folder(s) removed); failed to delete: {}",
                cleared,
                failures.join("; ")
            );
        }
        Ok(DiscordCacheResult {
            discord_was_running,
            cleared,
        })
    }
}

/// Force-close every `Discord.exe`, returning whether any process was running.
/// Uses `taskkill` (like `service.bat`) with the no-window flag so no console
/// flashes; the exit code distinguishes "killed" (0) from "not found" (128).
fn kill_discord() -> bool {
    use std::process::Command;
    let mut cmd = Command::new("taskkill");
    cmd.args(["/IM", "Discord.exe", "/F"]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }
    matches!(cmd.output(), Ok(out) if out.status.success())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, ZapretMaintenance) {
        let tmp = tempfile::tempdir().unwrap();
        let m = ZapretMaintenance::new(tmp.path().to_path_buf(), reqwest::Client::new());
        (tmp, m)
    }

    #[tokio::test]
    async fn ipset_mode_round_trips_through_filesystem() {
        let (_tmp, m) = fixture();
        // Nothing written yet → file absent → Unknown.
        assert_eq!(m.status().await.ipset_mode, IpsetMode::Unknown);

        // Switching to "any" writes an empty list.
        m.set_ipset_mode(IpsetMode::Any).await.unwrap();
        assert_eq!(m.status().await.ipset_mode, IpsetMode::Any);

        // "none" writes the single placeholder entry.
        m.set_ipset_mode(IpsetMode::None).await.unwrap();
        assert_eq!(m.status().await.ipset_mode, IpsetMode::None);

        // Without a saved list, restoring "loaded" must fail clearly.
        assert!(m.set_ipset_mode(IpsetMode::Loaded).await.is_err());
    }

    #[tokio::test]
    async fn game_filter_flag_round_trips() {
        let (_tmp, m) = fixture();
        assert_eq!(m.status().await.game_filter, GameFilterMode::Disabled);

        m.set_game_filter(GameFilterMode::Tcp).await.unwrap();
        assert_eq!(m.status().await.game_filter, GameFilterMode::Tcp);

        // Disabling removes the flag file (back to the default).
        m.set_game_filter(GameFilterMode::Disabled).await.unwrap();
        assert_eq!(m.status().await.game_filter, GameFilterMode::Disabled);
    }
}
