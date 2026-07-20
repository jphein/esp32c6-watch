use ed25519_compact::{KeyPair, Seed};
use ota_proto::*;

fn keypair() -> KeyPair {
    // Deterministic (no RNG) — fixed seed -> fixed keypair for reproducible tests.
    KeyPair::from_seed(Seed::from_slice(&[7u8; 32]).unwrap())
}

// ---- frame round-trips ----------------------------------------------------

#[test]
fn otam_round_trip() {
    let m = b"12345|100000|".to_vec();
    let sig = [0x5au8; 64];
    let mut out = [0u8; OTAM_PREFIX.len() + 3 + 2 + 1 + 96 + 64];
    let n = encode_otam(42, 0x1234, &m, &sig, &mut out).unwrap();
    match parse_ota_frame(&out[..n]).unwrap() {
        OtaFrame::Meta { target, session, m: mm, sig: ss } => {
            assert_eq!(target, 42);
            assert_eq!(session, 0x1234);
            assert_eq!(mm, &m[..]);
            assert_eq!(ss, &sig);
        }
        _ => panic!("expected Meta"),
    }
}

#[test]
fn otad_round_trip() {
    let payload = [0xABu8; CHUNK_PAYLOAD];
    let mut out = [0u8; OTAD_PREFIX.len() + 3 + 2 + 2 + CHUNK_PAYLOAD];
    let n = encode_otad(7, 9, 1000, &payload, &mut out).unwrap();
    match parse_ota_frame(&out[..n]).unwrap() {
        OtaFrame::Data { target, session, seq, payload: p } => {
            assert_eq!((target, session, seq), (7, 9, 1000));
            assert_eq!(p, &payload[..]);
        }
        _ => panic!("expected Data"),
    }
}

#[test]
fn otan_round_trip() {
    let bitmap = [0x01u8, 0x00, 0x80, 0, 0, 0, 0, 0];
    let mut out = [0u8; OTAN_PREFIX.len() + 3 + 2 + 2 + OTAN_BITMAP_BYTES];
    let n = encode_otan(3, 5, 128, &bitmap, &mut out).unwrap();
    match parse_ota_frame(&out[..n]).unwrap() {
        OtaFrame::Nak { origin, session, window_base, bitmap: b } => {
            assert_eq!((origin, session, window_base), (3, 5, 128));
            assert_eq!(b, &bitmap[..]);
        }
        _ => panic!("expected Nak"),
    }
}

// ---- ADVERSARIAL: malformed frames must return None, never panic ----------

#[test]
fn hostile_frames_never_panic() {
    // Empty / garbage / wrong prefix.
    assert!(parse_ota_frame(&[]).is_none());
    assert!(parse_ota_frame(b"not a frame").is_none());
    assert!(parse_ota_frame(b"SMOLv1 XXXX ").is_none());
    // Prefixes with no body.
    assert!(parse_ota_frame(OTAM_PREFIX).is_none());
    assert!(parse_ota_frame(OTAD_PREFIX).is_none());
    assert!(parse_ota_frame(OTAN_PREFIX).is_none());
    // OTAM with a hostile M_len (> SIGNED_MSG_MAX) — must not over-read.
    let mut f = OTAM_PREFIX.to_vec();
    f.extend_from_slice(b"042"); // target
    f.extend_from_slice(&[0, 0]); // session
    f.push(255); // M_len way over cap
    f.extend_from_slice(&[0u8; 10]); // far too little to satisfy 255 + 64
    assert!(parse_ota_frame(&f).is_none());
    // OTAM claiming m_len=10 but frame too short for m+sig.
    let mut g = OTAM_PREFIX.to_vec();
    g.extend_from_slice(b"042");
    g.extend_from_slice(&[0, 0]);
    g.push(10);
    g.extend_from_slice(&[b'x'; 5]); // only 5 of 10 + no sig
    assert!(parse_ota_frame(&g).is_none());
    // OTAD payload longer than CHUNK_PAYLOAD.
    let mut d = OTAD_PREFIX.to_vec();
    d.extend_from_slice(b"042");
    d.extend_from_slice(&[0, 0, 0, 0]); // session + seq
    d.extend_from_slice(&[0u8; CHUNK_PAYLOAD + 1]);
    assert!(parse_ota_frame(&d).is_none());
    // Non-ASCII-digit id field.
    let mut bad_id = OTAD_PREFIX.to_vec();
    bad_id.extend_from_slice(b"abc");
    bad_id.extend_from_slice(&[0, 0, 0, 0]);
    assert!(parse_ota_frame(&bad_id).is_none());
}

