use super::*;

fn hex(value: &str) -> Vec<u8> {
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|p| u8::from_str_radix(std::str::from_utf8(p).unwrap(), 16).unwrap())
        .collect()
}

#[tokio::test]
async fn independent_python_mtproxy_vector() {
    // Generated independently with Python cryptography AES-256-CTR + hashlib.
    // Checks SHA256 key derivation, signed media DC, CTR offsets, both directions.
    let init: [u8; 64] = hex("000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f202122232425262728292a2b2c2d2e2f30313233343536377fb7a7ad45f7ade4").try_into().unwrap();
    let secret = std::array::from_fn(|i| i as u8);
    let mut handshake = parse_init(&init, &secret).unwrap();
    assert_eq!(handshake.dc, -4);
    assert_eq!(handshake.transport, Transport::Padded);
    let data = hex("5edecc6a7b130f29b9254661");
    let packet = read_packet(&mut &data[..], &mut handshake.decrypt, handshake.transport)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(packet, b"\x08\0\0\0abcdefgh");
    let mut reply = b"reply".to_vec();
    handshake.encrypt.apply_keystream(&mut reply);
    assert_eq!(reply, hex("d26ca7e4fd"));
    assert!(parse_init(&init, &[99; 16]).is_err());
}

#[test]
fn relay_init_has_valid_header_and_stream_offsets() {
    for transport in [
        Transport::Abridged,
        Transport::Intermediate,
        Transport::Padded,
    ] {
        let (init, mut encrypt, mut decrypt) = relay_init(-2, transport).unwrap();
        let mut server_decrypt = cipher(&init[8..56], None);
        let mut plain = init;
        server_decrypt.apply_keystream(&mut plain);
        assert_eq!(&plain[60..62], &(-2i16).to_le_bytes());
        let tag = match transport {
            Transport::Abridged => 0xef,
            Transport::Intermediate => 0xee,
            Transport::Padded => 0xdd,
        };
        assert_eq!(&plain[56..60], &[tag; 4]);
        let mut request = vec![7; 257];
        encrypt.apply_keystream(&mut request);
        for chunk in request.chunks_mut(3) {
            server_decrypt.apply_keystream(chunk);
        }
        assert_eq!(request, vec![7; 257]);
        let mut reverse = init[8..56].to_vec();
        reverse.reverse();
        let mut response = b"response".to_vec();
        cipher(&reverse, None).apply_keystream(&mut response);
        decrypt.apply_keystream(&mut response);
        assert_eq!(response, b"response");
    }
}

#[tokio::test]
async fn packet_reader_handles_fragmentation_quickack_and_coalescing() {
    use tokio::io::AsyncWriteExt;
    for (transport, header, payload) in [
        (Transport::Abridged, vec![0x82], vec![7; 8]),
        (Transport::Abridged, vec![0xff, 128, 0, 0], vec![8; 512]),
        (Transport::Intermediate, vec![8, 0, 0, 0x80], vec![9; 8]),
        (Transport::Padded, vec![11, 0, 0, 0x80], vec![10; 11]),
    ] {
        let mut packet = header;
        packet.extend(payload);
        let mut wire = packet.clone();
        wire.extend_from_slice(&packet);
        cipher(&[42; 48], None).apply_keystream(&mut wire);
        let (mut tx, mut rx) = tokio::io::duplex(3);
        let sender = tokio::spawn(async move {
            for chunk in wire.chunks(2) {
                tx.write_all(chunk).await.unwrap();
            }
        });
        let mut decrypt = cipher(&[42; 48], None);
        assert_eq!(
            read_packet(&mut rx, &mut decrypt, transport)
                .await
                .unwrap()
                .unwrap(),
            packet
        );
        assert_eq!(
            read_packet(&mut rx, &mut decrypt, transport)
                .await
                .unwrap()
                .unwrap(),
            packet
        );
        assert!(read_packet(&mut rx, &mut decrypt, transport)
            .await
            .unwrap()
            .is_none());
        sender.await.unwrap();
    }
}

#[tokio::test]
async fn invalid_lengths_and_truncation_are_rejected_before_large_allocation() {
    for len in [0, MAX_PACKET as u32, 0x7fff_ffff] {
        let mut wire = len.to_le_bytes();
        cipher(&[42; 48], None).apply_keystream(&mut wire);
        assert!(read_packet(
            &mut &wire[..],
            &mut cipher(&[42; 48], None),
            Transport::Padded
        )
        .await
        .is_err());
    }
    let mut wire = [8, 0, 0, 0, 1];
    cipher(&[42; 48], None).apply_keystream(&mut wire);
    assert!(read_packet(
        &mut &wire[..],
        &mut cipher(&[42; 48], None),
        Transport::Intermediate
    )
    .await
    .is_err());
}
