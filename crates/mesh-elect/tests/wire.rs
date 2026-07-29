//! ELECT wire-format tests.
//!
//! The wire format is the one thing that MUST be byte-identical in
//! `esp32c6-watch` and `smol`, because a mismatch does not fail loudly — it
//! partitions the fleet silently, which is the exact bug this whole change
//! exists to fix. So the encoding is pinned here against literal bytes, not just
//! round-tripped: a round-trip test passes happily while both sides agree on the
//! wrong thing.

use mesh_elect::wire::*;
use mesh_elect::{ch_index, N_CHANNELS};

fn sample() -> ElectFrame {
    let mut w = [0u8; N_CHANNELS];
    w[ch_index(1).unwrap()] = 12;
    w[ch_index(6).unwrap()] = 27;
    w[ch_index(11).unwrap()] = 5;
    ElectFrame {
        node_id: 42,
        epoch: 7,
        channel: 6,
        gateway: 3,
        w,
    }
}

/// The golden bytes. If this test has to change, the wire format changed, and
/// BOTH repos must be reflashed together — that is the point of pinning it.
#[test]
fn encoding_is_byte_exact() {
    let mut buf = [0u8; 128];
    let n = encode(&sample(), &mut buf).expect("encodes");
    assert_eq!(n, ELECT_LEN);
    let got = core::str::from_utf8(&buf[..n]).expect("ASCII on the wire");
    // Weight vector is ch1..ch13, two digits each, no separators:
    //   ch1=12 at [0..2], ch6=27 at [10..12], ch11=05 at [20..22], rest 00.
    assert_eq!(
        got,
        "SMOLv1 ELECT 042 0000000007 06 003 12000000002700000000050000",
        "wire format changed — smol must be updated in lockstep"
    );
    assert_eq!(ELECT_LEN, 61);
    assert!(ELECT_LEN <= 250, "must fit the ESP-NOW payload cap");
}

#[test]
fn round_trips() {
    let f = sample();
    let mut buf = [0u8; 128];
    let n = encode(&f, &mut buf).unwrap();
    assert_eq!(parse(&buf[..n]), Some(f));
}

#[test]
fn round_trips_at_field_extremes() {
    let f = ElectFrame {
        node_id: 255,
        epoch: u32::MAX,
        channel: 13,
        gateway: 255,
        w: [48; N_CHANNELS],
    };
    let mut buf = [0u8; 128];
    let n = encode(&f, &mut buf).unwrap();
    assert_eq!(parse(&buf[..n]), Some(f), "u32::MAX epoch fits 10 digits");
}

/// Byte 7 of the tag must not collide with any existing SMOLv1 frame, or old
/// nodes would misparse it instead of ignoring it.
#[test]
fn tag_does_not_collide_with_existing_frames() {
    assert_eq!(ELECT_PREFIX[7], b'E');
    // Every tag in use across both repos today.
    for other in [
        &b"SMOLv1 HELLO "[..],
        b"SMOLv1 ACK ",
        b"SMOLv1 TIME ",
        b"SMOLv1 CFG ",
        b"SMOLv1 RELAY ",
        b"SMOLv1 RELAYACK ",
        b"SMOLv1 PING ",
        b"SMOLv1 PINGACK ",
        b"SMOLv1 SAY ",
        b"SMOLv1 DIAG ",
        b"SMOLv1 BATT ",
        b"SMOLv1 GRID ",
        b"SMOLv1 STAT ",
        b"SMOLv1 SCAN ",
        b"SMOLv1 BEACON ",
    ] {
        assert_ne!(other[7], ELECT_PREFIX[7], "tag collision with {other:?}");
    }
}

/// Fed straight from unauthenticated broadcasts, so every rejection path must
/// hold. A parse that accepts junk is a parse that feeds junk into the election.
#[test]
fn rejects_malformed_frames() {
    let mut buf = [0u8; 128];
    let n = encode(&sample(), &mut buf).unwrap();

    assert_eq!(parse(&[]), None, "empty");
    assert_eq!(parse(b"SMOLv1 HELLO 042"), None, "wrong tag");
    assert_eq!(parse(&buf[..n - 1]), None, "truncated");
    let mut long = [b'0'; 128];
    long[..n].copy_from_slice(&buf[..n]);
    assert_eq!(parse(&long[..n + 1]), None, "trailing junk is not tolerated");

    // Non-digit in each numeric field.
    for pos in [13, 17, 28, 31, 35] {
        let mut bad = buf;
        bad[pos] = b'x';
        assert_eq!(parse(&bad[..n]), None, "non-digit at byte {pos} must reject");
    }
    // Missing separators.
    for pos in [16, 27, 30, 34] {
        let mut bad = buf;
        bad[pos] = b'0';
        assert_eq!(parse(&bad[..n]), None, "missing space at byte {pos}");
    }
}

/// Out-of-range channels must die at the parse boundary — before anything can
/// index an array with them.
#[test]
fn rejects_out_of_range_channel() {
    let mut buf = [0u8; 128];
    let n = encode(&sample(), &mut buf).unwrap();
    for (bad_ch, txt) in [(0u8, b"00"), (14, b"14"), (99, b"99")] {
        let mut bad = buf;
        bad[28..30].copy_from_slice(txt);
        assert_eq!(parse(&bad[..n]), None, "channel {bad_ch} must reject");
    }
    // And encode refuses to emit one.
    let mut f = sample();
    f.channel = 0;
    assert_eq!(encode(&f, &mut buf), None);
    f.channel = 14;
    assert_eq!(encode(&f, &mut buf), None);
}

#[test]
fn encode_refuses_a_short_buffer() {
    let mut small = [0u8; ELECT_LEN - 1];
    assert_eq!(encode(&sample(), &mut small), None);
    let mut exact = [0u8; ELECT_LEN];
    assert_eq!(encode(&sample(), &mut exact), Some(ELECT_LEN));
}

/// Every byte position of a valid frame is ASCII-printable, so an ELECT frame is
/// readable in a serial log without a hex dump — the same courtesy the rest of
/// SMOLv1 extends.
#[test]
fn frame_is_human_readable_in_a_log() {
    let mut buf = [0u8; 128];
    let n = encode(&sample(), &mut buf).unwrap();
    assert!(buf[..n].iter().all(|b| b.is_ascii_graphic() || *b == b' '));
}
