//! #30 — real-FFT spectrum analyzer for the Sound app (factory-firmware parity).
//!
//! 256-point real FFT (microfft, no_std/no-alloc) over one 16 kHz mono capture
//! window (`MIC_CH` chunks are exactly 256 samples ≈ 16 ms), Hann-windowed,
//! magnitudes folded into **12 log-spaced bands 80 Hz → 8 kHz**, each reported
//! in dBFS on the same `[-60, 0]` scale as the RMS meter ([`crate::rms_dbfs`]).
//!
//! CPU note: the C6 is RV32IMAC — **no FPU** — so the f32 FFT runs in
//! softfloat (~a few ms at 160 MHz, NOT the sub-millisecond an FPU would
//! give). The firmware therefore runs ONE FFT per Sound-screen tick (~15 Hz,
//! the latest window only), not one per captured chunk.

use crate::DBFS_FLOOR;

/// Number of spectrum bands (vertical bars in the Sound app).
pub const SPECTRUM_BANDS: usize = 12;

/// FFT length: one capture window (256 samples @ 16 kHz ≈ 16 ms, 62.5 Hz/bin).
pub const FFT_SIZE: usize = 256;

/// Band edges as FFT bin indices; band `b` covers bins
/// `BAND_EDGE_BINS[b] .. BAND_EDGE_BINS[b+1]` (end-exclusive).
///
/// Derived from 12 log-spaced edges 80 Hz → 8 kHz (`80 · 100^(k/12)`), rounded
/// to the nearest 62.5 Hz bin and forced strictly increasing so every band
/// owns ≥ 1 bin. Nominal band ranges (Hz):
///
/// | band |  0  |  1  |  2  |  3  |  4  |  5  |  6   |  7   |  8   |  9   |  10  |  11  |
/// |------|-----|-----|-----|-----|-----|-----|------|------|------|------|------|------|
/// | from |  80 | 117 | 172 | 253 | 371 | 545 |  800 | 1174 | 1724 | 2530 | 3713 | 5450 |
/// | to   | 117 | 172 | 253 | 371 | 545 | 800 | 1174 | 1724 | 2530 | 3713 | 5450 | 8000 |
///
/// Bin 0 (DC) is never read; the top band ends at bin 127 so the Nyquist term
/// (packed by microfft into `bins[0].im`) is never read either.
pub const BAND_EDGE_BINS: [usize; SPECTRUM_BANDS + 1] =
    [1, 2, 3, 4, 6, 9, 13, 19, 28, 40, 59, 87, 128];

/// Per-band dBFS spectrum of one capture window.
///
/// DC is removed (mean subtraction — a biased mic would otherwise leak through
/// the Hann mainlobe into bands 0–1), the window is Hann-weighted to stop
/// spectral leakage smearing the bars, and each band reports
/// `20·log10(√Σ|X_k|² / (N/4 · 32768))` clamped to `[DBFS_FLOOR, 0]` — i.e. a
/// full-scale on-bin sine rails its band at ~0 dBFS, matching the meter scale.
///
/// `samples` is one mono window; longer input is truncated to [`FFT_SIZE`],
/// shorter input is zero-padded (the mic path always delivers exactly 256).
/// Empty input returns all-floor.
pub fn spectrum_dbfs(samples: &[i16]) -> [f32; SPECTRUM_BANDS] {
    let mut out = [DBFS_FLOOR; SPECTRUM_BANDS];
    if samples.is_empty() {
        return out;
    }
    let n = samples.len().min(FFT_SIZE);
    let mean = samples[..n].iter().map(|&s| s as f32).sum::<f32>() / n as f32;
    let mut buf = [0.0f32; FFT_SIZE];
    for i in 0..n {
        buf[i] = (samples[i] as f32 - mean) * HANN_256[i];
    }
    let bins = microfft::real::rfft_256(&mut buf);
    // Full-scale reference: an on-bin sine of amplitude A yields a peak-bin
    // magnitude of A·N/2·CG with Hann coherent gain CG ≈ 0.5 → A·N/4.
    const FS_REF: f32 = (FFT_SIZE as f32 / 4.0) * 32768.0;
    for b in 0..SPECTRUM_BANDS {
        let mut power = 0.0f32;
        for k in BAND_EDGE_BINS[b]..BAND_EDGE_BINS[b + 1] {
            power += bins[k].re * bins[k].re + bins[k].im * bins[k].im;
        }
        let level = libm::sqrtf(power) / FS_REF;
        if level > 0.0 {
            out[b] = (20.0 * libm::log10f(level)).clamp(DBFS_FLOOR, 0.0);
        }
    }
    out
}

