use std::path::{Path, PathBuf};
use std::time::SystemTime;
use anyhow::{Result, Context};
use async_trait::async_trait;
use reqwest::header::USER_AGENT;
use sha2::{Digest, Sha256};
use crate::contracts::InstallStage;
use crate::ports::{Installer, ProgressCb};
use crate::zapret::winbundle::{WinBundleSource, BUNDLE_SUBDIR};
use crate::zapret::paths;

/// Hard ceiling on the compressed archive we will download (600 MB). The real
/// bundle is a handful of MB; this only fires on a corrupt/hostile server.
const MAX_DOWNLOAD_BYTES: u64 = 600 * 1024 * 1024;
/// Hard ceiling on the total uncompressed size of the archive (zip-bomb guard).
const MAX_UNCOMPRESSED_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// Hard ceiling on the number of entries in the archive.
const MAX_ARCHIVE_ENTRIES: usize = 20_000;

/// Lower-case hex encoding of a byte slice (avoids pulling in a hex crate).
fn to_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

pub struct ZapretInstaller {
    pub install_dir: PathBuf,
    pub bundle: WinBundleSource,
}

impl ZapretInstaller {
    pub fn new(install_dir: PathBuf, bundle: WinBundleSource) -> Self {
        Self { install_dir, bundle }
    }

