use crate::contracts::InstallStage;
use crate::ports::{Installer, ProgressCb};
use crate::zapret::github::GithubClient;
use crate::zapret::paths;
use anyhow::{Context, Result};
use async_trait::async_trait;
use reqwest::header::USER_AGENT;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Hard ceiling on the compressed archive we will download (600 MB). The real
/// zapret distribution is a few MB; this only fires on a corrupt/hostile server.
const MAX_DOWNLOAD_BYTES: u64 = 600 * 1024 * 1024;
/// Hard ceiling on the total uncompressed size of the archive (zip-bomb guard).
const MAX_UNCOMPRESSED_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// Hard ceiling on the number of entries in the archive.
const MAX_ARCHIVE_ENTRIES: usize = 20_000;

/// Optional pinned SHA-256 of the upstream archive. When `Some`, the download is
/// rejected unless its digest matches — turning the otherwise trust-on-transport
/// download into an integrity-checked one. Left `None` by default because the
/// upstream project publishes no checksum; `github.rs` still resolves `main` to
/// an immutable commit SHA before building the codeload URL.
const EXPECTED_ARCHIVE_SHA256: Option<&str> = None;

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

        // 2. Downloading stage. Unknown release size must remain indeterminate,
        // rather than report a zero-byte download while the archive arrives.
        let asset_size = (asset.size > 0).then_some(asset.size);
        on_progress(InstallStage::Downloading, 0, asset_size);
        tracing::info!("Downloading zip from: {}", asset.browser_download_url);
        if let Some(parent) = temp_zip_path.parent() {
            std::fs::create_dir_all(parent)
                .context("Failed to create parent directory for temp download file")?;
        }
        let digest = crate::download::to_file(
            self.github_client
                .client
                .get(&asset.browser_download_url)
                .header(USER_AGENT, "zapret-ui-updater"),
            temp_zip_path,
            MAX_DOWNLOAD_BYTES,
            |downloaded, total| {
                on_progress(InstallStage::Downloading, downloaded, total.or(asset_size));
            },
        )
        .await?;

        // Integrity: record the archive digest (auditable) and, when a hash is
        // pinned, refuse to install anything that doesn't match it.
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
                if file.is_symlink() {
                    anyhow::bail!(
                        "Archive contains an unsupported symbolic link: {}",
                        file.name()
                    );
                }
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
                #[cfg(target_os = "macos")]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let executable = outpath.extension().is_some_and(|ext| ext == "sh")
                        || outpath.file_name().is_some_and(|name| name == "utunws");
                    std::fs::set_permissions(
                        &outpath,
                        std::fs::Permissions::from_mode(if executable { 0o755 } else { 0o644 }),
                    )?;
                }
            }
            on_progress(
                InstallStage::Extracting,
                (i + 1) as u64,
                Some(total_files as u64),
            );
        }

        // Handle possible single root subdirectory inside the extracted files
        let entries = std::fs::read_dir(temp_extract_dir)
            .context("Failed to enumerate extracted files")?
            .collect::<std::io::Result<Vec<_>>>()
            .context("Failed to enumerate extracted files")?;

        if entries.len() == 1 && entries[0].file_type().map(|t| t.is_dir()).unwrap_or(false) {
            let sub_dir = entries[0].path();
            tracing::info!(
                "Promoting single root directory contents from {:?}",
                sub_dir
            );
            for entry in
                std::fs::read_dir(&sub_dir).context("Failed to enumerate archive root directory")?
            {
                let entry = entry.context("Failed to enumerate archive root directory")?;
                let from = entry.path();
                let to = temp_extract_dir.join(entry.file_name());
                std::fs::rename(&from, &to)
                    .context("Failed to promote file out of root subdirectory")?;
            }
            std::fs::remove_dir(&sub_dir).context("Failed to remove promoted archive root")?;
        }

        #[cfg(target_os = "macos")]
        {
            crate::zapret::macos_bundle::promote_payload(temp_extract_dir)?;
            let arch = tokio::process::Command::new("/usr/bin/lipo")
                .args(["-verify_arch", "arm64"])
                .arg(temp_extract_dir.join("bin/utunws"))
                .output()
                .await
                .context("Checking Apple Silicon core architecture")?;
            if !arch.status.success() {
                anyhow::bail!(
                    "ZapretMac release does not contain an arm64 engine: {}",
                    String::from_utf8_lossy(&arch.stderr)
                );
            }
            let data = crate::zapret::macos_bundle::user_data_dir()?;
            crate::zapret::macos_bundle::validate_data_dir(&data)?;
            crate::zapret::macos_bundle::initialize_user_data(temp_extract_dir, &data)?;
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

        // Carry over user-maintained data before committing the replacement.
        // A failed copy or user-list creation must leave the live install and
        // its backup untouched.
        #[cfg(not(target_os = "macos"))]
        {
            if self.install_dir.is_dir() {
                preserve_user_state(&self.install_dir, temp_extract_dir)?;
            }
            crate::zapret::batparse::ensure_user_lists(temp_extract_dir)
                .context("Failed to create winws user list files")?;
        }

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

        // 5. Done stage
        on_progress(InstallStage::Done, 1, Some(1));
        Ok(())
    }
}

