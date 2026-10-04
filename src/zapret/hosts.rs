//! Safe, repeatable updates of the Windows system hosts file.

use anyhow::{Context, Result};
use std::collections::HashSet;
use std::ffi::OsString;
use std::io::Write;
use std::net::IpAddr;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

const BEGIN: &str = "# BEGIN zapret-ui managed hosts";
const END: &str = "# END zapret-ui managed hosts";

extern "system" {
    fn GetSystemDirectoryW(buffer: *mut u16, size: u32) -> u32;
    fn ReplaceFileW(
        replaced: *const u16,
        replacement: *const u16,
        backup: *const u16,
        flags: u32,
        exclude: *mut std::ffi::c_void,
        reserved: *mut std::ffi::c_void,
    ) -> i32;
}

pub fn system_hosts_path() -> Result<PathBuf> {
    let mut buffer = vec![0u16; 32_768];
    let len = unsafe { GetSystemDirectoryW(buffer.as_mut_ptr(), buffer.len() as u32) } as usize;
    if len == 0 || len >= buffer.len() {
        anyhow::bail!(
            "Failed to resolve the Windows system directory: {}",
            std::io::Error::last_os_error()
        );
    }
    Ok(PathBuf::from(OsString::from_wide(&buffer[..len])).join("drivers/etc/hosts"))
}

fn source_hostnames(source: &str) -> Result<HashSet<String>> {
    if source.len() > 256 * 1024 {
        anyhow::bail!("the downloaded hosts file is unexpectedly large");
    }
    let mut names = HashSet::new();
    for (line_number, line) in source.lines().enumerate() {
        let body = line.split('#').next().unwrap_or("").trim();
        if body.is_empty() {
            continue;
        }
        let mut fields = body.split_whitespace();
        let ip = fields.next().unwrap();
        if ip.parse::<IpAddr>().is_err() {
            anyhow::bail!(
                "invalid IP address on downloaded hosts line {}",
                line_number + 1
            );
        }
        let mut count = 0;
        for host in fields {
            if host.len() > 253
                || !host.contains('.')
                || !host.split('.').all(|label| {
                    !label.is_empty()
                        && label.len() <= 63
                        && !label.starts_with('-')
                        && !label.ends_with('-')
                        && label
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
                })
            {
                anyhow::bail!(
                    "invalid hostname on downloaded hosts line {}",
                    line_number + 1
                );
            }
            names.insert(host.to_ascii_lowercase());
            count += 1;
        }
        if count == 0 {
            anyhow::bail!(
                "missing hostname on downloaded hosts line {}",
                line_number + 1
            );
        }
    }
    if names.is_empty() {
        anyhow::bail!("the downloaded hosts file was empty");
    }
    Ok(names)
}

/// Retain unrelated mappings and comments, replace the managed block, and
/// remove older manual mappings for domains now supplied by upstream.
pub fn merge(current: &str, source: &str) -> Result<Option<String>> {
    let names = source_hostnames(source)?;
    let newline = if current.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let (bom, text) = if let Some(rest) = current.strip_prefix('\u{feff}') {
        ("\u{feff}", rest)
    } else {
        ("", current)
    };
    let mut out = Vec::new();
    let mut in_block = false;
    let mut found_block = false;

    for original in text.lines() {
        let line = original.trim_end_matches('\r');
        if line.trim() == BEGIN {
            if in_block || found_block {
                anyhow::bail!("hosts file has multiple zapret-ui managed blocks");
            }
            found_block = true;
            in_block = true;
            continue;
        }
        if line.trim() == END {
            if !in_block {
                anyhow::bail!("hosts file has an unmatched zapret-ui end marker");
            }
            in_block = false;
            continue;
        }
        if in_block {
            continue;
        }

        let (body, comment) = line.split_once('#').unwrap_or((line, ""));
        let mut fields = body.split_whitespace();
        let Some(ip) = fields.next() else {
            out.push(line.to_string());
            continue;
        };
        if ip.parse::<IpAddr>().is_err() {
            out.push(line.to_string());
            continue;
        }
        let hosts: Vec<_> = fields.collect();
        let retained: Vec<_> = hosts
            .iter()
            .copied()
            .filter(|host| !names.contains(&host.to_ascii_lowercase()))
            .collect();
        if retained.len() == hosts.len() {
            out.push(line.to_string());
        } else if !retained.is_empty() {
            let mut replacement = format!("{ip} {}", retained.join(" "));
            if !comment.is_empty() {
                replacement.push_str(" #");
                replacement.push_str(comment);
            }
            out.push(replacement);
        } else if !comment.is_empty() {
            out.push(format!("#{comment}"));
        }
    }
    if in_block {
        anyhow::bail!("hosts file has an unmatched zapret-ui begin marker");
    }
    while out.last().is_some_and(|line| line.is_empty()) {
        out.pop();
    }
    if !out.is_empty() {
        out.push(String::new());
    }
    out.push(BEGIN.to_string());
    out.extend(
        source
            .lines()
            .map(|line| line.trim_end_matches('\r').to_string()),
    );
    out.push(END.to_string());

    let merged = format!("{bom}{}{newline}", out.join(newline));
    Ok((merged != current).then_some(merged))
}

