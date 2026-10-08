use crate::contracts::InstallStage;
use crate::ports::{Installer, ProgressCb};
use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const REPO: &str = "https://github.com/bol-van/zapret2";
const MAX_DOWNLOAD: u64 = 128 * 1024 * 1024;
const MAX_EXTRACTED: u64 = 512 * 1024 * 1024;
const MAX_ENTRIES: usize = 20_000;
const BINARIES: &[&str] = &[
    "winws2.exe",
    "cygwin1.dll",
    "WinDivert.dll",
    "WinDivert64.sys",
];

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct Release {
    tag: String,
    archive_sha256: String,
    manifest_sha256: String,
}

pub struct Zapret2Installer {
    install_dir: PathBuf,
    client: reqwest::Client,
    cache: Mutex<Option<(Instant, Release)>>,
}

impl Zapret2Installer {
    pub fn new(install_dir: PathBuf, client: reqwest::Client) -> Self {
        Self {
            install_dir,
            client,
            cache: Mutex::new(None),
        }
    }

    async fn text(&self, url: String) -> Result<String> {
        let mut response = self
            .client
            .get(url)
            .header(reqwest::header::USER_AGENT, "zapret-ui")
            .timeout(Duration::from_secs(30))
            .send()
            .await?
            .error_for_status()?;
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            if bytes.len() + chunk.len() > 2 * 1024 * 1024 {
                bail!("zapret2 metadata exceeds the 2 MiB limit");
            }
            bytes.extend_from_slice(&chunk);
        }
        String::from_utf8(bytes).context("zapret2 metadata is not UTF-8")
    }

    async fn release(&self) -> Result<Release> {
        if let Some((at, release)) = self
            .cache
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
        {
            if at.elapsed() < Duration::from_secs(300) {
                return Ok(release.clone());
            }
        }
        let feed = self
            .text(format!("{REPO}/releases.atom"))
            .await
            .context("Cannot fetch zapret2 release feed")?;
        let mut last_error = None;
        for tag in crate::release_feed::tags(&feed)
            .into_iter()
            .filter(|tag| valid_tag(tag))
            .take(5)
        {
            let resolved = async {
                let html = self
                    .text(format!("{REPO}/releases/expanded_assets/{tag}"))
                    .await?;
                let archive = format!("zapret2-{tag}.zip");
                Ok::<_, anyhow::Error>(Release {
                    archive_sha256: asset_digest(&html, &tag, &archive)
                        .context("Missing published SHA-256 for zapret2 ZIP")?,
                    manifest_sha256: asset_digest(&html, &tag, "sha256sum.txt")
                        .context("Missing published SHA-256 for zapret2 checksum manifest")?,
                    tag,
                })
            }
            .await;
            match resolved {
                Ok(release) => {
                    *self.cache.lock().unwrap_or_else(|p| p.into_inner()) =
                        Some((Instant::now(), release.clone()));
                    return Ok(release);
                }
                Err(error) => last_error = Some(error),
            }
        }
        Err(last_error.unwrap_or_else(|| {
            anyhow::anyhow!("No complete official zapret2 release with SHA-256 checksums")
        }))
    }
}

#[async_trait::async_trait]
impl Installer for Zapret2Installer {
    async fn is_installed(&self) -> bool {
        super::is_valid_install_dir(&self.install_dir)
    }

    async fn installed_version(&self) -> Option<String> {
        if !self.is_installed().await {
            return None;
        }
        std::fs::read_to_string(self.install_dir.join("version.txt"))
            .ok()
            .map(|v| v.trim().to_owned())
            .filter(|v| valid_tag(v))
    }

    async fn latest_version(&self) -> Result<String> {
        match self.release().await {
            Ok(release) => Ok(release.tag),
            Err(error) => {
                // Offline status checks may show the last successfully installed
                // release; installation itself never skips online verification.
                if let Some(version) = self.installed_version().await {
                    tracing::warn!("zapret2 version lookup unavailable: {error:#}");
                    Ok(version)
                } else {
                    Err(error)
                }
            }
        }
    }

