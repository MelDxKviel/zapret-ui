use super::*;
use aes::cipher::KeyIvInit;
use tokio::net::TcpListener;
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
        drop(remote);
        relay.await.unwrap().unwrap();
    })
    .await
    .unwrap();
}
