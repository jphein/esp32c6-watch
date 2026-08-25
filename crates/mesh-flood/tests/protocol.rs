//! Host tests for the vendored multihop layer. smol verifies these modules in
//! its own scratch harnesses (flood_verify, mac_verify); the vendored copy
//! carries its own so a drifted re-vendor cannot pass silently.

use mesh_flood::flood::{forward_decision, ForwardAction, HopLatch, SeenSet, MAX_HOP, SEEN_RING};
use mesh_flood::wire::{
    encode_relay, encode_relayack2, encode_up2, hmac_sha256, parse_relay, parse_relayack2,
    parse_up2,
};

#[test]
fn seen_set_dedups_and_overflows_oldest_first() {
    let mut s = SeenSet::new();
    assert!(!s.seen_or_insert(7, 100, 0), "first sighting is new");
    assert!(s.seen_or_insert(7, 100, 0), "second sighting is a dup");
    assert!(!s.seen_or_insert(7, 100, 1), "fragments are independent keys");
    for i in 0..SEEN_RING as u16 {
        s.insert(9, 200 + i, 0);
    }
    assert!(
        !s.seen_or_insert(7, 100, 0),
        "oldest entry evicted after SEEN_RING inserts"
    );
}

#[test]
fn forward_decision_matrix() {
    assert_eq!(forward_decision(true, MAX_HOP, true), ForwardAction::DedupDrop);
    assert_eq!(forward_decision(true, MAX_HOP, false), ForwardAction::Reassemble);
    assert_eq!(forward_decision(true, 1, false), ForwardAction::Reassemble);
    assert_eq!(
        forward_decision(false, 2, false),
        ForwardAction::Forward { hop: 1 }
    );
    assert_eq!(forward_decision(false, 1, false), ForwardAction::TtlDrop);
}

#[test]
fn hop_latch_escalates_and_holds() {
    let mut l = HopLatch::new();
    assert!(!l.latched());
    assert_eq!(l.origin_hop(false), 1, "unlatched leaf sends single-hop");
    l.on_relay_exhausted(false);
    l.on_relay_exhausted(false);
    assert!(!l.latched(), "two strikes is not enough");
    l.on_relay_exhausted(false);
    assert!(l.latched(), "third strike latches");
    assert_eq!(l.origin_hop(false), MAX_HOP, "latched leaf emits the full budget");
    assert_eq!(l.origin_hop(true), 1, "a probe is always single-hop");
}

#[test]
fn relay_roundtrip() {
    let mut buf = [0u8; 128];
    let chunk = b"telemetry-bytes";
    let n = encode_relay(42, 31337 % 32768, 1, 3, chunk, &mut buf);
    assert!(n > 0);
    let (src, msgid, frag, count, payload) =
        parse_relay(&buf[..n]).expect("own encoding parses");
    assert_eq!((src, frag, count), (42, 1, 3));
    assert_eq!(msgid, 31337 % 32768);
    assert_eq!(payload, chunk);
}

#[test]
fn up2_envelope_roundtrip_wraps_any_inner() {
    let mut inner = [0u8; 64];
    let inner_len = encode_relay(7, 555, 0, 1, b"stranded", &mut inner);
    let mut buf = [0u8; 160];
    let n = encode_up2(7, 999, MAX_HOP, &inner[..inner_len], &mut buf);
    assert!(n > 0);
    let (origin, env_msgid, hop, wrapped) = parse_up2(&buf[..n]).expect("parses");
    assert_eq!((origin, env_msgid, hop), (7, 999, MAX_HOP));
    assert_eq!(wrapped, &inner[..inner_len], "inner frame rides verbatim");
    let mut fwd = [0u8; 160];
    let m = encode_up2(origin, env_msgid, hop - 1, wrapped, &mut fwd);
    let (_, _, hop2, wrapped2) = parse_up2(&fwd[..m]).unwrap();
    assert_eq!(hop2, hop - 1);
    assert_eq!(wrapped2, wrapped);
}

#[test]
fn relayack2_roundtrip() {
    let mut buf = [0u8; 64];
    let n = encode_relayack2(42, 555, 0b0000_0111, MAX_HOP, &mut buf);
    let (target, msgid, bitmap, hop) = parse_relayack2(&buf[..n]).expect("parses");
    assert_eq!((target, msgid, bitmap, hop), (42, 555, 0b0000_0111, MAX_HOP));
}

#[test]
fn hmac_sha256_rfc4231_case2() {
    let tag = hmac_sha256(b"Jefe", b"what do ya want for nothing?");
    let expected: [u8; 32] = [
        0x5b, 0xdc, 0xc1, 0x46, 0xbf, 0x60, 0x75, 0x4e, 0x6a, 0x04, 0x24, 0x26, 0x08, 0x95,
        0x75, 0xc7, 0x5a, 0x00, 0x3f, 0x08, 0x9d, 0x27, 0x39, 0x83, 0x9d, 0xec, 0x58, 0xb9,
        0x64, 0xec, 0x38, 0x43,
    ];
    assert_eq!(tag, expected);
}

#[test]
fn etx_cost_tracks_delivery_not_strength() {
    use mesh_flood::etx::LinkQuality;
    let mut good = LinkQuality::default();
    let mut flaky = LinkQuality::default();
    for i in 0..64 {
        good.tick(true);
        flaky.tick(i % 3 != 0); // ~2/3 delivery
    }
    assert!(
        good.cost() < flaky.cost(),
        "a reliable link costs less than a flaky one ({} vs {})",
        good.cost(),
        flaky.cost()
    );
}

#[test]
fn cfgsched_round_robin_never_starves_the_tail() {
    use mesh_flood::cfgsched::{RelayCursor, CFG_RELAY_MAX_BURST};
    let mut c = RelayCursor::new();
    let cache_len = 7; // more entries than one burst
    let mut hit = [0u32; 7];
    for _ in 0..16 {
        let mut out = [0usize; CFG_RELAY_MAX_BURST];
        let n = c.take(cache_len, &mut out);
        for &idx in &out[..n] {
            hit[idx] += 1;
        }
    }
    let (min, max) = (hit.iter().min().unwrap(), hit.iter().max().unwrap());
    assert!(*min > 0, "every entry was relayed at least once (no tail starvation)");
    assert!(max - min <= 1, "round-robin stays fair: {hit:?}");
}
