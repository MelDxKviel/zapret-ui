//! tg-ws-proxy-compatible relays. No pool, refresh loop or startup fetch.
use anyhow::{bail, ensure, Result};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};
use tokio::sync::{Mutex, OnceCell, Semaphore};

const DIRECTORY: &str =
    "https://raw.githubusercontent.com/Flowseal/tg-ws-proxy/main/.github/cfproxy-domains.txt";
const MAX_DIRECTORY_BYTES: usize = 8192;
const MAX_DOMAINS: usize = 32;
pub(super) const MAX_ATTEMPTS: usize = 4;

fn valid_domain(domain: &str) -> bool {
    if domain.len() > 245 || !domain.is_ascii() {
        return false;
    }
    let labels: Vec<_> = domain.split('.').collect();
    labels.len() >= 2
        && labels.iter().all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'-')
        })
        && labels
            .last()
            .is_some_and(|label| label.len() >= 2 && label.bytes().all(|c| c.is_ascii_alphabetic()))
        && !domain.ends_with(".localhost")
        && !domain.ends_with(".local")
}

pub(super) fn domains(value: &str) -> Result<Vec<String>> {
    ensure!(
        value.len() <= MAX_DIRECTORY_BYTES,
        "telegram.error_relay_domains"
    );
    let mut seen = BTreeSet::new();
    let mut result = Vec::new();
    for domain in value.split(|c: char| c.is_whitespace() || c == ',' || c == ';') {
        if domain.is_empty() {
            continue;
        }
        let domain = domain.to_ascii_lowercase();
        ensure!(valid_domain(&domain), "telegram.error_relay_domains");
        if seen.insert(domain.clone()) {
            result.push(domain);
        }
        ensure!(result.len() <= MAX_DOMAINS, "telegram.error_relay_domains");
    }
    Ok(result)
}

// The upstream directory uses a Caesar rotation for its .co.uk domain pool.
// This is a public encoding, not encryption or a trust/integrity mechanism.
fn decode_domain(value: &str) -> String {
    let Some(prefix) = value.strip_suffix(".com") else {
        return value.to_owned();
    };
    let rotation = prefix.bytes().filter(u8::is_ascii_alphabetic).count() % 26;
    let mut result = String::with_capacity(prefix.len() + 6);
    for byte in prefix.bytes() {
        result.push(if byte.is_ascii_lowercase() {
            (b'a' + (usize::from(byte - b'a') + 26 - rotation) as u8 % 26) as char
        } else {
            byte as char
        });
    }
    result.push_str(".co.uk");
    result
}

fn parse_directory(value: &str) -> Result<Vec<String>> {
    ensure!(
        value.len() <= MAX_DIRECTORY_BYTES,
        "relay directory exceeds size limit"
    );
    let decoded = value
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty() && !s.starts_with('#'))
        .map(|domain| decode_domain(&domain.to_ascii_lowercase()))
        .collect::<Vec<_>>()
        .join(" ");
    let result = domains(&decoded)?;
    ensure!(!result.is_empty(), "relay directory is empty");
    Ok(result)
}

async fn fetch_directory() -> Result<Vec<String>> {
    let client = reqwest::Client::builder()
        .https_only(true)
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .pool_max_idle_per_host(0)
        .connect_timeout(Duration::from_secs(3))
        .timeout(Duration::from_secs(5))
        .build()?;
    let mut response = client
        .get(DIRECTORY)
        .header(reqwest::header::USER_AGENT, "zapret-ui")
        .send()
        .await?;
    if !response.status().is_success() {
        bail!("relay directory HTTP {}", response.status().as_u16());
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        ensure!(
            body.len() + chunk.len() <= MAX_DIRECTORY_BYTES,
            "relay directory exceeds size limit"
        );
        body.extend_from_slice(&chunk);
    }
    parse_directory(std::str::from_utf8(&body)?)
}

pub(super) struct Relay {
    custom: Vec<String>,
    cached: OnceCell<Vec<String>>,
    preferred: Mutex<BTreeMap<u16, String>>,
    cursor: AtomicUsize,
    pub dials: Semaphore,
}