    async fn install_or_update(&self, on_progress: ProgressCb) -> Result<()> {
        let parent = self
            .install_dir
            .parent()
            .context("zapret2 installation needs a parent directory")?;
        let work_root = prepare_work_dir(&self.install_dir)?;
        let lock_path = parent.join("zapret2.install.lock");
        // A crash releases this exclusive Windows handle. No stale-lock heuristic.
        let _lock = {
            use std::os::windows::fs::OpenOptionsExt;
            std::fs::OpenOptions::new().create(true).truncate(false).write(true).share_mode(0)
                .open(lock_path).context("Another zapret2 update is in progress, or its install directory is unavailable")?
        };
        if let Ok(meta) = std::fs::symlink_metadata(&self.install_dir) {
            if !meta.is_dir() || super::is_link(&meta) {
                bail!("zapret2 install directory must not be a link or junction");
            }
        }
        let work = tempfile::Builder::new()
            .prefix("install-")
            .tempdir_in(&work_root)?;
        on_progress(InstallStage::Resolving, 0, None);
        let release = self.release().await?;
        let manifest = self
            .text(format!(
                "{REPO}/releases/download/{}/sha256sum.txt",
                release.tag
            ))
            .await?;
        verify_digest(manifest.as_bytes(), &release.manifest_sha256)
            .context("zapret2 checksum manifest integrity failure")?;
        let checksums = parse_manifest(&manifest)?;
        let zip_path = work.path().join("release.zip");
        let actual_hash = crate::download::to_file(
            self.client
                .get(format!(
                    "{REPO}/releases/download/{0}/zapret2-{0}.zip",
                    release.tag
                ))
                .header(reqwest::header::USER_AGENT, "zapret-ui")
                .timeout(Duration::from_secs(300)),
            &zip_path,
            MAX_DOWNLOAD,
            |bytes, total| on_progress(InstallStage::Downloading, bytes, total),
        )
        .await
        .context("Cannot download official zapret2 ZIP")?;
        if !actual_hash.eq_ignore_ascii_case(&release.archive_sha256) {
            bail!("zapret2 ZIP SHA-256 mismatch; existing installation was not changed");
        }
        tracing::info!(
            "Verified official zapret2 {} ZIP SHA-256 {actual_hash}",
            release.tag
        );
        let install_dir = self.install_dir.clone();
        tokio::task::spawn_blocking(move || {
            // The worker can outlive cancellation of its async caller. Keep
            // exclusive installation ownership until it finishes promotion.
            let _lock = _lock;
            let staged = work.path().join("staged");
            on_progress(InstallStage::Extracting, 0, None);
            extract_runtime(&zip_path, &staged, &release.tag, &checksums)?;
            super::catalog::write_defaults(&staged)?;
            if install_dir.exists() {
                preserve_user_state(&install_dir, &staged)?;
            }
            std::fs::write(staged.join("version.txt"), &release.tag)?;
            std::fs::write(
                staged.join("release.json"),
                serde_json::to_vec_pretty(&release)?,
            )?;
            on_progress(InstallStage::Verifying, 0, None);
            if !super::is_valid_install_dir(&staged) {
                bail!("zapret2 runtime dependencies are incomplete");
            }
            atomic_promote(&staged, &install_dir)?;
            on_progress(InstallStage::Done, 1, Some(1));
            Ok::<_, anyhow::Error>(())
        })
        .await
        .context("zapret2 installation worker failed")??;
        Ok(())
    }
}

fn valid_tag(tag: &str) -> bool {
    tag.len() <= 48
        && tag.strip_prefix('v').is_some_and(|v| {
            v.contains('.')
                && v.split('.')
                    .all(|n| !n.is_empty() && n.bytes().all(|c| c.is_ascii_digit()))
        })
}

/// Read only the digest inside the exact asset's list row. A source-code ZIP,
/// another tag, or a digest from the next asset can never satisfy verification.
fn asset_digest(html: &str, tag: &str, asset: &str) -> Option<String> {
    let expected = format!("href=\"/bol-van/zapret2/releases/download/{tag}/{asset}\"");
    html.split("<li ").skip(1).find_map(|row| {
        let (row, _) = row.split_once("</li>")?;
        if !row.contains(&expected) {
            return None;
        }
        let (_, tail) = row.split_once("sha256:")?;
        let digest: String = tail.chars().take_while(char::is_ascii_hexdigit).collect();
        (digest.len() == 64).then(|| digest.to_ascii_lowercase())
    })
}

