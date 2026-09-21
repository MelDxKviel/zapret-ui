//! Pure helpers for the upstream ZapretMac release format. Also tested on Windows.
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

pub const REPO: &str = "https://github.com/Flowseal/zapret-mac-discord-youtube";
pub const ASSET: &str = "ZapretMac-macOS-universal.zip";
pub const SERVICE_ROOT: &str = "/Library/Application Support/ZapretMac";
pub const SERVICE_PLIST: &str = "/Library/LaunchDaemons/io.github.flowseal.zapretmac.plist";
pub const SERVICE_LABEL: &str = "system/io.github.flowseal.zapretmac";

pub fn release_tag(atom: &str) -> Option<String> {
    let rest = atom.split_once("/releases/tag/")?.1;
    let tag = rest.split(['"', '<', '/', '\'', ' ', '\n', '\r']).next()?;
    // Tags become URL path components, never commands or local paths.
    (!tag.is_empty()
        && tag
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c)))
    .then(|| tag.to_owned())
}

pub fn valid_strategy_id(id: &str) -> bool {
    id == "general"
        || id.strip_prefix("general-").is_some_and(|s| {
            s.split('-').all(|p| {
                !p.is_empty()
                    && p.bytes()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
            })
        })
}

pub fn valid_payload(path: &Path) -> bool {
    [
        "bin/utunws",
        "install.sh",
        "run.sh",
        "stop.sh",
        "restart.sh",
        "watchdog.sh",
        "test-strategies.sh",
        "update-app.sh",
        "strategies.tsv",
        "strategies/general-simple-fake.conf.in",
        "io.github.flowseal.zapretmac.plist.in",
        "ipset-none.txt",
        "ipset-any.txt",
    ]
    .iter()
    .all(|p| path.join(p).is_file())
        && path.join("default-lists").is_dir()
}

/// Ignore the GUI and __MACOSX resource forks. Only the verified engine payload
/// is promoted into the application-managed core directory.
pub fn promote_payload(extracted: &Path) -> Result<()> {
    let payload = [
        extracted.join("ZapretMac.app/Contents/Resources/Payload"),
        extracted.join("Contents/Resources/Payload"),
    ]
    .into_iter()
    .find(|p| valid_payload(p))
    .context("ZapretMac release is missing its engine Payload")?;
    for entry in std::fs::read_dir(payload)? {
        let entry = entry?;
        std::fs::rename(entry.path(), extracted.join(entry.file_name()))?;
    }
    // These directories are inside the fresh temporary extraction tree only.
    for name in ["ZapretMac.app", "Contents", "__MACOSX"] {
        let dir = extracted.join(name);
        if dir.is_dir() {
            std::fs::remove_dir_all(dir)?;
        }
    }
    Ok(())
}

pub fn user_data_dir() -> Result<PathBuf> {
    let home = directories::BaseDirs::new().context("Cannot resolve home directory")?;
    Ok(home
        .home_dir()
        .join("Library/Application Support/ZapretMac"))
}

/// Upstream interpolates this path into sed and XML. Reject characters those
/// formats cannot represent safely instead of passing them into a root script.
pub fn validate_data_dir(path: &Path) -> Result<()> {
    let s = path.to_str().context("User data path must be UTF-8")?;
    if !s.starts_with("/Users/")
        || !s.ends_with("/Library/Application Support/ZapretMac")
        || s.chars().any(|c| c.is_control() || "&<>|\\".contains(c))
        || path
            .components()
            .any(|p| matches!(p, std::path::Component::ParentDir))
    {
        bail!("Unsupported home path for ZapretMac: {}", path.display());
    }
    Ok(())
}

pub fn initialize_user_data(payload: &Path, data: &Path) -> Result<()> {
    let lists = data.join("lists");
    std::fs::create_dir_all(&lists)?;
    for name in [
        "list-general.txt",
        "list-general-user.txt",
        "list-google.txt",
        "list-exclude.txt",
        "list-exclude-user.txt",
        "ipset-all.txt",
        "ipset-exclude.txt",
        "ipset-exclude-user.txt",
    ] {
        let target = lists.join(name);
        if !target.exists() {
            std::fs::copy(payload.join("default-lists").join(name), target)?;
        }
    }
    for (name, value) in [
        ("selected-strategy", "general-simple-fake\n"),
        ("ipset-mode", "none\n"),
    ] {
        let path = data.join(name);
        if !path.exists() {
            std::fs::write(path, value)?;
        }
    }
    Ok(())
}

pub fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

pub fn applescript_string(value: &str) -> String {
    format!(
        "\"{}\"",
        value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n")
            .replace('\r', "\\r")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn release_feed_and_untrusted_identifiers() {
        assert_eq!(
            release_tag("<link href=\"https://github.com/o/r/releases/tag/v1.1.2\"/>"),
            Some("v1.1.2".into())
        );
        assert_eq!(release_tag("/releases/tag/$(evil)"), None);
        assert!(valid_strategy_id("general-simple-fake-alt2"));
        for s in [
            "../general",
            "general/evil",
            "general-",
            "general--alt",
            "general-$(id)",
        ] {
            assert!(!valid_strategy_id(s));
        }
    }
    #[test]
    fn quotes_both_interpreters_and_validates_upstream_path() {
        assert_eq!(shell_quote("a'b $(id)"), "'a'\\''b $(id)'");
        assert_eq!(applescript_string("a\"b\\c"), "\"a\\\"b\\\\c\"");
        assert!(validate_data_dir(Path::new(
            "/Users/John Smith/Library/Application Support/ZapretMac"
        ))
        .is_ok());
        for p in [
            "/tmp/ZapretMac",
            "/Users/a&b/Library/Application Support/ZapretMac",
            "/Users/../Library/Application Support/ZapretMac",
        ] {
            assert!(validate_data_dir(Path::new(p)).is_err());
        }
    }
    #[test]
    fn user_lists_and_preferences_survive_core_updates() {
        let tmp = tempfile::tempdir().unwrap();
        let payload = tmp.path().join("payload");
        std::fs::create_dir_all(payload.join("default-lists")).unwrap();
        for name in [
            "list-general.txt",
            "list-general-user.txt",
            "list-google.txt",
            "list-exclude.txt",
            "list-exclude-user.txt",
            "ipset-all.txt",
            "ipset-exclude.txt",
            "ipset-exclude-user.txt",
        ] {
            std::fs::write(payload.join("default-lists").join(name), "default").unwrap();
        }
        let data = tmp.path().join("data");
        initialize_user_data(&payload, &data).unwrap();
        std::fs::write(data.join("lists/list-general-user.txt"), "custom.example").unwrap();
        std::fs::write(data.join("ipset-mode"), "loaded").unwrap();
        initialize_user_data(&payload, &data).unwrap();
        assert_eq!(
            std::fs::read_to_string(data.join("lists/list-general-user.txt")).unwrap(),
            "custom.example"
        );
        assert_eq!(
            std::fs::read_to_string(data.join("ipset-mode")).unwrap(),
            "loaded"
        );
    }
}
