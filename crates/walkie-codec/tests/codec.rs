//! Host tests for the walkie-talkie codec (#71).
//!
//! Prior art: atomic14/esp32-walkie-talkie runs **16 kHz, 8-bit LINEAR**
//! (`(sample + 32768) >> 8`), 1 byte/sample, 250 B ESP-NOW packets — i.e.
//! 128 kbps at ~64 packets/s, and it works in the field. That validates the
//! transport. We deliberately differ in two ways, and these tests pin both:
//!
//! 1. **µ-law, not linear 8-bit.** Linear truncation keeps ~48 dB SNR at full
//!    scale but collapses at low level — quiet speech lands in the bottom few
//!    bits. µ-law holds roughly constant SNR across the range, which is exactly
//!    why telephony chose it. `mulaw_beats_linear8_at_low_level` measures it.
//! 2. **8 kHz, not 16 kHz** → 64 kbps, half their bitrate. They run a dedicated
//!    radio doing nothing else; this watch time-shares one radio between WiFi,
//!    BLE and the mesh, and repaints the screen for ~200 ms at a time. Headroom
//!    is worth more to us than bandwidth.

use walkie_codec::*;

// --- µ-law round trip ------------------------------------------------------

/// Round-tripping must stay within µ-law's quantisation envelope across the
/// whole range — including the extremes, where a naive implementation wraps.
#[test]
fn mulaw_round_trip_bounded_error() {
    for s in [
        i16::MIN, -32000, -20000, -8000, -1000, -100, -1, 0, 1, 100, 1000, 8000, 20000, 32000,
        i16::MAX,
    ] {
        let back = mulaw_decode(mulaw_encode(s));
        // µ-law is logarithmic: absolute error grows with magnitude, so bound it
        // relative to the sample plus a floor for the near-zero segment.
        let tol = (s.unsigned_abs() as i32 / 16) + 132;
        let err = (back as i32 - s as i32).abs();
        assert!(err <= tol, "s={s} back={back} err={err} > tol={tol}");
    }
}

/// Sign must survive, and zero must not drift — a DC offset on every frame
/// would be audible as a click at frame boundaries.
#[test]
fn mulaw_preserves_sign_and_near_zero() {
    assert!(mulaw_decode(mulaw_encode(1000)) > 0);
    assert!(mulaw_decode(mulaw_encode(-1000)) < 0);
    assert!(mulaw_decode(mulaw_encode(0)).abs() < 132, "zero must stay near zero");
}

/// Monotonic: louder in must never come out quieter. A broken segment/mantissa
/// split shows up here and nowhere else.
#[test]
fn mulaw_is_monotonic() {
    let mut prev = i32::MIN;
    let mut s = -32000i32;
    while s <= 32000 {
        let v = mulaw_decode(mulaw_encode(s as i16)) as i32;
        assert!(v >= prev, "not monotonic at {s}: {v} < {prev}");
        prev = v;
        s += 250;
    }
}

/// THE design justification: at low level µ-law must beat the reference
/// project's linear 8-bit conversion. This is the whole reason for the choice.
#[test]
fn mulaw_beats_linear8_at_low_level() {
    // The reference implementation, verbatim.
    fn linear8(s: i16) -> u8 {
        ((s as i32 + 32768) >> 8) as u8
    }
    fn linear8_back(b: u8) -> i16 {
        (((b as i32) << 8) - 32768) as i16
    }
    let mut mu_err = 0i64;
    let mut lin_err = 0i64;
    // Quiet speech: a few hundred LSB, where linear 8-bit has ~2 bits left.
    for s in (-800..=800).step_by(7) {
        let s = s as i16;
        mu_err += (mulaw_decode(mulaw_encode(s)) as i64 - s as i64).abs();
        lin_err += (linear8_back(linear8(s)) as i64 - s as i64).abs();
    }
    assert!(
        mu_err * 2 < lin_err,
        "mu-law should be far better at low level: mu={mu_err} linear={lin_err}"
    );
}

// --- decimation ------------------------------------------------------------

/// Averaging pairs, not dropping: DC must pass through untouched.
#[test]
fn decimation_halves_and_preserves_dc() {
    let mut src = Vec::new();
    for _ in 0..8 {
        src.extend_from_slice(&1000i16.to_le_bytes());
    }
    let mut out = [0i16; 8];
    let n = decimate_16k_to_8k(&src, &mut out);
    assert_eq!(n, 4, "8 samples in -> 4 out");
    assert!(out[..4].iter().all(|&v| v == 1000), "DC must survive: {:?}", &out[..4]);
}