impl Relay {
    pub fn new(custom: &str) -> Result<Self> {
        Ok(Self {
            custom: domains(custom)?,
            cached: OnceCell::new(),
            preferred: Mutex::new(BTreeMap::new()),
            cursor: AtomicUsize::new(0),
            dials: Semaphore::new(8),
        })
    }

    pub async fn candidates(&self, dc: u16) -> Vec<String> {
        let pool = if !self.custom.is_empty() {
            &self.custom
        } else {
            self.cached.get_or_init(|| async {
                match fetch_directory().await {
                    Ok(domains) => domains,
                    Err(_) => {
                        // No untrusted response text or periodic retry. Restart
                        // reloads the directory, and custom domains skip it.
                        tracing::warn!("Telegram relay directory unavailable; restart proxy to retry or specify a custom domain");
                        Vec::new()
                    }
                }
            }).await
        };
        if pool.is_empty() {
            return Vec::new();
        }
        let preferred = self.preferred.lock().await.get(&dc).cloned();
        let offset = (self
            .cursor
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(usize::from(dc)))
            % pool.len();
        let mut result = Vec::new();
        if let Some(domain) = preferred {
            result.push(domain);
        }
        for index in 0..pool.len() {
            let domain = &pool[(offset + index) % pool.len()];
            if !result.contains(domain) {
                result.push(domain.clone());
            }
            if result.len() == MAX_ATTEMPTS {
                break;
            }
        }
        result
    }

    pub async fn remember(&self, dc: u16, domain: String) {
        self.preferred.lock().await.insert(dc, domain);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn domains_reject_urls_addresses_and_unbounded_input() {
        let invalid_settings = crate::contracts::TelegramProxySettings {
            relay_domains: "https://example.org/?secret=private-token".into(),
            ..Default::default()
        };
        assert!(!format!("{invalid_settings:?}").contains("private-token"));
        assert_eq!(
            domains(" Example.org,example.org; other.net ").unwrap(),
            ["example.org", "other.net"]
        );
        for input in [
            "https://example.org",
            "example.org/path",
            "127.0.0.1",
            "[::1]",
            "localhost",
            "x.local",
            "x..org",
            "-x.org",
            "x.org:443",
            "x.org?secret=private",
            "éxample.org",
        ] {
            assert!(domains(input).is_err(), "{input}");
        }
        let many = (0..33)
            .map(|n| format!("relay{n}.example.org"))
            .collect::<Vec<_>>()
            .join(" ");
        assert!(domains(&many).is_err());
        assert!(parse_directory(&"x".repeat(MAX_DIRECTORY_BYTES + 1)).is_err());
        assert!(parse_directory("# no entries\n").is_err());
    }

    #[test]
    fn directory_decoding_matches_upstream_without_accepting_malformed_hosts() {
        assert_eq!(decode_domain("def.com"), "abc.co.uk");
        assert_eq!(decode_domain("relay.example.org"), "relay.example.org");
        assert_eq!(
            parse_directory("# comment\ndef.com\ndef.com\n").unwrap(),
            ["abc.co.uk"]
        );
        assert!(parse_directory("https://bad.example.org").is_err());
    }

    #[tokio::test]
    async fn custom_domains_are_lazy_bounded_and_prefer_a_successful_route() {
        let relay =
            Relay::new("a.example.org b.example.org c.example.org d.example.org e.example.org")
                .unwrap();
        assert!(relay.cached.get().is_none());
        assert_eq!(relay.candidates(2).await.len(), MAX_ATTEMPTS);
        relay.remember(2, "e.example.org".into()).await;
        let candidates = relay.candidates(2).await;
        assert_eq!(candidates[0], "e.example.org");
        assert_eq!(candidates.len(), MAX_ATTEMPTS);
        assert!(relay.cached.get().is_none());
        assert_eq!(relay.dials.available_permits(), 8);
    }
}
