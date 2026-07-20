//! Shared RSSI smoothing + proximity mapping for roster-consuming screens
//! (Marauder's Watch #58, treasure-hunt #60, finder, bench).
//!
//! Pure, `no_std`, host-testable — no radio, no esp-hal. A consumer of the
//! ESP-NOW roster RSSI the mesh already tracks (no new frames). One shared
//! smoother + one proximity mapping so every screen reads a peer as equally
//! "near". Ported verbatim from smol `rust/clock/src/rssi.rs` into a standalone
//! crate so the watch's game layer (hunt/finder/bench) shares the exact logic.
//!
//! Raw ESP-NOW RSSI swings ±5–10 dB frame-to-frame from multipath, so an
//! exponentially-weighted moving average (EWMA) is applied per peer before any
//! bar/word/trend is derived. All integer math — no `f32` on the `no_std` path.

#![no_std]

/// EWMA factor as a /256 fixed-point numerator. α ≈ 0.30 → `77/256`. A new sample
/// moves the smoothed value ~30% of the way toward it: fast enough to track a
/// walking user, slow enough to kill the per-frame jitter.
const ALPHA_NUM: i32 = 77;

/// Max distinct peers tracked. The fleet is tiny; a linear scan over this is free.
const CAP: usize = 16;

/// Per-peer smoothed-RSSI table, keyed by node id. Fixed capacity, heap-free.
pub struct RssiSmoother {
    ids: [u8; CAP],
    /// Smoothed dBm (integer). Valid for `..len`.
    vals: [i32; CAP],
    len: usize,
}

impl Default for RssiSmoother {
    fn default() -> Self {
        Self::new()
    }
}

impl RssiSmoother {
    pub const fn new() -> Self {
        Self {
            ids: [0; CAP],
            vals: [0; CAP],
            len: 0,
        }
    }

    /// Fold a fresh raw RSSI for `id` into its EWMA and return the smoothed value.
    /// First sight seeds with the raw sample (no ramp-in from zero). If the table
    /// is ever full, an unknown id returns the raw sample unsmoothed (still
    /// correct, just un-averaged).
    pub fn update(&mut self, id: u8, raw: i32) -> i32 {
        for i in 0..self.len {
            if self.ids[i] == id {
                // smoothed += (raw - smoothed) * α
                self.vals[i] += (raw - self.vals[i]) * ALPHA_NUM / 256;
                return self.vals[i];
            }
        }
        if self.len < CAP {
            self.ids[self.len] = id;
            self.vals[self.len] = raw;
            self.len += 1;
        }
        raw
    }

    /// The smoothed value for `id` without updating it, if seen. Used by the
    /// treasure-hunt to read the target between updates.
    pub fn get(&self, id: u8) -> Option<i32> {
        for i in 0..self.len {
            if self.ids[i] == id {
                return Some(self.vals[i]);
            }
        }
        None
    }
}

/// Coarse nearness tiers from a (smoothed) RSSI. Thresholds per the #58 spec.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Proximity {
    Here,
    Near,
    Room,
    Far,
    Gone,
}

/// Map a smoothed RSSI (dBm) to a nearness tier.
pub fn proximity(rssi: i32) -> Proximity {
    if rssi >= -45 {
        Proximity::Here
    } else if rssi >= -60 {
        Proximity::Near
    } else if rssi >= -75 {
        Proximity::Room
    } else if rssi >= -88 {
        Proximity::Far
    } else {
        Proximity::Gone
    }
}

/// Fixed-width (4 char) label for a tier — padded so columns don't ragged.
pub fn label(p: Proximity) -> &'static str {
    match p {
        Proximity::Here => "HERE",
        Proximity::Near => "NEAR",
        Proximity::Room => "ROOM",
        Proximity::Far => "FAR ",
        Proximity::Gone => "GONE",
    }
}

/// Signal-bar count (0..=4) for a tier — the phone-style strength glyph:
/// `Here`=4 … `Gone`=0.
pub fn tier_bars(p: Proximity) -> u8 {
    match p {
        Proximity::Here => 4,
        Proximity::Near => 3,
        Proximity::Room => 2,
        Proximity::Far => 1,
        Proximity::Gone => 0,
    }
}

/// Fill length in pixels (0..=`width`) for a proximity bar: linear over the useful
/// RSSI span `[-90, -35] → [0, width]`, clamped. Used by the treasure-hunt's hero
/// bar.
pub fn bar_px(rssi: i32, width: i32) -> i32 {
    let clamped = rssi.clamp(-90, -35);
    ((clamped + 90) * width) / 55
}

/// A tiny fixed-capacity, heap-free line builder (~one short OLED row). Overflow
/// is silently dropped — lines built here are bounded and short by construction.
/// (Slint renderers can use `SharedString` instead; kept for parity + reuse.)
pub struct Line {
    buf: [u8; 24],
    len: usize,
}

impl Default for Line {
    fn default() -> Self {
        Self::new()
    }
}

impl Line {
    pub fn new() -> Self {
        Self { buf: [0; 24], len: 0 }
    }
    pub fn as_str(&self) -> &str {
        core::str::from_utf8(&self.buf[..self.len]).unwrap_or("")
    }
}

impl core::fmt::Write for Line {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        for &b in s.as_bytes() {
            if self.len < self.buf.len() {
                self.buf[self.len] = b;
                self.len += 1;
            }
        }
        Ok(())
    }
}

/// Left-truncate to at most `n` bytes, on a UTF-8 char boundary — never panics.
///
/// Fed **untrusted mesh peer names** (arrive over ESP-NOW, attacker-controllable,
/// not guaranteed ASCII), so it must not byte-slice mid-multibyte-char (that would
/// panic "not a char boundary" = a remote-crash vector). Walks back from `n` to the
/// largest char boundary ≤ `n`. (Diverges from smol's verbatim `clip` on purpose —
/// smol carries the same latent panic; flagged for reverse-upstream.)
pub fn clip(s: &str, n: usize) -> &str {
    if s.len() <= n {
        return s;
    }
    let mut end = n;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}