/// Keep user data and custom presets while accepting updated upstream defaults.
/// A legacy install has no manifest telling us whether a colliding file was
/// edited, so preserve its differing original in a content-addressed backup.
#[cfg(any(not(target_os = "macos"), test))]
fn preserve_user_state(current: &Path, staged: &Path) -> Result<()> {
    fn linked(metadata: &std::fs::Metadata) -> bool {
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            if metadata.file_attributes() & 0x400 != 0 {
                return true;
            }
        }
        metadata.file_type().is_symlink()
    }

    fn copy_regular_file(src: &Path, dst: &Path) -> Result<()> {
        let metadata = std::fs::symlink_metadata(src)
            .with_context(|| format!("Failed to inspect user data {}", src.display()))?;
        if !metadata.file_type().is_file() || linked(&metadata) {
            anyhow::bail!("User data must be a regular file: {}", src.display());
        }
        if metadata.len() > 128 * 1024 * 1024 {
            anyhow::bail!(
                "User data file exceeds the preservation limit: {}",
                src.display()
            );
        }
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)
                .context("Failed to create staged user-data directory")?;
        }
        std::fs::copy(src, dst)
            .with_context(|| format!("Failed to preserve user data {}", src.display()))?;
        Ok(())
    }

    fn hash_file(path: &Path) -> Result<Vec<u8>> {
        use sha2::{Digest, Sha256};
        use std::io::Read;
        let meta = std::fs::symlink_metadata(path)?;
        if !meta.is_file() || linked(&meta) {
            anyhow::bail!("User data must be a regular file");
        }
        let mut file = std::fs::File::open(path)?.take(128 * 1024 * 1024 + 1);
        let mut buffer = [0u8; 64 * 1024];
        let mut total = 0u64;
        let mut hash = Sha256::new();
        loop {
            let count = file.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            total += count as u64;
            if total > 128 * 1024 * 1024 {
                anyhow::bail!("User data file exceeds preservation limit");
            }
            hash.update(&buffer[..count]);
        }
        Ok(hash.finalize().to_vec())
    }

    fn preserve_collision(
        source: &Path,
        target: &Path,
        backup_root: &Path,
        relative: &Path,
    ) -> Result<()> {
        let original_hash = hash_file(source)?;
        if original_hash != hash_file(target)? {
            let digest: String = original_hash
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect();
            copy_regular_file(source, &backup_root.join(digest).join(relative))?;
        }
        Ok(())
    }

    // Also retain previous backups. Bound recursion and reject reparse points
    // before traversing: an elevated update must not follow user junctions.
    fn preserve_tree(
        source: &Path,
        target: &Path,
        backup_root: Option<&Path>,
        relative: &Path,
        visited: &mut usize,
        bytes: &mut u64,
        depth: usize,
    ) -> Result<()> {
        *visited += 1;
        if depth > 16 || *visited > 20_000 {
            anyhow::bail!("User data exceeds preservation entry/depth limit");
        }
        let meta = std::fs::symlink_metadata(source)?;
        if linked(&meta) {
            anyhow::bail!("User data must not contain links or junctions");
        }
        let name = source
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_ascii_lowercase();
        let user_owned = name.ends_with("-user.txt")
            || matches!(name.as_str(), "ipset-all.txt" | "ipset-all.txt.backup");
        if user_owned && !meta.is_file() {
            anyhow::bail!("User list must be a regular file: {}", source.display());
        }
        if meta.is_dir() {
            std::fs::create_dir_all(target)?;
            for entry in std::fs::read_dir(source)? {
                let entry = entry?;
                preserve_tree(
                    &entry.path(),
                    &target.join(entry.file_name()),
                    backup_root,
                    &relative.join(entry.file_name()),
                    visited,
                    bytes,
                    depth + 1,
                )?;
            }
        } else if meta.is_file() {
            *bytes = bytes.saturating_add(meta.len());
            if *bytes > 512 * 1024 * 1024 {
                anyhow::bail!("User data exceeds preservation byte limit");
            }
            if let Some(backup_root) = backup_root.filter(|_| target.exists() && !user_owned) {
                preserve_collision(source, target, backup_root, relative)?;
            } else {
                copy_regular_file(source, target)?;
            }
        } else {
            anyhow::bail!("User data must be a regular file or directory");
        }
        Ok(())
    }

    let mut visited = 0;
    let mut bytes = 0;
    for folder in ["user-backups", "lists"] {
        let source = current.join(folder);
        match std::fs::symlink_metadata(&source) {
            Ok(_) => preserve_tree(
                &source,
                &staged.join(folder),
                (folder == "lists")
                    .then_some(staged.join("user-backups/lists"))
                    .as_deref(),
                Path::new(""),
                &mut visited,
                &mut bytes,
                0,
            )?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).context("Failed to inspect saved core data"),
        }
    }
    for entry in std::fs::read_dir(current)? {
        let entry = entry?;
        if entry
            .path()
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("bat"))
        {
            visited += 1;
            if visited > 20_000 {
                anyhow::bail!("Too many custom strategy files");
            }
            let target = staged.join(entry.file_name());
            if target.exists() {
                preserve_collision(
                    &entry.path(),
                    &target,
                    &staged.join("user-backups/presets"),
                    Path::new(&entry.file_name()),
                )?;
            } else {
                copy_regular_file(&entry.path(), &target)?;
            }
        }
    }

    let flag = current.join("utils").join("game_filter.enabled");
    let staged_flag = staged.join("utils").join("game_filter.enabled");
    if std::fs::symlink_metadata(current.join("utils")).is_ok_and(|meta| linked(&meta)) {
        anyhow::bail!("User tuning directory must not be a link or junction");
    }
    match std::fs::symlink_metadata(&flag) {
        Ok(_) => copy_regular_file(&flag, &staged_flag)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // Absence means Disabled; do not enable a flag shipped by an archive.
            match std::fs::remove_file(&staged_flag) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e).context("Failed to preserve disabled game filter"),
            }
        }
        Err(e) => return Err(e).context("Failed to inspect game filter setting"),
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

        // Serialize install/update against itself with a kernel-owned lock.
        // A crash releases the lock, so later runs can reuse the lock file.
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

