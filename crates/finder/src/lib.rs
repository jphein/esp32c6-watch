//! Nearest-peer range/proximity meter (smol-port #45 item 7) — "which peer is
//! closest, and how near am I getting?"
//!
//! Pure, `no_std`, host-testable — no radio, no rendering. Where [`hunt`] walks
//! toward one *chosen* target, `finder` scans the *whole* roster each tick and
//! surfaces the **strongest** peer as the one you're closing on: a proximity tier,
//! a signal-bar count, and a closer/farther trend. The Slint mesh page renders the
//! resulting [`FinderView`] (a small `main.rs` wiring step, done later).
//!
//! Smoothing + proximity mapping come from [`rssi`] (shared with the roster
//! screens and the treasure-hunt), so a peer reads as equally "near" everywhere.
//! Like [`rssi`]/[`hunt`] this crate is dependency-light — `rssi` only, fixed
//! arrays, heap-free.
//!
//! # Staleness
//!
//! Raw ESP-NOW peers don't announce every tick, and a peer that walks out of range
//! stops being heard entirely. The finder keeps a per-peer last-heard timestamp and
//! only nominates peers heard within [`STALE_MS`] as the nearest — otherwise a peer
//! that left would stay frozen at its last strong reading and be picked forever.
//!
//! [`hunt`]: https://docs.rs/hunt

#![no_std]

use rssi::{Proximity, RssiSmoother};

/// Max distinct peers tracked. Matches `rssi`'s internal smoother capacity so the
/// last-heard table and the EWMA table stay in lock-step. The fleet is tiny; a
/// linear scan over this is free.
pub const MAX_PEERS: usize = 16;

/// A peer not heard within this many ms is no longer a candidate for "nearest"
/// (assume it left range). Generous vs the mesh's roster cadence.
pub const STALE_MS: u64 = 5000;

/// Compare the nearest peer's smoothed RSSI against its value this many ms ago for
/// the closer/farther trend (same cadence as the treasure-hunt).
pub const TREND_LAG_MS: u64 = 1500;

/// Trend deadband (dB): `|delta|` below this reads STEADY (below the noise floor),
/// so standing still never flickers closer/farther.
pub const TREND_DEADBAND: i32 = 2;

/// Smoothed RSSI (dBm) attributed to an unseen peer — very weak.
const UNSEEN: i32 = -100;

/// Which way the nearest peer is moving relative to you.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Trend {
    /// Nearest peer's signal is rising — you're closing in.
    Closer,
    /// Nearest peer's signal is dropping — you're falling behind.
    Farther,
    /// Within the deadband — holding station (also the state right after a new
    /// peer becomes nearest, before a direction is known).
    Steady,
    /// No peer heard within [`STALE_MS`] — nothing to find yet.
    Searching,
}

impl Trend {
    /// ASCII trend word for the hero line.
    pub fn word(self) -> &'static str {
        match self {
            Trend::Closer => "CLOSER",
            Trend::Farther => "FARTHER",
            Trend::Steady => "STEADY",
            Trend::Searching => "SEARCHING",
        }
    }

    /// Trend arrow (up/down) — empty for Steady/Searching.
    pub fn arrow(self) -> &'static str {
        match self {
            Trend::Closer => "^",
            Trend::Farther => "v",
            _ => "",
        }
    }
}

/// One tick's snapshot for the renderer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FinderView {
    /// Nearest present peer's node id (`None` while searching).
    pub nearest: Option<u8>,
    /// Smoothed RSSI (dBm) of the nearest peer ([`UNSEEN`] when searching).
    pub smoothed_rssi: i32,
    /// Proximity tier of the nearest peer.
    pub proximity: Proximity,
    /// Signal-bar count (0..=4) for the nearest peer.
    pub bars: u8,
    /// Direction of travel relative to the nearest peer.
    pub trend: Trend,
    /// Number of peers currently heard (within [`STALE_MS`]).
    pub peer_count: usize,
}

impl FinderView {
    /// Is a peer currently in range to home in on?
    pub fn is_present(&self) -> bool {
        self.nearest.is_some()
    }

    /// Fixed-width proximity label ("HERE"/"NEAR"/…) via [`rssi::label`].
    pub fn proximity_label(&self) -> &'static str {
        rssi::label(self.proximity)
    }

    /// Hero-bar fill in pixels (0..=`width`) for the nearest peer, via
    /// [`rssi::bar_px`]. `0` when searching (UNSEEN clamps to empty).
    pub fn bar_px(&self, width: i32) -> i32 {
        rssi::bar_px(self.smoothed_rssi, width)
    }
}

