//! Pure audio DSP for the watch: mic sound-level metering (RMS → dBFS) plus
//! the playback-side format conversion + SFX synthesis for the shared I2S TX
//! seam (`audio_out`, issue #23).
//!
//! `no_std`, no hardware / esp-hal deps → host-unit-testable. Uses `libm` for
//! sqrt/log10/sin so the code is identical on the riscv32 target and under
//! `cargo test` on the host.

#![no_std]

mod spectrum;
pub use spectrum::{
    spectrum_dbfs, SpectrumEnvelope, BAND_EDGE_BINS, FFT_SIZE, SPECTRUM_BANDS,
};

/// Lower clamp for the meter, in dBFS. Silence / near-silence reads here.
pub const DBFS_FLOOR: f32 = -60.0;

/// RMS level of a 16-bit PCM window in dBFS (0 dBFS = full scale).
///
/// DC is removed first (subtract the window mean) so a biased mic doesn't
/// inflate the level. Returns [`DBFS_FLOOR`] for an empty or silent window; the
/// result is clamped to `DBFS_FLOOR..=0.0`.
pub fn rms_dbfs(samples: &[i16]) -> f32 {
    if samples.is_empty() {
        return DBFS_FLOOR;
    }
    let n = samples.len() as f32;
    let mean = samples.iter().map(|&s| s as f32).sum::<f32>() / n;
    let sum_sq = samples
        .iter()
        .map(|&s| {
            let x = s as f32 - mean;
            x * x
        })
        .sum::<f32>();
    let rms = libm::sqrtf(sum_sq / n);
    if rms <= 0.0 {
        return DBFS_FLOOR;
    }
    let dbfs = 20.0 * libm::log10f(rms / 32768.0);
    dbfs.clamp(DBFS_FLOOR, 0.0)
}

/// Peak absolute amplitude of a DC-removed window: `max |sample - mean|`.
///
/// Companion to [`rms_dbfs`] for the scrolling waveform (per-window amplitude,
/// auto-scaled by the caller) and a fast peak envelope. DC is removed first so a
/// biased mic doesn't inflate it. Returns 0 for an empty window. Result fits
/// `u16` (a DC-removed 16-bit sample magnitude can reach ~32768).
pub fn peak_abs(samples: &[i16]) -> u16 {
    if samples.is_empty() {
        return 0;
    }
    let n = samples.len() as f32;
    let mean = samples.iter().map(|&s| s as f32).sum::<f32>() / n;
    let mut peak = 0.0f32;
    for &s in samples {
        let a = libm::fabsf(s as f32 - mean);
        if a > peak {
            peak = a;
        }
    }
    if peak > 65535.0 {
        65535
    } else {
        peak as u16
    }
}

// === Playback-side helpers (shared I2S TX seam, issue #23) ====================
//
// The watch's project-standard PCM format is MONO 16 kHz s16le (STT, HA
// speaker, bridge). The I2S TX ring runs 16 kHz 16-bit STEREO
// (Data16Channel16), so playback duplicates each mono sample into L/R.

/// Expand mono s16le PCM into interleaved stereo s16le (L = R = mono sample).
///
/// Consumes whole samples only: a trailing odd byte in `mono` is ignored, and
/// conversion stops early if `out` can't fit another 4-byte frame. Returns the
/// number of STEREO bytes written (always a multiple of 4).
pub fn mono_to_stereo_le(mono: &[u8], out: &mut [u8]) -> usize {
    let frames = (mono.len() / 2).min(out.len() / 4);
    for i in 0..frames {
        let lo = mono[2 * i];
        let hi = mono[2 * i + 1];
        out[4 * i] = lo;
        out[4 * i + 1] = hi;
        out[4 * i + 2] = lo;
        out[4 * i + 3] = hi;
    }
    frames * 4
}

