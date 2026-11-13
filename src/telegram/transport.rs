use super::{
    protocol::{self, Cipher, Handshake, MAX_PACKET},
    relay, settings, tls,
};
use crate::contracts::TelegramProxySettings;
use aes::cipher::StreamCipher;
use anyhow::{bail, Result};
use futures_util::{future::BoxFuture, FutureExt, SinkExt, StreamExt};
use std::{
    collections::{BTreeMap, BTreeSet},
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{lookup_host, TcpSocket, TcpStream, ToSocketAddrs},
    sync::{Mutex, Semaphore},
    time::{timeout, Instant},
};
use tokio_tungstenite::{
    client_async_tls_with_config,
    tungstenite::{client::IntoClientRequest, protocol::WebSocketConfig, Message},
    Connector, MaybeTlsStream, WebSocketStream,
};

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;
const MAX_WSS_ADDRESSES: usize = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum TlsMode {
    Standard,
    WithoutSni,
    Fronted,
}

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
    tls_without_sni: Arc<rustls::ClientConfig>,
    verifier: Arc<rustls::client::WebPkiServerVerifier>,
    dials: Semaphore,
    relay: Option<relay::Relay>,
    /// Rate-limit diagnostics without a timer or work while the proxy is off.
    failure_logs: Mutex<BTreeMap<i16, Instant>>,
}

impl Routes {
    pub fn new(settings: &TelegramProxySettings) -> Result<Self> {
        let roots = Arc::new(rustls::RootCertStore::from_iter(
            webpki_roots::TLS_SERVER_ROOTS.iter().cloned(),
        ));
        let verifier = rustls::client::WebPkiServerVerifier::builder_with_provider(
            roots.clone(),
            Arc::new(rustls::crypto::ring::default_provider()),
        )
        .build()?;
        let tls = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()?
        .with_root_certificates(roots)
        .with_no_client_auth();
        // Some networks filter the Telegram name in ClientHello. Omit SNI on
        // the second attempt, but STILL authenticate the Telegram hostname.
        // Unlike upstream's fronting mode this never disables certificate checks.
        let mut tls_without_sni = tls.clone();
        tls_without_sni.enable_sni = false;
        Ok(Self {
            overrides: settings::overrides(&settings.dc_overrides)?,
            fallback: settings.tcp_fallback,
            timeout: Duration::from_secs(u64::from(settings.connect_timeout_secs)),
            tls: Arc::new(tls),
            tls_without_sni: Arc::new(tls_without_sni),
            verifier,
            dials: Semaphore::new(32),
            relay: settings
                .relay_enabled
                .then(|| relay::Relay::new(&settings.relay_domains))
                .transpose()?,
            failure_logs: Mutex::new(BTreeMap::new()),
        })
    }

