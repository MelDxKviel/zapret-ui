//! MTProxy obfuscation → standard Telegram obfuscation. This only removes the
//! transport envelope: MTProto message contents remain encrypted end to end.
//! Protocol reference: https://core.telegram.org/mtproto/mtproto-transports
use aes::cipher::{KeyIvInit, StreamCipher};
use anyhow::{bail, ensure, Result};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt};

pub(super) type Cipher = ctr::Ctr128BE<aes::Aes256>;
pub(super) const MAX_PACKET: usize = 4 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Transport {
    Abridged,
    Intermediate,
    Padded,
}

pub(super) struct Handshake {
    pub dc: i16,
    pub transport: Transport,
    pub decrypt: Cipher,
    pub encrypt: Cipher,
}

fn cipher(material: &[u8], secret: Option<&[u8; 16]>) -> Cipher {
    let key: [u8; 32] = match secret {
        Some(secret) => {
            let mut hash = Sha256::new();
            hash.update(&material[..32]);
            hash.update(secret);
            hash.finalize().into()
        }
        None => material[..32].try_into().unwrap(),
    };
    Cipher::new_from_slices(&key, &material[32..48]).expect("fixed key and IV lengths")
}

pub(super) fn parse_init(init: &[u8; 64], secret: &[u8; 16]) -> Result<Handshake> {
    let mut decrypt = cipher(&init[8..56], Some(secret));
    let mut plain = *init;
    decrypt.apply_keystream(&mut plain);
    let transport = match &plain[56..60] {
        [0xef, 0xef, 0xef, 0xef] => Transport::Abridged,
        [0xee, 0xee, 0xee, 0xee] => Transport::Intermediate,
        [0xdd, 0xdd, 0xdd, 0xdd] => Transport::Padded,
        _ => bail!("Invalid MTProxy secret or transport"),
    };
    let dc = i16::from_le_bytes([plain[60], plain[61]]);
    ensure!(
        matches!(dc.unsigned_abs(), 1..=5 | 203),
        "Unsupported Telegram DC"
    );
    let mut reverse = init[8..56].to_vec();
    reverse.reverse();
    Ok(Handshake {
        dc,
        transport,
        decrypt,
        encrypt: cipher(&reverse, Some(secret)),
    })
}

pub(super) fn relay_init(dc: i16, transport: Transport) -> Result<([u8; 64], Cipher, Cipher)> {
    let mut init = [0u8; 64];
    loop {
        getrandom::fill(&mut init).map_err(|e| anyhow::anyhow!("OS random source: {e}"))?;
        if init[0] != 0xef
            && init[4..8] != [0; 4]
            && !matches!(
                &init[..4],
                b"HEAD"
                    | b"POST"
                    | b"GET "
                    | b"OPTI"
                    | [0xee, 0xee, 0xee, 0xee]
                    | [0xdd, 0xdd, 0xdd, 0xdd]
                    | [0x16, 0x03, 0x01, 0x02]
            )
        {
            break;
        }
    }
    let mut encrypt = cipher(&init[8..56], None);
    let mut reverse = init[8..56].to_vec();
    reverse.reverse();
    let decrypt = cipher(&reverse, None);
    init[56..60].fill(match transport {
        Transport::Abridged => 0xef,
        Transport::Intermediate => 0xee,
        Transport::Padded => 0xdd,
    });
    init[60..62].copy_from_slice(&dc.to_le_bytes());
    let mut encrypted = init;
    encrypt.apply_keystream(&mut encrypted);
    init[56..].copy_from_slice(&encrypted[56..]);
    Ok((init, encrypt, decrypt))
}

/// Read/decrypt one complete transport packet. Never split a packet across WS
/// messages. Called inside a persistent uplink future, not a cancellable select
/// iteration, so partial reads cannot lose bytes or advance CTR twice.
pub(super) async fn read_packet<R: AsyncRead + Unpin>(
    reader: &mut R,
    decrypt: &mut Cipher,
    transport: Transport,
) -> Result<Option<Vec<u8>>> {
    let mut header = [0; 4];
    if reader.read(&mut header[..1]).await? == 0 {
        return Ok(None);
    }
    decrypt.apply_keystream(&mut header[..1]);
    let header_len = if transport == Transport::Abridged && header[0] & 0x7f != 0x7f {
        1
    } else {
        4
    };
    if header_len == 4 {
        reader.read_exact(&mut header[1..]).await?;
        decrypt.apply_keystream(&mut header[1..]);
    }
    let len = match (transport, header_len) {
        (Transport::Abridged, 1) => usize::from(header[0] & 0x7f) * 4,
        (Transport::Abridged, _) => {
            (u32::from_le_bytes([header[1], header[2], header[3], 0]) as usize) * 4
        }
        _ => (u32::from_le_bytes(header) & 0x7fff_ffff) as usize,
    };
    ensure!(
        len > 0 && len <= MAX_PACKET - header_len,
        "Invalid MTProto packet length"
    );
    let mut packet = vec![0; header_len + len];
    packet[..header_len].copy_from_slice(&header[..header_len]);
    reader.read_exact(&mut packet[header_len..]).await?;
    decrypt.apply_keystream(&mut packet[header_len..]);
    Ok(Some(packet))
}

#[cfg(test)]
mod tests;
