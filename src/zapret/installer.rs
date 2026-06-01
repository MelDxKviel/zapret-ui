use crate::contracts::InstallStage;
use crate::ports::{Installer, ProgressCb};
use crate::zapret::github::GithubClient;
use crate::zapret::paths;
use anyhow::{Context, Result};
use async_trait::async_trait;
use reqwest::header::USER_AGENT;
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Hard ceiling on the compressed archive we will download (600 MB). The real
/// zapret distribution is a few MB; this only fires on a corrupt/hostile server.
const MAX_DOWNLOAD_BYTES: u64 = 600 * 1024 * 1024;
/// Hard ceiling on the total uncompressed size of the archive (zip-bomb guard).
const MAX_UNCOMPRESSED_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// Hard ceiling on the number of entries in the archive.
const MAX_ARCHIVE_ENTRIES: usize = 20_000;
/// A lock this old is stale even if its PID cannot be parsed.
const INSTALL_LOCK_STALE_AFTER: Duration = Duration::from_secs(2 * 60 * 60);

/// Optional pinned SHA-256 of the upstream archive. When `Some`, the download is
/// rejected unless its digest matches — turning the otherwise trust-on-transport
/// download into an integrity-checked one. Left `None` by default because the
/// upstream project publishes no checksum; `github.rs` still resolves `main` to
/// an immutable commit SHA before building the codeload URL.
const EXPECTED_ARCHIVE_SHA256: Option<&str> = None;

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
    pub github_client: GithubClient,
}

impl ZapretInstaller {
    pub fn new(install_dir: PathBuf, github_client: GithubClient) -> Self {
        Self {
            install_dir,
            github_client,
        }
    }

    async fn perform_install_or_update(
        &self,
        on_progress: &ProgressCb,
        temp_zip_path: &Path,
        temp_extract_dir: &Path,
    ) -> Result<()> {
        // 1. Resolving stage
        on_progress(InstallStage::Resolving, 0, None);

        let release = self
            .github_client
            .get_latest_release()
            .await
            .context("Failed to get latest release metadata from GitHub")?;

        // Find zip asset: name ends with .zip, not containing "source"
        let asset = release
            .assets
            .iter()
            .find(|a| {
                let name_lower = a.name.to_lowercase();
                name_lower.ends_with(".zip") && !name_lower.contains("source")
            })
            .or_else(|| {
                release
                    .assets
                    .iter()
                    .find(|a| a.name.to_lowercase().ends_with(".zip"))
            })
            .ok_or_else(|| anyhow::anyhow!("No ZIP asset found in the latest release"))?;

        // Refuse anything that isn't fetched over TLS — we run the result as
        // SYSTEM in service mode, so a plaintext (MITM-able) transport is not OK.
        if !asset.browser_download_url.starts_with("https://") {
            return Err(anyhow::anyhow!(
                "Refusing to download zapret over a non-HTTPS URL: {}",
                asset.browser_download_url
            ));
        }

        // 2. Downloading stage
        on_progress(InstallStage::Downloading, 0, Some(asset.size));
        tracing::info!("Downloading zip from: {}", asset.browser_download_url);

        let response = self
            .github_client
            .client
            .get(&asset.browser_download_url)
            .header(USER_AGENT, "zapret-ui-updater")
            .send()
            .await
            .context("Failed to send download request to GitHub")?;

        if !response.status().is_success() {
            return Err(anyhow::anyhow!(
                "Failed to download asset: HTTP {}",
                response.status()
            ));
        }

        let total_size = response.content_length().unwrap_or(asset.size);

        // Ensure parent directory exists for temp_zip_path
        if let Some(parent) = temp_zip_path.parent() {
            std::fs::create_dir_all(parent)
                .context("Failed to create parent directory for temp download file")?;
        }

        let mut file = tokio::fs::File::create(temp_zip_path)
            .await
            .context("Failed to create temporary download file")?;

        let mut downloaded: u64 = 0;
        let mut hasher = Sha256::new();
        let mut response = response; // make mutable to use chunk()

        while let Some(chunk) = response
            .chunk()
            .await
            .context("Error occurred while reading download stream")?
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
            file.write_all(&chunk)
                .await
                .context("Failed to write download chunk to disk")?;
            on_progress(InstallStage::Downloading, downloaded, Some(total_size));
        }