/// Synthesize a mono s16le sine tone with linear attack/release ramps.
///
/// `ramp_ms` of linear fade-in and fade-out (pop insurance on the speaker amp)
/// is applied inside the `duration_ms` window; it is clamped so the two ramps
/// never overlap. Writes at most `buf.len()` bytes (whole samples) and returns
/// the number of MONO bytes written.
pub fn fill_tone_mono_s16le(
    buf: &mut [u8],
    sample_rate: u32,
    freq_hz: u32,
    duration_ms: u32,
    amplitude: i16,
    ramp_ms: u32,
) -> usize {
    let total = ((sample_rate * duration_ms / 1000) as usize).min(buf.len() / 2);
    let ramp = ((sample_rate * ramp_ms / 1000) as usize).min(total / 2);
    let w = 2.0 * core::f32::consts::PI * freq_hz as f32 / sample_rate as f32;
    for i in 0..total {
        let mut a = amplitude as f32 * libm::sinf(w * i as f32);
        if ramp > 0 {
            if i < ramp {
                a *= i as f32 / ramp as f32;
            } else if i >= total - ramp {
                a *= (total - 1 - i) as f32 / ramp as f32;
            }
        }
        let s = a as i16; // |a| <= amplitude by construction
        let b = s.to_le_bytes();
        buf[2 * i] = b[0];
        buf[2 * i + 1] = b[1];
    }
    total * 2
}

/// Length of the UI tap-click clip in mono BYTES at 16 kHz (12 ms).
pub const CLICK_LEN: usize = 192 * 2;

/// Synthesize the UI tap-click: a 12 ms exponentially-decaying 1.8 kHz sine
/// ("tick"), 0.5 ms linear attack, peak ~9000 (≈ −11 dBFS — subtle on the tiny
/// speaker). Mono s16le at `sample_rate`; returns MONO bytes written.
pub fn fill_click_mono_s16le(buf: &mut [u8], sample_rate: u32) -> usize {
    fill_click_with_peak(buf, sample_rate, 9000.0)
}

/// The every-touch tick (#49): the SAME 12 ms 1.8 kHz decaying "tick" as
/// [`fill_click_mono_s16le`] but QUIETER (peak ~6000 ≈ −15 dBFS) — texture,
/// not notification, when it plays on every tap instead of one opt-in control.
pub fn fill_tick_mono_s16le(buf: &mut [u8], sample_rate: u32) -> usize {
    fill_click_with_peak(buf, sample_rate, 6000.0)
}

/// Length of the watch-ping receiver chime in mono BYTES at 16 kHz (700 ms).
/// Prefer [`PING_CHIME_8K_LEN`] on the watch — see why there.
pub const PING_CHIME_LEN: usize = 11_200 * 2;

/// Length of the chime stored at **8 kHz** (700 ms) — 11 200 B, HALF of
/// [`PING_CHIME_LEN`].
///
/// The watch stores the chime at 8 kHz and the playback feeder duplicates each
/// sample back up to the 16 kHz TX ring. This is free quality-wise: the chime is
/// built from PURE SINES whose highest partial is the top C6 at 1046.5 Hz, so
/// 8 kHz sampling (Nyquist 4 kHz) is still ~4x oversampled.
///
/// It matters because the buffer is heap-resident for the life of the firmware,
/// and main-heap bytes are contested: growing the stack to clear the WiFi blob's
/// globals (#65) cost 12 KB of heap, which made the notification shade OOM
/// (`memory allocation of 4096 bytes failed` on a swipe-down). Halving this
/// buffer repays 11 200 B of that debt, so the stack can stay safe WITHOUT
/// starving the UI — instead of trading one crash for the other.
pub const PING_CHIME_8K_LEN: usize = 5_600 * 2;

