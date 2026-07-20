//! Treasure-hunt (#60) game LOGIC — an RSSI warmer/colder hunt: pick a target
//! peer, walk toward it guided by smoothed signal strength.
//!
//! Pure, `no_std`, host-testable — no radio, no rendering (smol's `hunt.rs` mixed
//! both; this is the logic half, the Slint `hunt.slint` page is the view). Feed it
//! the target's raw RSSI each tick (from the mesh roster the watch already tracks —
//! no new frames); it returns a [`HuntView`] for the page to render.
//!
//! Smoothing + proximity come from [`rssi`] (shared with the mesh roster's screens).
//! Trend is smoothed-now vs ~[`TREND_LAG_MS`]-ago with a ±[`TREND_DEADBAND`] dB
//! deadband so standing still never flickers; FOUND is hold-to-confirm so a lucky
//! spike can't declare victory.

#![no_std]

use rssi::{Proximity, RssiSmoother};

/// Compare the smoothed RSSI against its value this many ms ago for the trend.
pub const TREND_LAG_MS: u64 = 1500;
/// Trend deadband (dB): |delta| below this reads SAME (below the noise floor).
pub const TREND_DEADBAND: i32 = 2;
/// Smoothed RSSI (dBm) at/above which the target counts as "found"…
pub const FOUND_RSSI: i32 = -40;
/// …once held for this long (a lucky spike can't declare victory).
pub const FOUND_HOLD_MS: u64 = 1000;
/// Seed value for an unseen target (very weak).
const UNSEEN: i32 = -100;

/// The hero readout for the page.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Trend {
    Warmer,
    Colder,
    Same,
    Found,
    /// Target dropped out of the roster (reacquiring).
    Lost,
}

impl Trend {
    /// ASCII trend word for the hero line.
    pub fn word(self) -> &'static str {
        match self {
            Trend::Warmer => "WARMER",
            Trend::Colder => "COLDER",
            Trend::Same => "SAME",
            Trend::Found => "FOUND!",
            Trend::Lost => "LOST",
        }
    }
    /// Trend arrow (up/down) — empty for Found/Lost/Same.
    pub fn arrow(self) -> &'static str {
        match self {
            Trend::Warmer => "^",
            Trend::Colder => "v",
            _ => "",
        }
    }
}

/// One tick's snapshot for the renderer.
#[derive(Clone, Copy, Debug)]
pub struct HuntView {
    /// Current target node id (None until a peer exists to hunt).
    pub target: Option<u8>,
    pub trend: Trend,
    /// Smoothed RSSI (dBm) of the target.
    pub smoothed_rssi: i32,
    /// Target currently present in the roster.
    pub present: bool,
    pub proximity: Proximity,
}

pub struct HuntState {
    smoother: RssiSmoother,
    target: Option<u8>,
    trend_ref: i32,
    trend_ref_ms: u64,
    found_since_ms: Option<u64>,
}

impl Default for HuntState {
    fn default() -> Self {
        Self::new()
    }
}

impl HuntState {
    pub fn new() -> Self {
        Self {
            smoother: RssiSmoother::new(),
            target: None,
            trend_ref: UNSEEN,
            trend_ref_ms: 0,
            found_since_ms: None,
        }
    }

    pub fn target(&self) -> Option<u8> {
        self.target
    }

    /// Reset trend + found state to the current reading (fresh hunt never inherits
    /// the previous target's warmth). Called when the target changes.
    fn arm(&mut self, rssi: i32, now_ms: u64) {
        self.trend_ref = rssi;
        self.trend_ref_ms = now_ms;
        self.found_since_ms = None;
    }

    /// Set the target explicitly (e.g. tap-to-pick a peer), re-arming the trend.
    pub fn set_target(&mut self, id: u8, now_ms: u64) {
        self.target = Some(id);
        let seed = self.smoother.get(id).unwrap_or(UNSEEN);
        self.arm(seed, now_ms);
    }

    /// Cycle the target to the next id in `ids` (roster order, wraps). No-op if
    /// `ids` is empty. Called on the "next target" tap.
    pub fn cycle_target(&mut self, ids: &[u8], now_ms: u64) {
        if ids.is_empty() {
            return;
        }
        let next = match self.target.and_then(|t| ids.iter().position(|&i| i == t)) {
            Some(pos) => ids[(pos + 1) % ids.len()],
            None => ids[0],
        };
        self.set_target(next, now_ms);
    }

    /// Advance one tick. `present_raw` is the target's raw RSSI this tick from the
    /// roster, or `None` if the target isn't currently heard. Returns the view to
    /// render. If there's no target yet, pass the strongest known id via
    /// [`set_target`]/[`cycle_target`] first (the caller owns roster→id mapping).
    pub fn update(&mut self, present_raw: Option<i32>, now_ms: u64) -> HuntView {
        let target = self.target;
        let (present, smoothed) = match (target, present_raw) {
            (Some(t), Some(raw)) => (true, self.smoother.update(t, raw)),
            (Some(t), None) => (false, self.smoother.get(t).unwrap_or(UNSEEN)),
            (None, _) => (false, UNSEEN),
        };

        // Trend vs the ~TREND_LAG_MS-ago reference, then roll the reference.
        let delta = smoothed - self.trend_ref;
        if now_ms.saturating_sub(self.trend_ref_ms) >= TREND_LAG_MS {
            self.trend_ref = smoothed;
            self.trend_ref_ms = now_ms;
        }

        // FOUND: hold-to-confirm above the threshold (only while present).
        let found = if present && smoothed >= FOUND_RSSI {
            let since = *self.found_since_ms.get_or_insert(now_ms);
            now_ms.saturating_sub(since) >= FOUND_HOLD_MS
        } else {
            self.found_since_ms = None;
            false
        };

        let trend = if target.is_none() {
            Trend::Lost
        } else if found {
            Trend::Found
        } else if !present {
            Trend::Lost
        } else if delta > TREND_DEADBAND {
            Trend::Warmer
        } else if delta < -TREND_DEADBAND {
            Trend::Colder
        } else {
            Trend::Same
        };

        HuntView {
            target,
            trend,
            smoothed_rssi: smoothed,
            present,
            proximity: rssi::proximity(smoothed),
        }
    }
}
