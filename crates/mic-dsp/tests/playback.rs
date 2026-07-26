//! Host tests for the playback-side helpers (shared I2S TX seam, issue #23):
//! mono→stereo expansion and the SFX synths (beep tone + UI click).

use mic_dsp::{
    fill_click_mono_s16le, fill_ping_chime_mono_s16le, fill_tick_mono_s16le,
    fill_tone_mono_s16le, mono_to_stereo_le, CLICK_LEN, PING_CHIME_LEN,
};

fn s16(buf: &[u8], i: usize) -> i16 {
    i16::from_le_bytes([buf[2 * i], buf[2 * i + 1]])
}

// === mono_to_stereo_le ========================================================

/// Each mono sample is duplicated into L and R, preserving order + byte layout.
#[test]
fn stereo_duplicates_each_sample() {
    let mono: [i16; 3] = [1000, -2000, 32767];
    let mut mono_bytes = [0u8; 6];
    for (i, s) in mono.iter().enumerate() {
        mono_bytes[2 * i..2 * i + 2].copy_from_slice(&s.to_le_bytes());
    }
    let mut out = [0u8; 12];
    let n = mono_to_stereo_le(&mono_bytes, &mut out);
    assert_eq!(n, 12);
    for (i, &s) in mono.iter().enumerate() {
        assert_eq!(s16(&out, 2 * i), s, "L of frame {i}");
        assert_eq!(s16(&out, 2 * i + 1), s, "R of frame {i}");
    }
}

/// A trailing odd byte is ignored — never emit a half-sample.
#[test]
fn stereo_ignores_trailing_odd_byte() {
    let mono = [0x34u8, 0x12, 0xFF]; // one sample + a stray byte
    let mut out = [0u8; 8];
    let n = mono_to_stereo_le(&mono, &mut out);
    assert_eq!(n, 4);
    assert_eq!(s16(&out, 0), 0x1234);
    assert_eq!(s16(&out, 1), 0x1234);
}

/// Output space limits the conversion (whole 4-byte frames only).
#[test]
fn stereo_respects_out_capacity() {
    let mono = [1u8, 0, 2, 0, 3, 0]; // 3 samples
    let mut out = [0u8; 9]; // room for 2 frames + 1 stray byte
    let n = mono_to_stereo_le(&mono, &mut out);
    assert_eq!(n, 8);
    assert_eq!(s16(&out, 2), 2); // second frame L
    assert_eq!(out[8], 0, "stray byte untouched");
}

#[test]
fn stereo_empty_in_or_out_is_zero() {
    let mut out = [0u8; 8];
    assert_eq!(mono_to_stereo_le(&[], &mut out), 0);
    assert_eq!(mono_to_stereo_le(&[1, 0], &mut []), 0);
}

// === fill_tone_mono_s16le (Snake beep: 800 Hz / 50 ms) ========================

/// 50 ms at 16 kHz = 800 samples = 1600 mono bytes.
#[test]
fn tone_length_matches_duration() {
    let mut buf = [0u8; 1600];
    let n = fill_tone_mono_s16le(&mut buf, 16_000, 800, 50, 12_000, 2);
    assert_eq!(n, 1600);
}

/// The ramps make the edges soft: the first and last samples are (near) zero,
/// while the middle reaches most of the requested amplitude.
#[test]
fn tone_ramps_and_amplitude() {
    let mut buf = [0u8; 1600];
    let n = fill_tone_mono_s16le(&mut buf, 16_000, 800, 50, 12_000, 2);
    let samples = n / 2;
    assert_eq!(s16(&buf, 0), 0, "attack starts at zero");
    assert!(
        s16(&buf, samples - 1).unsigned_abs() < 500,
        "release ends near zero, got {}",
        s16(&buf, samples - 1)
    );
    let peak = (0..samples).map(|i| s16(&buf, i).unsigned_abs()).max().unwrap();
    assert!(peak > 10_000, "tone should reach near amplitude, got {peak}");
    assert!(peak <= 12_000, "tone must not exceed amplitude, got {peak}");
}

/// A tiny output buffer truncates to whole samples instead of panicking.
#[test]
fn tone_truncates_to_buffer() {
    let mut buf = [0u8; 7];
    let n = fill_tone_mono_s16le(&mut buf, 16_000, 800, 50, 12_000, 2);
    assert_eq!(n, 6);
}

// === fill_click_mono_s16le (UI tap click) =====================================

/// The click fills exactly CLICK_LEN bytes at 16 kHz (12 ms).
#[test]
fn click_length() {
    let mut buf = [0u8; CLICK_LEN];
    assert_eq!(fill_click_mono_s16le(&mut buf, 16_000), CLICK_LEN);
}

