use crate::contracts::TelegramProxySettings;
use anyhow::{bail, ensure, Result};
use std::collections::BTreeMap;
use std::net::Ipv4Addr;

pub(super) fn overrides(value: &str) -> Result<BTreeMap<u16, Ipv4Addr>> {
    let mut result = BTreeMap::new();
    for entry in value
        .split(|c: char| c.is_whitespace() || c == ',' || c == ';')
        .filter(|s| !s.is_empty())
    {
        let Some((dc, ip)) = entry.split_once(':') else {
            bail!("telegram.error_dc");
        };
        let dc: u16 = dc
            .parse()
            .map_err(|_| anyhow::anyhow!("telegram.error_dc"))?;
        let ip: Ipv4Addr = ip
            .parse()
            .map_err(|_| anyhow::anyhow!("telegram.error_dc"))?;
        ensure!(
            matches!(dc, 1..=5 | 203)
                && !ip.is_unspecified()
                && !ip.is_multicast()
                && !ip.is_broadcast(),
            "telegram.error_dc"
        );
        ensure!(result.insert(dc, ip).is_none(), "telegram.error_dc");
    }
    Ok(result)
}

pub(super) fn decode_secret(value: &str) -> Result<[u8; 16]> {
    ensure!(
        value.len() == 32 && value.bytes().all(|c| c.is_ascii_hexdigit()),
        "telegram.error_secret"
    );
    let mut secret = [0; 16];
    for (out, hex) in secret.iter_mut().zip(value.as_bytes().chunks_exact(2)) {
        *out = u8::from_str_radix(std::str::from_utf8(hex)?, 16)?;
    }
    Ok(secret)
}

pub(super) fn prepare(mut settings: TelegramProxySettings) -> Result<TelegramProxySettings> {
    ensure!(settings.port > 0, "telegram.error_port");
    ensure!(
        (1..=30).contains(&settings.connect_timeout_secs),
        "telegram.error_timeout"
    );
    ensure!(
        (1..=256).contains(&settings.max_connections),
        "telegram.error_limit"
    );
    let map = overrides(&settings.dc_overrides)?;
    settings.dc_overrides = map
        .iter()
        .map(|(dc, ip)| format!("{dc}:{ip}"))
        .collect::<Vec<_>>()
        .join(" ");
    settings.secret = settings.secret.trim().to_ascii_lowercase();
    // Accept a copied padded-intermediate secret as well as the bare 16 bytes.
    if settings.secret.len() == 34 && settings.secret.starts_with("dd") {
        settings.secret.drain(..2);
    }
    if settings.secret.is_empty() {
        let mut secret = [0; 16];
        getrandom::fill(&mut secret).map_err(|_| anyhow::anyhow!("telegram.error_random"))?;
        settings.secret = secret.iter().map(|b| format!("{b:02x}")).collect();
    }
    decode_secret(&settings.secret)?;
    Ok(settings)
}