/// Synthesize the watch-to-watch ping chime (#35, melody #58, voiced #58b): a
/// warm rising MAJOR ARPEGGIO — C5 → E5 → G5 → C6 (a C-major triad climbing an
/// octave, the classic "good news" motif) voiced like a struck bell rather than
/// a beeper. ~700 ms; mono s16le at `sample_rate`; returns MONO bytes written.
///
/// # Voicing (why these numbers)
///
/// The first cut was **jarring on-glass** — the fault was the envelope and the
/// balance, not the notes:
/// - **Descending peaks.** The old version made the TOP note the LOUDEST
///   (C6 @ 9500). High frequency at maximum amplitude is exactly what a tiny
///   watch speaker turns into a shriek. Real chimes put the weight in the root
///   and let the upper notes shimmer, so the peaks now fall 9500 → 5000 as the
///   pitch climbs. This is the single biggest change.
/// - **Raised-cosine attack (22 ms, was a linear 8 ms).** A fast linear ramp is
///   a transient — heard as a click in front of each note. A half-cosine bloom
///   over 22 ms has no corner in it, so notes arrive instead of hitting.
/// - **Long bell decay (tau 210-300 ms, was 70-90 ms).** Short taus read as
///   "plucked" and leave the arpeggio feeling clipped and nervous; a long tail
///   rings, and the top C6 gets the longest (300 ms) so it blooms and fades out
///   as the arrival rather than stopping dead.
/// - **Slower arpeggio (125 ms apart, was 110) + 45 ms cosine fade-out**
///   (was a 12 ms linear cut) — unhurried, and the tail lands on true silence.
///
/// Peaks are chosen so even a worst-case all-in-phase sum stays under ~12 000
/// (well below clip) — no normalization pass, no clipping, deterministic level.
pub fn fill_ping_chime_mono_s16le(buf: &mut [u8], sample_rate: u32) -> usize {
    /// One struck note: (start ms, length ms, freq Hz, peak, decay tau ms).
    /// Pitch and start time both ascend (the rising arpeggio — host-tested by
    /// zero-crossing rate per window); PEAK DESCENDS (see voicing notes).
    const NOTES: [(u32, u32, f32, f32, f32); 4] = [
        (0, 420, 523.25, 9_500.0, 200.0),   // C5 — the root carries the level
        (125, 380, 659.25, 6_800.0, 200.0), // E5
        (250, 340, 783.99, 4_600.0, 220.0), // G5
        (375, 325, 1046.50, 3_200.0, 300.0), // C6 — softest + longest: shimmer
    ];
    const TOTAL_MS: u32 = 700;
    const ATTACK_MS: f32 = 22.0;
    const FADE_MS: u32 = 45;
    /// Global ring-down applied over the WHOLE chime (tau, ms). Descending
    /// per-note peaks alone are NOT enough: four overlapping notes SUM, and the
    /// late region (where the high notes live) measured louder than the root
    /// even with the top note at half its level. This taper makes "later" mean
    /// "quieter" unconditionally, and is also just how a struck bell behaves —
    /// the whole body decays, not each partial independently.
    const RING_TAU_MS: f32 = 1_100.0;

    let sr = sample_rate as f32;
    let total = ((sample_rate * TOTAL_MS / 1000) as usize).min(buf.len() / 2);
    let fade = ((sample_rate * FADE_MS / 1000) as usize).min(total);
    let attack = (sr * ATTACK_MS / 1000.0).max(1.0);
    for i in 0..total {
        let mut a = 0.0f32;
        for (start_ms, len_ms, freq, peak, tau_ms) in NOTES {
            let s0 = (sample_rate * start_ms / 1000) as usize;
            let len = (sample_rate * len_ms / 1000) as usize;
            if i < s0 || i >= s0 + len {
                continue;
            }
            let t = (i - s0) as f32;
            // Raised-cosine bloom (no corner) × exponential bell decay.
            let bloom = if t < attack {
                0.5 * (1.0 - libm::cosf(core::f32::consts::PI * t / attack))
            } else {
                1.0
            };
            let env = libm::expf(-t / (sr * tau_ms / 1000.0)) * bloom;
            let w = 2.0 * core::f32::consts::PI * freq / sr;
            a += peak * env * libm::sinf(w * t);
        }
        // Global ring-down: the whole chime decays as one body.
        a *= libm::expf(-(i as f32) / (sr * RING_TAU_MS / 1000.0));
        if i >= total - fade {
            // Cosine fade-out — a linear cut still has a corner at the joint.
            let x = (total - 1 - i) as f32 / fade as f32;
            a *= 0.5 * (1.0 - libm::cosf(core::f32::consts::PI * x));
        }
        let s = a.clamp(i16::MIN as f32, i16::MAX as f32) as i16;
        let b = s.to_le_bytes();
        buf[2 * i] = b[0];
        buf[2 * i + 1] = b[1];
    }
    total * 2
}

fn fill_click_with_peak(buf: &mut [u8], sample_rate: u32, peak: f32) -> usize {
    const FREQ_HZ: f32 = 1800.0;
    let total = ((sample_rate as usize * 12 / 1000).min(buf.len() / 2)).max(0);
    let attack = (sample_rate as usize / 2000).max(1); // 0.5 ms
    let tau = sample_rate as f32 * 0.003; // 3 ms decay constant
    let w = 2.0 * core::f32::consts::PI * FREQ_HZ / sample_rate as f32;
    for i in 0..total {
        let env = libm::expf(-(i as f32) / tau)
            * if i < attack { i as f32 / attack as f32 } else { 1.0 };
        let s = (peak * env * libm::sinf(w * i as f32)) as i16;
        let b = s.to_le_bytes();
        buf[2 * i] = b[0];
        buf[2 * i + 1] = b[1];
    }
    total * 2
}
