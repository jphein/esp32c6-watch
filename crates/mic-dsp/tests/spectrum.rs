use mic_dsp::{
    spectrum_dbfs, SpectrumEnvelope, BAND_EDGE_BINS, DBFS_FLOOR, FFT_SIZE, SPECTRUM_BANDS,
};

const BIN_HZ: f32 = 16_000.0 / FFT_SIZE as f32; // 62.5 Hz

/// One 256-sample window of a sine at `freq_hz` with amplitude `amp`.
fn sine_window(freq_hz: f32, amp: f32) -> [i16; FFT_SIZE] {
    let mut w = [0i16; FFT_SIZE];
    let step = 2.0 * std::f32::consts::PI * freq_hz / 16_000.0;
    for (i, s) in w.iter_mut().enumerate() {
        *s = (amp * (step * i as f32).sin()) as i16;
    }
    w
}

fn argmax(bands: &[f32; SPECTRUM_BANDS]) -> usize {
    let mut best = 0;
    for b in 1..SPECTRUM_BANDS {
        if bands[b] > bands[best] {
            best = b;
        }
    }
    best
}

/// A tone at the middle bin of every band must light THAT band the brightest —
/// the full whistle-sweep acceptance in miniature.
#[test]
fn sine_lands_in_its_band() {
    for b in 0..SPECTRUM_BANDS {
        let mid_bin = (BAND_EDGE_BINS[b] + BAND_EDGE_BINS[b + 1]) / 2;
        let freq = mid_bin as f32 * BIN_HZ;
        let bands = spectrum_dbfs(&sine_window(freq, 16_384.0));
        assert_eq!(
            argmax(&bands),
            b,
            "tone at {freq} Hz (bin {mid_bin}) should peak band {b}, got {:?}",
            bands
        );
    }
}

/// 1 kHz (bin 16, on-bin) at half scale: band 6 (800–1174 Hz) reads ≈ −6 dBFS
/// and clearly dominates its neighbours.
#[test]
fn one_khz_sine_reads_near_minus_six() {
    let bands = spectrum_dbfs(&sine_window(1_000.0, 16_384.0));
    assert!(
        bands[6] > -9.0 && bands[6] < -3.0,
        "band 6 should read ~-6 dBFS, got {}",
        bands[6]
    );
    assert!(bands[6] - bands[5] >= 6.0, "band 5 too hot: {:?}", bands);
    assert!(bands[6] - bands[7] >= 6.0, "band 7 too hot: {:?}", bands);
}

/// A full-scale on-bin sine rails its band at ~0 dBFS (meter-scale parity).
#[test]
fn full_scale_sine_rails_its_band() {
    let bands = spectrum_dbfs(&sine_window(1_000.0, 32_760.0));
    assert!(
        bands[6] > -2.0 && bands[6] <= 0.0,
        "full-scale sine should rail band 6, got {}",
        bands[6]
    );
}

/// Silence floors every band.
#[test]
fn silence_floors_all_bands() {
    let bands = spectrum_dbfs(&[0i16; FFT_SIZE]);
    assert_eq!(bands, [DBFS_FLOOR; SPECTRUM_BANDS]);
}

/// Empty input floors every band (guard, mirrors rms_dbfs).
#[test]
fn empty_input_floors_all_bands() {
    assert_eq!(spectrum_dbfs(&[]), [DBFS_FLOOR; SPECTRUM_BANDS]);
}

/// Pure DC must NOT light the low bands — guards the mean-subtraction step
/// (a biased mic would otherwise leak through the Hann mainlobe into band 0).
#[test]
fn dc_reads_floor() {
    let bands = spectrum_dbfs(&[16_000i16; FFT_SIZE]);
    assert_eq!(bands, [DBFS_FLOOR; SPECTRUM_BANDS]);
}

/// White noise spreads across ALL bands (no band stuck at the floor, none
/// railed). Max-hold over 32 windows to tame single-window chi-square variance
/// in the narrow low bands.
#[test]
fn white_noise_spreads_across_bands() {
    let mut lcg: u32 = 0x1234_5678;
    let mut hold = [DBFS_FLOOR; SPECTRUM_BANDS];
    for _ in 0..32 {
        let mut w = [0i16; FFT_SIZE];
        for s in w.iter_mut() {
            lcg = lcg.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            // top 16 bits, centred → uniform in [-8192, 8191]
            *s = ((lcg >> 16) as i32 - 32_768).clamp(-32_768, 32_767) as i16 / 4;
        }
        let bands = spectrum_dbfs(&w);
        for b in 0..SPECTRUM_BANDS {
            hold[b] = hold[b].max(bands[b]);
        }
    }
    for (b, &v) in hold.iter().enumerate() {
        assert!(v > -50.0, "band {b} stuck near floor under white noise: {v}");
        assert!(v < -5.0, "band {b} implausibly hot under white noise: {v}");
    }
}

/// Envelope: instant attack, BAR_RELEASE_DB/update release; the peak tick
/// decays slower than the bar and both settle back to the floor.
#[test]
fn envelope_attack_release_peak_hold() {
    let mut env = SpectrumEnvelope::new();
    let loud = [-10.0f32; SPECTRUM_BANDS];
    let quiet = [DBFS_FLOOR; SPECTRUM_BANDS];

    env.update(&loud);
    assert_eq!(env.bars()[0], -10.0, "attack must be instant");
    assert_eq!(env.peaks()[0], -10.0);

    env.update(&quiet);
    let bar = env.bars()[0];
    let peak = env.peaks()[0];
    assert!(
        (bar - (-10.0 - SpectrumEnvelope::BAR_RELEASE_DB)).abs() < 1e-4,
        "bar should release by {} dB, got {bar}",
        SpectrumEnvelope::BAR_RELEASE_DB
    );
    assert!(
        (peak - (-10.0 - SpectrumEnvelope::PEAK_DECAY_DB)).abs() < 1e-4,
        "peak should decay by {} dB, got {peak}",
        SpectrumEnvelope::PEAK_DECAY_DB
    );
    assert!(peak > bar, "peak tick must linger above the released bar");

    for _ in 0..100 {
        env.update(&quiet);
    }
    assert_eq!(env.bars(), &[DBFS_FLOOR; SPECTRUM_BANDS]);
    assert_eq!(env.peaks(), &[DBFS_FLOOR; SPECTRUM_BANDS]);

    env.update(&loud);
    env.reset();
    assert_eq!(env.bars(), &[DBFS_FLOOR; SPECTRUM_BANDS], "reset must floor the bars");
}