#[test]
fn fuzz_every_prefix_length_is_safe() {
    // A valid OTAM, then feed every truncation length — none may panic.
    let sig = [1u8; 64];
    let mut out = [0u8; 256];
    let n = encode_otam(1, 1, b"1|2|", &sig, &mut out).unwrap();
    for len in 0..=n + 4 {
        let slice = &out[..len.min(out.len())];
        let _ = parse_ota_frame(slice); // must not panic for any prefix
    }
}

// ---- manifest -------------------------------------------------------------

#[test]
fn announce_from_signed_parses_and_rejects() {
    let sha_hex = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
    let mut m = alloc_string("999|123456|");
    m.push_str(sha_hex);
    let a = Announce::from_signed(m.as_bytes(), &[0u8; 64]).unwrap();
    assert_eq!(a.build, 999);
    assert_eq!(a.size, 123456);
    assert_eq!(a.signed_msg(), m.as_bytes());

    // malformed: missing field, bad hex length, non-numeric, over-cap, empty
    assert!(Announce::from_signed(b"999|123456", &[0; 64]).is_none()); // no sha field
    assert!(Announce::from_signed(b"999|123456|deadbeef", &[0; 64]).is_none()); // sha too short
    assert!(Announce::from_signed(b"x|1|00", &[0; 64]).is_none()); // bad build
    assert!(Announce::from_signed(b"", &[0; 64]).is_none());
    assert!(Announce::from_signed(&[b'a'; SIGNED_MSG_MAX + 1], &[0; 64]).is_none()); // over cap
}

// ---- ed25519 verify -------------------------------------------------------

#[test]
fn verify_accepts_valid_and_rejects_tampered() {
    let kp = keypair();
    let pk: [u8; 32] = *kp.pk;
    let msg = b"1000|200000|abc";
    let sig: [u8; 64] = *kp.sk.sign(msg, None);

    assert!(verify_signature_with(&pk, msg, &sig)); // genuine
    assert!(!verify_signature_with(&pk, b"1001|200000|abc", &sig)); // tampered msg
    let mut bad = sig;
    bad[0] ^= 0xff;
    assert!(!verify_signature_with(&pk, msg, &bad)); // tampered sig
    // Default fleet key can't verify a random sig — fail closed (no panic).
    assert!(!verify_signature(msg, &[0u8; 64]));
    assert!(!verify_signature_with(&[0u8; 32], msg, &sig)); // junk key
}

// ---- anti-rollback gate ---------------------------------------------------

#[test]
fn gate_enforces_monotonicity_floor_and_size() {
    const SLOT: u32 = 0x40_0000; // 4 MiB
    assert_eq!(gate(100, 1000, 100, 50, SLOT), Err(Reject::NotNewer)); // build == running
    assert_eq!(gate(99, 1000, 100, 50, SLOT), Err(Reject::NotNewer)); // downgrade
    assert_eq!(gate(60, 1000, 50, 80, SLOT), Err(Reject::BelowFloor)); // below floor
    assert_eq!(gate(200, 0, 100, 50, SLOT), Err(Reject::BadSize)); // zero
    assert_eq!(gate(200, SLOT + 1, 100, 50, SLOT), Err(Reject::BadSize)); // oversize
    assert_eq!(gate(200, 1000, 100, 50, SLOT), Ok(())); // fresh + bounded
}

// ---- window math + sha ----------------------------------------------------

#[test]
fn window_mask_and_bitmap() {
    assert_eq!(window_full_mask(0), 0);
    assert_eq!(window_full_mask(3), 0b111);
    assert_eq!(window_full_mask(64), u64::MAX);
    assert_eq!(window_full_mask(100), u64::MAX); // clamps at 64
    assert_eq!(bitmap_to_u64(&[0x01, 0x00]), 1); // LE, short slice zero-extends
    assert_eq!(bitmap_to_u64(&[0xff; 8]), u64::MAX);
    assert_eq!(bitmap_to_u64(&[]), 0);
}

#[test]
fn sha256_known_vector() {
    // sha256("abc")
    let expect: [u8; 32] = [
        0xba, 0x78, 0x16, 0xbf, 0x8f, 0x01, 0xcf, 0xea, 0x41, 0x41, 0x40, 0xde, 0x5d, 0xae, 0x22,
        0x23, 0xb0, 0x03, 0x61, 0xa3, 0x96, 0x17, 0x7a, 0x9c, 0xb4, 0x10, 0xff, 0x61, 0xf2, 0x00,
        0x15, 0xad,
    ];
    assert_eq!(sha256(b"abc"), expect);
    // streaming == one-shot
    let mut c = Sha256Ctx::new();
    c.update(b"a");
    c.update(b"bc");
    assert_eq!(c.finish(), expect);
}

fn alloc_string(s: &str) -> String {
    s.to_string()
}
