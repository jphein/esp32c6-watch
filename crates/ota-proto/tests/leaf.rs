//! The leaf session state machine, driven end-to-end with a Vec sink — the
//! window advance, re-ack, NAK cadence, finalize choreography, and deadlines.
//! (The Accept path of evaluate_meta needs a real signature over the baked
//! pubkey and is exercised on the bench; the rejection paths are covered here
//! and the gate itself has its own tests.)

use ota_proto::leaf::{
    ImageSink, LeafAction, LeafSession, MetaVerdict, total_chunks, LEAF_IDLE_NAK_MS,
    LEAF_PROGRESS_STALL_MS,
};
use ota_proto::{parse_ota_frame, OtaFrame, CHUNK_PAYLOAD, WINDOW_BYTES, WINDOW_CHUNKS};
use sha2::{Digest, Sha256};

struct VecSink {
    bytes: Vec<u8>,
    finalized: Option<(u32, [u8; 32])>,
    fail_feed: bool,
}
impl VecSink {
    fn new() -> Self {
        Self { bytes: Vec::new(), finalized: None, fail_feed: false }
    }
}
impl ImageSink for VecSink {
    fn feed_window(&mut self, b: &[u8]) -> bool {
        if self.fail_feed {
            return false;
        }
        self.bytes.extend_from_slice(b);
        true
    }
    fn finalize(&mut self, size: u32, sha: &[u8; 32]) -> bool {
        if self.bytes.len() as u32 != size {
            return false;
        }
        let d = Sha256::digest(&self.bytes);
        if d[..] != sha[..] {
            return false;
        }
        self.finalized = Some((size, *sha));
        true
    }
}

const GW: [u8; 6] = [2, 2, 2, 2, 2, 2];
const ME: u8 = 122;

fn armed_session(image: &[u8]) -> (LeafSession, [u8; 32]) {
    let sha: [u8; 32] = Sha256::digest(image).into();
    let mut s = LeafSession::new();
    s.arm(7, 99, image.len() as u32, sha, GW, 1_000);
    (s, sha)
}

fn feed_all(
    s: &mut LeafSession,
    image: &[u8],
    sink: &mut VecSink,
) -> Vec<LeafAction> {
    let mut win = vec![0u8; WINDOW_BYTES];
    let mut out = [0u8; 64];
    let total = total_chunks(image.len() as u32);
    let mut actions = Vec::new();
    for seq in 0..total {
        let off = seq as usize * CHUNK_PAYLOAD;
        let end = (off + CHUNK_PAYLOAD).min(image.len());
        let a = s.on_data(
            ME, 7, seq as u16, &image[off..end], GW, ME, 2_000, &mut win, sink, &mut out,
        );
        actions.push(a);
    }
    actions
}

#[test]
fn small_image_end_to_end() {
    // 3 chunks (fits one window): feed all, expect finalize-ack then Complete.
    let image: Vec<u8> = (0..CHUNK_PAYLOAD * 2 + 57).map(|i| i as u8).collect();
    let (mut s, _sha) = armed_session(&image);
    let mut sink = VecSink::new();
    let actions = feed_all(&mut s, &image, &mut sink);
    // last chunk completes the (single) window -> finalize-ack Nak
    assert!(matches!(actions.last(), Some(LeafAction::Nak(n)) if *n > 0));
    assert!(sink.finalized.is_some(), "sink verified + staged");
    assert_eq!(sink.bytes, image, "bytes written in order");
    // the finalize-ack window: after the repeats, Complete surfaces
    let mut out = [0u8; 64];
    let mut now = 2_000;
    let mut complete = None;
    for _ in 0..16 {
        now += LEAF_IDLE_NAK_MS + 1;
        match s.tick(ME, now, &mut out) {
            LeafAction::Complete { build } => {
                complete = Some(build);
                break;
            }
            LeafAction::Nak(_) | LeafAction::None => {}
            a => panic!("unexpected {a:?}"),
        }
    }
    assert_eq!(complete, Some(99));
    assert!(!s.is_active());
}

#[test]
fn multi_window_advances_and_acks() {
    // 65 chunks = two windows (64 + 1).
    let image: Vec<u8> = (0..CHUNK_PAYLOAD * 64 + 10).map(|i| (i % 251) as u8).collect();
    let (mut s, _) = armed_session(&image);
    let mut sink = VecSink::new();
    let actions = feed_all(&mut s, &image, &mut sink);
    // chunk 63 completes window 0 -> advance-ack; chunk 64 completes window 1 -> finalize-ack
    let naks = actions.iter().filter(|a| matches!(a, LeafAction::Nak(_))).count();
    assert_eq!(naks, 2, "one advance-ack + one finalize-ack");
    assert_eq!(sink.bytes, image);
    // the advance-ack was a valid all-zero OTAN for window base 0
}

