//! The 32-bytes-per-millisecond contract, and the Range windowing built on it.
//!
//! The numbers here are not invented — they are the live daemon's, captured
//! 2026-07-29 from `GET /api/chapters`. If the identity these assert ever stops
//! holding, every Range request and every highlight offset in the app is wrong
//! by a growing amount, which is exactly the silent cumulative desync the design
//! spec's §8.1 warns about.

use story_proto::*;

/// Live chapter 1: `duration_ms` 452,729 and `total_bytes` 14,487,328.
const CH1_MS: u32 = 452_729;
const CH1_BYTES: u32 = 14_487_328;

/// Live chapter 2: `duration_ms` 594,544 and `total_bytes` 19,025,408.
const CH2_MS: u32 = 594_544;
const CH2_BYTES: u32 = 19_025_408;

#[test]
fn live_chapter_durations_convert_exactly() {
    assert_eq!(ms_to_bytes(CH1_MS), CH1_BYTES);
    assert_eq!(ms_to_bytes(CH2_MS), CH2_BYTES);
    // And back again, exactly — the identity is not merely one-way.
    assert_eq!(bytes_to_ms(CH1_BYTES), CH1_MS);
    assert_eq!(bytes_to_ms(CH2_BYTES), CH2_MS);
}

#[test]
fn the_contract_is_thirty_two_bytes_per_millisecond() {
    assert_eq!(BYTES_PER_MS, 32);
    assert_eq!(BYTES_PER_SEC, 32_000);
    assert_eq!(ms_to_bytes(1), 32);
    assert_eq!(ms_to_bytes(1000), 32_000);
}

#[test]
fn one_play_chunk_is_sixteen_milliseconds() {
    assert_eq!(PLAY_CHUNK, 512);
    assert_eq!(CHUNK_MS, 16);
    assert_eq!(ms_to_bytes(CHUNK_MS), PLAY_CHUNK as u32);
}

#[test]
fn bytes_to_ms_floors_like_the_daemon() {
    // The spec's own example: 32 bytes is 1 ms, so a legal 34-byte buffer is
    // 1.0625 ms. Both ends floor, which is what keeps them in agreement.
    assert_eq!(bytes_to_ms(34), 1);
    assert_eq!(bytes_to_ms(63), 1);
    assert_eq!(bytes_to_ms(64), 2);
    assert_eq!(bytes_to_ms(0), 0);
}

#[test]
fn ms_to_bytes_saturates_rather_than_wrapping() {
    // 37 hours of audio would be needed to reach here, so only nonsense input
    // does — and it must not wrap into a plausible small offset.
    assert_eq!(ms_to_bytes(u32::MAX), u32::MAX);
    assert_eq!(ms_to_bytes(200_000_000), u32::MAX);
}

#[test]
fn sample_alignment_never_splits_a_sixteen_bit_sample() {
    assert_eq!(align_sample(0), 0);
    assert_eq!(align_sample(1), 0);
    assert_eq!(align_sample(2), 2);
    assert_eq!(align_sample(513), 512);
    // Every byte offset derived from a millisecond is already aligned, because
    // 32 is even — this is a belt-and-braces check on a Range boundary.
    for ms in [0u32, 1, 17, 4120, CH1_MS] {
        assert_eq!(align_sample(ms_to_bytes(ms)), ms_to_bytes(ms));
    }
}

// ---------------------------------------------------------------------------
// Range windows
// ---------------------------------------------------------------------------

#[test]
fn window_covers_sixty_seconds() {
    assert_eq!(WINDOW_BYTES, 1_920_000);
    let (first, last) = window_at(0, CH1_BYTES).unwrap();
    assert_eq!(first, 0);
    // Inclusive end, so one byte short of the window length.
    assert_eq!(last, WINDOW_BYTES - 1);
    assert_eq!(last - first + 1, WINDOW_BYTES);
}

#[test]
fn windows_tile_a_whole_chapter_with_no_gap_and_no_overlap() {
    let mut pos = 0u32;
    let mut covered = 0u64;
    let mut windows = 0;
    while let Some((first, last)) = window_at(pos, CH1_BYTES) {
        assert_eq!(first, pos, "window must start exactly where the last ended");
        assert!(last >= first);
        covered += (last - first + 1) as u64;
        pos = last + 1;
        windows += 1;
        assert!(windows < 100, "runaway");
    }
    // Every byte of the chapter, exactly once.
    assert_eq!(covered, CH1_BYTES as u64);
    assert_eq!(pos, CH1_BYTES);
    // ~7.5 minutes at 60 s per window.
    assert_eq!(windows, 8);
}

#[test]
fn window_at_end_of_file_is_none() {
    assert!(window_at(CH1_BYTES, CH1_BYTES).is_none());
    assert!(window_at(CH1_BYTES + 1, CH1_BYTES).is_none());
    // A chapter with no audio yet must not produce a request.
    assert!(window_at(0, 0).is_none());
}

#[test]
fn final_window_is_clamped_to_the_file() {
    // Start 100 bytes from the end.
    let pos = CH1_BYTES - 100;
    let (first, last) = window_at(pos, CH1_BYTES).unwrap();
    assert_eq!(first, pos);
    assert_eq!(last, CH1_BYTES - 1, "must not ask beyond the last byte");
}

#[test]
fn resume_offset_from_a_timestamp_is_exact() {
    // Resume 5 minutes into chapter 1.
    let ms = 5 * 60 * 1000;
    let off = ms_to_bytes(ms);
    assert_eq!(off, 9_600_000);
    // Landing exactly on a sample and a whole millisecond is what makes resume
    // free — no re-sync, no index.
    assert_eq!(bytes_to_ms(off), ms);
    assert_eq!(align_sample(off), off);
}
