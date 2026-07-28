//! Walkie-talkie codec (#71): G.711 µ-law + 16→8 kHz decimation + VOX frame
//! packing, for push-to-talk voice between watches over ESP-NOW.
//!
//! `no_std`, integer-only, **no FPU** — the C6 has none, so anything per-sample
//! must be shifts/tables. Host-testable by design: this crate holds all the
//! logic that can be wrong, so the firmware side is only plumbing (the
//! `mic-dsp` / `climate-model` pattern — firmware modules can't build on host
//! because `esp-hal`'s build script panics there).
//!
//! # Why µ-law at 8 kHz
//!
//! ESP-NOW caps a payload at 250 B. Minus a SMOLv1 prefix (~14 B) that leaves
//! ~236 B of audio per frame, so the encoding decides whether this is feasible:
//!
//! | encoding            | bitrate  | frames/s | |
//! |---------------------|----------|----------|--|
//! | 16 kHz s16 (raw mic)| 256 kbps | ~137/s   | at/past practical ESP-NOW |
//! | 8 kHz s16           | 128 kbps | ~68/s    | marginal |
//! | **8 kHz µ-law**     | **64 kbps** | **~34/s** | chosen |
//! | 8 kHz IMA-ADPCM     | 32 kbps  | ~17/s    | upgrade path |
//!
//! µ-law is what landline telephony used: perceptually ~12 bits in 8, and a
//! pure table/shift operation. The radio is also time-shared (WiFi XOR ESP-NOW
//! XOR BLE), so leaving headroom matters more than squeezing quality.

#![no_std]

/// Mono samples per VOX frame. 240 samples @ 8 kHz = **30 ms** of audio, and
/// 240 µ-law bytes + a 14 B SMOLv1 prefix = 254 B... which EXCEEDS the 250 B
/// ESP-NOW payload, so this is deliberately 224: 224 + 14 = 238 B, comfortably
/// inside 250 with room for the prefix to grow.
///
/// 224 samples @ 8 kHz = **28 ms** → ~35.7 frames/s → 64 kbps of payload.
pub const VOX_SAMPLES: usize = 224;

/// Encoded payload bytes per frame (µ-law is 1 byte per sample).
pub const VOX_PAYLOAD: usize = VOX_SAMPLES;

/// Source samples consumed per frame at the mic's 16 kHz (2:1 decimation).
pub const VOX_SRC_SAMPLES: usize = VOX_SAMPLES * 2;

/// Source bytes consumed per frame from a 16 kHz s16le mic buffer.
pub const VOX_SRC_BYTES: usize = VOX_SRC_SAMPLES * 2;

/// µ-law bias and clip, per G.711.
const MU_BIAS: i32 = 0x84;
const MU_CLIP: i32 = 32635;

/// Encode one 16-bit sample to G.711 µ-law.
///
/// Integer-only. Sign-magnitude with a segment (exponent) and 4-bit mantissa,
/// then bit-inverted — the standard on-wire form, so a decoder elsewhere
/// (or a host tool) interoperates.
pub fn mulaw_encode(sample: i16) -> u8 {
    /// Upper bound of each µ-law segment, per G.711. The segment is the index of
    /// the first bound the biased magnitude fits under. An earlier version tried
    /// to derive this by counting leading bits of `s >> 7`, which is NOT
    /// equivalent and decoded roughly 2x off (-8000 came back as -15996). The
    /// table is the definition; use it.
    const SEG_END: [i32; 8] = [0xFF, 0x1FF, 0x3FF, 0x7FF, 0xFFF, 0x1FFF, 0x3FFF, 0x7FFF];

    let mut s = sample as i32;
    let sign = if s < 0 {
        // -32768 negates out of i16 range; we're in i32 so this is safe, and the
        // clip below bounds it anyway.
        s = -s;
        0x80u8
    } else {
        0
    };
    if s > MU_CLIP {
        s = MU_CLIP;
    }
    s += MU_BIAS;

    let mut seg = 0usize;
    while seg < 8 && s > SEG_END[seg] {
        seg += 1;
    }
    if seg >= 8 {
        // Only reachable if MU_CLIP were raised past the last segment.
        return !(sign | 0x7F);
    }
    let mantissa = ((s >> (seg as i32 + 3)) & 0x0F) as u8;
    !(sign | ((seg as u8) << 4) | mantissa)
}