    async fn perform_install_or_update(
        &self,
        on_progress: &ProgressCb,
        temp_zip_path: &Path,
        temp_extract_dir: &Path,
    ) -> Result<()> {
        // 1. Resolving — figure out which commit of the bundle we're pulling.
        on_progress(InstallStage::Resolving, 0, None);

        let release = self.bundle.get_latest_release().await
            .context("Failed to resolve latest zapret-win-bundle snapshot")?;

        // We always download the same codeload zip — it's the branch tarball,
        // not a per-tag asset. Require TLS: this drops to SYSTEM in service
        // mode, so a MITM-able transport is unacceptable.
        if !release.archive_url.starts_with("https://") {
            return Err(anyhow::anyhow!(
                "Refusing to download bundle over a non-HTTPS URL: {}",
                release.archive_url
            ));
        }

        // 2. Downloading
        on_progress(InstallStage::Downloading, 0, None);
        tracing::info!("Downloading bundle zip from: {}", release.archive_url);

        let response = self.bundle.client.get(&release.archive_url)
            .header(USER_AGENT, "zapret-ui-bundle")
            .send()
            .await
            .context("Failed to send bundle download request")?;

        if !response.status().is_success() {
            return Err(anyhow::anyhow!("Bundle download failed: HTTP {}", response.status()));
        }

        let total_size = response.content_length().unwrap_or(0);

        if let Some(parent) = temp_zip_path.parent() {
            std::fs::create_dir_all(parent)
                .context("Failed to create parent directory for temp download file")?;
        }

        let mut file = tokio::fs::File::create(temp_zip_path).await
            .context("Failed to create temporary download file")?;

        let mut downloaded: u64 = 0;
        let mut hasher = Sha256::new();
        let mut response = response; // make mutable to use chunk()

        while let Some(chunk) = response.chunk().await
            .context("Error occurred while reading bundle download stream")?
        {
            downloaded += chunk.len() as u64;
            if downloaded > MAX_DOWNLOAD_BYTES {
                return Err(anyhow::anyhow!(
                    "Download aborted: archive exceeds the {} MB safety limit",
                    MAX_DOWNLOAD_BYTES / (1024 * 1024)
                ));
            }
            hasher.update(&chunk);
            use tokio::io::AsyncWriteExt;
            file.write_all(&chunk).await
                .context("Failed to write download chunk to disk")?;
            // codeload doesn't always send Content-Length; pass Some(total) only
            // when we actually got one, otherwise let the UI show an
            // indeterminate spinner.
            on_progress(
                InstallStage::Downloading,
                downloaded,
                if total_size > 0 { Some(total_size) } else { None },
            );
        }

        use tokio::io::AsyncWriteExt;
        file.flush().await
            .context("Failed to flush temporary file after download")?;
        drop(file);

        // Audit trail. With a moving `master` branch there's nothing to pin
        // against, but logging the digest of what we actually fetched lets a
        // forensic check confirm two machines pulled identical bytes.
        let digest = to_hex(&hasher.finalize());
        tracing::info!("Downloaded bundle SHA-256 = {digest}");

        // 3. Extracting — full unpack into temp, then promote only what we need.
        on_progress(InstallStage::Extracting, 0, None);
        tracing::info!("Extracting bundle zip to {:?}", temp_extract_dir);

        std::fs::create_dir_all(temp_extract_dir)
            .context("Failed to create temporary extraction directory")?;

        let zip_file = std::fs::File::open(temp_zip_path)
            .context("Failed to open downloaded bundle archive")?;

        let mut archive = zip::ZipArchive::new(zip_file)
            .context("Failed to parse downloaded bundle archive format")?;

        let total_files = archive.len();
        if total_files > MAX_ARCHIVE_ENTRIES {
            return Err(anyhow::anyhow!(
                "Archive rejected: {total_files} entries exceeds the {MAX_ARCHIVE_ENTRIES} limit"
            ));
        }
        on_progress(InstallStage::Extracting, 0, Some(total_files as u64));

        let mut total_uncompressed: u64 = 0;
        for i in 0..total_files {
            let mut file = archive.by_index(i)
                .context("Failed to read file from bundle archive by index")?;

            total_uncompressed += file.size();
            if total_uncompressed > MAX_UNCOMPRESSED_BYTES {
                return Err(anyhow::anyhow!(
                    "Archive rejected: uncompressed size exceeds the {} GB limit (possible zip bomb)",
                    MAX_UNCOMPRESSED_BYTES / (1024 * 1024 * 1024)
                ));
            }

            let outpath = match file.enclosed_name() {
                Some(path) => temp_extract_dir.join(path),
                None => continue,
            };

            if file.name().ends_with('/') {
                std::fs::create_dir_all(&outpath)
                    .context("Failed to create subdirectory inside extraction folder")?;
            } else {
                if let Some(p) = outpath.parent() {
                    if !p.exists() {
                        std::fs::create_dir_all(p)
                            .context("Failed to create parent directory for extracted file")?;
                    }
                }
                let mut outfile = std::fs::File::create(&outpath)
                    .context("Failed to create file inside extraction folder")?;
                std::io::copy(&mut file, &mut outfile)
                    .context("Failed to write extracted file contents to disk")?;
            }
            on_progress(InstallStage::Extracting, (i + 1) as u64, Some(total_files as u64));
        }

        // Promote the Windows subdir to the extract root. The codeload zip is
        // always `<repo>-<branch>/<everything>/` and the actual Windows
        // distribution lives at `<root>/zapret-winws/`. We dig down, move its
        // contents into a fresh `promoted/` dir, and continue from there —
        // ignoring `arm64/`, `blockcheck/`, `cygwin/`, `tools/`, etc., which
        // we don't ship.
        let zapret_winws_dir = locate_bundle_subdir(temp_extract_dir, BUNDLE_SUBDIR)
            .ok_or_else(|| anyhow::anyhow!(
                "Downloaded bundle does not contain a `{BUNDLE_SUBDIR}/` subtree"
            ))?;
        let promoted = temp_extract_dir.join(".promoted");
        std::fs::create_dir_all(&promoted)
            .context("Failed to create promoted-staging directory")?;
        promote_subtree(&zapret_winws_dir, &promoted)
            .context("Failed to promote zapret-winws subtree")?;

        // Record the resolved snapshot version so `installed_version` has
        // something to return. The bundle has no `version.txt` upstream, so we
        // write our own.
        std::fs::write(promoted.join("version.txt"), &release.tag_name)
            .context("Failed to write version.txt")?;

        // 4. Verifying — make sure the promoted tree actually contains what we
        // expect *before* swapping it over the live install, so a bad/partial
        // archive can never leave the user with a broken installation.
        on_progress(InstallStage::Verifying, 0, None);
        if !paths::is_valid_install_dir(&promoted) {
            return Err(anyhow::anyhow!(
                "Verification failed: promoted bundle is missing winws2.exe or lua/"
            ));
        }

        // Atomic swap. `tempfile::TempDir` will clean the extraction scaffold
        // (everything we didn't promote) when the caller's guard drops.
        if self.install_dir.exists() {
            let timestamp = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis();
            let old_dir = self.install_dir.parent()
                .unwrap_or(&self.install_dir)
                .join(format!("zapret.old.{}", timestamp));

            std::fs::rename(&self.install_dir, &old_dir)
                .context("Failed to rename existing installation to backup directory")?;

            if let Err(e) = std::fs::rename(&promoted, &self.install_dir) {
                let _ = std::fs::rename(&old_dir, &self.install_dir);
                return Err(e).context("Failed to move promoted bundle to target installation path");
            }

            if !paths::is_valid_install_dir(&self.install_dir) {
                let _ = std::fs::remove_dir_all(&self.install_dir);
                let _ = std::fs::rename(&old_dir, &self.install_dir);
                return Err(anyhow::anyhow!(
                    "Verification failed after swap; restored the previous installation"
                ));
            }

            if let Err(err) = std::fs::remove_dir_all(&old_dir) {
                tracing::warn!("Failed to delete old backup folder {:?}: {}", old_dir, err);
            }
        } else {
            if let Some(parent) = self.install_dir.parent() {
                std::fs::create_dir_all(parent)
                    .context("Failed to create parent directory for installation path")?;
            }
            std::fs::rename(&promoted, &self.install_dir)
                .context("Failed to move promoted bundle to target installation path")?;

            if !paths::is_valid_install_dir(&self.install_dir) {
                return Err(anyhow::anyhow!(
                    "Verification failed: installed files are missing winws2.exe or lua/"
                ));
            }
        }

        // Sanity-check that every file a built-in strategy will reference is
        // present in the freshly-installed tree. Anything missing means the
        // bundle layout drifted from what we expect; we log it (so support
        // tickets carry the smoking gun) but don't fail the install — the
        // user is free to pick a different strategy whose inputs are intact.
        let mut missing = std::collections::BTreeSet::new();
        for def in crate::zapret::strategies::builtin_strategies() {
            for rel in def.required_files {
                if !self.install_dir.join(rel).exists() {
                    missing.insert((*rel).to_string());
                }
            }
        }
        if !missing.is_empty() {
            tracing::warn!(
                "Install finished but {} strategy input file(s) are missing: {:?}",
                missing.len(),
                missing
            );
        }

        // 5. Done
        on_progress(InstallStage::Done, 1, Some(1));
        Ok(())
    }
}

