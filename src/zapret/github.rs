use anyhow::{Context, Result};
use reqwest::header::USER_AGENT;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::SystemTime;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct GithubRelease {
    pub tag_name: String,
    pub html_url: String,
    pub assets: Vec<GithubAsset>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct GithubAsset {
    pub name: String,
    pub browser_download_url: String,
    pub size: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CachedRelease {
    pub release: GithubRelease,
    pub fetched_at: SystemTime,
}

pub struct GithubClient {
    pub client: reqwest::Client,
    pub cache_path: Option<PathBuf>,
}

impl GithubClient {
    pub fn new(client: reqwest::Client, cache_path: Option<PathBuf>) -> Self {
        Self { client, cache_path }
    }

    fn get_cache_path(&self) -> Option<PathBuf> {
        if self.cache_path.is_some() {
            return self.cache_path.clone();
        }
        // Fallback to default config directory
        if let Some(base) = directories::BaseDirs::new() {
            let cache_dir = base.config_dir().join("zapret-ui");
            return Some(cache_dir.join("release_cache.json"));
        }
        None
    }

    pub fn read_cache(&self) -> Option<CachedRelease> {
        let path = self.get_cache_path()?;
        if !path.exists() {
            return None;
        }
        let content = std::fs::read_to_string(&path).ok()?;
        serde_json::from_str(&content).ok()
    }

    pub fn write_cache(&self, release: &GithubRelease) {
        if let Some(path) = self.get_cache_path() {
            let cached = CachedRelease {
                release: release.clone(),
                fetched_at: SystemTime::now(),
            };
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Ok(content) = serde_json::to_string(&cached) {
                let _ = std::fs::write(&path, content);
            }
        }
    }

    async fn fetch_branch_commit(&self, branch: &str) -> Result<String> {
        let url =
            format!("https://github.com/Flowseal/zapret-discord-youtube/commits/{branch}.atom");
        let resp = self
            .client
            .get(&url)
            .header(USER_AGENT, "zapret-ui-updater")
            .send()
            .await
            .context("Failed to fetch upstream commit feed")?;
        if !resp.status().is_success() {
            anyhow::bail!("commit feed returned HTTP {}", resp.status());
        }
        let body = resp
            .text()
            .await
            .context("Failed to read upstream commit feed")?;
        parse_first_commit_sha(&body)
            .ok_or_else(|| anyhow::anyhow!("Could not resolve the latest upstream commit SHA"))
    }

    pub async fn get_latest_release(&self) -> Result<GithubRelease> {
        // NOTE: api.github.com is blocked by many RU ISPs/DPI (the exact thing zapret bypasses).
        // We deliberately avoid it. First resolve `main` to an immutable commit from the atom feed,
        // then read version.txt and download the codeload archive for that exact commit.
        const BRANCH: &str = "main";
        let commit_sha = match self.fetch_branch_commit(BRANCH).await {
            Ok(sha) => sha,
            Err(e) => {
                if let Some(cached) = self.read_cache() {
                    tracing::warn!("commit resolve failed ({e:#}), returning stale cache");
                    return Ok(cached.release);
                }
                return Err(e)
                    .context("Failed to resolve Flowseal/zapret-discord-youtube main commit");
            }
        };
        let version_url = format!(
            "https://raw.githubusercontent.com/Flowseal/zapret-discord-youtube/{commit_sha}/.service/version.txt"
        );
        let zip_url =
            format!("https://codeload.github.com/Flowseal/zapret-discord-youtube/zip/{commit_sha}");

        tracing::info!("Fetching zapret version from {}", version_url);

        let version = match self
            .client
            .get(&version_url)
            .header(USER_AGENT, "zapret-ui-updater")
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => resp
                .text()
                .await
                .map(|t| t.trim().to_string())
                .unwrap_or_else(|_| commit_sha.chars().take(12).collect()),
            Ok(resp) => {
                tracing::warn!("version.txt request returned {}", resp.status());
                // Cache fallback before giving up
                if let Some(cached) = self.read_cache() {
                    tracing::warn!("Returning stale cached version");
                    return Ok(cached.release);
                }
                commit_sha.chars().take(12).collect()
            }
            Err(e) => {
                if let Some(cached) = self.read_cache() {
                    tracing::warn!("version fetch failed ({e}), returning stale cache");
                    return Ok(cached.release);
                }
                return Err(e).context(
                    "Failed to fetch zapret version (raw.githubusercontent.com unreachable)",
                );
            }
        };

        let release = GithubRelease {
            tag_name: version.clone(),
            html_url: "https://github.com/Flowseal/zapret-discord-youtube".to_string(),
            assets: vec![GithubAsset {
                name: format!("zapret-discord-youtube-{version}.zip"),
                browser_download_url: zip_url.clone(),
                size: 0,
            }],
        };

        tracing::info!(
            "Resolved zapret release: {} -> {}",
            release.tag_name,
            zip_url
        );
        self.write_cache(&release);
        Ok(release)
    }
}

fn parse_first_commit_sha(body: &str) -> Option<String> {
    for marker in ["/commit/", "Grit::Commit/"] {
        let mut rest = body;
        while let Some(idx) = rest.find(marker) {
            let start = idx + marker.len();
            let candidate: String = rest[start..].chars().take(40).collect();
            if candidate.len() == 40 && candidate.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Some(candidate);
            }
            rest = &rest[start..];
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_commit_sha_from_atom_link() {
        let body = r#"<feed><entry><link href="https://github.com/Flowseal/zapret-discord-youtube/commit/0123456789abcdef0123456789abcdef01234567" /></entry></feed>"#;
        assert_eq!(
            parse_first_commit_sha(body).as_deref(),
            Some("0123456789abcdef0123456789abcdef01234567")
        );
    }
}
