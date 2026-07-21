use mqtt_codec::*;

// ---- encoders: byte-exact / round-trip -------------------------------------

#[test]
fn connect_persistent_session_flags() {
    let mut out = [0u8; 64];
    let n = encode_connect(&mut out, b"cid", b"user", b"pass").unwrap();
    assert_eq!(out[0], 0x10, "CONNECT packet type");
    // variable header: [rl][00 04 M Q T T][04 level][flags] → flags at index 9
    // (1 type + 1 remaining-len byte + 6 proto-name field + 1 level).
    assert_eq!(out[9], 0xC0, "flags = username|password, clean-session CLEARED (#101 persistent)");
    assert!(n >= 12);
}

#[test]
fn publish_qos0_roundtrips_through_parse() {
    let mut out = [0u8; 64];
    let n = encode_publish(&mut out, b"watch/x", b"hello", false).unwrap();
    assert_eq!(out[0], 0x30, "PUBLISH QoS0, retain clear");
    let (inc, total) = parse_packet(&out[..n]).unwrap();
    assert_eq!(total, n);
    match inc {
        Incoming::Publish { topic, payload, packet_id } => {
            assert_eq!(topic, b"watch/x");
            assert_eq!(payload, b"hello");
            assert_eq!(packet_id, None, "QoS0 has no packet id");
        }
        _ => panic!("expected Publish"),
    }
}

#[test]
fn publish_retain_flag_sets_bit0() {
    let mut out = [0u8; 32];
    let n = encode_publish(&mut out, b"t", b"", true).unwrap();
    assert_eq!(out[0], 0x31, "retain sets bit0");
    assert!(parse_packet(&out[..n]).is_some());
}

#[test]
fn subscribe_qos0_vs_qos1_trailing_byte() {
    let mut a = [0u8; 32];
    let na = encode_subscribe(&mut a, 1, b"cmd/x").unwrap();
    assert_eq!(a[0], 0x82, "SUBSCRIBE reserved low-nibble 0b0010");
    assert_eq!(a[na - 1], 0x00, "requested QoS 0");

    let mut b = [0u8; 32];
    let nb = encode_subscribe_qos1(&mut b, 1, b"cmd/x").unwrap();
    assert_eq!(b[0], 0x82);
    assert_eq!(b[nb - 1], 0x01, "#101 requested QoS 1 for command topics");
    // packet id big-endian right after the remaining-length byte.
    assert_eq!((a[2], a[3]), (0x00, 0x01));
}

#[test]
fn puback_is_exactly_four_bytes() {
    let mut out = [0u8; 8];
    let n = encode_puback(&mut out, 0x1234).unwrap();
    assert_eq!(n, 4);
    assert_eq!(&out[..4], &[0x40, 0x02, 0x12, 0x34], "#101 PUBACK echoes the packet id");
}

#[test]
fn disconnect_is_two_bytes() {
    let mut out = [0u8; 4];
    let n = encode_disconnect(&mut out).unwrap();
    assert_eq!(&out[..n], &[0xE0, 0x00]);
}

#[test]
fn encoders_fail_closed_on_short_buffer() {
    // A buffer too small must yield None (never a truncated packet).
    let mut tiny = [0u8; 3];
    assert_eq!(encode_publish(&mut tiny, b"topic-too-long", b"x", false), None);
    assert_eq!(encode_puback(&mut [0u8; 3], 1), None);
}

// ---- inbound parse ---------------------------------------------------------

#[test]
fn parse_connack_return_code() {
    assert!(matches!(
        parse_packet(&[0x20, 0x02, 0x00, 0x00]).unwrap().0,
        Incoming::ConnAck { return_code: 0 }
    ));
    assert!(matches!(
        parse_packet(&[0x20, 0x02, 0x00, 0x05]).unwrap().0,
        Incoming::ConnAck { return_code: 5 } // not authorized
    ));
}

#[test]
fn parse_suback() {
    let (inc, total) = parse_packet(&[0x90, 0x03, 0x00, 0x01, 0x00]).unwrap();
    assert!(matches!(inc, Incoming::SubAck));
    assert_eq!(total, 5);
}

