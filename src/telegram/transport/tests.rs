use super::*;
use aes::cipher::KeyIvInit;
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::{accept_async, client_async};

async fn pair() -> (TcpStream, TcpStream) {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let (a, b) = tokio::join!(
        TcpStream::connect(listener.local_addr().unwrap()),
        listener.accept()
    );
    (a.unwrap(), b.unwrap().0)
}

fn cipher(seed: u8) -> Cipher {
    Cipher::new_from_slices(&[seed; 32], &[seed; 16]).unwrap()
}

#[test]
fn websocket_routes_keep_regular_and_media_backends_distinct() {
    let routes = Routes::new(&TelegramProxySettings {
        dc_overrides: "2:149.154.167.220 4:149.154.167.220 203:91.105.192.100".into(),
        ..Default::default()
    })
    .unwrap();
    for (dc, domain) in [
        (2, "kws2.web.telegram.org"),
        (-2, "kws2-1.web.telegram.org"),
        (4, "kws4.web.telegram.org"),
        (-4, "kws4-1.web.telegram.org"),
    ] {
        assert_eq!(
            routes.wss_route(dc),
            Some((domain.into(), "149.154.167.220:443".parse().unwrap()))
        );
    }
    for dc in [1, -1, 3, -3, 5, -5, 203, -203] {
        assert_eq!(routes.wss_route(dc), None, "DC{dc}");
    }
}

#[test]
fn websocket_dns_candidates_include_distinct_ipv4_and_ipv6_addresses() {
    let addresses = [
        "149.154.167.220:443",
        "149.154.167.99:443",
        "149.154.167.99:443",
        "[2001:67c:4e8:f002::a]:443",
        "149.154.167.100:443",
        "[2001:67c:4e8:f002::a]:443",
    ]
    .map(|address| address.parse().unwrap());
    assert_eq!(
        alternate_ws_addresses(addresses, "149.154.167.220".parse().unwrap()),
        [addresses[1], addresses[3], addresses[4]]
    );

    let many = (1..=100).map(|last| SocketAddr::from(([10, 0, 0, last], 443)));
    let bounded = alternate_ws_addresses(many, "149.154.167.220".parse().unwrap());
    assert_eq!(bounded.len(), MAX_WSS_ADDRESSES);
    assert_eq!(bounded[0], SocketAddr::from(([10, 0, 0, 1], 443)));
    assert_eq!(bounded[15], SocketAddr::from(([10, 0, 0, 16], 443)));
}

#[test]
fn connection_diagnostics_report_safe_categories_without_upstream_text() {
    let io = std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        "private secret and proxy link must not be logged",
    );
    assert_eq!(io_failure(&io), "I/O PermissionDenied");
    assert_eq!(
        ws_failure(&tokio_tungstenite::tungstenite::Error::Io(io).into()),
        "I/O PermissionDenied"
    );
    let refused = std::io::Error::from_raw_os_error(10061);
    assert!(io_failure(&refused).contains("OS 10061"));

    let response = tokio_tungstenite::tungstenite::http::Response::builder()
        .status(403)
        .header("private", "secret")
        .body(Some(b"private upstream response".to_vec()))
        .unwrap();
    assert_eq!(
        ws_failure(&tokio_tungstenite::tungstenite::Error::Http(Box::new(response)).into()),
        "HTTP 403"
    );
}

#[cfg(windows)]
#[tokio::test]
async fn outbound_connection_closes_while_spawned_child_is_alive() {
    use std::{path::PathBuf, process::Stdio};

    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let address = listener.local_addr().unwrap();
    let (outbound, accepted) = timeout(Duration::from_secs(2), async {
        tokio::join!(connect_tcp(address), listener.accept())
    })
    .await
    .unwrap();
    let mut outbound = outbound.unwrap();
    let (mut server, _) = accepted.unwrap();
    drop(listener);

    timeout(Duration::from_secs(2), async {
        outbound.write_all(b"ping").await.unwrap();
        let mut data = [0; 4];
        server.read_exact(&mut data).await.unwrap();
        assert_eq!(&data, b"ping");
        server.write_all(b"pong").await.unwrap();
        outbound.read_exact(&mut data).await.unwrap();
        assert_eq!(&data, b"pong");
    })
    .await
    .unwrap();

    // Closing the proxy's upstream must produce EOF even if another process
    // was launched while it was connected. The child only waits on its stdin.
    let shell = PathBuf::from(std::env::var_os("SystemRoot").unwrap())
        .join("System32")
        .join("cmd.exe");
    let mut child = tokio::process::Command::new(shell)
        .args(["/D", "/Q", "/C", "set /p proxy_test_wait="])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(0x08000000)
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    assert!(child.try_wait().unwrap().is_none());

    drop(outbound);
    let mut data = [0; 1];
    let read = timeout(Duration::from_secs(2), server.read(&mut data)).await;
    let child_still_running = child.try_wait().unwrap().is_none();
    child.kill().await.unwrap();

    assert!(
        child_still_running,
        "the inheritance probe must remain alive"
    );
    assert!(
        matches!(read, Ok(Ok(0))),
        "child retained upstream socket: {read:?}"
    );
}