/// Decode one G.711 µ-law byte back to 16-bit.
pub fn mulaw_decode(byte: u8) -> i16 {
    let b = !byte;
    let sign = b & 0x80;
    let seg = (b >> 4) & 0x07;
    let mantissa = (b & 0x0F) as i32;
    let t = ((mantissa << 3) + MU_BIAS) << seg;
    // G.711: sign selects BIAS-t vs t-BIAS (not a plain negate of t).
    if sign != 0 {
        (MU_BIAS - t) as i16
    } else {
        (t - MU_BIAS) as i16
    }
}

/// Decimate 16 kHz s16le mono → 8 kHz mono samples by **averaging pairs**.
///
/// Averaging rather than dropping every other sample: a plain drop aliases
/// everything above 4 kHz straight back into the voice band, which on speech
/// sounds like added grit. A 2-tap average is a (weak but free) low-pass — one
/// add and one shift per output sample, no FPU.
///
/// Returns the number of output samples written. Ignores a trailing odd sample
/// and never writes past `out`.
pub fn decimate_16k_to_8k(src: &[u8], out: &mut [i16]) -> usize {
    let mut n = 0;
    let mut i = 0;
    // 4 source bytes = 2 samples = 1 output sample.
    while i + 4 <= src.len() && n < out.len() {
        let a = i16::from_le_bytes([src[i], src[i + 1]]) as i32;
        let b = i16::from_le_bytes([src[i + 2], src[i + 3]]) as i32;
        out[n] = ((a + b) / 2) as i16;
        n += 1;
        i += 4;
    }
    n
}

/// Encode a 16 kHz s16le mic buffer into µ-law payload bytes at 8 kHz.
///
/// Returns payload bytes written. Consumes `4 * returned` source bytes.
pub fn encode_frame(src: &[u8], payload: &mut [u8]) -> usize {
    let mut n = 0;
    let mut i = 0;
    while i + 4 <= src.len() && n < payload.len() {
        let a = i16::from_le_bytes([src[i], src[i + 1]]) as i32;
        let b = i16::from_le_bytes([src[i + 2], src[i + 3]]) as i32;
        payload[n] = mulaw_encode(((a + b) / 2) as i16);
        n += 1;
        i += 4;
    }
    n
}

/// Decode a µ-law payload back to **16 kHz** s16le, duplicating each sample
/// (zero-order hold 2x) so it feeds the watch's 16 kHz TX ring directly.
///
/// Returns bytes written to `out`. Needs `4 * payload.len()` bytes of room; a
/// short `out` stops cleanly on a whole-sample boundary rather than truncating
/// mid-sample.
pub fn decode_frame_to_16k(payload: &[u8], out: &mut [u8]) -> usize {
    let mut w = 0;
    for &b in payload {
        if w + 4 > out.len() {
            break;
        }
        let s = mulaw_decode(b).to_le_bytes();
        out[w] = s[0];
        out[w + 1] = s[1];
        out[w + 2] = s[0];
        out[w + 3] = s[1];
        w += 4;
    }
    w
}

/// Peak absolute amplitude of a 16 kHz s16le buffer — drives the PTT level bar
/// and lets the TX side skip sending near-silence.
pub fn peak_abs(src: &[u8]) -> u16 {
    let mut peak = 0u16;
    let mut i = 0;
    while i + 2 <= src.len() {
        let s = i16::from_le_bytes([src[i], src[i + 1]]);
        // -32768 has no positive counterpart; saturate rather than wrap.
        let a = s.unsigned_abs();
        if a > peak {
            peak = a;
        }
        i += 2;
    }
    peak
}

/// Sequence-gap classification for packet-loss concealment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeqStep {
    /// The expected next frame.
    InOrder,
    /// `n` frames were lost before this one — conceal by repeating the last.
    Gap(u16),
    /// Late/reordered/duplicate: already played past this. Drop it.
    Stale,
    /// First frame of a transmission.
    First,
}

/// Classify `seq` against the last accepted sequence number.
///
/// Wrapping-aware: a 16-bit counter rolls over every ~30 minutes of continuous
/// talk at ~36 frames/s, and treating a rollover as "stale" would mute the rest
/// of the transmission. Anything within a forward half-space counts as forward.
pub fn seq_step(last: Option<u16>, seq: u16) -> SeqStep {
    match last {
        None => SeqStep::First,
        Some(l) => {
            let d = seq.wrapping_sub(l);
            if d == 0 {
                SeqStep::Stale
            } else if d < 0x8000 {
                if d == 1 {
                    SeqStep::InOrder
                } else {
                    SeqStep::Gap(d - 1)
                }
            } else {
                SeqStep::Stale
            }
        }
    }
}