/// Walk `root` looking for a directory whose final component matches `name`.
/// The codeload zip nests the actual content under `<repo>-<branch>/`, so we
/// scan down a few levels rather than hardcoding the prefix. Returns the
/// first match (depth-first), or `None` if nothing matched.
fn locate_bundle_subdir(root: &Path, name: &str) -> Option<PathBuf> {
    // Direct hit (won't happen for codeload zips, but handles hand-prepared
    // archives in tests).
    let direct = root.join(name);
    if direct.is_dir() {
        return Some(direct);
    }
    let entries = std::fs::read_dir(root).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            // One level down is what we expect from codeload.
            let candidate = path.join(name);
            if candidate.is_dir() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Move every entry under `src` to `dst`. Uses rename (same-volume = atomic);
/// falls back to copy + delete if rename fails across mount points (shouldn't
/// happen since both paths live in the same tempdir, but safe to handle).
fn promote_subtree(src: &Path, dst: &Path) -> Result<()> {
    let entries = std::fs::read_dir(src)
        .with_context(|| format!("Failed to read promote source {src:?}"))?;
    for entry in entries.flatten() {
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if std::fs::rename(&from, &to).is_err() {
            // Cross-device fallback. Recursive copy then delete the source.
            copy_recursive(&from, &to)
                .with_context(|| format!("Failed to copy {from:?} → {to:?}"))?;
            if from.is_dir() {
                let _ = std::fs::remove_dir_all(&from);
            } else {
                let _ = std::fs::remove_file(&from);
            }
        }
    }
    Ok(())
}

fn copy_recursive(src: &Path, dst: &Path) -> Result<()> {
    if src.is_dir() {
        std::fs::create_dir_all(dst)?;
        for entry in std::fs::read_dir(src)?.flatten() {
            copy_recursive(&entry.path(), &dst.join(entry.file_name()))?;
        }
    } else {
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(src, dst)?;
    }
    Ok(())
}

#[async_trait]
impl Installer for ZapretInstaller {
    async fn is_installed(&self) -> bool {
        paths::is_valid_install_dir(&self.install_dir)
    }

    async fn installed_version(&self) -> Option<String> {
        if !self.is_installed().await {
            return None;
        }
        std::fs::read_to_string(self.install_dir.join("version.txt"))
            .ok()
            .map(|s| s.trim().to_string())
    }

    async fn latest_version(&self) -> Result<String> {
        let release = self.bundle.get_latest_release().await?;
        Ok(release.tag_name)
    }

    async fn install_or_update(&self, on_progress: ProgressCb) -> Result<()> {
        let parent_dir = self.install_dir.parent()
            .unwrap_or(&self.install_dir)
            .to_path_buf();
        std::fs::create_dir_all(&parent_dir)
            .context("Failed to create parent directory for installation")?;

        // Serialize install/update against itself: a single lock file in the
        // parent dir, removed when this guard drops. Prevents two concurrent
        // runs from clobbering each other's temp data or the install swap.
        let _lock = InstallLock::acquire(&parent_dir.join("zapret-ui.install.lock"))?;

        // Unique per-run working directory (auto-removed on drop) instead of
        // fixed temp names another process could race on.
        let work = tempfile::Builder::new()
            .prefix("zapret-work-")
            .tempdir_in(&parent_dir)
            .context("Failed to create temporary working directory")?;
        let temp_zip_path = work.path().join("download.zip");
        let temp_extract_dir = work.path().join("extract");

        self.perform_install_or_update(&on_progress, &temp_zip_path, &temp_extract_dir).await
        // `work` (and any leftovers) and `_lock` are cleaned up on drop.
    }
}

/// A best-effort cross-process lock implemented as an exclusively-created file.
/// Removed on drop so a crash leaves at most a stale empty file (which the next
/// run will recreate-or-fail on — acceptable for a desktop installer).
struct InstallLock {
    path: PathBuf,
}

impl InstallLock {
    fn acquire(path: &Path) -> Result<Self> {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
        {
            Ok(_) => Ok(Self { path: path.to_path_buf() }),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                Err(anyhow::anyhow!(
                    "Another install/update is already in progress (lock file present)"
                ))
            }
            Err(e) => Err(e).context("Failed to create install lock file"),
        }
    }
}

