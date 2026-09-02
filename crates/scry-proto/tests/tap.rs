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
