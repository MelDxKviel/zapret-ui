use super::{
    protocol::{self, Cipher, Handshake, MAX_PACKET},
    settings,
};
use crate::contracts::TelegramProxySettings;
use aes::cipher::StreamCipher;
use anyhow::{bail, Result};
use futures_util::{SinkExt, StreamExt};
use std::{collections::BTreeMap, net::Ipv4Addr, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{lookup_host, TcpSocket, TcpStream, ToSocketAddrs},
    sync::Mutex,
    time::{timeout, Instant},
};
use tokio_tungstenite::{
    client_async_tls_with_config,
    tungstenite::{client::IntoClientRequest, protocol::WebSocketConfig, Message},
    Connector, MaybeTlsStream, WebSocketStream,
};

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

// Keep upstream sockets out of subsequently spawned winws/helper processes too.
// TcpSocket uses non-inheritable Windows handles at creation, without a race
// between creating the socket and clearing its inheritance flag.
async fn connect_tcp(address: impl ToSocketAddrs) -> std::io::Result<TcpStream> {
    let mut last_error = None;
    for address in lookup_host(address).await? {
        let socket = if address.is_ipv4() {
            TcpSocket::new_v4()
        } else {
            TcpSocket::new_v6()
        };
        let result = match socket {
            Ok(socket) => socket.connect(address).await,
            Err(error) => Err(error),
        };
        match result {
            Ok(stream) => return Ok(stream),
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error.unwrap_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "no resolved Telegram address",
        )
    }))
}

pub(super) struct Routes {
    overrides: BTreeMap<u16, Ipv4Addr>,
    fallback: bool,
    timeout: Duration,
    tls: Arc<rustls::ClientConfig>,
    /// Demand-driven backoff, bounded by the supported DCs. No timer task.
    failed: Mutex<BTreeMap<i16, Instant>>,
    failed_overrides: Mutex<BTreeMap<u16, Instant>>,
}

impl Routes {
    pub fn new(settings: &TelegramProxySettings) -> Result<Self> {
        let roots =
            rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let tls = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()?
        .with_root_certificates(roots)
        .with_no_client_auth();
        Ok(Self {
            overrides: settings::overrides(&settings.dc_overrides)?,
            fallback: settings.tcp_fallback,
            timeout: Duration::from_secs(u64::from(settings.connect_timeout_secs)),
            tls: Arc::new(tls),
            failed: Mutex::new(BTreeMap::new()),
            failed_overrides: Mutex::new(BTreeMap::new()),
        })
    }

    async fn connect(&self, dc: i16) -> Result<Upstream> {
        let id = dc.unsigned_abs();
        let backoff = self.fallback
            && self
                .failed
                .lock()
                .await
                .get(&dc)
                .is_some_and(|t| t.elapsed() < Duration::from_secs(30));
        if !backoff {
            let ws_dc = if id == 203 { 2 } else { id };
            let names = if dc < 0 {
                [
                    format!("kws{ws_dc}-1.web.telegram.org"),
                    format!("kws{ws_dc}.web.telegram.org"),
                ]
            } else {
                [
                    format!("kws{ws_dc}.web.telegram.org"),
                    format!("kws{ws_dc}-1.web.telegram.org"),
                ]
            };
            // An upstream IP override can stop working for a particular DC.
            // Still try the official domain before falling back to TCP.
            let destinations = self
                .overrides
                .get(&id)
                .copied()
                .map(Some)
                .into_iter()
                .chain(std::iter::once(None));
            for override_ip in destinations {
                if override_ip.is_some()
                    && self
                        .failed_overrides
                        .lock()
                        .await
                        .get(&id)
                        .is_some_and(|t| t.elapsed() < Duration::from_secs(60))
                {
                    continue;
                }
                for domain in &names {
                    let result = timeout(self.timeout, async {
                        let socket = if let Some(ip) = override_ip {
                            connect_tcp((ip, 443)).await?
                        } else {
                            connect_tcp((domain.as_str(), 443)).await?
                        };
                        socket.set_nodelay(true)?;
                        // The override changes the destination IP only. The TLS name
                        // and certificate are still validated against Telegram.
                        let mut request = format!("wss://{domain}/apiws").into_client_request()?;
                        request
                            .headers_mut()
                            .insert("Sec-WebSocket-Protocol", "binary".parse()?);
                        let ws_config = WebSocketConfig::default()
                            .read_buffer_size(16 * 1024)
                            .write_buffer_size(0)
                            .max_message_size(Some(MAX_PACKET))
                            .max_frame_size(Some(MAX_PACKET));
                        let (ws, _) = client_async_tls_with_config(
                            request,
                            socket,
                            Some(ws_config),
                            Some(Connector::Rustls(self.tls.clone())),
                        )
                        .await?;
                        Ok::<_, anyhow::Error>(ws)
                    })
                    .await;
                    match result {
                        Ok(Ok(ws)) => {
                            self.failed.lock().await.remove(&dc);
                            tracing::debug!("Telegram DC{dc}: WSS connected");
                            return Ok(Upstream::WebSocket(Box::new(ws)));
                        }
                        Ok(Err(e)) => tracing::debug!("Telegram DC{dc}: WSS unavailable: {e}"),
                        Err(_) => {
                            tracing::debug!("Telegram DC{dc}: WSS timed out");
                            // Both hostnames share this override IP. Move straight
                            // to DNS after a timeout instead of doubling the wait.
                            if override_ip.is_some() {
                                break;
                            }
                        }
                    }
                }
                if override_ip.is_some() {
                    self.failed_overrides
                        .lock()
                        .await
                        .insert(id, Instant::now());
                }
            }
            self.failed.lock().await.insert(dc, Instant::now());
        }
        if self.fallback {
            if let Some(ip) = dc_ip(id) {
                if let Ok(Ok(socket)) = timeout(self.timeout, connect_tcp((ip, 443))).await {
                    socket.set_nodelay(true)?;
                    tracing::debug!("Telegram DC{dc}: using TCP fallback");
                    return Ok(Upstream::Tcp(socket));
                }
            }
        }
        Err(UpstreamUnavailable.into())
    }
}

