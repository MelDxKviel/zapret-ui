use std::path::{Path, PathBuf};
use std::time::SystemTime;
use anyhow::{Result, Context};
use async_trait::async_trait;
use reqwest::header::USER_AGENT;
use crate::contracts::InstallStage;
use crate::ports::{Installer, ProgressCb};
use crate::zapret::github::GithubClient;
use crate::zapret::paths;

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
        
        let release = self.github_client.get_latest_release().await
            .context("Failed to get latest release metadata from GitHub")?;
        
        // Find zip asset: name ends with .zip, not containing "source"
        let asset = release.assets.iter().find(|a| {
            let name_lower = a.name.to_lowercase();
            name_lower.ends_with(".zip") && !name_lower.contains("source")
        }).or_else(|| {
            release.assets.iter().find(|a| a.name.to_lowercase().ends_with(".zip"))
        }).ok_or_else(|| anyhow::anyhow!("No ZIP asset found in the latest release"))?;

        // 2. Downloading stage
        on_progress(InstallStage::Downloading, 0, Some(asset.size));
        tracing::info!("Downloading zip from: {}", asset.browser_download_url);

        let response = self.github_client.client.get(&asset.browser_download_url)
            .header(USER_AGENT, "zapret-ui-updater")
            .send()
            .await
            .context("Failed to send download request to GitHub")?;

        if !response.status().is_success() {
            return Err(anyhow::anyhow!("Failed to download asset: HTTP {}", response.status()));
        }

        let total_size = response.content_length().unwrap_or(asset.size);
        
        // Ensure parent directory exists for temp_zip_path
        if let Some(parent) = temp_zip_path.parent() {
            std::fs::create_dir_all(parent)
                .context("Failed to create parent directory for temp download file")?;
        }

        let mut file = tokio::fs::File::create(temp_zip_path).await
            .context("Failed to create temporary download file")?;
        
        let mut downloaded: u64 = 0;
        let mut response = response; // make mutable to use chunk()

        while let Some(chunk) = response.chunk().await
            .context("Error occurred while reading download stream")? 
        {
            use tokio::io::AsyncWriteExt;
            file.write_all(&chunk).await
                .context("Failed to write download chunk to disk")?;
            downloaded += chunk.len() as u64;
            on_progress(InstallStage::Downloading, downloaded, Some(total_size));
        }
        
        use tokio::io::AsyncWriteExt;
        file.flush().await
            .context("Failed to flush temporary file after download")?;
        drop(file);

        // 3. Extracting stage
        on_progress(InstallStage::Extracting, 0, None);
        tracing::info!("Extracting zip to temporary folder: {:?}", temp_extract_dir);

        std::fs::create_dir_all(temp_extract_dir)
            .context("Failed to create temporary extraction directory")?;

        let zip_file = std::fs::File::open(temp_zip_path)
            .context("Failed to open downloaded ZIP archive")?;
        
        let mut archive = zip::ZipArchive::new(zip_file)
            .context("Failed to parse downloaded ZIP archive format")?;

        let total_files = archive.len();
        on_progress(InstallStage::Extracting, 0, Some(total_files as u64));

        for i in 0..total_files {
            let mut file = archive.by_index(i)
                .context("Failed to read file from ZIP archive by index")?;
            
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

        // Handle possible single root subdirectory inside the extracted files
        let mut entries = Vec::new();
        if let Ok(rd) = std::fs::read_dir(temp_extract_dir) {
            for entry in rd.flatten() {
                entries.push(entry);
            }
        }
        
        if entries.len() == 1 && entries[0].file_type().map(|t| t.is_dir()).unwrap_or(false) {
            let sub_dir = entries[0].path();
            tracing::info!("Promoting single root directory contents from {:?}", sub_dir);
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
        let version = std::fs::read_to_string(temp_extract_dir.join(".service").join("version.txt"))
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| release.tag_name.clone());
        std::fs::write(temp_extract_dir.join("version.txt"), &version)
            .context("Failed to write version.txt")?;

        // 4. Verifying stage
        on_progress(InstallStage::Verifying, 0, None);

        // Move to destination atomically
        if self.install_dir.exists() {
            let timestamp = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis();
            let old_dir = self.install_dir.parent()
                .unwrap_or(&self.install_dir)
                .join(format!("zapret.old.{}", timestamp));
            
            std::fs::rename(&self.install_dir, &old_dir)
                .context("Failed to rename existing installation to backup directory")?;

            if let Err(e) = std::fs::rename(temp_extract_dir, &self.install_dir) {
                // Try rolling back
                let _ = std::fs::rename(&old_dir, &self.install_dir);
                return Err(e).context("Failed to move extracted folder to target installation path");
            }

            // Cleanup old directory (might fail if files are locked, but it shouldn't fail the install)
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
        }

        // Verify that the install directory contains a valid installation
        if !paths::is_valid_install_dir(&self.install_dir) {
            return Err(anyhow::anyhow!("Verification failed: installed files are missing or incomplete"));
        }

        // Create the user list files winws.exe expects (service.bat:load_user_lists).
        crate::zapret::batparse::ensure_user_lists(&self.install_dir);

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
        let parent_dir = self.install_dir.parent()
            .unwrap_or(&self.install_dir);
        
        let temp_zip_path = parent_dir.join("zapret_download.zip");
        let temp_extract_dir = parent_dir.join("zapret_extract.tmp");

        // Clean up any stale files from prior attempts
        if temp_zip_path.exists() {
            let _ = std::fs::remove_file(&temp_zip_path);
        }
        if temp_extract_dir.exists() {
            let _ = std::fs::remove_dir_all(&temp_extract_dir);
        }

        let res = self.perform_install_or_update(&on_progress, &temp_zip_path, &temp_extract_dir).await;

        // Cleanup temporary folders/files
        if temp_zip_path.exists() {
            let _ = std::fs::remove_file(&temp_zip_path);
        }
        if temp_extract_dir.exists() {
            let _ = std::fs::remove_dir_all(&temp_extract_dir);
        }

        res
    }
}
