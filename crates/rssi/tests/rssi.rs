use core::fmt::Write;
use rssi::*;

#[test]
fn first_sight_seeds_with_raw() {
    let mut s = RssiSmoother::new();
    assert_eq!(s.update(1, -50), -50); // no ramp-in from zero
}

#[test]
fn ewma_converges_toward_new_sample() {
    let mut s = RssiSmoother::new();
    assert_eq!(s.update(1, -50), -50);
    // -50 + (-90 - -50)*77/256 = -50 + (-3080/256 = -12) = -62
    assert_eq!(s.update(1, -90), -62);
    // moves ~30% toward the new sample, not all the way
    let v = s.update(1, -90);
    assert!(v < -62 && v > -90);
}

#[test]
fn per_id_independent() {
    let mut s = RssiSmoother::new();
    s.update(1, -50);
    s.update(2, -80);
    assert_eq!(s.get(1), Some(-50));
    assert_eq!(s.get(2), Some(-80));
    assert_eq!(s.get(99), None);
}

#[test]
fn proximity_thresholds() {
    assert_eq!(proximity(-40), Proximity::Here);
    assert_eq!(proximity(-45), Proximity::Here); // inclusive boundary
    assert_eq!(proximity(-50), Proximity::Near);
    assert_eq!(proximity(-70), Proximity::Room);
    assert_eq!(proximity(-80), Proximity::Far);
    assert_eq!(proximity(-95), Proximity::Gone);
}

#[test]
fn labels_are_fixed_width_4() {
    for p in [Proximity::Here, Proximity::Near, Proximity::Room, Proximity::Far, Proximity::Gone] {
        assert_eq!(label(p).len(), 4);
    }
    assert_eq!(label(Proximity::Far), "FAR "); // padded
}

#[test]
fn tier_bars_range() {
    assert_eq!(tier_bars(Proximity::Here), 4);
    assert_eq!(tier_bars(Proximity::Gone), 0);
}

#[test]
fn bar_px_clamps_and_scales() {
    assert_eq!(bar_px(-35, 100), 100); // top of useful span -> full
    assert_eq!(bar_px(-90, 100), 0); // bottom -> empty
    assert_eq!(bar_px(-200, 100), 0); // clamps below -90
    assert_eq!(bar_px(0, 100), 100); // clamps above -35
    assert_eq!(bar_px(-62, 100), 50); // (28*100)/55 = 50
}

#[test]
fn line_builds_and_truncates() {
    let mut l = Line::new();
    let _ = write!(l, "hi {}", 7);
    assert_eq!(l.as_str(), "hi 7");

    let mut full = Line::new();
    // 24-byte cap: overflow silently dropped
    let _ = write!(full, "{}", "x".repeat(40));
    assert_eq!(full.as_str().len(), 24);
}

#[test]
fn clip_left_truncates() {
    assert_eq!(clip("abcdef", 3), "abc");
    assert_eq!(clip("ab", 5), "ab"); // shorter than n -> unchanged
}
