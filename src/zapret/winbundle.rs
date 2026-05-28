//! Source of zapret2 Windows binaries: the `bol-van/zapret-win-bundle` repo.
//!
//! Replaces the old `Flowseal/zapret-discord-youtube` source (see
//! `src/zapret/github.rs` in pre-`zapret-2` history). The upstream
//! `bol-van/zapret2` releases do not ship Windows binaries, so we pull from
//! `bol-van/zapret-win-bundle` instead, which packages `winws2.exe`,
//! `WinDivert*` and the Lua libraries together under `zapret-winws/`.
//!
//! The bundle has no tagged releases — only a moving `master` branch — so we
//! resolve "latest" from `commits/master.atom` (each feed entry carries the
//! commit SHA in `<id>` and the timestamp in `<updated>`) and download the
//! whole-repo zip via `codeload.github.com`. Both endpoints are reachable when
//! `api.github.com` is DPI-blocked, which is exactly the case for the ISPs
//! this tool targets — same trick `selfupdate.rs` uses for the app's own
//! release feed.

use std::path::PathBuf;
use std::time::SystemTime;

use anyhow::{anyhow, bail, Context, Result};
use reqwest::header::USER_AGENT;
use serde::{Deserialize, Serialize};

/// Owner/repo pair we pull from. Centralized as constants so a fork (or a
/// mirror) only needs one edit if upstream ever moves.
pub const BUNDLE_OWNER: &str = "bol-van";
pub const BUNDLE_REPO: &str = "zapret-win-bundle";
/// Branch we track. The repo only has `master`.
pub const BUNDLE_BRANCH: &str = "master";

/// Subdirectory inside the bundle zip that holds the actual Windows
/// distribution. Everything else in the bundle (`arm64/`, `blockcheck/`,
/// `cygwin/`, `tools/`, `win7/`, `windivert-hide/`) is ignored by the
/// installer — we only extract files under this prefix.
pub const BUNDLE_SUBDIR: &str = "zapret-winws";

/// What the installer needs to know about the "latest" upstream snapshot.
///
/// Unlike a real GitHub release (which has tags + multiple assets), the bundle
/// is one floating branch — so `tag_name` is a synthetic, human-readable
/// `master@<sha7> (YYYY-MM-DD)` string, and `archive_url` is a single
/// codeload zip URL.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct BundleRelease {
    /// Display string, e.g. `"master@ea10010 (2026-05-27)"`. Stored verbatim
    /// in `version.txt` and surfaced to the UI as both "installed" and
    /// "latest" version. Comparison is a string equality (the
    /// `crate::zapret::updater::is_update_available` semver path falls
    /// through to string compare on non-semver inputs, which is what we want).
    pub tag_name: String,
    /// Where the user can see this commit in the browser.
    pub html_url: String,
    /// Direct codeload URL for the whole-repo zip on this branch.
    pub archive_url: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct CachedRelease {
    release: BundleRelease,
    fetched_at: SystemTime,
}

/// Resolves the latest bundle snapshot and exposes the shared `reqwest::Client`
/// used both for the atom feed and the archive download.
pub struct WinBundleSource {
    pub client: reqwest::Client,
    pub cache_path: Option<PathBuf>,
}

impl WinBundleSource {
    pub fn new(client: reqwest::Client, cache_path: Option<PathBuf>) -> Self {
        Self { client, cache_path }
    }

    fn effective_cache_path(&self) -> Option<PathBuf> {
        if let Some(p) = &self.cache_path {
            return Some(p.clone());
        }
        let base = directories::BaseDirs::new()?;
        Some(base.config_dir().join("zapret-ui").join("bundle_cache.json"))
    }

    fn read_cache(&self) -> Option<CachedRelease> {
        let path = self.effective_cache_path()?;
        if !path.exists() {
            return None;
        }
        let content = std::fs::read_to_string(&path).ok()?;
        serde_json::from_str(&content).ok()
    }

    fn write_cache(&self, release: &BundleRelease) {
        let Some(path) = self.effective_cache_path() else { return; };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let cached = CachedRelease {
            release: release.clone(),
            fetched_at: SystemTime::now(),
        };
        if let Ok(content) = serde_json::to_string(&cached) {
            let _ = std::fs::write(&path, content);
        }
    }