    async fn connect_ws(
        &self,
        domain: &str,
        address: SocketAddr,
        mode: TlsMode,
        dials: &Semaphore,
    ) -> Result<Ws> {
        let _dial = dials.acquire().await?;
        let socket = connect_tcp(address).await?;
        socket.set_nodelay(true)?;
        // The override changes only the destination IP. TLS still validates
        // Telegram's hostname and certificate.
        let wire_name = if mode == TlsMode::Fronted {
            tls::FRONT_SNI
        } else {
            domain
        };
        let mut request = format!("wss://{wire_name}/apiws").into_client_request()?;
        request.headers_mut().insert("Host", domain.parse()?);
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
            Some(Connector::Rustls(match mode {
                TlsMode::Standard => self.tls.clone(),
                TlsMode::WithoutSni => self.tls_without_sni.clone(),
                TlsMode::Fronted => Arc::new(tls::fronted_config(
                    &self.tls,
                    self.verifier.clone(),
                    domain,
                )?),
            })),
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
        let mut pending = BTreeSet::new();
        for address in candidates {
            for mode in [TlsMode::Standard, TlsMode::WithoutSni, TlsMode::Fronted] {
                pending.insert((address, mode));
                attempts.push(async move {
                    // Hedge a filtered ClientHello without doubling healthy
                    // connections. Losing futures drop and close their sockets.
                    if mode != TlsMode::Standard {
                        tokio::time::sleep(Duration::from_millis(if mode == TlsMode::WithoutSni {
                            250
                        } else {
                            500
                        }))
                        .await;
                    }
                    let result = self.connect_ws(domain, address, mode, &self.dials).await;
                    (address, mode, result)
                });
            }
        }
        match timeout(self.timeout, async {
            while let Some((address, mode, result)) = attempts.next().await {
                pending.remove(&(address, mode));
                match result {
                    Ok(ws) => {
                        tracing::debug!(
                            "Telegram DC{dc}: WSS {domain} connected via {address}, TLS={mode:?}"
                        );
                        return Some(ws);
                    }
                    Err(error) => {
                        let reason = ws_failure(&error);
                        tracing::debug!("Telegram DC{dc}: WSS {domain} via {address}: {reason}");
                        failures.push(format!(
                            "WSS {domain} via {address}, TLS={mode:?}: {reason}"
                        ));
                    }
                }
            }
            None
        })
        .await
        {
            Ok(result) => result,
            Err(_) => {
                for (address, _) in pending {
                    failures.push(format!("WSS {domain} via {address}: timed out"));
                }
                tracing::debug!("Telegram DC{dc}: WSS {domain} connection timed out");
                None
            }
        }
    }

    fn wss_domains(&self, dc: i16) -> Vec<String> {
        let id = dc.unsigned_abs();
        if !(1..=5).contains(&id) {
            return Vec::new();
        }
        let mut domains = Vec::new();
        if dc < 0 {
            domains.push(format!("kws{id}-1.web.telegram.org"));
        }
        // tg-ws-proxy also falls back to the regular hostname for media.
        // Preserve the signed DC in relay_init: the requested session stays media.
        domains.push(format!("kws{id}.web.telegram.org"));
        domains
    }

    async fn connect_relay(&self, dc: i16, failures: &mut Vec<String>) -> Option<Ws> {
        let relay = self.relay.as_ref()?;
        let id = dc.unsigned_abs();
        if !matches!(id, 1..=5 | 203) {
            return None;
        }
        let domains = relay.candidates(id).await;
        if domains.is_empty() {
            failures.push("relay directory unavailable or empty".into());
            return None;
        }
        let mut attempts = futures_util::stream::FuturesUnordered::new();
        for (index, base) in domains.into_iter().enumerate() {
            attempts.push(async move {
                tokio::time::sleep(Duration::from_millis(index as u64 * 150)).await;
                let domain = format!("kws{id}.{base}");
                let result = timeout(self.timeout, async {
                    let addresses = lookup_host((domain.as_str(), 443)).await?;
                    let addresses =
                        alternate_ws_addresses(addresses, IpAddr::V4(Ipv4Addr::UNSPECIFIED));
                    let mut sockets = futures_util::stream::FuturesUnordered::new();
                    for address in addresses.into_iter().take(2) {
                        // Relays always authenticate their own DNS name using
                        // normal TLS. Dedicated permits keep blocked direct TCP
                        // attempts from starving the optional fallback.
                        sockets.push(self.connect_ws(
                            &domain,
                            address,
                            TlsMode::Standard,
                            &relay.dials,
                        ));
                    }
                    let mut last_error = anyhow::anyhow!("no resolved relay address");
                    while let Some(result) = sockets.next().await {
                        match result {
                            Ok(ws) => return Ok(ws),
                            Err(error) => last_error = error,
                        }
                    }
                    Err(last_error)
                })
                .await;
                (base, domain, result)
            });
        }
        while let Some((base, domain, result)) = attempts.next().await {
            match result {
                Ok(Ok(ws)) => {
                    relay.remember(id, base).await;
                    tracing::info!("Telegram DC{dc}: connected through WSS relay (TLS verified)");
                    tracing::debug!("Telegram DC{dc}: relay {domain}");
                    return Some(ws);
                }
                Ok(Err(error)) => failures.push(format!("Relay {domain}: {}", ws_failure(&error))),
                Err(_) => failures.push(format!("Relay {domain}: timed out")),
            }
        }
        None
    }