/// A cross-process lock held by an open file, rather than a PID/age heuristic.
/// The kernel releases Windows exclusive handles and Unix advisory locks on
/// crash. Unix keeps the file in place to avoid an unlink/reopen race.
struct InstallLock {
    #[cfg(windows)]
    path: PathBuf,
    file: Option<std::fs::File>,
}

impl InstallLock {
    fn acquire(path: &Path) -> Result<Self> {
        match Self::create(path) {
            Ok(lock) => Ok(lock),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::AlreadyExists | std::io::ErrorKind::WouldBlock
                ) || (cfg!(windows) && e.raw_os_error() == Some(32)) =>
            {
                Err(anyhow::anyhow!(
                    "Another install/update is already in progress (lock file present)"
                ))
            }
            Err(e) => Err(e).context("Failed to create install lock file"),
        }
    }

    fn create(path: &Path) -> std::io::Result<Self> {
        let mut options = std::fs::OpenOptions::new();
        options.write(true);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            options.create(true).share_mode(0);
        }
        #[cfg(not(windows))]
        options.create(true).truncate(false);
        let file = options.open(path)?;
        #[cfg(not(windows))]
        file.try_lock().map_err(|error| match error {
            std::fs::TryLockError::WouldBlock => {
                std::io::Error::from(std::io::ErrorKind::WouldBlock)
            }
            std::fs::TryLockError::Error(error) => error,
        })?;
        Ok(Self {
            #[cfg(windows)]
            path: path.to_path_buf(),
            file: Some(file),
        })
    }
}