        use tokio::io::AsyncWriteExt;
        file.flush()
            .await
            .context("Failed to flush temporary file after download")?;
        drop(file);

        // Integrity: record the archive digest (auditable) and, when a hash is
        // pinned, refuse to install anything that doesn't match it.
        let digest = to_hex(&hasher.finalize());
        tracing::info!("Downloaded archive SHA-256 = {digest}");
        if let Some(expected) = EXPECTED_ARCHIVE_SHA256 {
            if !expected.eq_ignore_ascii_case(&digest) {
                return Err(anyhow::anyhow!(
                    "Integrity check failed: archive SHA-256 {digest} does not match the pinned {expected}"
                ));
            }
            tracing::info!("Archive SHA-256 matches the pinned value");
        }

        // 3. Extracting stage
        on_progress(InstallStage::Extracting, 0, None);
        tracing::info!("Extracting zip to temporary folder: {:?}", temp_extract_dir);

        std::fs::create_dir_all(temp_extract_dir)
            .context("Failed to create temporary extraction directory")?;

        let zip_file =
            std::fs::File::open(temp_zip_path).context("Failed to open downloaded ZIP archive")?;

        let mut archive = zip::ZipArchive::new(zip_file)
            .context("Failed to parse downloaded ZIP archive format")?;

        let total_files = archive.len();
        if total_files > MAX_ARCHIVE_ENTRIES {
            return Err(anyhow::anyhow!(
                "Archive rejected: {total_files} entries exceeds the {MAX_ARCHIVE_ENTRIES} limit"
            ));
        }
        on_progress(InstallStage::Extracting, 0, Some(total_files as u64));

        let mut total_uncompressed: u64 = 0;
        for i in 0..total_files {
            let mut file = archive
                .by_index(i)
                .context("Failed to read file from ZIP archive by index")?;

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
            on_progress(
                InstallStage::Extracting,
                (i + 1) as u64,
                Some(total_files as u64),
            );
        }

        // Handle possible single root subdirectory inside the extracted files
        let mut entries = Vec::new();
        if let Ok(rd) = std::fs::read_dir(temp_extract_dir) {
            for entry in rd.flatten() {
                entries.push(entry);
            }
        }

        if entries.len() == 1 && entries[0].file_type().map(|t| t.is_dir()).unwrap_or(false) {
            let sub_dir = entries[0].path();
            tracing::info!(
                "Promoting single root directory contents from {:?}",
                sub_dir
            );
            if let Ok(rd) = std::fs::read_dir(&sub_dir) {
                for entry in rd.flatten() {
                    let from = entry.path();
                    let to = temp_extract_dir.join(entry.file_name());
                    std::fs::rename(&from, &to)
                        .context("Failed to promote file out of root subdirectory")?;
                }
            }
            let _ = std::fs::remove_dir(&sub_dir);
        }

