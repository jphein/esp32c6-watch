use mic_dsp::{peak_abs, rms_dbfs, DBFS_FLOOR};

/// A full-scale square wave (alternating ±32767) has RMS ≈ full scale, so its
/// level should sit at ~0 dBFS.
#[test]
fn full_scale_square_is_about_zero_dbfs() {
    let w: [i16; 8] = [32767, -32767, 32767, -32767, 32767, -32767, 32767, -32767];
    let db = rms_dbfs(&w);
    assert!(db > -0.5 && db <= 0.0, "expected ~0 dBFS, got {db}");
}

#[test]
fn silence_reads_floor() {
    let w = [0i16; 32];
    assert_eq!(rms_dbfs(&w), DBFS_FLOOR);
}

#[test]
fn empty_window_reads_floor() {
    assert_eq!(rms_dbfs(&[]), DBFS_FLOOR);
}

/// A constant (pure-DC) signal has zero AC energy once DC is removed, so it must
/// read the floor — NOT ~-6 dBFS. This guards the mean-subtraction step.
#[test]
fn constant_dc_reads_floor() {
    let w = [16384i16; 32];
    let db = rms_dbfs(&w);
    assert_eq!(db, DBFS_FLOOR, "DC bias not removed: got {db}");
}

/// A half-amplitude square wave (±16384) has RMS = FS/2, i.e. exactly
/// 20·log10(0.5) ≈ -6.02 dBFS.
#[test]
fn half_amplitude_square_is_about_minus_6_dbfs() {
    let w: [i16; 8] = [16384, -16384, 16384, -16384, 16384, -16384, 16384, -16384];
    let db = rms_dbfs(&w);
    assert!((db - -6.02).abs() < 0.2, "expected ~-6 dBFS, got {db}");
}

/// Level is clamped: nothing reads below the floor.
#[test]
fn never_below_floor() {
    let quiet = [1i16, -1, 1, -1];
    assert!(rms_dbfs(&quiet) >= DBFS_FLOOR);
}

// === peak_abs (waveform amplitude) ===

#[test]
fn peak_empty_is_zero() {
    assert_eq!(peak_abs(&[]), 0);
}

/// Pure DC has no AC once the mean is removed → peak 0 (mirrors rms's DC guard).
#[test]
fn peak_constant_dc_is_zero() {
    assert_eq!(peak_abs(&[16384i16; 16]), 0);
}

/// A ±16384 square (mean 0) has peak amplitude 16384.
#[test]
fn peak_symmetric_square() {
    let w: [i16; 8] = [16384, -16384, 16384, -16384, 16384, -16384, 16384, -16384];
    assert_eq!(peak_abs(&w), 16384);
}

/// DC bias is removed before the peak: [1000, -1000] biased by +500 → mean 250,
/// max |x-mean| = |−1000−250| = 1250.
#[test]
fn peak_removes_dc_bias() {
    let w: [i16; 4] = [1500, -500, 1500, -500];
    // mean = 500; deviations = {1000, -1000} → peak 1000.
    assert_eq!(peak_abs(&w), 1000);
}