fn parse_manifest(text: &str) -> Result<HashMap<String, String>> {
    let mut result = HashMap::new();
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let (hash, path) = line
            .split_once(char::is_whitespace)
            .context("Malformed SHA-256 manifest")?;
        let path = path.trim_start().trim_start_matches('*');
        if hash.len() != 64
            || !hash.bytes().all(|b| b.is_ascii_hexdigit())
            || !super::safe_relative(path)
        {
            bail!("Invalid checksum manifest entry");
        }
        if result
            .insert(path.into(), hash.to_ascii_lowercase())
            .is_some()
        {
            bail!("Duplicate checksum manifest path");
        }
    }
    Ok(result)
}

fn verify_digest(bytes: &[u8], expected: &str) -> Result<()> {
    let actual = digest_hex(bytes);
    if !actual.eq_ignore_ascii_case(expected) {
        bail!("SHA-256 mismatch");
    }
    Ok(())
}

fn digest_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn extract_runtime(
    zip_path: &Path,
    staged: &Path,
    tag: &str,
    checksums: &HashMap<String, String>,
) -> Result<()> {
    let archive_file = std::fs::File::open(zip_path)
        .with_context(|| format!("Cannot open zapret2 archive {}", zip_path.display()))?;
    let mut archive = zip::ZipArchive::new(archive_file)
        .with_context(|| format!("Cannot read zapret2 archive {}", zip_path.display()))?;
    if archive.len() > MAX_ENTRIES {
        bail!("zapret2 ZIP has too many entries");
    }
    let prefix = format!("zapret2-{tag}/");
    let mut size = 0u64;
    let mut paths = HashSet::new();
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index)?;
        let name = entry.name().trim_end_matches('/');
        if entry.enclosed_name().is_none() || !super::safe_relative(name) {
            bail!("Unsafe zapret2 archive path");
        }
        if entry.is_symlink() {
            bail!("zapret2 archive contains a symbolic link");
        }
        size = size
            .checked_add(entry.size())
            .context("Archive size overflow")?;
        if size > MAX_EXTRACTED {
            bail!("zapret2 ZIP exceeds the uncompressed size limit");
        }
        if entry.is_dir() {
            continue;
        }
        let Some(relative) = entry.name().strip_prefix(&prefix) else {
            bail!("Unexpected zapret2 archive root");
        };
        let target = if let Some(binary) = relative.strip_prefix("binaries/windows-x86_64/") {
            if BINARIES.contains(&binary) {
                Some(format!("bin/{binary}"))
            } else {
                None
            }
        } else if relative.starts_with("lua/")
            || relative.starts_with("files/fake/")
            || matches!(relative, "LICENSE" | "COPYING" | "docs/LICENSE.txt")
        {
            Some(relative.to_string())
        } else {
            None
        };
        let Some(target) = target else {
            continue;
        };
        if !paths.insert(target.to_ascii_lowercase()) {
            bail!("Duplicate zapret2 runtime path");
        }
        if entry.size() > 64 * 1024 * 1024 {
            bail!("zapret2 runtime file exceeds size limit");
        }
        let mut bytes = Vec::new();
        entry
            .by_ref()
            .take(64 * 1024 * 1024 + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 != entry.size() {
            bail!("zapret2 ZIP entry size mismatch");
        }
        if target.starts_with("bin/") {
            let digest = checksums
                .get(entry.name())
                .context("Missing Windows binary SHA-256")?;
            verify_digest(&bytes, digest).context("zapret2 Windows binary integrity failure")?;
        }
        let destination = staged.join(&target);
        std::fs::create_dir_all(destination.parent().context("Missing runtime parent")?)?;
        std::fs::write(&destination, &bytes).with_context(|| {
            format!(
                "Cannot write zapret2 runtime file {}",
                destination.display()
            )
        })?;
        if target.starts_with("bin/") {
            verify_x64_pe(&destination)?;
        }
    }
    if !super::is_valid_install_dir(staged) {
        bail!(
            "Official release does not contain the complete Windows x64 runtime and Lua libraries"
        );
    }
    Ok(())
}

fn verify_x64_pe(path: &Path) -> Result<()> {
    let mut file = std::fs::File::open(path).with_context(|| {
        format!(
            "Cannot open zapret2 runtime file {} to check its architecture",
            path.display()
        )
    })?;
    let mut dos = [0u8; 64];
    file.read_exact(&mut dos)
        .context("Truncated Windows runtime file")?;
    if &dos[..2] != b"MZ" {
        bail!("Runtime file is not a Windows PE image");
    }
    let offset = u32::from_le_bytes(dos[60..64].try_into().unwrap()) as u64;
    if offset < 64 || offset > file.metadata()?.len().saturating_sub(6) {
        bail!("Invalid Windows PE header offset");
    }
    file.seek(SeekFrom::Start(offset))?;
    let mut pe = [0u8; 6];
    file.read_exact(&mut pe)?;
    if &pe[..4] != b"PE\0\0" || pe[4..] != [0x64, 0x86] {
        bail!("zapret2 runtime file is not x86_64");
    }
    Ok(())
}