    async fn connect(&self, dc: i16) -> Result<Upstream> {
        let id = dc.unsigned_abs();
        let mut failures = Vec::new();
        // Overrides change only the destination IP, not whether WSS is enabled.
        // An empty overrides field uses official Telegram DNS as promised by UI.
        // All WSS paths share ONE deadline. A stalled override/DNS/media host
        // must not postpone other paths or multiply the configured timeout.
        let mut attempts: futures_util::stream::FuturesUnordered<
            BoxFuture<'_, (Option<Ws>, Vec<String>)>,
        > = futures_util::stream::FuturesUnordered::new();
        for (index, domain) in self.wss_domains(dc).into_iter().enumerate() {
            let override_ip = self.overrides.get(&id).copied();
            if let Some(ip) = override_ip {
                let domain = domain.clone();
                attempts.push(
                    async move {
                        if index > 0 {
                            tokio::time::sleep(Duration::from_millis(200)).await;
                        }
                        let mut reasons = Vec::new();
                        let ws = self
                            .race_ws(dc, &domain, vec![SocketAddr::from((ip, 443))], &mut reasons)
                            .await;
                        (ws, reasons)
                    }
                    .boxed(),
                );
            }
            attempts.push(
                async move {
                    if index > 0 {
                        tokio::time::sleep(Duration::from_millis(200)).await;
                    }
                    let mut reasons = Vec::new();
                    let addresses = match lookup_host((domain.as_str(), 443)).await {
                        Ok(addresses) => alternate_ws_addresses(
                            addresses,
                            IpAddr::V4(override_ip.unwrap_or(Ipv4Addr::UNSPECIFIED)),
                        ),
                        Err(error) => {
                            reasons.push(format!("DNS {domain}: {}", io_failure(&error)));
                            Vec::new()
                        }
                    };
                    let ws = if addresses.is_empty() {
                        None
                    } else {
                        self.race_ws(dc, &domain, addresses, &mut reasons).await
                    };
                    (ws, reasons)
                }
                .boxed(),
            );
        }
        if self.relay.is_some() {
            attempts.push(
                async {
                    // Prefer a healthy official route; hedge IP-blocked networks
                    // without waiting for the full direct connection timeout.
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    let mut reasons = Vec::new();
                    let ws = self.connect_relay(dc, &mut reasons).await;
                    (ws, reasons)
                }
                .boxed(),
            );
        }
        // A cold relay directory has a separate bounded five-second bootstrap.
        // No directory client or traffic exists when the feature is disabled.
        let deadline = self.timeout
            + if self.relay.is_some() {
                Duration::from_secs(5)
            } else {
                Duration::ZERO
            };
        let wss = timeout(deadline, async {
            while let Some((ws, reasons)) = attempts.next().await {
                failures.extend(reasons);
                if ws.is_some() {
                    return ws;
                }
            }
            None
        })
        .await;
        // Cancel losing futures now, before attempting TCP fallback.
        drop(attempts);
        if let Ok(Some(ws)) = wss {
            self.failure_logs.lock().await.remove(&dc);
            return Ok(Upstream::WebSocket(Box::new(ws)));
        } else if wss.is_err() {
            failures.push("WSS connection deadline exceeded".into());
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
        Err(UpstreamUnavailable(dc).into())
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
pub(super) struct UpstreamUnavailable(pub i16);
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
    ready: tokio::sync::mpsc::Sender<i16>,
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
            let _ = ready.try_send(handshake.dc);
            bridge_ws(client, *ws, handshake, encrypt, decrypt).await
        }
        Upstream::Tcp(mut upstream) => {
            timeout(routes.timeout, upstream.write_all(&init)).await??;
            let _ = ready.try_send(handshake.dc);
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
