//! Host tests for the nearest-peer finder. Run:
//! `cargo test -p finder --target x86_64-unknown-linux-gnu`
//! (the repo `.cargo/config.toml` defaults to the riscv target, so host tests
//! need the explicit host `--target`).
//!
//! EWMA arithmetic matches `crates/rssi` (α = 77/256, integer, truncating):
//! `smoothed += (raw - smoothed) * 77 / 256`.

use finder::*;
use rssi::Proximity;

#[test]
fn picks_strongest_peer_as_nearest() {
    let mut f = FinderState::new();
    let v = f.update(&[(1, -70), (2, -50), (3, -80)], 0);
    assert_eq!(v.nearest, Some(2)); // -50 is the strongest signal
    assert_eq!(v.smoothed_rssi, -50); // first sight seeds with raw
    assert_eq!(v.proximity, Proximity::Near);
    assert_eq!(v.bars, 3);
    assert_eq!(v.peer_count, 3);
    assert_eq!(v.trend, Trend::Steady); // just acquired → no direction yet
    assert!(v.is_present());
}

#[test]
fn nearest_switches_to_a_closer_peer() {
    let mut f = FinderState::new();
    assert_eq!(f.update(&[(1, -60)], 0).nearest, Some(1));
    let v = f.update(&[(1, -60), (2, -45)], 100);
    assert_eq!(v.nearest, Some(2)); // peer 2 is now closer
    assert_eq!(f.nearest(), Some(2));
}

#[test]
fn approaching_reads_closer() {
    let mut f = FinderState::new();
    f.update(&[(1, -70)], 0); // ref armed at -70
    // -70 + (-40 - -70)*77/256 = -70 + 9 = -61 ; delta +9 > deadband → Closer
    let v = f.update(&[(1, -40)], 100);
    assert_eq!(v.trend, Trend::Closer);
    assert_eq!(v.smoothed_rssi, -61);
}

#[test]
fn receding_reads_farther() {
    let mut f = FinderState::new();
    f.update(&[(1, -50)], 0);
    f.update(&[(1, -50)], 1500); // ref rolls to -50 (>= TREND_LAG_MS)
    // -50 + (-80 - -50)*77/256 = -50 + (-9) = -59 ; delta -9 < -deadband → Farther
    let v = f.update(&[(1, -80)], 1600);
    assert_eq!(v.trend, Trend::Farther);
    assert_eq!(v.smoothed_rssi, -59);
}

#[test]
fn steady_signal_reads_steady_after_ref_rolls() {
    let mut f = FinderState::new();
    f.update(&[(1, -60)], 0);
    f.update(&[(1, -60)], 1500); // ref rolls to -60
    let v = f.update(&[(1, -60)], 1600);
    assert_eq!(v.trend, Trend::Steady);
}

#[test]
fn no_peers_reads_searching() {
    let mut f = FinderState::new();
    let v = f.update(&[], 0);
    assert_eq!(v.nearest, None);
    assert_eq!(v.trend, Trend::Searching);
    assert_eq!(v.peer_count, 0);
    assert!(!v.is_present());
    assert_eq!(v.bars, 0);
    assert_eq!(v.proximity, Proximity::Gone);
}

#[test]
fn stale_peer_is_dropped() {
    let mut f = FinderState::new();
    assert_eq!(f.update(&[(1, -50)], 0).nearest, Some(1));
    // still within the staleness window
    assert_eq!(f.update(&[], STALE_MS).nearest, Some(1));
    // just past it → the peer is assumed gone
    let v = f.update(&[], STALE_MS + 1);
    assert_eq!(v.nearest, None);
    assert_eq!(v.trend, Trend::Searching);
    assert_eq!(v.peer_count, 0);
}

#[test]
fn switching_nearest_rearms_trend_no_false_closer() {
    let mut f = FinderState::new();
    f.update(&[(1, -50)], 0);
    f.update(&[(1, -50)], 1500);
    // Peer 2 appears far stronger (-30 vs the -50 reference). Without re-arming
    // this would fake a huge Closer jump; the finder must read Steady on acquisition.
    let v = f.update(&[(1, -50), (2, -30)], 1600);
    assert_eq!(v.nearest, Some(2));
    assert_eq!(v.trend, Trend::Steady);
}

#[test]
fn proximity_and_bars_map_via_rssi() {
    let mut f = FinderState::new();
    let v = f.update(&[(1, -40)], 0);
    assert_eq!(v.proximity, Proximity::Here);
    assert_eq!(v.bars, 4);
    assert_eq!(v.proximity_label(), "HERE");
    assert!(v.is_present());
}

#[test]
fn bar_px_delegates_to_rssi() {
    let mut f = FinderState::new();
    let v = f.update(&[(1, -40)], 0);
    // rssi::bar_px(-40, 100) = clamp(-40,-90,-35)=-40 → (-40+90)*100/55 = 90
    assert_eq!(v.bar_px(100), 90);
    // searching → empty bar
    let s = f.update(&[], STALE_MS + 1);
    assert_eq!(s.bar_px(100), 0);
}

#[test]
fn first_sight_seeds_raw() {
    let mut f = FinderState::new();
    let v = f.update(&[(1, -55)], 0);
    assert_eq!(v.smoothed_rssi, -55); // no ramp-in from zero
}

#[test]
fn bounded_many_peers_never_panics() {
    let mut f = FinderState::new();
    // Feed far more distinct peers than MAX_PEERS; strongest is id 0 (-50).
    for i in 0..30u8 {
        f.observe(i, -50 - i as i32, 0);
    }
    let v = f.tick(0);
    assert!(v.nearest.is_some());
    assert!(v.peer_count <= MAX_PEERS);
    assert_eq!(v.peer_count, MAX_PEERS); // table saturates at capacity
    assert_eq!(v.nearest, Some(0)); // strongest of the first MAX_PEERS ids
}

#[test]
fn trend_words_and_arrows() {
    assert_eq!(Trend::Closer.word(), "CLOSER");
    assert_eq!(Trend::Closer.arrow(), "^");
    assert_eq!(Trend::Farther.word(), "FARTHER");
    assert_eq!(Trend::Farther.arrow(), "v");
    assert_eq!(Trend::Steady.word(), "STEADY");
    assert_eq!(Trend::Steady.arrow(), "");
    assert_eq!(Trend::Searching.word(), "SEARCHING");
    assert_eq!(Trend::Searching.arrow(), "");
}

#[test]
fn observe_then_tick_matches_update() {
    // The two entry points must agree.
    let mut a = FinderState::new();
    let va = a.update(&[(1, -60), (2, -55)], 0);

    let mut b = FinderState::new();
    b.observe(1, -60, 0);
    b.observe(2, -55, 0);
    let vb = b.tick(0);

    assert_eq!(va, vb);
    assert_eq!(va.nearest, Some(2));
}
