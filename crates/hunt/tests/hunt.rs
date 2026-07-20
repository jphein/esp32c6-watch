use hunt::*;

#[test]
fn cycle_target_wraps() {
    let mut h = HuntState::new();
    let ids = [5u8, 6, 7];
    h.cycle_target(&ids, 0);
    assert_eq!(h.target(), Some(5)); // none -> first
    h.cycle_target(&ids, 0);
    assert_eq!(h.target(), Some(6));
    h.cycle_target(&ids, 0);
    assert_eq!(h.target(), Some(7));
    h.cycle_target(&ids, 0);
    assert_eq!(h.target(), Some(5)); // wrap
}

#[test]
fn cycle_target_empty_is_noop() {
    let mut h = HuntState::new();
    h.cycle_target(&[], 0);
    assert_eq!(h.target(), None);
}

#[test]
fn rising_signal_reads_warmer() {
    let mut h = HuntState::new();
    h.set_target(1, 0); // trend_ref seeded at -100 (unseen)
    let v = h.update(Some(-60), 100);
    assert_eq!(v.trend, Trend::Warmer);
    assert!(v.present);
    assert_eq!(v.smoothed_rssi, -60); // first sight seeds with raw
}

#[test]
fn steady_signal_reads_same_after_ref_rolls() {
    let mut h = HuntState::new();
    h.set_target(1, 0);
    h.update(Some(-60), 0);
    h.update(Some(-60), 1500); // ref rolls to -60 here (>= TREND_LAG_MS)
    let v = h.update(Some(-60), 1600);
    assert_eq!(v.trend, Trend::Same);
}

#[test]
fn dropping_signal_reads_colder() {
    let mut h = HuntState::new();
    h.set_target(1, 0);
    h.update(Some(-50), 0);
    h.update(Some(-50), 1500); // ref rolls to -50
    // -50 + (-80 - -50)*77/256 = -50 + (-9) = -59; delta -9 < -2 -> Colder
    let v = h.update(Some(-80), 1600);
    assert_eq!(v.trend, Trend::Colder);
    assert_eq!(v.smoothed_rssi, -59);
}

#[test]
fn absent_target_reads_lost() {
    let mut h = HuntState::new();
    h.set_target(3, 0);
    let v = h.update(None, 0);
    assert_eq!(v.trend, Trend::Lost);
    assert!(!v.present);
}

#[test]
fn no_target_reads_lost() {
    let mut h = HuntState::new();
    let v = h.update(Some(-40), 0);
    assert_eq!(v.trend, Trend::Lost);
    assert_eq!(v.target, None);
}

#[test]
fn found_requires_hold_above_threshold() {
    let mut h = HuntState::new();
    h.set_target(2, 0);
    let v0 = h.update(Some(-30), 0); // over -40 but not held yet
    assert_ne!(v0.trend, Trend::Found);
    let v1 = h.update(Some(-30), 1000); // held >= FOUND_HOLD_MS
    assert_eq!(v1.trend, Trend::Found);
}

#[test]
fn trend_word_and_arrow() {
    assert_eq!(Trend::Warmer.word(), "WARMER");
    assert_eq!(Trend::Warmer.arrow(), "^");
    assert_eq!(Trend::Colder.arrow(), "v");
    assert_eq!(Trend::Found.arrow(), "");
    assert_eq!(Trend::Same.arrow(), "");
}