        // Determine version: prefer the upstream .service/version.txt, fall back to release tag.
        let version =
            std::fs::read_to_string(temp_extract_dir.join(".service").join("version.txt"))
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| release.tag_name.clone());
        std::fs::write(temp_extract_dir.join("version.txt"), &version)
            .context("Failed to write version.txt")?;

        // 4. Verifying stage
        on_progress(InstallStage::Verifying, 0, None);

        // Verify the freshly-extracted tree *before* we touch the live install,
        // so a bad/incomplete archive can never leave the user without a working
        // installation (the previous version stays in place on failure).
        if !paths::is_valid_install_dir(temp_extract_dir) {
            return Err(anyhow::anyhow!(
                "Verification failed: extracted files are missing or incomplete"
            ));
        }

        // Move to destination atomically
        if self.install_dir.exists() {
            let timestamp = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis();
            let old_dir = self
                .install_dir
                .parent()
                .unwrap_or(&self.install_dir)
                .join(format!("zapret.old.{}", timestamp));

            std::fs::rename(&self.install_dir, &old_dir)
                .context("Failed to rename existing installation to backup directory")?;

            if let Err(e) = std::fs::rename(temp_extract_dir, &self.install_dir) {
                // Try rolling back
                let _ = std::fs::rename(&old_dir, &self.install_dir);
                return Err(e)
                    .context("Failed to move extracted folder to target installation path");
            }

            // Re-verify after the swap; if the moved tree somehow isn't valid,
            // restore the backup rather than leaving a broken install behind.
            if !paths::is_valid_install_dir(&self.install_dir) {
                let _ = std::fs::remove_dir_all(&self.install_dir);
                let _ = std::fs::rename(&old_dir, &self.install_dir);
                return Err(anyhow::anyhow!(
                    "Verification failed after swap; restored the previous installation"
                ));
            }

            // Only now that the new install is verified do we drop the backup
            // (might fail if files are locked, but that shouldn't fail the install).
            if let Err(err) = std::fs::remove_dir_all(&old_dir) {
                tracing::warn!("Failed to delete old backup folder {:?}: {}", old_dir, err);
            }
        } else {
            if let Some(parent) = self.install_dir.parent() {
                std::fs::create_dir_all(parent)
                    .context("Failed to create parent directory for installation path")?;
            }
            std::fs::rename(temp_extract_dir, &self.install_dir)
                .context("Failed to move extracted folder to target installation path")?;

            if !paths::is_valid_install_dir(&self.install_dir) {
                return Err(anyhow::anyhow!(
                    "Verification failed: installed files are missing or incomplete"
                ));
            }
        }

        // Create the user list files winws.exe expects (service.bat:load_user_lists).
        crate::zapret::batparse::ensure_user_lists(&self.install_dir)
            .context("Failed to create winws user list files")?;

        // 5. Done stage
        on_progress(InstallStage::Done, 1, Some(1));
        Ok(())
    }
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
        let release = self.github_client.get_latest_release().await?;
        Ok(release.tag_name)
    }

    async fn install_or_update(&self, on_progress: ProgressCb) -> Result<()> {
        let parent_dir = self
            .install_dir
            .parent()
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

        self.perform_install_or_update(&on_progress, &temp_zip_path, &temp_extract_dir)
            .await
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
        match Self::create(path) {
            Ok(lock) => Ok(lock),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && lock_is_stale(path) => {
                let _ = std::fs::remove_file(path);
                Self::create(path).context("Failed to recreate stale install lock file")
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(anyhow::anyhow!(
                "Another install/update is already in progress (lock file present)"
            )),
            Err(e) => Err(e).context("Failed to create install lock file"),
        }
    }

    fn create(path: &Path) -> std::io::Result<Self> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?;
        writeln!(file, "pid={}", std::process::id())?;
        Ok(Self {
            path: path.to_path_buf(),
        })
    }
}

fn lock_is_stale(path: &Path) -> bool {
    if let Ok(meta) = std::fs::metadata(path) {
        if let Ok(modified) = meta.modified() {
            if SystemTime::now()
                .duration_since(modified)
                .map(|age| age > INSTALL_LOCK_STALE_AFTER)
                .unwrap_or(false)
            {
                return true;
            }
        }
    }

    let Ok(content) = std::fs::read_to_string(path) else {
        return false;
    };
    let Some(pid) = content
        .lines()
        .find_map(|line| line.strip_prefix("pid="))
        .and_then(|pid| pid.trim().parse::<u32>().ok())
    else {
        return false;
    };

    let mut sys = sysinfo::System::new();
    sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
    !sys.processes().keys().any(|p| p.as_u32() == pid)
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
    fn stale_install_lock_with_dead_pid_is_replaced() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("x.lock");
        std::fs::write(&path, "pid=999999999\n").unwrap();
        assert!(InstallLock::acquire(&path).is_ok());
    }
}
