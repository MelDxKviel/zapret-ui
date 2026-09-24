use super::*;
use aes::cipher::{KeyIvInit, StreamCipher};
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    time::{timeout, Duration},
};

fn free_port() -> u16 {
    std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

#[tokio::test]
async fn lazy_start_stop_rebind_and_close_incomplete_clients() {
    let proxy = LocalTelegramProxy::default();
    assert!(!proxy.is_running().await);
    let options = TelegramProxySettings {
        port: free_port(),
        ..Default::default()
    };
    // Merely constructing/preparing the module does not open its port.
    let settings = proxy.prepare_settings(options).unwrap();
    assert!(TcpStream::connect((Ipv4Addr::LOCALHOST, settings.port))
        .await
        .is_err());
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let cb: TelegramStatusCb = Arc::new(move |status| {
        let _ = tx.send(status);
    });
    for _ in 0..3 {
        proxy.start(settings.clone(), cb.clone()).await.unwrap();
        proxy.start(settings.clone(), cb.clone()).await.unwrap(); // idempotent
        let mut client = TcpStream::connect((Ipv4Addr::LOCALHOST, settings.port))
            .await
            .unwrap();
        client.write_all(&[7; 10]).await.unwrap(); // stalled handshake
        timeout(Duration::from_secs(2), async {
            while rx.recv().await.unwrap().connections == 0 {}
        })
        .await
        .unwrap();
        proxy.stop().await.unwrap();
        assert!(!proxy.is_running().await);
        let mut buf = [0; 1];
        let read = timeout(Duration::from_secs(2), client.read(&mut buf))
            .await
            .unwrap();
        assert!(matches!(read, Ok(0) | Err(_)));
        // Stop has joined the listener; its port can be reused immediately.
        let probe = TcpListener::bind((Ipv4Addr::LOCALHOST, settings.port))
            .await
            .unwrap();
        drop(probe);
    }
    proxy.stop().await.unwrap();
}

#[tokio::test]
async fn occupied_port_fails_without_running_task() {
    let blocker = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let proxy = LocalTelegramProxy::default();
    let settings = TelegramProxySettings {
        port: blocker.local_addr().unwrap().port(),
        ..Default::default()
    };
    assert_eq!(
        proxy
            .start(settings, Arc::new(|_| {}))
            .await
            .unwrap_err()
            .to_string(),
        "telegram.error_bind"
    );
    assert!(!proxy.is_running().await);
}

#[cfg(windows)]
#[tokio::test]
async fn stopping_proxy_releases_port_while_spawned_child_is_alive() {
    use std::{path::PathBuf, process::Stdio};

    let proxy = LocalTelegramProxy::default();
    let settings = TelegramProxySettings {
        port: free_port(),
        ..Default::default()
    };
    proxy
        .start(settings.clone(), Arc::new(|_| {}))
        .await
        .unwrap();

    // Windows children may inherit handles when their standard I/O is piped.
    // Keep a harmless child waiting for input, like a long-lived winws process,
    // while stopping the proxy. An inherited listener would keep its port busy.
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

    let stopped = timeout(Duration::from_secs(2), proxy.stop()).await;
    let rebound = TcpListener::bind((Ipv4Addr::LOCALHOST, settings.port)).await;
    let child_still_running = child.try_wait().unwrap().is_none();
    child.kill().await.unwrap();

    stopped.unwrap().unwrap();
    assert!(
        child_still_running,
        "the inheritance probe must remain alive"
    );
    assert!(
        rebound.is_ok(),
        "child retained the proxy listener: {rebound:?}"
    );
}

#[tokio::test]
async fn connection_limit_is_enforced_and_stop_does_not_wait_for_handshake_timeout() {
    let proxy = LocalTelegramProxy::default();
    let settings = TelegramProxySettings {
        port: free_port(),
        max_connections: 1,
        ..Default::default()
    };
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    proxy
        .start(
            settings.clone(),
            Arc::new(move |status| {
                let _ = tx.send(status);
            }),
        )
        .await
        .unwrap();
    let _a = TcpStream::connect((Ipv4Addr::LOCALHOST, settings.port))
        .await
        .unwrap();
    timeout(Duration::from_secs(2), async {
        while rx.recv().await.unwrap().connections == 0 {}
    })
    .await
    .unwrap();
    let _b = TcpStream::connect((Ipv4Addr::LOCALHOST, settings.port))
        .await
        .unwrap();
    timeout(Duration::from_secs(2), proxy.stop())
        .await
        .unwrap()
        .unwrap();
    while let Ok(status) = rx.try_recv() {
        assert!(status.connections <= 1);
    }
}

#[test]
fn settings_normalize_validate_and_never_print_secret() {
    let proxy = LocalTelegramProxy::default();
    let a = proxy.prepare_settings(Default::default()).unwrap();
    let b = proxy.prepare_settings(Default::default()).unwrap();
    assert_ne!(a.secret, b.secret);
    assert_eq!(a.secret.len(), 32);
    assert!(!format!("{a:?}").contains(&a.secret));
    assert!(a
        .link()
        .starts_with("tg://proxy?server=127.0.0.1&port=1443&secret=dd"));
    let mut input = a.clone();
    input.secret = format!(" DD{} ", a.secret.to_ascii_uppercase());
    input.dc_overrides = "4:149.154.167.220; 2:149.154.167.220".into();
    assert_eq!(proxy.prepare_settings(input).unwrap(), a);
    for (field, value) in [
        ("port", "0"),
        ("timeout", "0"),
        ("limit", "257"),
        ("secret", "g"),
        ("dc", "4:1.2.3.4 4:2.3.4.5"),
        ("dc", "6:1.2.3.4"),
        ("dc", "4:0.0.0.0"),
    ] {
        let mut input = b.clone();
        match field {
            "port" => input.port = value.parse().unwrap(),
            "timeout" => input.connect_timeout_secs = value.parse().unwrap(),
            "limit" => input.max_connections = value.parse().unwrap(),
            "secret" => input.secret = value.into(),
            _ => input.dc_overrides = value.into(),
        }
        assert!(proxy.prepare_settings(input).is_err(), "{field}");
    }
    let mut invalid = a;
    invalid.secret = "&injected=1".into();
    assert!(invalid.link().is_empty());
}

/// Opt-in protocol smoke test. Sends an unauthenticated req_pq_multi with a
/// random nonce; requires no Telegram account and never touches client settings.
#[tokio::test]
#[ignore = "requires a reachable Telegram WebSocket endpoint"]
async fn live_telegram_wss_mtproto_roundtrip() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .with_test_writer()
        .try_init();
    timeout(Duration::from_secs(35), async {
        let proxy = LocalTelegramProxy::default();
        let settings = proxy
            .prepare_settings(TelegramProxySettings {
                port: free_port(),
                tcp_fallback: false,
                ..Default::default()
            })
            .unwrap();
        let secret = settings::decode_secret(&settings.secret).unwrap();
        proxy
            .start(
                settings.clone(),
                Arc::new(|s| {
                    if !s.error.is_empty() {
                        eprintln!("{}", s.error);
                    }
                }),
            )
            .await
            .unwrap();
        let mut client = TcpStream::connect((Ipv4Addr::LOCALHOST, settings.port))
            .await
            .unwrap();
        let mut init = [0; 64];
        getrandom::fill(&mut init).unwrap();
        init[56..60].fill(0xdd);
        init[60..62].copy_from_slice(&2i16.to_le_bytes());
        let key = Sha256::digest([&init[8..40], &secret].concat());
        let mut encrypt = protocol::Cipher::new_from_slices(&key, &init[40..56]).unwrap();
        let mut reverse = init[8..56].to_vec();
        reverse.reverse();
        let key = Sha256::digest([&reverse[..32], &secret].concat());
        let mut decrypt = protocol::Cipher::new_from_slices(&key, &reverse[32..]).unwrap();
        let mut wire = init;
        encrypt.apply_keystream(&mut wire);
        init[56..].copy_from_slice(&wire[56..]);
        client.write_all(&init).await.unwrap();

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap();
        let message_id =
            ((now.as_secs() << 32) | ((u64::from(now.subsec_nanos()) << 32) / 1_000_000_000)) & !3;
        let mut nonce = [0; 16];
        getrandom::fill(&mut nonce).unwrap();
        let mut message = Vec::new();
        message.extend_from_slice(&40u32.to_le_bytes()); // padded intermediate, no padding
        message.extend_from_slice(&0u64.to_le_bytes()); // unauthenticated MTProto
        message.extend_from_slice(&message_id.to_le_bytes());
        message.extend_from_slice(&20u32.to_le_bytes());
        message.extend_from_slice(&0xbe7e8ef1u32.to_le_bytes()); // req_pq_multi
        message.extend_from_slice(&nonce);
        encrypt.apply_keystream(&mut message);
        client.write_all(&message).await.unwrap();
        let response =
            protocol::read_packet(&mut client, &mut decrypt, protocol::Transport::Padded)
                .await
                .unwrap()
                .unwrap();
        assert!(response.len() >= 44, "short MTProto response");
        assert_eq!(&response[4..12], &[0; 8]);
        assert_eq!(&response[24..28], &0x05162463u32.to_le_bytes()); // resPQ
        assert_eq!(&response[28..44], &nonce);
        proxy.stop().await.unwrap();
    })
    .await
    .unwrap();
}
