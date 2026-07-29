//! The retry budget: when does a flaky link end a chapter?
//!
//! These exist because the logic they cover was **wrong in production and had no
//! test to catch it**. It lived in the firmware crate, which cannot build for the
//! host, so the flaw surfaced only in a daemon access log: windows failing after
//! delivering ~4.6 s of audio each, three strikes spent in ~14 s, chapter
//! abandoned while still making forward progress.

use story_proto::*;

/// One second of audio — the real measured shape of the failures in the log.
const SECOND: u32 = BYTES_PER_SEC as u32;

#[test]
fn a_healthy_link_never_gives_up() {
    let mut b = RetryBudget::new();
    for _ in 0..1000 {
        b.delivered();
    }
    assert_eq!(b.total(), 0);
}

#[test]
fn three_consecutive_dead_windows_give_up() {
    let mut b = RetryBudget::new();
    assert!(!b.failed(0), "first stall should retry");
    assert!(!b.failed(0), "second stall should retry");
    assert!(b.failed(0), "third consecutive stall means the far end is gone");
    assert_eq!(b.total(), 3);
}

#[test]
fn progress_clears_the_strike_count() {
    // THE REGRESSION TEST. The observed failure delivered 146,432 B = 4.576 s
    // before failing. Three of those must NOT end the chapter — the link is slow,
    // not dead, and abandoning it loses a story that was still playing.
    let mut b = RetryBudget::new();
    for i in 0..20 {
        assert!(
            !b.failed(146_432),
            "attempt {i}: a failure that delivered 4.576 s must not count as a strike"
        );
    }
    assert_eq!(b.total(), 20, "but every failure is still counted for reporting");
}

#[test]
fn a_stall_after_progress_starts_from_zero_strikes() {
    let mut b = RetryBudget::new();
    assert!(!b.failed(0));
    assert!(!b.failed(0)); // two strikes
    assert!(!b.failed(SECOND)); // progress -> cleared
    assert!(!b.failed(0));
    assert!(!b.failed(0));
    assert!(b.failed(0), "three strikes again, from the cleared count");
}

#[test]
fn progress_just_below_the_threshold_is_still_a_stall() {
    let mut b = RetryBudget::new();
    assert!(!b.failed(SECOND - 1));
    assert!(!b.failed(SECOND - 1));
    assert!(b.failed(SECOND - 1), "sub-second dribble is not progress");
}

#[test]
fn exactly_the_threshold_counts_as_progress() {
    let mut b = RetryBudget::new();
    for _ in 0..10 {
        assert!(!b.failed(MIN_PROGRESS_BYTES));
    }
}

#[test]
fn the_total_cap_guarantees_termination_on_a_trickling_link() {
    // Without this, progress-resets would let a link that delivers exactly one
    // second per attempt retry forever.
    let mut b = RetryBudget::new();
    let mut n = 0u32;
    loop {
        n += 1;
        if b.failed(SECOND) {
            break;
        }
        assert!(n < 1000, "must terminate");
    }
    assert_eq!(n as u16, MAX_TOTAL_RETRIES);
    assert_eq!(b.total(), MAX_TOTAL_RETRIES);
}

#[test]
fn a_delivered_window_between_stalls_also_clears() {
    let mut b = RetryBudget::new();
    assert!(!b.failed(0));
    assert!(!b.failed(0));
    b.delivered();
    assert!(!b.failed(0));
    assert!(!b.failed(0));
    assert!(b.failed(0));
}

#[test]
fn the_total_counter_saturates_rather_than_wrapping() {
    // A wrap on `total` would reset the termination guarantee mid-chapter, so this
    // has to actually REACH the boundary. An earlier version of this test looped
    // 300 times and asserted `u16::MAX.min(300)` — which is just `300`, so it
    // never went near saturation while its name claimed it did. Clippy caught the
    // dead `min`; the weak test was mine.
    let mut b = RetryBudget::new();
    for _ in 0..(u16::MAX as u32 + 100) {
        b.failed(SECOND);
    }
    assert_eq!(b.total(), u16::MAX, "must pin at the ceiling, not wrap to 0");
}