#[tokio::test]
async fn websocket_bridge_preserves_packet_boundaries_both_directions_and_pongs() {
    timeout(Duration::from_secs(5), async {
        let (mut desktop, local) = pair().await;
        let (ws_client, ws_server) = pair().await;
        let (ws, remote) = tokio::join!(
            client_async("ws://localhost/apiws", ws_client),
            accept_async(ws_server)
        );
        let (ws, _) = ws.unwrap();
        let mut remote = remote.unwrap();
        let handshake = Handshake {
            dc: -4,
            transport: protocol::Transport::Padded,
            decrypt: cipher(1),
            encrypt: cipher(2),
        };
        let relay = tokio::spawn(bridge_ws(local, ws, handshake, cipher(3), cipher(4)));
        let packet = b"\x08\0\0\x80abcdefgh";
        let mut data = packet.to_vec();
        data.extend(packet);
        cipher(1).apply_keystream(&mut data);
        // Deliberately fragment TCP headers/payloads and coalesce two packets.
        for chunk in data.chunks(3) {
            desktop.write_all(chunk).await.unwrap();
        }
        let mut dc_decrypt = cipher(3);
        for _ in 0..2 {
            let Message::Binary(bytes) = remote.next().await.unwrap().unwrap() else {
                panic!("expected binary packet")
            };
            let mut decoded = bytes.to_vec();
            dc_decrypt.apply_keystream(&mut decoded);
            assert_eq!(&decoded, packet);
        }
        remote
            .send(Message::Ping(b"ping".to_vec().into()))
            .await
            .unwrap();
        assert_eq!(
            remote.next().await.unwrap().unwrap(),
            Message::Pong(b"ping".to_vec().into())
        );
        // Telegram can split a reply across WS messages; CTR state must persist.
        let mut response = b"\x08\0\0\0response".to_vec();
        cipher(4).apply_keystream(&mut response);
        for chunk in response.chunks(3) {
            remote
                .send(Message::Binary(chunk.to_vec().into()))
                .await
                .unwrap();
        }
        let mut received = [0; 12];
        desktop.read_exact(&mut received).await.unwrap();
        cipher(2).apply_keystream(&mut received);
        assert_eq!(&received, b"\x08\0\0\0response");
        remote.close(None).await.unwrap();
        relay.await.unwrap().unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn tcp_fallback_bridge_preserves_stream_and_closes_on_eof() {
    timeout(Duration::from_secs(5), async {
        let (mut desktop, local) = pair().await;
        let (upstream, mut remote) = pair().await;
        let handshake = Handshake {
            dc: 2,
            transport: protocol::Transport::Intermediate,
            decrypt: cipher(1),
            encrypt: cipher(2),
        };
        let relay = tokio::spawn(bridge_tcp(local, upstream, handshake, cipher(3), cipher(4)));
        let mut input = vec![0x77; 70000];
        cipher(1).apply_keystream(&mut input);
        let sender = tokio::spawn(async move {
            desktop.write_all(&input).await.unwrap();
            desktop
        });
        let mut got = vec![0; 70000];
        remote.read_exact(&mut got).await.unwrap();
        cipher(3).apply_keystream(&mut got);
        assert_eq!(got, vec![0x77; 70000]);
        let mut desktop = sender.await.unwrap();
        let mut reply = *b"ok";
        cipher(4).apply_keystream(&mut reply);
        remote.write_all(&reply).await.unwrap();
        desktop.read_exact(&mut reply).await.unwrap();
        cipher(2).apply_keystream(&mut reply);
        assert_eq!(&reply, b"ok");
        // Exercise graceful EOF, rather than Winsock's implicit shutdown on
        // socket close, which may reset the connection under scheduling load.
        remote.shutdown().await.unwrap();
        let mut eof = [0; 1];
        assert_eq!(desktop.read(&mut eof).await.unwrap(), 0);
        assert_eq!(remote.read(&mut eof).await.unwrap(), 0);
        relay.await.unwrap().unwrap();
    })
    .await
    .unwrap();
}
