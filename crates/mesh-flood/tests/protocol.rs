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

// ---- multi-node COMPOSITION (de-risks the multi-node bench window) ----------
// The unit tests above cover each piece; these drive the pieces AGAINST each
// other, the way three real nodes would: a stranded leaf escalates and emits
// UP2, a relay forwards it, a second relay dedups it, and the gateway
// reassembles. If the pieces compose wrong (hop math, dedup key, envelope
// re-encode), it surfaces here in software instead of on the bench.

#[test]
fn stranded_leaf_reaches_gateway_through_one_relay() {
    use mesh_flood::flood::{forward_decision, ForwardAction, HopLatch, SeenSet, MAX_HOP};
    use mesh_flood::wire::{encode_relay, encode_up2, parse_up2};

    // LEAF: three unacked relays -> latch -> emits at MAX_HOP.
    let mut leaf = HopLatch::new();
    for _ in 0..3 {
        leaf.on_relay_exhausted(false);
    }
    assert!(leaf.latched());
    let hop = leaf.origin_hop(false);
    assert_eq!(hop, MAX_HOP);

    // Leaf wraps its telemetry RELAY fragment in a UP2 envelope at that hop.
    let origin = 176u8; // arcane-beacon
    let env_msgid = 4242u16;
    let mut inner = [0u8; 96];
    let ilen = encode_relay(origin, 7, 0, 1, b"telemetry from the drawer", &mut inner);
    let mut frame = [0u8; 250];
    let flen = encode_up2(origin, env_msgid, hop, &inner[..ilen], &mut frame);

    // RELAY node (not the gateway, hops left): forwards at hop-1.
    let mut relay = SeenSet::new();
    let (r_origin, r_msgid, r_hop, r_inner) = parse_up2(&frame[..flen]).unwrap();
    assert_eq!((r_origin, r_msgid, r_hop), (origin, env_msgid, MAX_HOP));
    let seen = relay.seen_or_insert(r_origin, r_msgid, 0);
    let fwd = match forward_decision(false, r_hop, seen) {
        ForwardAction::Forward { hop } => hop,
        other => panic!("relay should forward, got {other:?}"),
    };
    assert_eq!(fwd, MAX_HOP - 1);
    let mut fwd_frame = [0u8; 250];
    let fwlen = encode_up2(r_origin, r_msgid, fwd, r_inner, &mut fwd_frame);

    // A SECOND relay that already saw this envelope drops it (loop guard).
    assert_eq!(
        forward_decision(false, fwd, relay.seen_or_insert(r_origin, r_msgid, 0)),
        ForwardAction::DedupDrop
    );

    // GATEWAY hears the forwarded frame: reassembles, never re-forwards.
    let (g_origin, _, g_hop, g_inner) = parse_up2(&fwd_frame[..fwlen]).unwrap();
    assert_eq!(g_origin, origin);
    assert_eq!(g_hop, 1, "one hop consumed reaching the gateway");
    assert_eq!(
        forward_decision(true, g_hop, false),
        ForwardAction::Reassemble
    );
    assert_eq!(g_inner, &inner[..ilen], "the leaf's fragment arrived intact");
}

#[test]
fn ttl_exhausts_before_an_unreachable_gateway() {
    // A leaf two relays from a gateway on a MAX_HOP=2 mesh: the frame dies at
    // the second relay (hop budget spent) rather than looping forever — the
    // property that makes managed flood terminate.
    use mesh_flood::flood::{forward_decision, ForwardAction, SeenSet};
    use mesh_flood::wire::{encode_up2, parse_up2};
    let mut frame = [0u8; 64];
    let n = encode_up2(9, 1, 2, b"x", &mut frame);
    // relay 1: 2 -> 1
    let (_, _, h1, inner) = parse_up2(&frame[..n]).unwrap();
    let f1 = match forward_decision(false, h1, SeenSet::new().seen_or_insert(9, 1, 0)) {
        ForwardAction::Forward { hop } => hop,
        o => panic!("{o:?}"),
    };
    let mut frame2 = [0u8; 64];
    let n2 = encode_up2(9, 1, f1, inner, &mut frame2);
    // relay 2: hop is now 1 -> TtlDrop (never reaches the gateway)
    let (_, _, h2, _) = parse_up2(&frame2[..n2]).unwrap();
    assert_eq!(h2, 1);
    assert_eq!(
        forward_decision(false, h2, false),
        ForwardAction::TtlDrop,
        "budget spent — the flood terminates instead of looping"
    );
}