/// A successful write leaves a timestamped backup next to `hosts`.
pub fn update_system_hosts(path: &Path, source: &str) -> Result<bool> {
    let current =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let Some(merged) = merge(&current, source)? else {
        return Ok(false);
    };
    crate::zapret::elevation::check_elevation()?;
    replace_with_backup(path, &current, &merged)?;
    flush_dns_cache();
    Ok(true)
}

fn replace_with_backup(path: &Path, current: &str, merged: &str) -> Result<()> {
    let dir = path
        .parent()
        .context("hosts path has no parent directory")?;

    let backup = dir.join(format!(
        "hosts.zapret-ui.{}-{}.bak",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos()
    ));
    let mut backup_file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&backup)
        .context("creating hosts backup")?;
    backup_file
        .write_all(current.as_bytes())
        .context("backing up hosts")?;
    backup_file.sync_all().context("syncing hosts backup")?;
    drop(backup_file);

    let mut temporary = tempfile::Builder::new()
        .prefix("hosts.zapret-ui.")
        .tempfile_in(dir)
        .context("creating temporary hosts file")?;
    temporary
        .write_all(merged.as_bytes())
        .context("writing temporary hosts")?;
    temporary
        .as_file()
        .sync_all()
        .context("syncing temporary hosts")?;
    let temporary = temporary.into_temp_path();
    let wide = |p: &Path| {
        p.as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect::<Vec<_>>()
    };
    let replaced = wide(path);
    let replacement = wide(temporary.as_ref());
    let result = unsafe {
        ReplaceFileW(
            replaced.as_ptr(),
            replacement.as_ptr(),
            std::ptr::null(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if result == 0 {
        let error = std::io::Error::last_os_error();
        // ReplaceFileW can fail after moving the original file. Restore our
        // saved copy if that left the hosts path absent.
        if !path.exists() {
            std::fs::copy(&backup, path).with_context(|| {
                format!(
                    "restoring hosts from {} after failed replacement",
                    backup.display()
                )
            })?;
        }
        anyhow::bail!(
            "replacing hosts failed (backup: {}): {}",
            backup.display(),
            error
        );
    }
    tracing::info!("Updated hosts file; backup: {}", backup.display());
    Ok(())
}

fn flush_dns_cache() {
    use std::os::windows::process::CommandExt;
    let command = system_hosts_path().ok().and_then(|path| {
        path.parent()?
            .parent()?
            .parent()
            .map(|p| p.join("ipconfig.exe"))
    });
    let Some(command) = command else {
        tracing::warn!("Could not locate ipconfig.exe to flush the DNS cache");
        return;
    };
    match std::process::Command::new(command)
        .arg("/flushdns")
        .creation_flags(0x08000000)
        .output()
    {
        Ok(output) if output.status.success() => {}
        Ok(output) => tracing::warn!("DNS cache flush exited with {}", output.status),
        Err(error) => tracing::warn!("Could not flush the DNS cache: {error}"),
    }
}

#[cfg(test)]
mod tests {
    use super::{merge, replace_with_backup};

    const SOURCE: &str = "1.2.3.4 example.com\n5.6.7.8 example.com\n9.8.7.6 new.example.com\n";

    #[test]
    fn preserves_unrelated_entries_and_replaces_old_mappings() {
        let current =
            "# personal\r\n1.1.1.1 example.com other.test # shared\r\n2.2.2.2 unrelated.test\r\n";
        let merged = merge(current, SOURCE).unwrap().unwrap();
        assert!(merged.contains("1.1.1.1 other.test # shared\r\n"));
        assert!(merged.contains("2.2.2.2 unrelated.test\r\n"));
        assert!(!merged.contains("1.1.1.1 example.com"));
        assert!(merged.contains("1.2.3.4 example.com\r\n5.6.7.8 example.com"));
        assert_eq!(merge(&merged, SOURCE).unwrap(), None);
    }

    #[test]
    fn replaces_managed_block_when_upstream_changes() {
        let first = merge("127.0.0.1 localhost\n", SOURCE).unwrap().unwrap();
        let updated = merge(&first, "4.3.2.1 example.com\n").unwrap().unwrap();
        assert!(!updated.contains("new.example.com"));
        assert!(updated.contains("127.0.0.1 localhost"));
        assert_eq!(
            updated.matches("# BEGIN zapret-ui managed hosts").count(),
            1
        );
    }

    #[test]
    fn rejects_invalid_source_and_broken_markers() {
        assert!(merge("", "<html>error</html>").is_err());
        assert!(merge("# BEGIN zapret-ui managed hosts\n", SOURCE).is_err());
    }

    #[test]
    fn replaces_file_and_keeps_original_backup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hosts");
        let original = "127.0.0.1 localhost\r\n";
        let merged = merge(original, SOURCE).unwrap().unwrap();
        std::fs::write(&path, original).unwrap();
        replace_with_backup(&path, original, &merged).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), merged);
        let backups: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".bak"))
            .collect();
        assert_eq!(backups.len(), 1);
        assert_eq!(
            std::fs::read_to_string(backups[0].path()).unwrap(),
            original
        );
    }
}
