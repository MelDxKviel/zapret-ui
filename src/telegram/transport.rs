use super::{
    protocol::{self, Cipher, Handshake, MAX_PACKET},
    settings,
};
use crate::contracts::TelegramProxySettings;
use aes::cipher::StreamCipher;
use anyhow::{bail, Result};
use futures_util::{SinkExt, StreamExt};
use std::{
    collections::{BTreeMap, BTreeSet},
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::Arc,
    time::Duration,
};
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
const MAX_WSS_ADDRESSES: usize = 16;

// Keep resolver order, but bound parallel attempts and avoid retrying the
// override IP (or the same DNS address twice) after it has already failed.
fn alternate_ws_addresses(
    addresses: impl IntoIterator<Item = SocketAddr>,
    override_ip: IpAddr,
) -> Vec<SocketAddr> {
    let mut seen = BTreeSet::new();
    addresses
        .into_iter()
        .filter(|address| address.ip() != override_ip && seen.insert(address.ip()))
        .take(MAX_WSS_ADDRESSES)
        .collect()
}

// Do not interpolate arbitrary upstream error text into logs: HTTP response
// bodies and headers are untrusted, and proxy links/secrets must stay private.
fn io_failure(error: &std::io::Error) -> String {
    match error.raw_os_error() {
        Some(code) => format!("I/O {:?} (OS {code})", error.kind()),
        None => format!("I/O {:?}", error.kind()),
    }
}