    /// Resolve the latest snapshot. On network failure, falls back to the
    /// last cached snapshot so an offline user still gets a working install
    /// path (matches the original `GithubClient` behavior).
    pub async fn get_latest_release(&self) -> Result<BundleRelease> {
        let atom_url = format!(
            "https://github.com/{BUNDLE_OWNER}/{BUNDLE_REPO}/commits/{BUNDLE_BRANCH}.atom"
        );
        let archive_url = format!(
            "https://codeload.github.com/{BUNDLE_OWNER}/{BUNDLE_REPO}/zip/refs/heads/{BUNDLE_BRANCH}"
        );

        tracing::info!("Fetching bundle commit feed from {atom_url}");

        let body = match self
            .client
            .get(&atom_url)
            .header(USER_AGENT, "zapret-ui-bundle")
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => match resp.text().await {
                Ok(text) => text,
                Err(e) => {
                    if let Some(cached) = self.read_cache() {
                        tracing::warn!("Failed to read atom body ({e}); returning cached snapshot");
                        return Ok(cached.release);
                    }
                    return Err(e).context("Failed to read bundle commit feed body");
                }
            },
            Ok(resp) => {
                let code = resp.status();
                if let Some(cached) = self.read_cache() {
                    tracing::warn!("Atom feed returned HTTP {code}; returning cached snapshot");
                    return Ok(cached.release);
                }
                bail!("commits/{BUNDLE_BRANCH}.atom returned HTTP {code}");
            }
            Err(e) => {
                if let Some(cached) = self.read_cache() {
                    tracing::warn!("Atom feed request failed ({e}); returning cached snapshot");
                    return Ok(cached.release);
                }
                return Err(e).context(
                    "Failed to fetch bundle commit feed (github.com unreachable)",
                );
            }
        };

        let (sha, date) = parse_first_commit(&body)
            .ok_or_else(|| anyhow!("Bundle commit feed had no parseable entries"))?;
        let short = &sha[..sha.len().min(7)];
        let release = BundleRelease {
            tag_name: format!("{BUNDLE_BRANCH}@{short} ({date})"),
            html_url: format!(
                "https://github.com/{BUNDLE_OWNER}/{BUNDLE_REPO}/commit/{sha}"
            ),
            archive_url,
        };

        tracing::info!("Resolved bundle snapshot: {}", release.tag_name);
        self.write_cache(&release);
        Ok(release)
    }
}

/// Pull the first `<entry>`'s commit SHA and date out of a GitHub
/// `commits/<branch>.atom` feed. `<id>` looks like
/// `tag:github.com,2008:Grit::Commit/<sha40>`; `<updated>` is ISO-8601 like
/// `2026-05-27T14:07:03Z` — we only keep the `YYYY-MM-DD` prefix.
fn parse_first_commit(atom: &str) -> Option<(String, String)> {
    const ID_MARKER: &str = "Grit::Commit/";
    let id_start = atom.find(ID_MARKER)? + ID_MARKER.len();
    let id_rest = &atom[id_start..];
    let sha: String = id_rest
        .chars()
        .take_while(|c| c.is_ascii_hexdigit())
        .collect();
    if sha.len() < 7 {
        return None;
    }

    let after_id = &atom[id_start + sha.len()..];
    const UPD_OPEN: &str = "<updated>";
    let upd_start = after_id.find(UPD_OPEN)? + UPD_OPEN.len();
    let upd_rest = &after_id[upd_start..];
    let upd_end = upd_rest
        .find(['T', '<'])
        .unwrap_or(upd_rest.len());
    let date = upd_rest[..upd_end].trim().to_string();
    if date.is_empty() {
        return None;
    }

    Some((sha, date))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_first_commit_entry() {
        let atom = r#"<?xml version="1.0" encoding="UTF-8"?>
<feed xmlns="http://www.w3.org/2005/Atom">
  <entry>
    <id>tag:github.com,2008:Grit::Commit/ea10010e3480e707b6d895aa208cde38c893906d</id>
    <link type="text/html" rel="alternate" href="https://github.com/bol-van/zapret-win-bundle/commit/ea10010e3480e707b6d895aa208cde38c893906d"/>
    <title>bundle</title>
    <updated>2026-05-27T14:07:03Z</updated>
  </entry>
</feed>"#;
        let (sha, date) = parse_first_commit(atom).expect("entry parsed");
        assert_eq!(sha, "ea10010e3480e707b6d895aa208cde38c893906d");
        assert_eq!(date, "2026-05-27");
    }

    #[test]
    fn returns_none_on_empty_feed() {
        assert!(parse_first_commit("<feed></feed>").is_none());
    }

    #[test]
    fn returns_none_on_short_sha() {
        let atom = "<id>Grit::Commit/abc</id><updated>2026-01-01T00:00:00Z</updated>";
        assert!(parse_first_commit(atom).is_none());
    }

    #[test]
    fn date_is_trimmed_to_day() {
        let atom = "Grit::Commit/aaaaaaa1111111111111111111111111111111<updated>2024-12-31T23:59:59Z</updated>";
        let (_, date) = parse_first_commit(atom).expect("ok");
        assert_eq!(date, "2024-12-31");
    }
}
