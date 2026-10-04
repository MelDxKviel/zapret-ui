//! Self-update for the zapret-ui binary.
//!
//! The CI release workflow (`.github/workflows/release.yml`) publishes a
//! `zapret-ui.exe` plus a `zapret-ui.exe.sha256` to each `v*` GitHub Release.
//! This module resolves the latest release, downloads that exe, verifies its
//! checksum and swaps it in for the running binary using the Windows
//! rename-self trick. macOS checks the ARM64 bundle ZIP and its checksum instead;
//! the GUI opens the release download so the complete signed bundle is replaced.
//!
//! Like [`crate::zapret::github`], we deliberately avoid `api.github.com`
//! (blocked by the DPI this tool bypasses). Release candidates are read from the
//! repository's `releases.atom` feed on `github.com`. The feed can also contain
//! bare tags, so an update is offered only after both assets are available and
//! the checksum is valid. The asset is fetched
//! from the `github.com/.../releases/download/<tag>/...` URL (which redirects to
//! `objects.githubusercontent.com`). Both are reachable when the API is not.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use reqwest::header::USER_AGENT;

use crate::ports::{DownloadProgressCb, SelfUpdater};
use crate::release_feed::tags as parse_release_tags;

/// Hard ceiling on the binary we will download (200 MB). The real exe is a few
/// MB; this only fires on a corrupt/hostile server.
const MAX_DOWNLOAD_BYTES: u64 = 200 * 1024 * 1024;

/// The release asset name produced by CI.
#[cfg(not(target_os = "macos"))]
const ASSET_NAME: &str = "zapret-ui.exe";
#[cfg(target_os = "macos")]
const ASSET_NAME: &str = "zapret-ui-macos-arm64.zip";

struct ReadyRelease {
    tag: String,
    sha256: String,
}

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
    pub fn from_repo_url(
        client: reqwest::Client,
        repo_url: &str,
        current: impl Into<String>,
    ) -> Self {
        let (owner, repo) = parse_owner_repo(repo_url)
            .unwrap_or_else(|| ("meldxkviel".to_string(), "zapret-ui".to_string()));
        Self {
            client,
            owner,
            repo,
            current: current.into(),
        }
    }

    async fn fetch_latest_release(&self) -> Result<Option<ReadyRelease>> {
        let feed_url = format!(
            "https://github.com/{}/{}/releases.atom",
            self.owner, self.repo
        );
        let download_base = format!(
            "https://github.com/{}/{}/releases/download",
            self.owner, self.repo
        );
        self.fetch_latest_release_from(&feed_url, &download_base)
            .await
    }

    async fn fetch_latest_release_from(
        &self,
        feed_url: &str,
        download_base: &str,
    ) -> Result<Option<ReadyRelease>> {
        tracing::info!("Fetching zapret-ui releases feed from {feed_url}");
        let resp = self
            .client
            .get(feed_url)
            .header(USER_AGENT, "zapret-ui-selfupdate")
            .send()
            .await
            .context("Failed to reach the releases feed (github.com unreachable)")?;
        if !resp.status().is_success() {
            bail!("releases.atom request returned HTTP {}", resp.status());
        }
        let body = resp
            .text()
            .await
            .context("Failed to read releases feed body")?;
        if !body.contains("<feed") || !body.contains("</feed>") {
            bail!("Invalid releases feed");
        }
        for tag in parse_release_tags(&body) {
            if !crate::zapret::updater::is_update_available(&self.current, &tag) {
                continue;
            }

            let Some(sha256) = self.fetch_release_checksum(download_base, &tag).await? else {
                tracing::debug!("Skipping zapret-ui {tag}: checksum is not ready");
                continue;
            };
            // HEAD follows GitHub's redirect to the asset storage without
            // downloading the executable during an update check.
            let resp = self
                .client
                .head(format!("{download_base}/{tag}/{ASSET_NAME}"))
                .header(USER_AGENT, "zapret-ui-selfupdate")
                .send()
                .await
                .context("Failed to check the release asset")?;
            if asset_is_missing(resp.status()) {
                tracing::debug!("Skipping zapret-ui {tag}: platform asset is not ready");
                continue;
            }
            if !resp.status().is_success() {
                bail!("asset request returned HTTP {}", resp.status());
            }
            return Ok(Some(ReadyRelease { tag, sha256 }));
        }
        Ok(None)
    }

    async fn fetch_release_checksum(
        &self,
        download_base: &str,
        tag: &str,
    ) -> Result<Option<String>> {
        let url = format!("{download_base}/{tag}/{ASSET_NAME}.sha256");
        let resp = self
            .client
            .get(&url)
            .header(USER_AGENT, "zapret-ui-selfupdate")
            .send()
            .await
            .context("Failed to fetch the release checksum")?;
        if asset_is_missing(resp.status()) {
            return Ok(None);
        }
        if !resp.status().is_success() {
            bail!("checksum request returned HTTP {}", resp.status());
        }
        let body = resp
            .text()
            .await
            .context("Failed to read the checksum body")?;
        // The file is "<hex>  zapret-ui.exe"; take the leading hex token.
        parse_sha256(&body)
            .map(Some)
            .ok_or_else(|| anyhow!("Malformed checksum file"))
    }

    async fn download_asset(
        &self,
        tag: &str,
        dest: &Path,
        on_progress: &DownloadProgressCb,
    ) -> Result<String> {
        let url = format!(
            "https://github.com/{}/{}/releases/download/{}/{ASSET_NAME}",
            self.owner, self.repo, tag
        );
        tracing::info!("Downloading {url}");
        crate::download::to_file(
            self.client
                .get(&url)
                .header(USER_AGENT, "zapret-ui-selfupdate"),
            dest,
            MAX_DOWNLOAD_BYTES,
            on_progress,
        )
        .await
    }
}