/// Only data areas are carried across upgrades. User strategies are argv JSON;
/// Lua and binaries always come from the newly verified official release.
fn preserve_user_state(current: &Path, staged: &Path) -> Result<()> {
    let mut entries = 0usize;
    let mut bytes = 0u64;
    fn copy(
        source: &Path,
        destination: &Path,
        entries: &mut usize,
        bytes: &mut u64,
        depth: usize,
    ) -> Result<()> {
        if depth > 16 {
            bail!("User data exceeds directory depth limit");
        }
        let meta = std::fs::symlink_metadata(source)?;
        if super::is_link(&meta) {
            bail!("User data must not contain links or junctions");
        }
        *entries += 1;
        *bytes = bytes.saturating_add(meta.len());
        if *entries > MAX_ENTRIES || *bytes > MAX_EXTRACTED {
            bail!("User data exceeds preservation size limit");
        }
        if meta.is_dir() {
            std::fs::create_dir_all(destination)?;
            for entry in std::fs::read_dir(source)? {
                let entry = entry?;
                let name = entry.file_name();
                if !super::safe_relative(&name.to_string_lossy()) {
                    bail!("Unsafe user data filename");
                }
                copy(
                    &entry.path(),
                    &destination.join(name),
                    entries,
                    bytes,
                    depth + 1,
                )?;
            }
        } else if meta.is_file() {
            if let Some(parent) = destination.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(source, destination)?;
        } else {
            bail!("User data is not a regular file or directory");
        }
        Ok(())
    }
    for relative in ["lists", "strategies", "files/user"] {
        let source = current.join(relative);
        let mut component_path = current.to_path_buf();
        for component in relative.split('/') {
            component_path.push(component);
            if std::fs::symlink_metadata(&component_path).is_ok_and(|meta| super::is_link(&meta)) {
                bail!("User data parent must not be a link or junction");
            }
        }
        match std::fs::symlink_metadata(&source) {
            Ok(_) => copy(&source, &staged.join(relative), &mut entries, &mut bytes, 0)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("Cannot inspect existing zapret2 user data"),
        }
    }
    // Preserve edits made directly to a shipped preset as a separate custom
    // candidate, while refreshing the reviewed default from this app version.
    let presets = current.join("presets");
    if let Ok(meta) = std::fs::symlink_metadata(&presets) {
        if !meta.is_dir() || super::is_link(&meta) {
            bail!("Preset directory must not be a link");
        }
        for (index, entry) in std::fs::read_dir(presets)?.enumerate() {
            if index >= 256 {
                bail!("Too many edited zapret2 presets");
            }
            let entry = entry?;
            if entry.path().extension().is_none_or(|ext| ext != "json") {
                continue;
            }
            let meta = entry.metadata()?;
            if !meta.is_file()
                || meta.len() > 64 * 1024
                || super::is_link(&std::fs::symlink_metadata(entry.path())?)
            {
                bail!("Edited preset must be a regular JSON file of at most 64 KiB");
            }
            let data = std::fs::read(entry.path())?;
            if std::fs::read(staged.join("presets").join(entry.file_name()))
                .ok()
                .as_deref()
                != Some(data.as_slice())
            {
                let digest = digest_hex(&data);
                let target = staged
                    .join("strategies")
                    .join(format!("saved-{}.json", &digest[..16]));
                std::fs::create_dir_all(target.parent().unwrap())?;
                if target.exists() && std::fs::read(&target)? != data {
                    bail!(
                        "A different custom strategy already occupies {}",
                        target.display()
                    );
                }
                std::fs::write(target, data)?;
            }
        }
    }
    Ok(())
}

/// A persistent sibling keeps every downloaded/staged/rollback binary inside
/// one exact engine-specific directory, on the installation's volume.
pub(crate) fn work_dir_for_install(installed: &Path) -> Result<PathBuf> {
    if installed
        .components()
        .any(|part| matches!(part, std::path::Component::ParentDir))
    {
        bail!("zapret2 installation path must not contain parent traversal");
    }
    let name = installed
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| super::safe_relative(name) && !name.contains('/'))
        .context("zapret2 installation needs a regular directory name")?;
    Ok(installed.with_file_name(format!("{name}-work")))
}