impl Drop for InstallLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_hex_encodes_lowercase() {
        assert_eq!(to_hex(&[0x00, 0x0f, 0xab, 0xff]), "000fabff");
    }

    #[test]
    fn install_lock_is_exclusive_and_released_on_drop() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("x.lock");
        let lock = InstallLock::acquire(&path).expect("first acquire succeeds");
        // A second acquire while held must fail.
        assert!(InstallLock::acquire(&path).is_err());
        drop(lock);
        // Once released, it can be acquired again.
        assert!(InstallLock::acquire(&path).is_ok());
    }

    #[test]
    fn locate_bundle_subdir_finds_nested_match() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("zapret-win-bundle-master").join("zapret-winws")).unwrap();
        std::fs::create_dir_all(root.join("zapret-win-bundle-master").join("arm64")).unwrap();
        let found = locate_bundle_subdir(root, "zapret-winws").expect("nested found");
        assert!(found.ends_with("zapret-winws"));
    }

    #[test]
    fn locate_bundle_subdir_finds_direct_match() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("zapret-winws")).unwrap();
        assert!(locate_bundle_subdir(tmp.path(), "zapret-winws").is_some());
    }

    #[test]
    fn locate_bundle_subdir_returns_none_on_miss() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("other-bundle-master").join("nope")).unwrap();
        assert!(locate_bundle_subdir(tmp.path(), "zapret-winws").is_none());
    }

    #[test]
    fn promote_subtree_moves_contents() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");
        std::fs::create_dir_all(src.join("sub")).unwrap();
        std::fs::create_dir_all(&dst).unwrap();
        std::fs::write(src.join("a.txt"), b"a").unwrap();
        std::fs::write(src.join("sub").join("b.txt"), b"b").unwrap();
        promote_subtree(&src, &dst).unwrap();
        assert!(dst.join("a.txt").exists());
        assert!(dst.join("sub").join("b.txt").exists());
        // Source emptied (only the now-empty src dir remains).
        let remaining: Vec<_> = std::fs::read_dir(&src).unwrap().collect();
        assert!(remaining.is_empty());
    }
}