/// A full-rate alternating signal (Nyquist at 16 kHz) must average to ~zero
/// rather than alias down into the voice band as a loud tone — the reason for
/// averaging instead of dropping every other sample.
#[test]
fn decimation_attenuates_nyquist_instead_of_aliasing() {
    let mut src = Vec::new();
    for i in 0..16 {
        let v: i16 = if i % 2 == 0 { 20000 } else { -20000 };
        src.extend_from_slice(&v.to_le_bytes());
    }
    let mut out = [0i16; 8];
    let n = decimate_16k_to_8k(&src, &mut out);
    assert_eq!(n, 8);
    let peak = out[..n].iter().map(|v| v.unsigned_abs()).max().unwrap();
    assert!(peak < 500, "Nyquist should cancel, got peak {peak}");
}

/// Odd trailing bytes and short outputs must truncate cleanly, never panic.
#[test]
fn decimation_handles_ragged_input() {
    let mut out = [0i16; 4];
    assert_eq!(decimate_16k_to_8k(&[], &mut out), 0);
    assert_eq!(decimate_16k_to_8k(&[1, 2, 3], &mut out), 0); // < one output sample
    let src = vec![0u8; 4 * 10];
    assert_eq!(decimate_16k_to_8k(&src, &mut out), 4, "bounded by out len");
}

// --- frame encode/decode ---------------------------------------------------

/// A full frame must fit ESP-NOW: payload + a SMOLv1 prefix under 250 B.
#[test]
fn frame_fits_esp_now_payload() {
    assert_eq!(VOX_PAYLOAD, VOX_SAMPLES);
    let prefix = 14; // "SMOLv1 VOX NNN SSSSS " worst case
    assert!(
        VOX_PAYLOAD + prefix <= 250,
        "frame {} + prefix {} exceeds the 250 B ESP-NOW limit",
        VOX_PAYLOAD,
        prefix
    );
    // And the frame should be a sane duration: 20-40 ms.
    let ms = VOX_SAMPLES * 1000 / 8000;
    assert!((20..=40).contains(&ms), "frame is {ms} ms");
}

/// encode -> decode must return 16 kHz bytes (2x samples) with the shape intact.
#[test]
fn encode_decode_round_trips_to_16k() {
    // 16 kHz ramp.
    let mut src = Vec::new();
    for i in 0..VOX_SRC_SAMPLES {
        let v = ((i as i32 * 37) % 20000 - 10000) as i16;
        src.extend_from_slice(&v.to_le_bytes());
    }
    let mut payload = [0u8; VOX_PAYLOAD];
    let n = encode_frame(&src, &mut payload);
    assert_eq!(n, VOX_SAMPLES, "one payload byte per 8 kHz sample");

    let mut out = vec![0u8; n * 4];
    let w = decode_frame_to_16k(&payload[..n], &mut out);
    assert_eq!(w, n * 4, "each byte -> 2 samples of 16 kHz s16le");
    // Zero-order hold: consecutive pairs must be identical.
    for i in 0..n {
        let a = i16::from_le_bytes([out[4 * i], out[4 * i + 1]]);
        let b = i16::from_le_bytes([out[4 * i + 2], out[4 * i + 3]]);
        assert_eq!(a, b, "sample {i} not duplicated");
    }
}

/// A short output buffer must stop on a whole-sample boundary — a half-written
/// sample would be heard as a click and would desync the stream.
#[test]
fn decode_short_output_stops_on_sample_boundary() {
    let payload = [0x7Fu8; 16];
    let mut out = [0u8; 10]; // room for 2 full samples (8 B) + 2 spare
    let w = decode_frame_to_16k(&payload, &mut out);
    assert_eq!(w % 4, 0, "must not truncate mid-sample, wrote {w}");
    assert!(w <= 10);
}

#[test]
fn peak_abs_handles_extremes_without_wrapping() {
    let mut src = Vec::new();
    src.extend_from_slice(&i16::MIN.to_le_bytes()); // -32768 has no +counterpart
    assert_eq!(peak_abs(&src), 32768);
    assert_eq!(peak_abs(&[]), 0);
    assert_eq!(peak_abs(&[0x00, 0x00]), 0);
}

// --- sequence / packet-loss concealment ------------------------------------

#[test]
fn seq_detects_order_gaps_and_staleness() {
    assert_eq!(seq_step(None, 5), SeqStep::First);
    assert_eq!(seq_step(Some(5), 6), SeqStep::InOrder);
    assert_eq!(seq_step(Some(5), 9), SeqStep::Gap(3), "3 frames lost");
    assert_eq!(seq_step(Some(5), 5), SeqStep::Stale, "duplicate");
    assert_eq!(seq_step(Some(9), 5), SeqStep::Stale, "reordered/late");
}

/// A 16-bit counter wraps after ~30 min of continuous talk at ~36 frames/s.
/// Treating the rollover as stale would mute the rest of the transmission.
#[test]
fn seq_survives_u16_rollover() {
    assert_eq!(seq_step(Some(0xFFFF), 0), SeqStep::InOrder);
    assert_eq!(seq_step(Some(0xFFFE), 1), SeqStep::Gap(2));
    // Still correctly rejects a genuinely old frame across the wrap.
    assert_eq!(seq_step(Some(0x0002), 0xFFF0), SeqStep::Stale);
}