/// The click's envelope decays: the loudest sample sits in the first quarter
/// and the final samples are near-silent (no pop on release).
#[test]
fn click_decays() {
    let mut buf = [0u8; CLICK_LEN];
    let n = fill_click_mono_s16le(&mut buf, 16_000);
    let samples = n / 2;
    let (mut peak, mut peak_at) = (0u16, 0usize);
    for i in 0..samples {
        let a = s16(&buf, i).unsigned_abs();
        if a > peak {
            peak = a;
            peak_at = i;
        }
    }
    assert!(peak > 4_000, "click should be audible, got peak {peak}");
    assert!(peak <= 9_000, "click must stay subtle, got peak {peak}");
    assert!(peak_at < samples / 4, "peak should be early, at {peak_at}/{samples}");
    let tail = (samples - 8..samples).map(|i| s16(&buf, i).unsigned_abs()).max().unwrap();
    assert!(tail < 500, "tail should be near-silent, got {tail}");
}

// === fill_tick_mono_s16le (every-touch tick, #49) =============================

/// The tick is the same 12 ms clip length as the click.
#[test]
fn tick_length() {
    let mut buf = [0u8; CLICK_LEN];
    assert_eq!(fill_tick_mono_s16le(&mut buf, 16_000), CLICK_LEN);
}

// === fill_ping_chime_mono_s16le (watch-to-watch ping, #35) ====================

/// Zero-crossing count over a sample window — a cheap dominant-frequency
/// probe: a sine at f Hz crosses zero ~2f times per second.
fn crossings(buf: &[u8], from: usize, to: usize) -> usize {
    (from + 1..to)
        .filter(|&i| (s16(buf, i - 1) < 0) != (s16(buf, i) < 0))
        .count()
}

/// The chime fills exactly PING_CHIME_LEN bytes at 16 kHz (300 ms).
#[test]
fn ping_chime_length() {
    let mut buf = [0u8; PING_CHIME_LEN];
    assert_eq!(fill_ping_chime_mono_s16le(&mut buf, 16_000), PING_CHIME_LEN);
}

/// Pop-free edges: starts at zero (linear attack) and the master fade leaves
/// the final samples near-silent. Loud enough to notice, bounded below clip.
#[test]
fn ping_chime_edges_and_level() {
    let mut buf = [0u8; PING_CHIME_LEN];
    let n = fill_ping_chime_mono_s16le(&mut buf, 16_000);
    let samples = n / 2;
    assert_eq!(s16(&buf, 0), 0, "attack starts at zero");
    let tail = (samples - 8..samples).map(|i| s16(&buf, i).unsigned_abs()).max().unwrap();
    assert!(tail < 500, "tail should be near-silent, got {tail}");
    let peak = (0..samples).map(|i| s16(&buf, i).unsigned_abs()).max().unwrap();
    assert!(peak > 8_000, "chime should be clearly audible, got peak {peak}");
    assert!(peak <= 14_000, "chime must stay pleasant, got peak {peak}");
}

/// Two-tone and RISING: the early window is dominated by E5 (~659 Hz), the
/// late window by B5 (~988 Hz) — verified by zero-crossing rate, ±20%.
#[test]
fn ping_chime_rises_two_tones() {
    let mut buf = [0u8; PING_CHIME_LEN];
    fill_ping_chime_mono_s16le(&mut buf, 16_000);
    // Early window 10..90 ms (note 1 alone): expect ~2 * 659 * 0.080 ≈ 105.
    let c1 = crossings(&buf, 160, 1440);
    // Late window 150..270 ms (note 2 dominant): ~2 * 988 * 0.120 ≈ 237.
    let c2 = crossings(&buf, 2400, 4320);
    assert!((84..=127).contains(&c1), "early window should ring at ~659 Hz, got {c1} crossings");
    assert!((190..=285).contains(&c2), "late window should ring at ~988 Hz, got {c2} crossings");
    assert!(c2 > c1, "the chime must RISE (got {c1} then {c2})");
}

/// The every-touch tick is strictly QUIETER than the launch click (texture,
/// not notification): audible, but peaking at ~6000 vs the click's ~9000.
#[test]
fn tick_is_quieter_than_click() {
    let mut click = [0u8; CLICK_LEN];
    let mut tick = [0u8; CLICK_LEN];
    fill_click_mono_s16le(&mut click, 16_000);
    let n = fill_tick_mono_s16le(&mut tick, 16_000);
    let peak = |b: &[u8]| (0..n / 2).map(|i| s16(b, i).unsigned_abs()).max().unwrap();
    let (cp, tp) = (peak(&click), peak(&tick));
    assert!(tp > 2_500, "tick should still be audible, got peak {tp}");
    assert!(tp <= 6_000, "tick must stay subtle, got peak {tp}");
    assert!(tp < cp, "tick ({tp}) must be quieter than click ({cp})");
}