impl Drop for InstallLock {
    fn drop(&mut self) {
        drop(self.file.take());
        // If another installer opened it between close and remove, its exclusive
        // Windows handle makes this deletion fail rather than unlinking its lock.
        #[cfg(windows)]
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn stale_install_lock_does_not_block_a_new_update() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("x.lock");
        std::fs::write(&path, "pid=999999999\n").unwrap();
        assert!(InstallLock::acquire(&path).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn install_lock_reuses_the_same_inode_across_acquisitions() {
        use std::os::unix::fs::MetadataExt;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("x.lock");
        let first = InstallLock::acquire(&path).unwrap();
        let inode = std::fs::metadata(&path).unwrap().ino();
        assert!(InstallLock::acquire(&path).is_err());
        drop(first);
        // Keeping the inode prevents a waiting installer from holding a lock
        // on an unlinked file while another installer creates a new one.
        assert_eq!(std::fs::metadata(&path).unwrap().ino(), inode);
        let _second = InstallLock::acquire(&path).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().ino(), inode);
        assert!(InstallLock::acquire(&path).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn an_open_install_lock_cannot_be_deleted_or_replaced() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("x.lock");
        let lock = InstallLock::acquire(&path).unwrap();
        assert!(std::fs::remove_file(&path).is_err());
        assert!(std::fs::write(&path, "pid=999999999\n").is_err());
        assert!(InstallLock::acquire(&path).is_err());
        drop(lock);
        assert!(InstallLock::acquire(&path).is_ok());
    }

    #[test]
    fn core_update_preserves_user_lists_filters_and_saved_ipset() {
        let current = tempfile::tempdir().unwrap();
        let staged = tempfile::tempdir().unwrap();
        for dir in [current.path(), staged.path()] {
            std::fs::create_dir_all(dir.join("lists")).unwrap();
            std::fs::create_dir_all(dir.join("utils")).unwrap();
        }
        for (name, content) in [
            ("list-general-user.txt", "my.example\n"),
            ("list-exclude-user.txt", "exclude.example\n"),
            ("ipset-exclude-user.txt", "192.0.2.1/32\n"),
            ("ipset-all.txt", "203.0.113.113/32\n"),
            ("ipset-all.txt.backup", "192.0.2.2/32\n"),
        ] {
            std::fs::write(current.path().join("lists").join(name), content).unwrap();
            std::fs::write(staged.path().join("lists").join(name), "archive defaults\n").unwrap();
        }
        std::fs::write(
            current.path().join("lists/list-general.txt"),
            "old upstream\n",
        )
        .unwrap();
        std::fs::write(
            staged.path().join("lists/list-general.txt"),
            "new upstream\n",
        )
        .unwrap();
        std::fs::write(current.path().join("utils/game_filter.enabled"), "tcp\n").unwrap();
        preserve_user_state(current.path(), staged.path()).unwrap();

        for name in [
            "list-general-user.txt",
            "list-exclude-user.txt",
            "ipset-exclude-user.txt",
            "ipset-all.txt",
            "ipset-all.txt.backup",
        ] {
            assert_eq!(
                std::fs::read(staged.path().join("lists").join(name)).unwrap(),
                std::fs::read(current.path().join("lists").join(name)).unwrap()
            );
        }
        assert_eq!(
            std::fs::read_to_string(staged.path().join("utils/game_filter.enabled")).unwrap(),
            "tcp\n"
        );
        assert_eq!(
            std::fs::read_to_string(staged.path().join("lists/list-general.txt")).unwrap(),
            "new upstream\n"
        );
    }

    #[test]
    fn core_update_keeps_game_filter_disabled() {
        let current = tempfile::tempdir().unwrap();
        let staged = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(staged.path().join("utils")).unwrap();
        std::fs::write(staged.path().join("utils/game_filter.enabled"), "all\n").unwrap();
        preserve_user_state(current.path(), staged.path()).unwrap();
        assert!(!staged.path().join("utils/game_filter.enabled").exists());
    }

    #[test]
    fn invalid_user_list_does_not_modify_live_installation() {
        let current = tempfile::tempdir().unwrap();
        let staged = tempfile::tempdir().unwrap();
        let invalid = current.path().join("lists/list-general-user.txt");
        std::fs::create_dir_all(&invalid).unwrap();
        assert!(preserve_user_state(current.path(), staged.path()).is_err());
        assert!(invalid.is_dir());
    }

    #[test]
    fn core_update_keeps_custom_presets_and_nonstandard_lists() {
        let current = tempfile::tempdir().unwrap();
        let staged = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(current.path().join("lists/custom")).unwrap();
        std::fs::write(current.path().join("my-provider.bat"), "custom preset").unwrap();
        std::fs::write(
            current.path().join("lists/my-hosts.txt"),
            "custom.example\n",
        )
        .unwrap();
        std::fs::write(
            current.path().join("lists/custom/ipset.txt"),
            "192.0.2.1/32\n",
        )
        .unwrap();
        preserve_user_state(current.path(), staged.path()).unwrap();
        for path in [
            "my-provider.bat",
            "lists/my-hosts.txt",
            "lists/custom/ipset.txt",
        ] {
            assert_eq!(
                std::fs::read(current.path().join(path)).unwrap(),
                std::fs::read(staged.path().join(path)).unwrap()
            );
        }
    }

    #[test]
    fn core_update_retains_differing_collisions_and_previous_backups() {
        let current = tempfile::tempdir().unwrap();
        let staged = tempfile::tempdir().unwrap();
        for dir in [current.path(), staged.path()] {
            std::fs::create_dir_all(dir.join("lists")).unwrap();
        }
        std::fs::write(current.path().join("general.bat"), "user-edited preset").unwrap();
        std::fs::write(staged.path().join("general.bat"), "updated official preset").unwrap();
        std::fs::write(
            current.path().join("lists/list-general.txt"),
            "user-edited list",
        )
        .unwrap();
        std::fs::write(
            staged.path().join("lists/list-general.txt"),
            "updated official list",
        )
        .unwrap();
        std::fs::create_dir_all(current.path().join("user-backups/previous")).unwrap();
        std::fs::write(
            current.path().join("user-backups/previous/saved.txt"),
            "older backup",
        )
        .unwrap();
        preserve_user_state(current.path(), staged.path()).unwrap();
        assert_eq!(
            std::fs::read_to_string(staged.path().join("general.bat")).unwrap(),
            "updated official preset"
        );
        assert_eq!(
            std::fs::read_to_string(staged.path().join("user-backups/previous/saved.txt")).unwrap(),
            "older backup"
        );
        for (folder, filename, content) in [
            ("presets", "general.bat", "user-edited preset"),
            ("lists", "list-general.txt", "user-edited list"),
        ] {
            let backup = std::fs::read_dir(staged.path().join("user-backups").join(folder))
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .path();
            assert_eq!(
                std::fs::read_to_string(backup.join(filename)).unwrap(),
                content
            );
        }
    }
}