/// Per-band bar envelope + peak-hold, the meter's feel applied to 12 bands
/// (see the `meter_env` / `meter_peak` handling in the firmware main loop):
/// fast attack (a bar jumps straight to a louder level), slow release (speech
/// visibly fills and holds instead of collapsing between syllables), and a
/// slower-decaying peak tick that lingers after transients.
///
/// Decay constants are per-[`update`](Self::update) call, tuned for the Sound
/// screen's ~15 Hz tick (one call per rendered frame).
pub struct SpectrumEnvelope {
    env: [f32; SPECTRUM_BANDS],
    peak: [f32; SPECTRUM_BANDS],
}

impl SpectrumEnvelope {
    /// Bar release per update: 3 dB per ~66 ms tick ≈ 45 dB/s — the same rate
    /// as the meter bar's 1.5 dB per 33 ms.
    pub const BAR_RELEASE_DB: f32 = 3.0;
    /// Peak-tick decay per update: 1 dB per tick ≈ 15 dB/s, matching the
    /// meter's peak marker (0.5 dB per 33 ms).
    pub const PEAK_DECAY_DB: f32 = 1.0;

    pub const fn new() -> Self {
        Self { env: [DBFS_FLOOR; SPECTRUM_BANDS], peak: [DBFS_FLOOR; SPECTRUM_BANDS] }
    }

    /// Drop every band back to the floor (Sound-screen open/close).
    pub fn reset(&mut self) {
        self.env = [DBFS_FLOOR; SPECTRUM_BANDS];
        self.peak = [DBFS_FLOOR; SPECTRUM_BANDS];
    }

    /// Fold one spectrum frame ([`spectrum_dbfs`]) into the envelopes.
    pub fn update(&mut self, bands: &[f32; SPECTRUM_BANDS]) {
        for b in 0..SPECTRUM_BANDS {
            self.env[b] = bands[b].max(self.env[b] - Self::BAR_RELEASE_DB).max(DBFS_FLOOR);
            self.peak[b] = (self.peak[b] - Self::PEAK_DECAY_DB).max(bands[b]).max(DBFS_FLOOR);
        }
    }

    /// Bar heights, dBFS in `[-60, 0]`.
    pub fn bars(&self) -> &[f32; SPECTRUM_BANDS] {
        &self.env
    }

    /// Peak-hold ticks, dBFS in `[-60, 0]`.
    pub fn peaks(&self) -> &[f32; SPECTRUM_BANDS] {
        &self.peak
    }
}

impl Default for SpectrumEnvelope {
    fn default() -> Self {
        Self::new()
    }
}