/// Check every existing directory component before creating or renaming files.
/// In particular, a pre-existing work-root junction must never redirect writes.
pub(crate) fn validate_plain_directory_path(path: &Path) -> Result<()> {
    if path
        .components()
        .any(|part| matches!(part, std::path::Component::ParentDir))
    {
        bail!(
            "Directory path must not contain parent traversal: {}",
            path.display()
        );
    }
    for component in std::path::absolute(path)?.ancestors() {
        match std::fs::symlink_metadata(component) {
            Ok(meta) if !meta.is_dir() || super::is_link(&meta) => {
                bail!(
                    "Directory must not be a file, link or junction: {}",
                    component.display()
                );
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("Cannot inspect directory {}", component.display()))
            }
        }
    }
    Ok(())
}

fn prepare_work_dir(installed: &Path) -> Result<PathBuf> {
    let root = work_dir_for_install(installed)?;
    validate_plain_directory_path(installed)?;
    validate_plain_directory_path(&root)?;
    std::fs::create_dir_all(&root)
        .with_context(|| format!("Cannot create zapret2 work directory {}", root.display()))?;
    validate_plain_directory_path(&root)?;
    Ok(root)
}

fn atomic_promote(staged: &Path, installed: &Path) -> Result<()> {
    atomic_promote_with(staged, installed, |from, to| std::fs::rename(from, to))
}

