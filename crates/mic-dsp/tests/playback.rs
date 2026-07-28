//! Host tests for the playback-side helpers (shared I2S TX seam, issue #23):
//! mono→stereo expansion and the SFX synths (beep tone + UI click).

use mic_dsp::{
    fill_click_mono_s16le, fill_ping_chime_mono_s16le, fill_tick_mono_s16le,
    fill_tone_mono_s16le, mono_to_stereo_le, CLICK_LEN, PING_CHIME_8K_LEN, PING_CHIME_LEN,
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

// === fill_ping_chime_mono_s16le (watch-to-watch ping, #35 / melody #58) =======

/// Zero-crossing count over a sample window — a cheap dominant-frequency
/// probe: a sine at f Hz crosses zero ~2f times per second.
fn crossings(buf: &[u8], from: usize, to: usize) -> usize {
    (from + 1..to)
        .filter(|&i| (s16(buf, i - 1) < 0) != (s16(buf, i) < 0))
        .count()
}

/// The chime fills exactly PING_CHIME_LEN bytes at 16 kHz (700 ms).
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

/// A 4-note RISING arpeggio C5→E5→G5→C6 (#58): each note's window rings at a
/// strictly higher rate than the last — verified by zero-crossing count.
/// Windows sit just after each note's onset (notes are 125 ms apart), where the
/// newest note leads: peaks now DESCEND with pitch (the #58b anti-shriek
/// voicing), so dominance comes from the previous note having decayed, not from
/// the new one being louder.
#[test]
fn ping_chime_rises_four_notes() {
    let mut buf = [0u8; PING_CHIME_LEN];
    fill_ping_chime_mono_s16le(&mut buf, 16_000);
    // ms → sample index at 16 kHz.
    let w = |a_ms: usize, b_ms: usize| crossings(&buf, a_ms * 16, b_ms * 16);
    // C5 ~523 Hz: 30..95 ms, alone (E5 enters at 125). ~2*523*0.065 ≈ 68.
    let c1 = w(30, 95);
    // E5 ~659 Hz: 155..220 ms. ~2*659*0.065 ≈ 86.
    let c2 = w(155, 220);
    // G5 ~784 Hz: 280..345 ms. ~2*784*0.065 ≈ 102.
    let c3 = w(280, 345);
    // C6 ~1047 Hz: 405..505 ms (the shimmer). ~2*1047*0.100 ≈ 209.
    let c4 = w(405, 505);
    assert!(c1 < c2, "C5 ({c1}) → E5 ({c2}) must rise");
    assert!(c2 < c3, "E5 ({c2}) → G5 ({c3}) must rise");
    assert!(c3 < c4, "G5 ({c3}) → C6 ({c4}) must rise");
    // Sanity on the octave leap: C6 is ~2x C5.
    assert!(c4 > 2 * c1, "top C6 ({c4}) should ring ~2x the root C5 ({c1})");
}

/// #58b anti-shriek voicing: the note peaks must DESCEND as the pitch climbs.
/// The first cut put the loudest note (C6 @ 9500) at the top of the arpeggio,
/// which a tiny watch speaker renders as a shriek — JP's "it's jarring". Lock
/// the shape in: the early (low) half of the chime must carry more level than
/// the late (high) half, so a future tweak can't silently re-invert it.
#[test]
fn ping_chime_weights_the_root_not_the_top() {
    let mut buf = [0u8; PING_CHIME_LEN];
    let n = fill_ping_chime_mono_s16le(&mut buf, 16_000);
    let samples = n / 2;
    let peak_in = |from: usize, to: usize| {
        (from..to.min(samples))
            .map(|i| s16(&buf, i).unsigned_abs())
            .max()
            .unwrap_or(0)
    };
    // Root region (C5 alone, 0..120 ms) vs the top-note region (C6, 375..700 ms).
    let root = peak_in(0, 120 * 16);
    let top = peak_in(390 * 16, samples);
    assert!(
        root > top,
        "root C5 ({root}) must be louder than top C6 ({top}) — a louder top \
         note is the shriek this voicing exists to avoid"
    );
    // And the top should be meaningfully softer, not marginally.
    assert!(top * 3 < root * 2, "top ({top}) should sit well under the root ({root})");
}

/// The bell decay must actually ring: 300 ms after the last note's onset the
/// chime is still sounding (a short/plucked tau reads as nervous and clipped,
/// which was part of the jarring character).
#[test]
fn ping_chime_rings_out_rather_than_stopping_dead() {
    let mut buf = [0u8; PING_CHIME_LEN];
    let n = fill_ping_chime_mono_s16le(&mut buf, 16_000);
    let samples = n / 2;
    // 600 ms in — past every onset, before the 45 ms fade — must still ring.
    let late = (600 * 16..(640 * 16).min(samples))
        .map(|i| s16(&buf, i).unsigned_abs())
        .max()
        .unwrap_or(0);
    assert!(late > 400, "chime should still be ringing at 600 ms, got {late}");
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

/// The watch stores the chime at 8 kHz (PING_CHIME_8K_LEN) to halve its heap
/// footprint — the #65 stack fix cost 12KB of main heap and the shade started
/// OOM'ing, so this buffer had to shrink. Pure sines <= 1046 Hz means 8 kHz is
/// still ~4x oversampled, but the SHAPE must survive the lower rate: same
/// duration, still a rising arpeggio, still root-weighted, still pop-free.
#[test]
fn ping_chime_at_8k_is_half_the_bytes_and_same_shape() {
    let mut b8 = [0u8; PING_CHIME_8K_LEN];
    let n8 = fill_ping_chime_mono_s16le(&mut b8, 8_000);
    assert_eq!(n8, PING_CHIME_8K_LEN, "8k form fills its buffer exactly");
    assert_eq!(PING_CHIME_8K_LEN * 2, PING_CHIME_LEN, "8k is exactly half");

    // Same wall-clock duration at the lower rate.
    assert_eq!(n8 / 2 * 1000 / 8_000, 700, "still 700 ms");

    // Pop-free edges, as at 16 kHz.
    assert_eq!(s16(&b8, 0), 0, "attack starts at zero");
    let samples = n8 / 2;
    let tail = (samples - 8..samples).map(|i| s16(&b8, i).unsigned_abs()).max().unwrap();
    assert!(tail < 500, "tail near-silent, got {tail}");

    // Root still outweighs the top note (the #58b anti-shriek property).
    let peak_in = |from: usize, to: usize| {
        (from..to.min(samples)).map(|i| s16(&b8, i).unsigned_abs()).max().unwrap_or(0)
    };
    let root = peak_in(0, 120 * 8);          // ms -> samples at 8 kHz
    let top = peak_in(390 * 8, samples);
    assert!(root > top, "root ({root}) must outweigh top ({top}) at 8k too");

    // Still a RISING arpeggio: zero-crossing rate climbs note to note.
    let w = |a_ms: usize, b_ms: usize| crossings(&b8, a_ms * 8, b_ms * 8);
    let (c1, c2, c3, c4) = (w(30, 95), w(155, 220), w(280, 345), w(405, 505));
    assert!(c1 < c2 && c2 < c3 && c3 < c4, "must rise: {c1} {c2} {c3} {c4}");
}