fn ws_failure(error: &anyhow::Error) -> String {
    use tokio_tungstenite::tungstenite::Error;
    for cause in error.chain() {
        if let Some(error) = cause.downcast_ref::<std::io::Error>() {
            return io_failure(error);
        }
        if let Some(error) = cause.downcast_ref::<Error>() {
            return match error {
                Error::Io(error) => io_failure(error),
                Error::Tls(_) => "TLS handshake failed".into(),
                Error::Http(response) => format!("HTTP {}", response.status().as_u16()),
                Error::Protocol(_) => "invalid WebSocket response".into(),
                Error::ConnectionClosed | Error::AlreadyClosed => "connection closed".into(),
                _ => "WebSocket connection failed".into(),
            };
        }
    }
    "WebSocket connection failed".into()
}

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
    /// Rate-limit diagnostics without a timer or work while the proxy is off.
    failure_logs: Mutex<BTreeMap<i16, Instant>>,
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
            failure_logs: Mutex::new(BTreeMap::new()),
        })
    }

    async fn connect_ws(&self, domain: &str, address: SocketAddr) -> Result<Ws> {
        let socket = connect_tcp(address).await?;
        socket.set_nodelay(true)?;
        // The override changes only the destination IP. TLS still validates
        // Telegram's hostname and certificate.
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
        Ok(ws)
    }

    async fn race_ws(
        &self,
        dc: i16,
        domain: &str,
        candidates: Vec<SocketAddr>,
        failures: &mut Vec<String>,
    ) -> Option<Ws> {
        let mut attempts = futures_util::stream::FuturesUnordered::new();
        let mut pending: BTreeSet<_> = candidates.iter().copied().collect();
        for address in candidates {
            attempts.push(async move {
                let result = self.connect_ws(domain, address).await;
                (address, result)
            });
        }
        match timeout(self.timeout, async {
            while let Some((address, result)) = attempts.next().await {
                pending.remove(&address);
                match result {
                    Ok(ws) => {
                        tracing::debug!("Telegram DC{dc}: WSS {domain} connected via {address}");
                        return Some(ws);
                    }
                    Err(error) => {
                        let reason = ws_failure(&error);
                        tracing::debug!("Telegram DC{dc}: WSS {domain} via {address}: {reason}");
                        failures.push(format!("WSS {domain} via {address}: {reason}"));
                    }
                }
            }
            None
        })
        .await
        {
            Ok(result) => result,
            Err(_) => {
                for address in pending {
                    failures.push(format!("WSS {domain} via {address}: timed out"));
                }
                tracing::debug!("Telegram DC{dc}: WSS {domain} connection timed out");
                None
            }
        }
    }

    fn wss_route(&self, dc: i16) -> Option<(String, SocketAddr)> {
        let id = dc.unsigned_abs();
        let ip = (id != 203)
            .then(|| self.overrides.get(&id).copied())
            .flatten()?;
        // The suffix selects a different Telegram backend role, not an
        // interchangeable hostname. Never race media and regular sessions.
        let suffix = if dc < 0 { "-1" } else { "" };
        Some((
            format!("kws{id}{suffix}.web.telegram.org"),
            SocketAddr::new(IpAddr::V4(ip), 443),
        ))
    }

    async fn connect(&self, dc: i16) -> Result<Upstream> {
        let id = dc.unsigned_abs();
        let mut failures = Vec::new();
        // The official WebSocket relay for DC2 must not be used as a relay for
        // DC203. Other DCs need an explicit matching route; otherwise try their
        // own TCP endpoint immediately instead of stalling on WSS guesses.
        if let Some((domain, address)) = self.wss_route(dc) {
            if let Some(ws) = self
                .race_ws(dc, &domain, vec![address], &mut failures)
                .await
            {
                self.failure_logs.lock().await.remove(&dc);
                tracing::debug!("Telegram DC{dc}: WSS connected");
                return Ok(Upstream::WebSocket(Box::new(ws)));
            }

            // DNS may have moved to another Telegram IP. Do not retry the same
            // IP after it has already consumed the connection timeout.
            let resolved = timeout(self.timeout, lookup_host((domain.as_str(), 443))).await;
            let alternate = match resolved {
                Ok(Ok(addresses)) => {
                    let addresses = alternate_ws_addresses(addresses, address.ip());
                    if addresses.is_empty() {
                        failures.push(format!("DNS {domain}: no alternate addresses"));
                    }
                    addresses
                }
                Ok(Err(error)) => {
                    failures.push(format!("DNS {domain}: {}", io_failure(&error)));
                    Vec::new()
                }
                Err(_) => {
                    failures.push(format!("DNS {domain}: timed out"));
                    Vec::new()
                }
            };
            if !alternate.is_empty() {
                if let Some(ws) = self.race_ws(dc, &domain, alternate, &mut failures).await {
                    self.failure_logs.lock().await.remove(&dc);
                    tracing::debug!("Telegram DC{dc}: WSS connected via DNS");
                    return Ok(Upstream::WebSocket(Box::new(ws)));
                }
            }
        } else {
            failures.push("WSS not configured for this DC".into());
        }
        if self.fallback {
            if let Some(ip) = dc_ip(id) {
                let address = SocketAddr::new(IpAddr::V4(ip), 443);
                match timeout(self.timeout, connect_tcp(address)).await {
                    Ok(Ok(socket)) => {
                        socket.set_nodelay(true)?;
                        self.failure_logs.lock().await.remove(&dc);
                        tracing::debug!("Telegram DC{dc}: using TCP fallback");
                        return Ok(Upstream::Tcp(socket));
                    }
                    Ok(Err(error)) => {
                        failures.push(format!("TCP {address}: {}", io_failure(&error)));
                    }
                    Err(_) => failures.push(format!("TCP {address}: timed out")),
                }
            } else {
                failures.push("TCP endpoint not configured for this DC".into());
            }
        } else {
            failures.push("TCP fallback disabled".into());
        }
        let mut logs = self.failure_logs.lock().await;
        if logs
            .get(&dc)
            .is_none_or(|t| t.elapsed() >= Duration::from_secs(30))
        {
            let reasons = failures.join("; ");
            tracing::warn!("Telegram DC{dc}: no reachable upstream; {reasons}");
            logs.insert(dc, Instant::now());
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
            // Propagate graceful EOF and check its result before releasing the
            // sockets. OwnedWriteHalf's Drop cannot report a failed shutdown.
            writer.shutdown().await?;
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
