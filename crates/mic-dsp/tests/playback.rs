//! Host tests for the playback-side helpers (shared I2S TX seam, issue #23):
//! mono→stereo expansion and the SFX synths (beep tone + UI click).

use mic_dsp::{fill_click_mono_s16le, fill_tone_mono_s16le, mono_to_stereo_le, CLICK_LEN};

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
