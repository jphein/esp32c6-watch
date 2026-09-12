use scry_proto::{be_row_to_pixels, parse_tap_host, TapHost};

#[test]
fn bound_host() {
    match parse_tap_host("HTTP/1.0 200 OK\r\n\r\n{\"host\":\"game\",\"imbued\":false}") {
        TapHost::Bound(h) => assert_eq!(h.as_str(), "game"),
        other => panic!("want Bound(game), got {other:?}"),
    }
}

#[test]
fn bound_host_spaced() {
    // Tolerate whitespace the server might emit: `"host" : "gatekeeper"`.
    match parse_tap_host("{\"host\" : \"gatekeeper\"}") {
        TapHost::Bound(h) => assert_eq!(h.as_str(), "gatekeeper"),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn unbound_sigil() {
    assert_eq!(parse_tap_host("{\"host\":null,\"uid\":\"AA:BB\"}"), TapHost::Unbound);
}

#[test]
fn no_host_field_rejected() {
    assert!(matches!(parse_tap_host("{\"error\":\"bad token\"}"), TapHost::Rejected(_)));
    assert!(matches!(parse_tap_host(""), TapHost::Rejected(_)));
}

#[test]
fn truncated_body_rejected_not_panicked() {
    // A body cut off mid-value must not panic and must not yield a host.
    assert!(matches!(parse_tap_host("{\"host\":\"gam"), TapHost::Rejected(_)));
    assert!(matches!(parse_tap_host("{\"host\":"), TapHost::Rejected(_)));
}

#[test]
fn empty_host_rejected() {
    assert!(matches!(parse_tap_host("{\"host\":\"\"}"), TapHost::Rejected(_)));
}

#[test]
fn overlong_host_rejected() {
    let long = "x".repeat(64);
    let body = format!("{{\"host\":\"{long}\"}}");
    assert!(matches!(parse_tap_host(&body), TapHost::Rejected(_)));
}

#[test]
fn be_pixels_roundtrip() {
    // 0x1234 big-endian = bytes [0x12,0x34].
    let wire = [0x12u8, 0x34, 0xAB, 0xCD];
    let mut out = [0u16; 2];
    be_row_to_pixels(&wire, &mut out);
    assert_eq!(out, [0x1234, 0xABCD]);
}

#[test]
fn be_pixels_short_wire_stops_early() {
    // A truncated strip fills only the pixels it has bytes for; the rest stay 0.
    let wire = [0x12u8, 0x34]; // one pixel of bytes
    let mut out = [0xFFFFu16; 3];
    be_row_to_pixels(&wire, &mut out);
    assert_eq!(out[0], 0x1234);
    assert_eq!(out[1], 0xFFFF); // untouched — no fabricated pixel
}

// ---- the inscribe rite --------------------------------------------------

#[test]
fn inscribe_url_peeled() {
    let body = "{\"host\": \"kaiken\", \"imbued\": true, \"inscribe\": \"https://scry.realm.watch/kaiken?k=bf745a5c0ed3\"}";
    assert_eq!(
        scry_proto::parse_tap_inscribe(body).unwrap().as_str(),
        "https://scry.realm.watch/kaiken?k=bf745a5c0ed3"
    );
}

#[test]
fn inscribe_absent_null_or_junk_is_none() {
    assert!(scry_proto::parse_tap_inscribe("{\"host\":\"game\"}").is_none());
    assert!(scry_proto::parse_tap_inscribe("{\"inscribe\":null}").is_none());
    assert!(scry_proto::parse_tap_inscribe("{\"inscribe\":\"https://x").is_none());
    assert!(scry_proto::parse_tap_inscribe("{\"inscribe\":\"javascript:alert(1)\"}").is_none());
    let long = format!("{{\"inscribe\":\"https://{}\"}}", "a".repeat(200));
    assert!(scry_proto::parse_tap_inscribe(&long).is_none());
}

#[test]
fn crc_a_vectors() {
    assert_eq!(scry_proto::crc_a(&[0x50, 0x00]), [0x57, 0xCD]); // HLTA frame
    assert_eq!(scry_proto::crc_a(&[0x00, 0x00]), [0xA0, 0x1E]);
    assert_eq!(scry_proto::crc_a(&[0x12, 0x34]), [0x26, 0xCF]);
}

#[test]
fn ndef_uri_tlv_kaiken() {
    let mut out = [0u8; scry_proto::NDEF_CAP];
    let n = scry_proto::ndef_uri_tlv("https://scry.realm.watch/kaiken?k=bf745a5c0ed3", &mut out).unwrap();
    assert_eq!(n, 48); // 12 pages
    let expect = b"\x03\x2b\xd1\x01\x27\x55\x04scry.realm.watch/kaiken?k=bf745a5c0ed3\xfe\x00\x00";
    assert_eq!(&out[..n], &expect[..]);
}

#[test]
fn ndef_uri_tlv_prefixes_and_overflow() {
    let mut out = [0u8; scry_proto::NDEF_CAP];
    let n = scry_proto::ndef_uri_tlv("https://www.example.org/", &mut out).unwrap();
    assert_eq!(out[6], 0x02);
    assert_eq!(&out[7..7 + "example.org/".len()], b"example.org/");
    assert_eq!(out[7 + "example.org/".len()], 0xFE);
    assert_eq!(n % 4, 0);
    let n = scry_proto::ndef_uri_tlv("ftp://x", &mut out).unwrap();
    assert_eq!(out[6], 0x00);
    assert_eq!(&out[7..n], b"ftp://x\xfe\x00");
    let long = format!("https://{}", "a".repeat(140));
    assert!(scry_proto::ndef_uri_tlv(&long, &mut out).is_none());
}

#[test]
fn t2t_capacity_from_cc() {
    assert_eq!(scry_proto::t2t_capacity(&[0xE1, 0x10, 0x12, 0x00]), Some(144)); // NTAG213
    assert_eq!(scry_proto::t2t_capacity(&[0xE1, 0x10, 0x3E, 0x00]), Some(496)); // NTAG215
    assert_eq!(scry_proto::t2t_capacity(&[0x00, 0x00, 0x00, 0x00]), None); // blank CC: refuse
    assert_eq!(scry_proto::t2t_capacity(&[0x00, 0x00, 0x00, 0xBD]), None); // MIFARE Classic trailer-ish
}
