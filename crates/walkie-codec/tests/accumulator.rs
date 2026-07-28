//! `VoxAccumulator` — repacking 512 B mic chunks into 896 B frames.
//!
//! The whole point of this type is that 896 is 1.75 × 512, so frames straddle
//! chunk boundaries forever and the carry is never trivially aligned.

use walkie_codec::{VoxAccumulator, VOX_PAYLOAD, VOX_SRC_BYTES};

/// A ramp, so a dropped or duplicated sample shows up as a discontinuity.
fn ramp(n: usize, start: u16) -> Vec<u8> {
    (0..n)
        .flat_map(|i| (start.wrapping_add(i as u16)).to_le_bytes())
        .collect()
}

#[test]
fn emits_a_frame_only_when_a_whole_one_is_buffered() {
    let mut acc = VoxAccumulator::new();
    let mut payload = [0u8; VOX_PAYLOAD];

    // 512 B is less than 896 -> carry, no frame.
    let (used, n) = acc.feed(&ramp(256, 0), &mut payload);
    assert_eq!((used, n), (512, 0));
    assert_eq!(acc.pending(), 512);

    // Next 512 completes 896 and leaves 128 -> but feed() only ever takes what
    // the current frame needs (384), so the caller must loop.
    let src = ramp(256, 256);
    let (used, n) = acc.feed(&src, &mut payload);
    assert_eq!(used, 384, "must take only the 384 B needed to finish the frame");
    assert_eq!(n, VOX_PAYLOAD);
    assert_eq!(acc.pending(), 0);

    // The caller's loop then carries the remaining 128 B.
    let (used, n) = acc.feed(&src[384..], &mut payload);
    assert_eq!((used, n), (128, 0));
    assert_eq!(acc.pending(), 128);
}

#[test]
fn the_documented_caller_loop_consumes_everything_and_conserves_samples() {
    // Drive it exactly as the firmware does: 512 B chunks, loop until drained.
    let mut acc = VoxAccumulator::new();
    let mut payload = [0u8; VOX_PAYLOAD];
    let mut frames = 0usize;
    let mut consumed_total = 0usize;

    const CHUNKS: usize = 70; // 70 * 512 = 35,840 B = 40 frames exactly
    for c in 0..CHUNKS {
        let src = ramp(256, (c * 256) as u16);
        let mut off = 0;
        while off < src.len() {
            let (used, n) = acc.feed(&src[off..], &mut payload);
            assert_ne!(used, 0, "loop must always make progress");
            off += used;
            consumed_total += used;
            if n > 0 {
                assert_eq!(n, VOX_PAYLOAD);
                frames += 1;
            }
        }
    }
    assert_eq!(consumed_total, CHUNKS * 512, "every captured byte accounted for");
    assert_eq!(frames, CHUNKS * 512 / VOX_SRC_BYTES, "40 frames from 35,840 B");
    assert_eq!(acc.pending(), 0, "35,840 is a whole number of frames");
}

#[test]
fn one_oversized_feed_can_complete_several_frames_in_a_row() {
    let mut acc = VoxAccumulator::new();
    let mut payload = [0u8; VOX_PAYLOAD];
    let src = ramp(VOX_SRC_BYTES * 3 / 2, 0); // 3 frames' worth
    let mut off = 0;
    let mut frames = 0;
    while off < src.len() {
        let (used, n) = acc.feed(&src[off..], &mut payload);
        if used == 0 {
            break;
        }
        off += used;
        if n > 0 {
            frames += 1;
        }
    }
    assert_eq!(frames, 3);
    assert_eq!(off, src.len());
}

#[test]
fn never_splits_a_16_bit_sample() {
    // An odd-length feed must leave the trailing byte rather than desynchronise
    // every following sample into byte-swapped noise.
    let mut acc = VoxAccumulator::new();
    let mut payload = [0u8; VOX_PAYLOAD];
    let (used, n) = acc.feed(&[1, 2, 3], &mut payload);
    assert_eq!((used, n), (2, 0));
    assert_eq!(acc.pending() % 2, 0);

    // A single byte cannot be consumed at all -> caller's `used == 0` guard.
    let (used, _) = acc.feed(&[9], &mut payload);
    assert_eq!(used, 0);

    // Pending stays even across a long ragged sequence.
    for len in [1usize, 3, 5, 7, 511, 513, 897] {
        let src = vec![0xAAu8; len];
        let mut off = 0;
        while off < src.len() {
            let (used, _) = acc.feed(&src[off..], &mut payload);
            if used == 0 {
                break;
            }
            off += used;
            assert_eq!(acc.pending() % 2, 0, "pending must stay sample-aligned");
        }
    }
}

#[test]
fn reset_drops_the_carry_so_a_new_transmission_starts_clean() {
    let mut acc = VoxAccumulator::new();
    let mut payload = [0u8; VOX_PAYLOAD];
    acc.feed(&ramp(256, 0), &mut payload);
    assert_ne!(acc.pending(), 0);
    acc.reset();
    assert_eq!(acc.pending(), 0);
    // After reset the next frame is built purely from new audio: feeding a full
    // frame's worth in one go must emit immediately.
    let (used, n) = acc.feed(&ramp(VOX_SRC_BYTES / 2, 0), &mut payload);
    assert_eq!(used, VOX_SRC_BYTES);
    assert_eq!(n, VOX_PAYLOAD);
}

#[test]
fn empty_feed_is_a_no_op() {
    let mut acc = VoxAccumulator::new();
    let mut payload = [0u8; VOX_PAYLOAD];
    assert_eq!(acc.feed(&[], &mut payload), (0, 0));
    assert_eq!(acc.pending(), 0);
}