fn dc_ip(id: u16) -> Option<Ipv4Addr> {
    Some(match id {
        1 => Ipv4Addr::new(149, 154, 175, 50),
        2 => Ipv4Addr::new(149, 154, 167, 51),
        3 => Ipv4Addr::new(149, 154, 175, 100),
        4 => Ipv4Addr::new(149, 154, 167, 91),
        5 => Ipv4Addr::new(149, 154, 171, 5),
        203 => Ipv4Addr::new(91, 105, 192, 100),
        _ => return None,
    })
}

#[derive(Debug)]
pub(super) struct UpstreamUnavailable;
impl std::fmt::Display for UpstreamUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Telegram upstream unavailable")
    }
}
impl std::error::Error for UpstreamUnavailable {}

enum Upstream {
    WebSocket(Box<Ws>),
    Tcp(TcpStream),
}

pub(super) async fn serve(
    mut client: TcpStream,
    secret: [u8; 16],
    routes: Arc<Routes>,
) -> Result<()> {
    client.set_nodelay(true)?;
    let mut init = [0; 64];
    timeout(Duration::from_secs(10), client.read_exact(&mut init)).await??;
    let handshake = protocol::parse_init(&init, &secret)?;
    let (init, encrypt, decrypt) = protocol::relay_init(handshake.dc, handshake.transport)?;
    match routes.connect(handshake.dc).await? {
        Upstream::WebSocket(mut ws) => {
            timeout(
                routes.timeout,
                ws.send(Message::Binary(init.to_vec().into())),
            )
            .await??;
            bridge_ws(client, *ws, handshake, encrypt, decrypt).await
        }
        Upstream::Tcp(mut upstream) => {
            timeout(routes.timeout, upstream.write_all(&init)).await??;
            bridge_tcp(client, upstream, handshake, encrypt, decrypt).await
        }
    }
}

async fn bridge_ws<S>(
    client: TcpStream,
    ws: WebSocketStream<S>,
    mut local: Handshake,
    mut encrypt: Cipher,
    mut decrypt: Cipher,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (mut reader, mut writer) = client.into_split();
    let (sink, mut stream) = ws.split();
    let sink = Mutex::new(sink);
    let upload = async {
        while let Some(mut packet) =
            protocol::read_packet(&mut reader, &mut local.decrypt, local.transport).await?
        {
            encrypt.apply_keystream(&mut packet);
            sink.lock()
                .await
                .send(Message::Binary(packet.into()))
                .await?;
        }
        Ok::<_, anyhow::Error>(())
    };
    let download = async {
        while let Some(message) = stream.next().await {
            match message? {
                Message::Binary(bytes) => {
                    let mut data = bytes.to_vec();
                    decrypt.apply_keystream(&mut data);
                    local.encrypt.apply_keystream(&mut data);
                    writer.write_all(&data).await?;
                }
                Message::Ping(_) => {
                    sink.lock().await.flush().await?;
                } // automatic pong
                Message::Close(_) => break,
                Message::Text(_) => bail!("Unexpected Telegram text frame"),
                _ => {}
            }
        }
        Ok::<_, anyhow::Error>(())
    };
    tokio::select! { result = upload => result, result = download => result }
}

async fn forward<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    mut reader: R,
    mut writer: W,
    mut decrypt: Cipher,
    mut encrypt: Cipher,
) -> Result<()> {
    let mut data = vec![0; 16 * 1024];
    loop {
        let len = reader.read(&mut data).await?;
        if len == 0 {
            return Ok(());
        }
        decrypt.apply_keystream(&mut data[..len]);
        encrypt.apply_keystream(&mut data[..len]);
        writer.write_all(&data[..len]).await?;
    }
}

async fn bridge_tcp(
    client: TcpStream,
    upstream: TcpStream,
    local: Handshake,
    encrypt: Cipher,
    decrypt: Cipher,
) -> Result<()> {
    let (cr, cw) = client.into_split();
    let (ur, uw) = upstream.into_split();
    tokio::select! {
        result = forward(cr, uw, local.decrypt, encrypt) => result,
        result = forward(ur, cw, decrypt, local.encrypt) => result,
    }
}

#[cfg(test)]
mod tests;