#[test]
fn parse_publish_qos1_captures_packet_id() {
    // header 0x32 = PUBLISH | QoS1; topic "a"; packet id 7; payload "hi".
    let frame = [0x32, 0x07, 0x00, 0x01, b'a', 0x00, 0x07, b'h', b'i'];
    let (inc, total) = parse_packet(&frame).unwrap();
    assert_eq!(total, frame.len());
    match inc {
        Incoming::Publish { topic, payload, packet_id } => {
            assert_eq!(topic, b"a");
            assert_eq!(payload, b"hi");
            assert_eq!(packet_id, Some(7), "#101 QoS1 packet id captured for PUBACK");
        }
        _ => panic!("expected Publish"),
    }
}

#[test]
fn parse_incomplete_packet_returns_none() {
    // A PUBLISH claiming remaining=10 but only 3 bytes present.
    assert!(parse_packet(&[0x30, 0x0A, 0x00]).is_none());
    assert!(parse_packet(&[0x20]).is_none()); // < 2 bytes
}

#[test]
fn parse_stream_advances_across_two_packets() {
    // CONNACK then SUBACK back-to-back in one accumulator.
    let buf = [0x20, 0x02, 0x00, 0x00, 0x90, 0x03, 0x00, 0x01, 0x00];
    let (a, ta) = parse_packet(&buf).unwrap();
    assert!(matches!(a, Incoming::ConnAck { return_code: 0 }));
    assert_eq!(ta, 4);
    let (b, tb) = parse_packet(&buf[ta..]).unwrap();
    assert!(matches!(b, Incoming::SubAck));
    assert_eq!(tb, 5);
}

// ---- multi-byte remaining-length varint ------------------------------------

#[test]
fn large_publish_uses_two_byte_varint_and_roundtrips() {
    let payload = [0xABu8; 200]; // remaining > 127 → 2-byte varint
    let mut out = [0u8; 256];
    let n = encode_publish(&mut out, b"t", &payload, false).unwrap();
    // remaining = 2 + 1(topic) + 200 = 203 → varint [0xCB, 0x01].
    assert_eq!((out[1], out[2]), (0xCB, 0x01), "2-byte remaining-length varint");
    let (inc, total) = parse_packet(&out[..n]).unwrap();
    assert_eq!(total, n);
    match inc {
        Incoming::Publish { topic, payload: p, packet_id } => {
            assert_eq!(topic, b"t");
            assert_eq!(p.len(), 200);
            assert_eq!(p, &payload[..]);
            assert_eq!(packet_id, None);
        }
        _ => panic!("expected Publish"),
    }
}

// ---- ADVERSARIAL: never panic on hostile/truncated input -------------------

#[test]
fn hostile_input_never_panics() {
    // empty / single byte
    assert!(parse_packet(&[]).is_none());
    assert!(parse_packet(&[0x30]).is_none());
    // malformed varint: 4 continuation bytes with high bit set = protocol violation
    assert!(parse_packet(&[0x30, 0x80, 0x80, 0x80, 0x80, 0x00]).is_none());
    // PUBLISH whose declared topic length runs past the body → Other, consumed, no panic.
    // header 0x30, remaining 4, topic-len 0xFFFF (way past), then 2 bytes.
    let f = [0x30, 0x04, 0xFF, 0xFF, 0x00, 0x00];
    match parse_packet(&f) {
        Some((Incoming::Other, total)) => assert_eq!(total, 6),
        other => panic!("expected Other/consumed, got {:?}", matches!(other, Some(_))),
    }
    // QoS1 PUBLISH too short to hold the packet id → Other, no panic.
    // header 0x32, remaining 3, topic-len 1, 'a', then only 1 byte (need 2 for pid).
    let g = [0x32, 0x03, 0x00, 0x01, b'a'];
    // remaining=3 but only topic(2+1)=3 present, pid needs 2 more → body too short → Other.
    assert!(matches!(parse_packet(&g), Some((Incoming::Other, 5)) | None));
    // fuzz every truncation length of a valid large publish — none may panic.
    let payload = [0x11u8; 130];
    let mut out = [0u8; 200];
    let n = encode_publish(&mut out, b"topic", &payload, false).unwrap();
    for len in 0..=n {
        let _ = parse_packet(&out[..len]); // must not panic for any prefix
    }
}
