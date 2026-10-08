use semver::Version;

/// Compare release precedence, treating local `git describe` builds as at least
/// their base tag. For example, `v0.4.0-3-g8609f33` contains commits after v0.4.0;
/// its Git suffix must not make the published v0.4.0 look like an upgrade.
/// Genuine prerelease tags keep their SemVer ordering. Non-SemVer core versions
/// retain the existing string-comparison fallback.
pub fn is_update_available(current: &str, latest: &str) -> bool {
    let latest_clean = latest.trim_start_matches('v');

    match (
        current_release_version(current),
        Version::parse(latest_clean),
    ) {
        (Some(curr_ver), Ok(lat_ver)) => lat_ver.cmp_precedence(&curr_ver).is_gt(),
        _ => current != latest,
    }
}

fn current_release_version(current: &str) -> Option<Version> {
    let clean = current.trim_start_matches('v');
    let clean = clean.strip_suffix("-dirty").unwrap_or(clean);
    // Parse from the right so a base such as `v0.5.0-rc.1` stays a prerelease.
    let described_base = clean.rsplit_once("-g").and_then(|(prefix, hash)| {
        if !(4..=64).contains(&hash.len()) || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let (base, commits) = prefix.rsplit_once('-')?;
        if commits.is_empty() || !commits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        Version::parse(base).ok()
    });
    described_base.or_else(|| Version::parse(clean).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_describe_builds_do_not_offer_their_base_release() {
        for current in [
            "v0.4.0-3-g8609f33",
            "0.4.0-3-g8609f33",
            "v0.4.0-3-g8609f33-dirty",
            "v0.4.0-0-g8609f33",
            "v0.4.0-dirty",
        ] {
            assert!(!is_update_available(current, "v0.4.0"), "{current}");
            assert!(!is_update_available(current, "v0.3.9"), "{current}");
            assert!(is_update_available(current, "v0.4.1"), "{current}");
        }
    }

    #[test]
    fn stable_and_real_prerelease_ordering_is_preserved() {
        assert!(!is_update_available("v0.4.0", "v0.4.0"));
        assert!(!is_update_available("v0.4.1", "v0.4.0"));
        assert!(is_update_available("v0.4.0", "v0.4.1"));
        assert!(is_update_available("v0.4.0-rc.1", "v0.4.0"));
        assert!(is_update_available("v0.4.0-rc.1", "v0.4.0-rc.2"));
        assert!(!is_update_available("v0.4.0", "v0.4.0-rc.1"));
        assert!(!is_update_available(
            "v0.4.0-rc.1-3-g8609f33",
            "v0.4.0-rc.1"
        ));
        assert!(is_update_available("v0.4.0-rc.1-3-g8609f33", "v0.4.0"));
        // A non-Git prerelease containing a similar suffix must remain one.
        assert!(is_update_available("v0.4.0-3-gnothex", "v0.4.0"));
    }

    #[test]
    fn build_metadata_does_not_change_release_precedence() {
        assert!(!is_update_available("v0.4.0+local", "v0.4.0+release"));
        assert!(!is_update_available("v0.4.0", "v0.4.0+release"));
        assert!(is_update_available("v0.4.0+local", "v0.4.1"));
    }

    #[test]
    fn non_semver_core_versions_keep_the_existing_fallback() {
        assert!(!is_update_available("1.2.3.4", "1.2.3.4"));
        assert!(is_update_available("1.2.3.4", "1.2.3.5"));
    }
}
