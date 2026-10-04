//! GitHub Atom release discovery shared by GUI and core updates (no API calls).

/// Candidates in feed order. Only entry links identify releases; titles and
/// release-note links can reference unrelated versions.
pub fn tags(atom: &str) -> Vec<String> {
    atom.split("<entry>")
        .skip(1)
        .filter_map(|entry| {
            let (entry, _) = entry.split_once("</entry>")?;
            entry.split("<link ").skip(1).find_map(|link| {
                let (link, _) = link.split_once('>')?;
                let (_, href) = link.split_once("href=")?;
                let quote = href.chars().next()?;
                if quote != '"' && quote != '\'' {
                    return None;
                }
                let href = href[1..].split(quote).next()?;
                let (_, tag) = href.split_once("/releases/tag/")?;
                if tag.is_empty() || tag.contains(['/', '?', '#']) {
                    return None;
                }
                Some(tag.to_string())
            })
        })
        .collect()
}
