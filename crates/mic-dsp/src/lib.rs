//! Pure DSP for the watch mic sound-level meter.
//!
//! `no_std`, no hardware / esp-hal deps → host-unit-testable. Uses `libm` for
//! sqrt/log10 so the code is identical on the riscv32 target and under
//! `cargo test` on the host.

#![no_std]

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
