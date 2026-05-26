//! Self-update for the zapret-ui binary.
//!
//! The CI release workflow (`.github/workflows/release.yml`) publishes a
//! `zapret-ui.exe` plus a `zapret-ui.exe.sha256` to each `v*` GitHub Release.
//! This module resolves the latest release, downloads that exe, verifies its
//! checksum and swaps it in for the running binary using the Windows
//! rename-self trick.
//!
//! Like [`crate::zapret::github`], we deliberately avoid `api.github.com`
//! (blocked by the DPI this tool bypasses). The latest tag is read from the
//! repository's `releases.atom` feed on `github.com`, and the asset is fetched
//! from the `github.com/.../releases/download/<tag>/...` URL (which redirects to
//! `objects.githubusercontent.com`). Both are reachable when the API is not.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use reqwest::header::USER_AGENT;
use sha2::{Digest, Sha256};

use crate::ports::{DownloadProgressCb, SelfUpdater};

/// Hard ceiling on the binary we will download (200 MB). The real exe is a few
/// MB; this only fires on a corrupt/hostile server.
const MAX_DOWNLOAD_BYTES: u64 = 200 * 1024 * 1024;

/// The release asset name produced by CI.
const ASSET_NAME: &str = "zapret-ui.exe";

pub struct GithubSelfUpdater {
    client: reqwest::Client,
    owner: String,
    repo: String,
    /// The version this binary was built as (`APP_VERSION`, e.g. `"v0.1.0"`).
    current: String,
}

impl GithubSelfUpdater {
    /// Build from a `https://github.com/<owner>/<repo>` URL (as produced by
    /// `CARGO_PKG_REPOSITORY`). Falls back to the known repo if parsing fails.
    pub fn from_repo_url(client: reqwest::Client, repo_url: &str, current: impl Into<String>) -> Self {
        let (owner, repo) = parse_owner_repo(repo_url).unwrap_or_else(|| {
            ("meldxkviel".to_string(), "zapret-ui".to_string())
        });
        Self { client, owner, repo, current: current.into() }
    }

    async fn fetch_latest_tag(&self) -> Result<String> {
        let url = format!("https://github.com/{}/{}/releases.atom", self.owner, self.repo);
        tracing::info!("Fetching zapret-ui releases feed from {url}");
        let resp = self
            .client
            .get(&url)
            .header(USER_AGENT, "zapret-ui-selfupdate")
            .send()
            .await
            .context("Failed to reach the releases feed (github.com unreachable)")?;
        if !resp.status().is_success() {
            bail!("releases.atom request returned HTTP {}", resp.status());
        }
        let body = resp.text().await.context("Failed to read releases feed body")?;
        parse_first_release_tag(&body)
            .ok_or_else(|| anyhow!("No published releases found in the feed"))
    }

    async fn fetch_expected_sha256(&self, tag: &str) -> Result<String> {
        let url = format!(
            "https://github.com/{}/{}/releases/download/{}/{ASSET_NAME}.sha256",
            self.owner, self.repo, tag
        );
        let resp = self
            .client
            .get(&url)
            .header(USER_AGENT, "zapret-ui-selfupdate")
            .send()
            .await
            .context("Failed to fetch the release checksum")?;
        if !resp.status().is_success() {
            bail!("checksum request returned HTTP {}", resp.status());
        }
        let body = resp.text().await.context("Failed to read the checksum body")?;
        // The file is "<hex>  zapret-ui.exe"; take the leading hex token.
        let hex = body
            .split_whitespace()
            .next()
            .map(|s| s.to_string())
            .filter(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()))
            .ok_or_else(|| anyhow!("Malformed checksum file"))?;
        Ok(hex)
    }

    async fn download_asset(&self, tag: &str, dest: &Path, on_progress: &DownloadProgressCb) -> Result<String> {
        let url = format!(
            "https://github.com/{}/{}/releases/download/{}/{ASSET_NAME}",
            self.owner, self.repo, tag
        );
        tracing::info!("Downloading {url}");
        let response = self
            .client
            .get(&url)
            .header(USER_AGENT, "zapret-ui-selfupdate")
            .send()
            .await
            .context("Failed to send download request")?;
        if !response.status().is_success() {
            bail!("Failed to download {ASSET_NAME}: HTTP {}", response.status());
        }

        let total = response.content_length();
        let mut file = tokio::fs::File::create(dest)
            .await
            .context("Failed to create temporary download file")?;
        let mut downloaded: u64 = 0;
        let mut hasher = Sha256::new();
        let mut response = response;
        on_progress(0, total);
        while let Some(chunk) = response
            .chunk()
            .await
            .context("Error reading download stream")?
        {
            downloaded += chunk.len() as u64;
            if downloaded > MAX_DOWNLOAD_BYTES {
                bail!(
                    "Download aborted: binary exceeds the {} MB safety limit",
                    MAX_DOWNLOAD_BYTES / (1024 * 1024)
                );
            }
            hasher.update(&chunk);
            use tokio::io::AsyncWriteExt;
            file.write_all(&chunk).await.context("Failed to write download chunk")?;
            on_progress(downloaded, total);
        }
        use tokio::io::AsyncWriteExt;
        file.flush().await.context("Failed to flush download")?;
        drop(file);

        Ok(to_hex(&hasher.finalize()))
    }
}

#[async_trait]
impl SelfUpdater for GithubSelfUpdater {
    fn current_version(&self) -> String {
        self.current.clone()
    }