fn atomic_promote_with(
    staged: &Path,
    installed: &Path,
    mut rename: impl FnMut(&Path, &Path) -> std::io::Result<()>,
) -> Result<()> {
    if !super::is_valid_install_dir(staged) {
        bail!("Cannot promote incomplete zapret2 runtime");
    }
    let work_root = prepare_work_dir(installed)?;
    validate_plain_directory_path(staged)?;
    let stage_path = std::path::absolute(staged)?
        .to_string_lossy()
        .replace('/', "\\")
        .to_lowercase();
    let root_path = std::path::absolute(&work_root)?
        .to_string_lossy()
        .replace('/', "\\")
        .to_lowercase();
    if !stage_path.starts_with(&format!("{}\\", root_path.trim_end_matches(['/', '\\']))) {
        bail!("zapret2 staging must remain inside its dedicated work directory");
    }
    // Outside the auto-cleaned TempDir: failed rollback retains the old core.
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let backup = work_root.join(format!("previous.{}.{nonce}", std::process::id()));
    let had_previous = installed.exists();
    if had_previous {
        rename(installed, &backup)
            .context("Cannot replace zapret2: stop its process or service first")?;
    }
    if let Err(error) = rename(staged, installed) {
        if had_previous {
            if let Err(rollback) = rename(&backup, installed) {
                bail!("zapret2 promotion failed ({error}); rollback failed ({rollback}); previous core retained at {}", backup.display());
            }
        }
        return Err(error).context("Cannot promote zapret2; previous installation restored");
    }
    if had_previous {
        if let Err(error) = std::fs::remove_dir_all(&backup) {
            tracing::warn!(
                "Cannot remove previous zapret2 backup {}: {error}",
                backup.display()
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn pe() -> Vec<u8> {
        let mut bytes = vec![0u8; 70];
        bytes[..2].copy_from_slice(b"MZ");
        bytes[60..64].copy_from_slice(&64u32.to_le_bytes());
        bytes[64..].copy_from_slice(b"PE\0\0\x64\x86");
        bytes
    }

    fn fixture_zip(path: &Path, tag: &str) -> HashMap<String, String> {
        let mut zip = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
        let mut hashes = HashMap::new();
        for name in BINARIES {
            let path = format!("zapret2-{tag}/binaries/windows-x86_64/{name}");
            zip.start_file(&path, zip::write::SimpleFileOptions::default())
                .unwrap();
            zip.write_all(&pe()).unwrap();
            hashes.insert(path, digest_hex(&pe()));
        }
        for name in ["zapret-lib.lua", "zapret-antidpi.lua"] {
            zip.start_file(
                format!("zapret2-{tag}/lua/{name}"),
                zip::write::SimpleFileOptions::default(),
            )
            .unwrap();
            zip.write_all(b"-- fixture\n").unwrap();
        }
        zip.finish().unwrap();
        hashes
    }

    #[test]
    fn release_hash_is_bound_to_exact_asset_row() {
        let digest = "a".repeat(64);
        let html = format!("<li class=\"asset\"><a href=\"/bol-van/zapret2/releases/download/v1.0/zapret2-v1.0.zip\">zip</a>sha256:{digest}</li><li class=\"asset\">sha256:{}</li>", "b".repeat(64));
        assert_eq!(
            asset_digest(&html, "v1.0", "zapret2-v1.0.zip"),
            Some(digest)
        );
        assert!(asset_digest(&html, "v1.1", "zapret2-v1.0.zip").is_none());
        assert!(asset_digest(
            &html.replace("sha256:", "hash:"),
            "v1.0",
            "zapret2-v1.0.zip"
        )
        .is_none());
        assert!(!valid_tag("v1.0/../../evil"));
    }

    #[test]
    fn official_layout_is_normalized_and_every_binary_must_match() {
        let dir = tempfile::tempdir().unwrap();
        let zip = dir.path().join("core.zip");
        let mut hashes = fixture_zip(&zip, "v1.0");
        extract_runtime(&zip, &dir.path().join("valid"), "v1.0", &hashes).unwrap();
        assert!(super::super::is_valid_install_dir(
            &dir.path().join("valid")
        ));
        *hashes.values_mut().next().unwrap() = "0".repeat(64);
        assert!(extract_runtime(&zip, &dir.path().join("invalid"), "v1.0", &hashes).is_err());
        assert!(extract_runtime(&zip, &dir.path().join("wrong-tag"), "v1.1", &hashes).is_err());
    }

    #[test]
    fn checksum_manifest_rejects_duplicate_and_unsafe_paths() {
        let line = format!("{}  core/file\n", "a".repeat(64));
        assert_eq!(parse_manifest(&line).unwrap().len(), 1);
        assert!(parse_manifest(&(line.clone() + &line)).is_err());
        assert!(parse_manifest(&line.replace("core/file", "../outside")).is_err());
        assert!(verify_digest(b"tampered", &"a".repeat(64)).is_err());
    }

    #[test]
    fn update_preserves_user_lists_strategies_and_blobs() {
        let dir = tempfile::tempdir().unwrap();
        let current = dir.path().join("current");
        let staged = dir.path().join("staged");
        super::super::catalog::write_defaults(&current).unwrap();
        super::super::catalog::write_defaults(&staged).unwrap();
        for relative in [
            "lists/list-general-user.txt",
            "lists/custom.txt",
            "strategies/custom.json",
            "files/user/custom.bin",
        ] {
            std::fs::write(current.join(relative), "user data").unwrap();
        }
        preserve_user_state(&current, &staged).unwrap();
        for relative in [
            "lists/list-general-user.txt",
            "lists/custom.txt",
            "strategies/custom.json",
            "files/user/custom.bin",
        ] {
            assert_eq!(
                std::fs::read_to_string(staged.join(relative)).unwrap(),
                "user data"
            );
        }
    }

    #[test]
    fn invalid_staging_cannot_replace_previous_installation() {
        let dir = tempfile::tempdir().unwrap();
        let installed = dir.path().join("installed");
        std::fs::create_dir(&installed).unwrap();
        std::fs::write(installed.join("important"), "old").unwrap();
        assert!(atomic_promote(&dir.path().join("missing"), &installed).is_err());
        assert_eq!(
            std::fs::read_to_string(installed.join("important")).unwrap(),
            "old"
        );
    }

    #[test]
    fn locked_staging_rolls_back_previous_installation() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = tempfile::tempdir().unwrap();
        let zip = dir.path().join("core.zip");
        let hashes = fixture_zip(&zip, "v1.0");
        let installed = dir.path().join("installed");
        let staged = prepare_work_dir(&installed).unwrap().join("staged");
        extract_runtime(&zip, &staged, "v1.0", &hashes).unwrap();
        std::fs::create_dir(&installed).unwrap();
        std::fs::write(installed.join("previous"), "previous core").unwrap();
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(3) // FILE_SHARE_READ | FILE_SHARE_WRITE, no DELETE
            .custom_flags(0x02000000) // FILE_FLAG_BACKUP_SEMANTICS: open directory
            .open(&staged)
            .unwrap();
        assert!(super::super::is_valid_install_dir(&staged));
        assert!(atomic_promote(&staged, &installed).is_err());
        assert_eq!(
            std::fs::read_to_string(installed.join("previous")).unwrap(),
            "previous core"
        );
        drop(lock);
    }

    #[test]
    fn work_directory_is_a_dedicated_sibling() {
        for name in ["zapret2", "custom-core", "core.v2"] {
            let installed = Path::new(r"C:\cores").join(name);
            assert_eq!(
                work_dir_for_install(&installed).unwrap(),
                Path::new(r"C:\cores").join(format!("{name}-work"))
            );
        }
        for path in [r"C:\", r"C:\cores\..\zapret2", r"C:\cores\zapret2."] {
            assert!(work_dir_for_install(Path::new(path)).is_err(), "{path}");
        }
    }

    #[test]
    fn existing_file_cannot_redirect_work_directory_creation() {
        let parent = tempfile::tempdir().unwrap();
        let installed = parent.path().join("zapret2");
        let work = work_dir_for_install(&installed).unwrap();
        std::fs::write(&work, "unrelated file").unwrap();
        assert!(prepare_work_dir(&installed).is_err());
        assert_eq!(std::fs::read_to_string(work).unwrap(), "unrelated file");
        assert!(!installed.exists());
    }

    #[test]
    fn failed_rollback_backup_survives_temporary_work_cleanup() {
        let parent = tempfile::tempdir().unwrap();
        let installed = parent.path().join("zapret2");
        std::fs::create_dir(&installed).unwrap();
        std::fs::write(installed.join("previous"), "previous core").unwrap();
        let work_root = prepare_work_dir(&installed).unwrap();
        let temporary = tempfile::tempdir_in(&work_root).unwrap();
        let staged = temporary.path().join("staged");
        let zip = temporary.path().join("fixture.zip");
        let hashes = fixture_zip(&zip, "v1.0");
        extract_runtime(&zip, &staged, "v1.0", &hashes).unwrap();
        let error = atomic_promote_with(&staged, &installed, |from, to| {
            if from == installed {
                std::fs::rename(from, to)
            } else {
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "injected rename failure",
                ))
            }
        })
        .unwrap_err();
        assert!(error.to_string().contains("rollback failed"));
        drop(temporary);
        let backups: Vec<_> = std::fs::read_dir(&work_root)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert_eq!(backups.len(), 1);
        assert_eq!(
            std::fs::read_to_string(backups[0].join("previous")).unwrap(),
            "previous core"
        );
        assert!(!staged.exists());
    }

    #[test]
    fn archive_and_runtime_open_errors_name_the_blocked_file() {
        let parent = tempfile::tempdir().unwrap();
        let archive = parent.path().join("missing.zip");
        let error = extract_runtime(
            &archive,
            &parent.path().join("staged"),
            "v1.0",
            &HashMap::new(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("missing.zip"));
        assert!(error.to_string().contains("Cannot open zapret2 archive"));
        let error = verify_x64_pe(&parent.path().join("winws2.exe")).unwrap_err();
        assert!(error.to_string().contains("winws2.exe"));
        assert!(error.to_string().contains("architecture"));
    }

    #[test]
    fn edited_shipped_preset_becomes_preserved_custom_candidate() {
        let current = tempfile::tempdir().unwrap();
        let staged = tempfile::tempdir().unwrap();
        super::super::catalog::write_defaults(current.path()).unwrap();
        super::super::catalog::write_defaults(staged.path()).unwrap();
        let preset = current.path().join("presets/multisplit.json");
        let edited = std::fs::read_to_string(&preset)
            .unwrap()
            .replace("pos=1,midsld", "pos=2,midsld");
        std::fs::write(preset, &edited).unwrap();
        preserve_user_state(current.path(), staged.path()).unwrap();
        let custom = std::fs::read_dir(staged.path().join("strategies"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        assert_eq!(std::fs::read_to_string(custom).unwrap(), edited);
        assert_ne!(
            std::fs::read_to_string(staged.path().join("presets/multisplit.json")).unwrap(),
            edited
        );
    }

    #[test]
    fn rejects_non_x64_runtime() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("binary");
        let mut bytes = pe();
        bytes[68] = 0x4c;
        bytes[69] = 0x01;
        std::fs::write(&path, bytes).unwrap();
        assert!(verify_x64_pe(&path).is_err());
    }
}