/// Symmetric Hann window, `w[n] = 0.5·(1 − cos(2πn/(N−1)))`, N = 256.
/// Literal table (1 KB ROM) — computed by the Python one-liner in the module
/// git history; regenerating: `0.5*(1-math.cos(2*math.pi*n/255))` for n in 0..256.
#[rustfmt::skip]
static HANN_256: [f32; 256] = [
    0.00000000, 0.00015177, 0.00060700, 0.00136541, 0.00242654, 0.00378975, 0.00545420, 0.00741888,
    0.00968261, 0.01224402, 0.01510153, 0.01825343, 0.02169779, 0.02543253, 0.02945537, 0.03376389,
    0.03835545, 0.04322727, 0.04837640, 0.05379971, 0.05949390, 0.06545553, 0.07168096, 0.07816643,
    0.08490798, 0.09190154, 0.09914286, 0.10662753, 0.11435102, 0.12230863, 0.13049554, 0.13890677,
    0.14753723, 0.15638166, 0.16543470, 0.17469085, 0.18414450, 0.19378990, 0.20362120, 0.21363243,
    0.22381751, 0.23417027, 0.24468440, 0.25535354, 0.26617120, 0.27713082, 0.28822574, 0.29944923,
    0.31079447, 0.32225458, 0.33382260, 0.34549150, 0.35725421, 0.36910357, 0.38103240, 0.39303346,
    0.40509945, 0.41722306, 0.42939692, 0.44161365, 0.45386582, 0.46614600, 0.47844673, 0.49076055,
    0.50307997, 0.51539753, 0.52770574, 0.53999713, 0.55226423, 0.56449961, 0.57669583, 0.58884548,
    0.60094120, 0.61297564, 0.62494149, 0.63683150, 0.64863843, 0.66035512, 0.67197446, 0.68348940,
    0.69489294, 0.70617816, 0.71733821, 0.72836632, 0.73925578, 0.75000000, 0.76059244, 0.77102668,
    0.78129638, 0.79139530, 0.80131732, 0.81105641, 0.82060666, 0.82996227, 0.83911756, 0.84806697,
    0.85680508, 0.86532657, 0.87362627, 0.88169914, 0.88954029, 0.89714494, 0.90450850, 0.91162647,
    0.91849455, 0.92510857, 0.93146450, 0.93755849, 0.94338684, 0.94894602, 0.95423264, 0.95924349,
    0.96397554, 0.96842592, 0.97259191, 0.97647100, 0.98006082, 0.98335920, 0.98636414, 0.98907380,
    0.99148655, 0.99360092, 0.99541563, 0.99692957, 0.99814183, 0.99905166, 0.99965853, 0.99996206,
    0.99996206, 0.99965853, 0.99905166, 0.99814183, 0.99692957, 0.99541563, 0.99360092, 0.99148655,
    0.98907380, 0.98636414, 0.98335920, 0.98006082, 0.97647100, 0.97259191, 0.96842592, 0.96397554,
    0.95924349, 0.95423264, 0.94894602, 0.94338684, 0.93755849, 0.93146450, 0.92510857, 0.91849455,
    0.91162647, 0.90450850, 0.89714494, 0.88954029, 0.88169914, 0.87362627, 0.86532657, 0.85680508,
    0.84806697, 0.83911756, 0.82996227, 0.82060666, 0.81105641, 0.80131732, 0.79139530, 0.78129638,
    0.77102668, 0.76059244, 0.75000000, 0.73925578, 0.72836632, 0.71733821, 0.70617816, 0.69489294,
    0.68348940, 0.67197446, 0.66035512, 0.64863843, 0.63683150, 0.62494149, 0.61297564, 0.60094120,
    0.58884548, 0.57669583, 0.56449961, 0.55226423, 0.53999713, 0.52770574, 0.51539753, 0.50307997,
    0.49076055, 0.47844673, 0.46614600, 0.45386582, 0.44161365, 0.42939692, 0.41722306, 0.40509945,
    0.39303346, 0.38103240, 0.36910357, 0.35725421, 0.34549150, 0.33382260, 0.32225458, 0.31079447,
    0.29944923, 0.28822574, 0.27713082, 0.26617120, 0.25535354, 0.24468440, 0.23417027, 0.22381751,
    0.21363243, 0.20362120, 0.19378990, 0.18414450, 0.17469085, 0.16543470, 0.15638166, 0.14753723,
    0.13890677, 0.13049554, 0.12230863, 0.11435102, 0.10662753, 0.09914286, 0.09190154, 0.08490798,
    0.07816643, 0.07168096, 0.06545553, 0.05949390, 0.05379971, 0.04837640, 0.04322727, 0.03835545,
    0.03376389, 0.02945537, 0.02543253, 0.02169779, 0.01825343, 0.01510153, 0.01224402, 0.00968261,
    0.00741888, 0.00545420, 0.00378975, 0.00242654, 0.00136541, 0.00060700, 0.00015177, 0.00000000,
];