/// Nearest-peer finder over the mesh roster. Heap-free, fixed capacity.
pub struct FinderState {
    smoother: RssiSmoother,
    /// Peer ids seen so far (valid for `..len`).
    ids: [u8; MAX_PEERS],
    /// Last-heard timestamp per id (valid for `..len`), for staleness.
    last_seen: [u64; MAX_PEERS],
    len: usize,
    /// The peer nominated as nearest on the previous tick (for trend re-arming).
    nearest: Option<u8>,
    trend_ref: i32,
    trend_ref_ms: u64,
}

impl Default for FinderState {
    fn default() -> Self {
        Self::new()
    }
}

impl FinderState {
    pub const fn new() -> Self {
        Self {
            smoother: RssiSmoother::new(),
            ids: [0; MAX_PEERS],
            last_seen: [0; MAX_PEERS],
            len: 0,
            nearest: None,
            trend_ref: UNSEEN,
            trend_ref_ms: 0,
        }
    }

    /// The peer nominated nearest on the last [`tick`](Self::tick)/[`update`](Self::update).
    pub fn nearest(&self) -> Option<u8> {
        self.nearest
    }

    /// Fold one peer's raw RSSI (this tick, from the roster) into the finder:
    /// updates its EWMA and marks it heard `now_ms`. Call once per heard peer, then
    /// [`tick`](Self::tick) — or use [`update`](Self::update) to do both. If the
    /// table is full an unknown id is dropped (bounded; never panics).
    pub fn observe(&mut self, id: u8, raw: i32, now_ms: u64) {
        self.smoother.update(id, raw);
        for i in 0..self.len {
            if self.ids[i] == id {
                self.last_seen[i] = now_ms;
                return;
            }
        }
        if self.len < MAX_PEERS {
            self.ids[self.len] = id;
            self.last_seen[self.len] = now_ms;
            self.len += 1;
        }
    }

    /// Recompute the view from the peers heard so far: nominate the strongest
    /// non-stale peer as nearest and derive its trend. Does not ingest samples —
    /// call [`observe`](Self::observe) first (or use [`update`](Self::update)).
    pub fn tick(&mut self, now_ms: u64) -> FinderView {
        // Strongest smoothed RSSI among peers heard within STALE_MS.
        let mut best: Option<(u8, i32)> = None;
        let mut peer_count = 0usize;
        for i in 0..self.len {
            if now_ms.saturating_sub(self.last_seen[i]) > STALE_MS {
                continue; // stale — assume out of range
            }
            peer_count += 1;
            let id = self.ids[i];
            let s = self.smoother.get(id).unwrap_or(UNSEEN);
            match best {
                Some((_, bs)) if s <= bs => {} // keep earlier peer on ties (stable)
                _ => best = Some((id, s)),
            }
        }

        let (nearest, smoothed) = match best {
            Some((id, s)) => (Some(id), s),
            None => (None, UNSEEN),
        };

        // A new nearest peer re-arms the trend to its current reading — never
        // inherit the previous peer's warmth (would fake a Closer/Farther jump).
        if nearest != self.nearest {
            self.nearest = nearest;
            self.trend_ref = smoothed;
            self.trend_ref_ms = now_ms;
        }

        // Trend vs the ~TREND_LAG_MS-ago reference, then roll the reference.
        let delta = smoothed - self.trend_ref;
        if now_ms.saturating_sub(self.trend_ref_ms) >= TREND_LAG_MS {
            self.trend_ref = smoothed;
            self.trend_ref_ms = now_ms;
        }

        let trend = if nearest.is_none() {
            Trend::Searching
        } else if delta > TREND_DEADBAND {
            Trend::Closer
        } else if delta < -TREND_DEADBAND {
            Trend::Farther
        } else {
            Trend::Steady
        };

        let proximity = rssi::proximity(smoothed);
        FinderView {
            nearest,
            smoothed_rssi: smoothed,
            proximity,
            bars: rssi::tier_bars(proximity),
            trend,
            peer_count,
        }
    }

    /// Ingest a whole tick's roster (`(id, raw_rssi)` for each heard peer) and
    /// return the view. The primary per-tick entry point. Peers absent from
    /// `roster` keep their last smoothed value and age toward [`STALE_MS`].
    pub fn update(&mut self, roster: &[(u8, i32)], now_ms: u64) -> FinderView {
        for &(id, raw) in roster {
            self.observe(id, raw, now_ms);
        }
        self.tick(now_ms)
    }
}