#[test]
fn stale_chunk_reacks_completed_window() {
    let image: Vec<u8> = (0..CHUNK_PAYLOAD * 64 + 10).map(|i| (i % 251) as u8).collect();
    let (mut s, _) = armed_session(&image);
    let mut sink = VecSink::new();
    let _ = feed_all(&mut s, &image, &mut sink);
    // gateway resends chunk 3 (window 0, already advanced): expect a re-ack
    let mut win = vec![0u8; WINDOW_BYTES];
    let mut out = [0u8; 64];
    let off = 3 * CHUNK_PAYLOAD;
    // NOTE: session is in finalize phase; a stale chunk still within the live
    // session re-acks its window (gateway missed our advance-ack)
    let a = s.on_data(
        ME, 7, 3, &image[off..off + CHUNK_PAYLOAD], GW, ME, 3_000, &mut win, &mut sink, &mut out,
    );
    match a {
        LeafAction::Nak(n) => {
            match parse_ota_frame(&out[..n]) {
                Some(OtaFrame::Nak { origin, session, window_base, bitmap }) => {
                    assert_eq!((origin, session, window_base), (ME, 7, 0));
                    assert!(bitmap.iter().all(|&b| b == 0), "all-zero = ack");
                }
                other => panic!("expected OTAN, got {other:?}"),
            }
        }
        other => panic!("expected re-ack, got {other:?}"),
    }
}

#[test]
fn gap_naks_and_stall_discard() {
    let image: Vec<u8> = (0..CHUNK_PAYLOAD * 3).map(|i| i as u8).collect();
    let (mut s, _) = armed_session(&image);
    let mut sink = VecSink::new();
    let mut win = vec![0u8; WINDOW_BYTES];
    let mut out = [0u8; 64];
    // deliver only chunk 1 (gap at 0 and 2)
    let off = CHUNK_PAYLOAD;
    let _ = s.on_data(ME, 7, 1, &image[off..off * 2], GW, ME, 2_000, &mut win, &mut sink, &mut out);
    // idle tick -> gap NAK naming the missing bits
    let a = s.tick(ME, 2_000 + LEAF_IDLE_NAK_MS + 1, &mut out);
    match a {
        LeafAction::Nak(n) => match parse_ota_frame(&out[..n]) {
            Some(OtaFrame::Nak { bitmap, .. }) => {
                let m = u64::from_le_bytes(bitmap.try_into().unwrap());
                assert_eq!(m, 0b101, "chunks 0 and 2 missing");
            }
            other => panic!("expected OTAN, got {other:?}"),
        },
        other => panic!("expected gap NAK, got {other:?}"),
    }
    // stall long enough -> discard
    let a = s.tick(ME, 3_000 + LEAF_PROGRESS_STALL_MS, &mut out);
    assert_eq!(a, LeafAction::Abort);
    assert!(!s.is_active());
}

#[test]
fn wrong_gateway_and_wrong_lengths_ignored() {
    let image: Vec<u8> = (0..CHUNK_PAYLOAD * 2).map(|i| i as u8).collect();
    let (mut s, _) = armed_session(&image);
    let mut sink = VecSink::new();
    let mut win = vec![0u8; WINDOW_BYTES];
    let mut out = [0u8; 64];
    let spoof = [9u8; 6];
    assert_eq!(
        s.on_data(ME, 7, 0, &image[..CHUNK_PAYLOAD], spoof, ME, 2_000, &mut win, &mut sink, &mut out),
        LeafAction::None,
        "chunks from a different MAC than the session's gateway are ignored"
    );
    assert_eq!(
        s.on_data(ME, 7, 0, &image[..CHUNK_PAYLOAD - 1], GW, ME, 2_000, &mut win, &mut sink, &mut out),
        LeafAction::None,
        "short non-final chunk ignored"
    );
    assert!(sink.bytes.is_empty());
}

#[test]
fn evaluate_meta_rejections() {
    let s = LeafSession::new();
    let m = b"999|1234|0000000000000000000000000000000000000000000000000000000000000000";
    assert_eq!(
        s.evaluate_meta(5, 1, m, &[0u8; 64], ME, 1, 0, 1 << 20),
        MetaVerdict::NotForUs
    );
    assert_eq!(
        s.evaluate_meta(ME, 1, m, &[0u8; 64], ME, 1, 0, 1 << 20),
        MetaVerdict::BadSignature,
        "a zero signature never verifies against the baked pubkey"
    );
}

#[test]
fn failed_flash_write_aborts() {
    let image: Vec<u8> = (0..CHUNK_PAYLOAD).map(|i| i as u8).collect();
    let (mut s, _) = armed_session(&image);
    let mut sink = VecSink::new();
    sink.fail_feed = true;
    let mut win = vec![0u8; WINDOW_BYTES];
    let mut out = [0u8; 64];
    let a = s.on_data(ME, 7, 0, &image, GW, ME, 2_000, &mut win, &mut sink, &mut out);
    assert_eq!(a, LeafAction::Abort);
    assert!(!s.is_active(), "good slot intact, session gone");
}