    async fn latest_version(&self) -> Result<String> {
        self.fetch_latest_tag().await
    }

    async fn download_and_apply(&self, on_progress: DownloadProgressCb) -> Result<()> {
        let tag = self.fetch_latest_tag().await?;

        let current_exe = std::env::current_exe().context("Failed to resolve current exe path")?;
        // Download into the same directory so the final rename is a same-volume
        // (atomic) move rather than a cross-device copy.
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let new_path = current_exe.with_file_name(format!("zapret-ui.update-{nonce}.exe"));

        // Download + checksum, with cleanup of the temp file on any failure.
        let result = async {
            let actual = self.download_asset(&tag, &new_path, &on_progress).await?;
            let expected = self.fetch_expected_sha256(&tag).await?;
            if !expected.eq_ignore_ascii_case(&actual) {
                bail!("Integrity check failed: downloaded SHA-256 {actual} != published {expected}");
            }
            tracing::info!("New zapret-ui.exe verified (SHA-256 {actual})");
            swap_in_place(&current_exe, &new_path)
        }
        .await;

        if result.is_err() {
            let _ = std::fs::remove_file(&new_path);
        }
        result
    }
}

/// Atomically replace the running exe with `new`. On Windows a running exe can
/// be *renamed* but not deleted/overwritten, so: rename current → `.old`, then
/// move the new file into the original path. Rolls back on failure. The stale
/// `.old` is cleaned up on the next launch via [`cleanup_old_binary`].
fn swap_in_place(current: &Path, new: &Path) -> Result<()> {
    let old = old_binary_path(current);
    let _ = std::fs::remove_file(&old);
    std::fs::rename(current, &old)
        .context("Failed to set aside the running exe (need write access to the app folder)")?;
    if let Err(e) = std::fs::rename(new, current) {
        // Roll back so the app still launches.
        let _ = std::fs::rename(&old, current);
        return Err(e).context("Failed to move the new exe into place");
    }
    Ok(())
}

/// The sidelined-binary path for `current` (e.g. `…\zapret-ui.exe.old`).
fn old_binary_path(current: &Path) -> PathBuf {
    let mut s = current.as_os_str().to_os_string();
    s.push(".old");
    PathBuf::from(s)
}

/// Best-effort removal of the previous binary left behind by a self-update.
/// Called once at startup (the old exe is no longer mapped by then).
pub fn cleanup_old_binary() {
    if let Ok(current) = std::env::current_exe() {
        let old = old_binary_path(&current);
        if old.exists() {
            if let Err(e) = std::fs::remove_file(&old) {
                tracing::debug!("Could not remove old binary {old:?}: {e}");
            } else {
                tracing::info!("Removed previous binary {old:?} after self-update");
            }
        }
    }
}

/// Extract `(owner, repo)` from a `https://github.com/<owner>/<repo>` URL.
fn parse_owner_repo(url: &str) -> Option<(String, String)> {
    let rest = url
        .trim_end_matches('/')
        .strip_prefix("https://github.com/")
        .or_else(|| url.trim_end_matches('/').strip_prefix("http://github.com/"))?;
    let mut parts = rest.split('/');
    let owner = parts.next()?.to_string();
    let repo = parts.next()?.trim_end_matches(".git").to_string();
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some((owner, repo))
}

/// Pull the first release tag out of a GitHub `releases.atom` feed. Entries are
/// newest-first; each links to `…/releases/tag/<TAG>`, which is the reliable
/// source of the tag (the `<title>` may be a custom release name).
fn parse_first_release_tag(atom: &str) -> Option<String> {
    const MARKER: &str = "/releases/tag/";
    let start = atom.find(MARKER)? + MARKER.len();
    let rest = &atom[start..];
    let end = rest
        .find(|c: char| c == '"' || c == '<' || c == '/' || c.is_whitespace())
        .unwrap_or(rest.len());
    let tag = rest[..end].trim();
    if tag.is_empty() {
        None
    } else {
        Some(tag.to_string())
    }
}

/// Lower-case hex encoding (avoids pulling in a hex crate; mirrors installer.rs).
fn to_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_owner_repo() {
        assert_eq!(
            parse_owner_repo("https://github.com/MelDxKviel/zapret-ui"),
            Some(("MelDxKviel".to_string(), "zapret-ui".to_string()))
        );
        assert_eq!(
            parse_owner_repo("https://github.com/foo/bar.git/"),
            Some(("foo".to_string(), "bar".to_string()))
        );
        assert_eq!(parse_owner_repo("https://example.com/foo/bar"), None);
    }

    #[test]
    fn parses_latest_tag_from_atom() {
        let atom = r#"
            <feed>
              <entry>
                <id>tag:github.com,2008:Repository/1/v0.2.0</id>
                <link rel="alternate" type="text/html" href="https://github.com/o/r/releases/tag/v0.2.0"/>
                <title>Shiny release</title>
              </entry>
              <entry>
                <link href="https://github.com/o/r/releases/tag/v0.1.0"/>
              </entry>
            </feed>
        "#;
        assert_eq!(parse_first_release_tag(atom).as_deref(), Some("v0.2.0"));
    }

    #[test]
    fn no_tag_when_feed_empty() {
        assert_eq!(parse_first_release_tag("<feed></feed>"), None);
    }

    #[test]
    fn old_binary_path_appends_suffix() {
        let p = old_binary_path(Path::new("C:/apps/zapret-ui.exe"));
        assert!(p.to_string_lossy().ends_with("zapret-ui.exe.old"));
    }
}