#[async_trait]
impl SelfUpdater for GithubSelfUpdater {
    fn current_version(&self) -> String {
        self.current.clone()
    }

    async fn latest_version(&self) -> Result<String> {
        Ok(self
            .fetch_latest_release()
            .await?
            .map(|release| release.tag)
            .unwrap_or_else(|| self.current.clone()))
    }

    async fn download_and_apply(&self, on_progress: DownloadProgressCb) -> Result<()> {
        if cfg!(target_os = "macos") {
            bail!("On macOS, update the complete Zapret UI.app bundle; in-place EXE updates are Windows-only");
        }
        let release = self.fetch_latest_release().await?.ok_or_else(|| {
            anyhow!(
                "No newer zapret-ui release with ready assets is available (current {})",
                self.current
            )
        })?;

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
            let actual = self
                .download_asset(&release.tag, &new_path, &on_progress)
                .await?;
            if !release.sha256.eq_ignore_ascii_case(&actual) {
                bail!(
                    "Integrity check failed: downloaded SHA-256 {actual} != published {}",
                    release.sha256
                );
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

fn asset_is_missing(status: reqwest::StatusCode) -> bool {
    matches!(
        status,
        reqwest::StatusCode::NOT_FOUND | reqwest::StatusCode::GONE
    )
}

fn parse_sha256(body: &str) -> Option<String> {
    body.split_whitespace()
        .next()
        .filter(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()))
        .map(str::to_string)
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
    fn parses_candidate_tags_from_atom() {
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
        assert_eq!(parse_release_tags(atom), ["v0.2.0", "v0.1.0"]);
    }

    #[test]
    fn no_tag_when_feed_empty() {
        assert!(parse_release_tags("<feed></feed>").is_empty());
    }

    #[test]
    fn ignores_release_links_in_notes_and_outside_entries() {
        let atom = r#"
            <feed>
              <link href="https://github.com/o/r/releases/tag/v9.0.0"/>
              <entry>
                <content>See https://github.com/o/r/releases/tag/v8.0.0</content>
                <link href='https://github.com/o/r/releases/tag/v0.2.0-rc.1'/>
              </entry>
              <entry><link href="https://github.com/o/r/releases/tag/"/></entry>
            </feed>
        "#;
        assert_eq!(parse_release_tags(atom), ["v0.2.0-rc.1"]);
    }

    const TEST_SHA256: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn feed(tags: &[&str]) -> String {
        let entries: String = tags
            .iter()
            .map(|tag| {
                format!(
                    r#"<entry><link href="https://github.com/o/r/releases/tag/{tag}"/></entry>"#
                )
            })
            .collect();
        format!("<feed>{entries}</feed>")
    }

    fn checksum() -> String {
        format!("{TEST_SHA256}  {ASSET_NAME}\n")
    }

    /// A local HTTP server with an exact request sequence, so checks cannot
    /// accidentally download an exe or query GitHub's API.
    async fn release_server(
        replies: Vec<(&'static str, u16, String)>,
    ) -> (String, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let socket = tokio::net::TcpSocket::new_v4().unwrap();
        socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let listener = socket.listen(8).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            for (expected, status, body) in replies {
                tokio::time::timeout(std::time::Duration::from_secs(5), async {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    let mut request = Vec::new();
                    let mut buf = [0u8; 1024];
                    while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                        let len = stream.read(&mut buf).await.unwrap();
                        assert!(len > 0, "request ended before headers");
                        request.extend_from_slice(&buf[..len]);
                    }
                    let request = String::from_utf8(request).unwrap();
                    assert_eq!(
                        request.lines().next().unwrap(),
                        expected.replace("zapret-ui.exe", ASSET_NAME)
                    );
                    let headers = format!(
                        "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    stream.write_all(headers.as_bytes()).await.unwrap();
                    if !expected.starts_with("HEAD ") {
                        stream.write_all(body.as_bytes()).await.unwrap();
                    }
                    stream.shutdown().await.unwrap();
                })
                .await
                .expect("release check did not make the expected request");
            }
        });
        (base, server)
    }

    async fn check_releases(
        replies: Vec<(&'static str, u16, String)>,
    ) -> Result<Option<ReadyRelease>> {
        let (base, server) = release_server(replies).await;
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .unwrap();
        let updater = GithubSelfUpdater::from_repo_url(client, "https://github.com/o/r", "v0.1.0");
        let result = updater
            .fetch_latest_release_from(&format!("{base}/feed"), &format!("{base}/downloads"))
            .await;
        server.await.unwrap();
        result
    }

    #[tokio::test]
    async fn offers_release_only_when_platform_asset_and_checksum_are_ready() {
        let release = check_releases(vec![
            ("GET /feed HTTP/1.1", 200, feed(&["v0.2.0", "v0.1.0"])),
            (
                "GET /downloads/v0.2.0/zapret-ui.exe.sha256 HTTP/1.1",
                200,
                checksum(),
            ),
            (
                "HEAD /downloads/v0.2.0/zapret-ui.exe HTTP/1.1",
                200,
                "exe".into(),
            ),
        ])
        .await
        .unwrap()
        .unwrap();
        assert_eq!(release.tag, "v0.2.0");
        assert_eq!(release.sha256, TEST_SHA256);
    }

    #[tokio::test]
    async fn skips_bare_tag_and_partial_release_for_older_ready_update() {
        let release = check_releases(vec![
            (
                "GET /feed HTTP/1.1",
                200,
                feed(&["v0.4.0", "v0.3.0", "v0.2.0", "v0.1.0"]),
            ),
            (
                "GET /downloads/v0.4.0/zapret-ui.exe.sha256 HTTP/1.1",
                404,
                String::new(),
            ),
            (
                "GET /downloads/v0.3.0/zapret-ui.exe.sha256 HTTP/1.1",
                200,
                checksum(),
            ),
            (
                "HEAD /downloads/v0.3.0/zapret-ui.exe HTTP/1.1",
                410,
                String::new(),
            ),
            (
                "GET /downloads/v0.2.0/zapret-ui.exe.sha256 HTTP/1.1",
                200,
                checksum(),
            ),
            (
                "HEAD /downloads/v0.2.0/zapret-ui.exe HTTP/1.1",
                200,
                "exe".into(),
            ),
        ])
        .await
        .unwrap()
        .unwrap();
        assert_eq!(release.tag, "v0.2.0");
    }

    #[tokio::test]
    async fn no_update_for_empty_feed_or_only_current_and_unfinished_releases() {
        for tags in [vec![], vec!["v0.1.0", "v0.0.9"]] {
            assert!(
                check_releases(vec![("GET /feed HTTP/1.1", 200, feed(&tags))])
                    .await
                    .unwrap()
                    .is_none()
            );
        }
        assert!(check_releases(vec![
            ("GET /feed HTTP/1.1", 200, feed(&["v0.2.0", "v0.1.0"])),
            (
                "GET /downloads/v0.2.0/zapret-ui.exe.sha256 HTTP/1.1",
                410,
                String::new(),
            ),
        ])
        .await
        .unwrap()
        .is_none());
    }

    #[tokio::test]
    async fn failed_checks_are_errors_instead_of_no_update() {
        for (status, body) in [(503, String::new()), (200, "<html>error</html>".into())] {
            assert!(check_releases(vec![("GET /feed HTTP/1.1", status, body)])
                .await
                .is_err());
        }
        for (status, body) in [
            (403, String::new()),
            (503, String::new()),
            (200, "invalid checksum".into()),
        ] {
            assert!(check_releases(vec![
                ("GET /feed HTTP/1.1", 200, feed(&["v0.2.0"])),
                (
                    "GET /downloads/v0.2.0/zapret-ui.exe.sha256 HTTP/1.1",
                    status,
                    body,
                ),
            ])
            .await
            .is_err());
        }
        assert!(check_releases(vec![
            ("GET /feed HTTP/1.1", 200, feed(&["v0.2.0"])),
            (
                "GET /downloads/v0.2.0/zapret-ui.exe.sha256 HTTP/1.1",
                200,
                checksum(),
            ),
            (
                "HEAD /downloads/v0.2.0/zapret-ui.exe HTTP/1.1",
                503,
                String::new(),
            ),
        ])
        .await
        .is_err());
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn macos_refuses_in_place_binary_updates_before_network_access() {
        let updater = GithubSelfUpdater::from_repo_url(
            reqwest::Client::new(),
            "https://github.com/o/r",
            "v0.1.0",
        );
        let error = updater
            .download_and_apply(Box::new(|_, _| {
                panic!("macOS must not start an executable download");
            }))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("complete Zapret UI.app bundle"));
        assert_eq!(ASSET_NAME, "zapret-ui-macos-arm64.zip");
    }

    #[test]
    fn old_binary_path_appends_suffix() {
        let p = old_binary_path(Path::new("C:/apps/zapret-ui.exe"));
        assert!(p.to_string_lossy().ends_with("zapret-ui.exe.old"));
    }
}
