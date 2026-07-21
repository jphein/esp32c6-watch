use diag_model::*;

// ---- capabilities ----------------------------------------------------------

#[test]
fn caps_set_get_clear() {
    let mut c = Caps::new();
    assert!(!c.has(Caps::WIFI));
    c.set(Caps::WIFI, true);
    c.set(Caps::MIC, true);
    assert!(c.has(Caps::WIFI));
    assert!(c.has(Caps::MIC));
    assert!(!c.has(Caps::BLE));
    c.set(Caps::WIFI, false);
    assert!(!c.has(Caps::WIFI));
    assert!(c.has(Caps::MIC)); // clearing one leaves the other
    assert_eq!(Caps::COUNT, 10);
}

#[test]
fn caps_bits_are_distinct() {
    // No two capability bits collide.
    let bits = [
        Caps::WIFI, Caps::BLE, Caps::MESH, Caps::MIC, Caps::IMU,
        Caps::RTC, Caps::TOUCH, Caps::LP_CORE, Caps::DISPLAY, Caps::RADIO,
    ];
    assert_eq!(bits.len(), Caps::COUNT);
    let mut or = 0u16;
    for b in bits {
        assert_eq!(or & b, 0, "bit {b:#x} overlaps an earlier one");
        or |= b;
    }
    assert_eq!(or.count_ones() as usize, Caps::COUNT);
}

// ---- FPS smoother ----------------------------------------------------------

#[test]
fn fps_seeds_to_first_sample_then_converges() {
    let mut f = Fps::new();
    // First sample seeds exactly: 30 frames in 1000 ms = 30 fps.
    assert_eq!(f.update(30, 1000), 30.0);
    // Feed a steady 60 fps; EMA converges upward toward 60.
    let mut last = f.value();
    for _ in 0..50 {
        let v = f.update(60, 1000);
        assert!(v >= last - 0.001, "EMA should climb toward 60");
        last = v;
    }
    assert!((f.value() - 60.0).abs() < 0.5, "converged near 60, got {}", f.value());
}

#[test]
fn fps_zero_interval_is_ignored() {
    let mut f = Fps::new();
    f.update(30, 1000);
    let before = f.value();
    assert_eq!(f.update(999, 0), before, "dt=0 must not divide-by-zero or spike");
}

#[test]
fn fps_default_is_zero() {
    assert_eq!(Fps::new().value(), 0.0);
}

// ---- realm words / sigil ---------------------------------------------------

#[test]
fn realm_words_in_range_and_deterministic() {
    // Same id → same words, every call.
    for id in [0u32, 1, 42, 0xDEAD_BEEF, u32::MAX] {
        let a = realm_words(id);
        let b = realm_words(id);
        assert_eq!(a, b);
    }
    // Words come from the tables (non-empty, ascii).
    let (adj, noun) = realm_words(0xDEAD_BEEF);
    assert!(!adj.is_empty() && !noun.is_empty());
    assert!(adj.is_ascii() && noun.is_ascii());
}

#[test]
fn build_id_stable_and_sha_drives_words() {
    // Same SHA → same id → same words.
    assert_eq!(build_id("2a375e9"), build_id("2a375e9"));
    let s1 = Sigil::new("0.5.1", "2a375e9", "2026-07-21");
    let s2 = Sigil::new("0.5.1", "2a375e9", "2026-07-21");
    assert_eq!((s1.adj, s1.noun), (s2.adj, s2.noun), "sigil words stable per SHA");
    // A different SHA usually yields different words (not guaranteed, but these two differ).
    let other = Sigil::new("0.5.1", "dbfb824", "2026-07-21");
    assert!(
        (s1.adj, s1.noun) != (other.adj, other.noun),
        "distinct SHAs should give distinct sigils here"
    );
    // Passthrough fields.
    assert_eq!(s1.version, "0.5.1");
    assert_eq!(s1.sha, "2a375e9");
    assert_eq!(s1.date, "2026-07-21");
}

// ---- metrics struct --------------------------------------------------------

#[test]
fn diag_metrics_default_is_zeroed() {
    let m = DiagMetrics::default();
    assert_eq!(m.heap_main_free, 0);
    assert_eq!(m.stack_gap, 0);
    assert_eq!(m.fps, 0.0);
    assert!(!m.caps.has(Caps::WIFI));
}
