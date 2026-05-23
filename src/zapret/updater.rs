use semver::Version;

/// Compares the current installed version and the latest version.
/// If both versions are semver-compliant (after stripping potential 'v' prefix),
/// it checks if `latest > current`. Otherwise, it falls back to string inequality.
pub fn is_update_available(current: &str, latest: &str) -> bool {
    let current_clean = current.trim_start_matches('v');
    let latest_clean = latest.trim_start_matches('v');

    match (Version::parse(current_clean), Version::parse(latest_clean)) {
        (Ok(curr_ver), Ok(lat_ver)) => lat_ver > curr_ver,
        _ => current != latest,
    }
}
