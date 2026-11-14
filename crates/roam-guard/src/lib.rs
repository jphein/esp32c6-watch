//! Roamer guard (#108, the follow-up to #89): two ways the watch could sit on an AP forever while unusable.
//!
//! #89's root cause (2026-08-25): one BSSID of the roaming SSID L2-isolated the watch. It associated, it
//! got a lease, `WifiPhase::ready()` said Up, and every TCP connect failed; ARP to the watch from katana
//! failed while other wireless hosts answered. Nothing in the roamer saw a problem, so it never left.
//!
//! - **Lease deadline**: associated with no DHCP lease for [`LEASE_TIMEOUT_MS`] is a failed attempt
//!   (the caller backs off and reconnects), and that BSSID is quarantined.
//! - **Reachability probe** after the lease: the caller connects once to a known LAN service and reports
//!   [`Probe`]. Unreachable → quarantine the BSSID and roam on ([`Verdict::Roam`]).
//! - **The target, not the AP**: if two DIFFERENT BSSIDs in a row fail the probe, the service is the
//!   likelier fault. Neither AP is blamed and probing rests for [`SUSPEND_MS`] ([`Verdict::TargetDown`]),
//!   so a broker outage can't make the watch flap across every AP.
//! - **Always online**: the firmware never pins a BSSID (#57, de-pinned 2026-07-29), so "avoid that AP"
//!   is a channel preference ([`avoid_mask`](RoamGuard::avoid_mask), [`pick_channel`]) plus a refusal of
//!   a landing on a quarantined BSSID. Both give way: after [`MAX_REJECTED_LANDINGS`] refusals the
//!   landing is accepted and probing rests, and a channel hint falls back to the strongest channel when
//!   every heard channel is avoided. Quarantines expire.
//!
//! Pure, `no_std`, host-testable: times are milliseconds from any monotonic clock (`Instant::as_millis`).

#![no_std]

pub type Bssid = [u8; 6];

/// 2.4 GHz channels `1..=13` (index `ch - 1`), as `mesh_elect::N_CHANNELS` and the scan vectors.
pub const N_CHANNELS: usize = 13;
/// Associated without a lease this long is a failed attempt. embassy-net's DHCP normally lands in a few
/// seconds; 20 s also covers a slow server's first DISCOVER retry.
pub const LEASE_TIMEOUT_MS: u64 = 20_000;
/// How long a BSSID that gave no lease, or a lease that reached nothing, is steered around.
pub const QUARANTINE_MS: u64 = 10 * 60_000;
/// How long probing rests after the probe target looked down from two APs (or after accepting a
/// quarantined AP because it was the only one left).
pub const SUSPEND_MS: u64 = 30 * 60_000;
/// Landings on a quarantined BSSID refused before one is accepted anyway.
pub const MAX_REJECTED_LANDINGS: u8 = 2;
/// Quarantine slots. A handful of APs per SSID; the oldest entry gives way when full.
const CAP: usize = 4;

/// What one reachability probe saw.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Probe {
    /// The target answered: a TCP connect completed, OR was refused with RST (the host is reachable).
    Reachable,
    /// No answer within the timeout: the path to the LAN is broken from here.
    Unreachable,
    /// The probe couldn't run (no route, socket state): no evidence either way.
    Inconclusive,
}

/// What to do with a fresh association.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Landing {
    Accept,
    /// A quarantined BSSID: disconnect and try again (the channel hint steers elsewhere).
    Reject,
    /// A quarantined BSSID, refused [`MAX_REJECTED_LANDINGS`] times already: it's all there is, take it.
    /// Probing rests ([`SUSPEND_MS`]) so it doesn't bounce straight back into a roam loop.
    AcceptExhausted,
}

/// What to do after a probe.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Verdict {
    Keep,
    /// This AP reaches nothing: it is quarantined; disconnect and reconnect elsewhere.
    Roam,
    /// A second AP in a row failed the same probe: the target is the likelier fault. Stay; probing rests.
    TargetDown,
}

#[derive(Clone, Copy)]
struct Entry {
    bssid: Bssid,
    ch: u8,
    until: u64,
    since: u64,
}

pub struct RoamGuard {
    bad: [Option<Entry>; CAP],
    /// The BSSID whose probe failed most recently, with no success since (the consensus rule).
    last_fail: Option<Bssid>,
    suspended_until: u64,
    rejected: u8,
}

impl Default for RoamGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl RoamGuard {
    pub const fn new() -> Self {
        Self { bad: [None; CAP], last_fail: None, suspended_until: 0, rejected: 0 }
    }

    pub fn is_bad(&self, b: &Bssid, now: u64) -> bool {
        self.bad.iter().flatten().any(|e| e.bssid == *b && now < e.until)
    }

    fn quarantine(&mut self, b: &Bssid, ch: u8, now: u64) {
        let entry = Entry { bssid: *b, ch, until: now.saturating_add(QUARANTINE_MS), since: now };
        // The same BSSID refreshes its slot; else a free or expired slot; else the oldest gives way.
        let slot = self
            .bad
            .iter()
            .position(|e| matches!(e, Some(e) if e.bssid == *b))
            .or_else(|| self.bad.iter().position(|e| e.map_or(true, |e| now >= e.until)))
            .unwrap_or_else(|| {
                let mut oldest = 0;
                for (i, e) in self.bad.iter().enumerate() {
                    if let (Some(e), Some(o)) = (e, self.bad[oldest]) {
                        if e.since < o.since {
                            oldest = i;
                        }
                    }
                }
                oldest
            });
        self.bad[slot] = Some(entry);
    }

    fn release(&mut self, b: &Bssid) {
        for e in self.bad.iter_mut() {
            if matches!(e, Some(x) if x.bssid == *b) {
                *e = None;
            }
        }
    }

    /// A fresh association on `b`.
    pub fn on_associated(&mut self, b: &Bssid, now: u64) -> Landing {
        if !self.is_bad(b, now) {
            self.rejected = 0;
            return Landing::Accept;
        }
        if self.rejected < MAX_REJECTED_LANDINGS {
            self.rejected += 1;
            return Landing::Reject;
        }
        self.rejected = 0;
        self.suspended_until = now.saturating_add(SUSPEND_MS);
        Landing::AcceptExhausted
    }

    /// Associated at `assoc_at` and still no lease at `now`: give up on this attempt?
    pub fn lease_overdue(assoc_at: u64, now: u64) -> bool {
        now.saturating_sub(assoc_at) >= LEASE_TIMEOUT_MS
    }

    /// The lease never came on `b` (channel `ch`): steer around it for a while.
    pub fn on_lease_timeout(&mut self, b: &Bssid, ch: u8, now: u64) {
        self.quarantine(b, ch, now);
    }

    /// Run the reachability probe on this ready edge?
    pub fn should_probe(&self, now: u64) -> bool {
        now >= self.suspended_until
    }

    pub fn on_probe(&mut self, b: &Bssid, ch: u8, p: Probe, now: u64) -> Verdict {
        match p {
            Probe::Inconclusive => Verdict::Keep,
            Probe::Reachable => {
                self.last_fail = None;
                Verdict::Keep
            }
            Probe::Unreachable => match self.last_fail {
                Some(prev) if prev != *b => {
                    // Two different APs, the same unreachable target: blame the target, release the AP
                    // the first failure quarantined, and rest.
                    self.release(&prev);
                    self.last_fail = None;
                    self.suspended_until = now.saturating_add(SUSPEND_MS);
                    Verdict::TargetDown
                }
                _ => {
                    self.last_fail = Some(*b);
                    self.quarantine(b, ch, now);
                    Verdict::Roam
                }
            },
        }
    }

    /// Bit `ch` set for each channel holding a live quarantine (bit 0 unused).
    pub fn avoid_mask(&self, now: u64) -> u16 {
        self.bad
            .iter()
            .flatten()
            .filter(|e| now < e.until && (1..=N_CHANNELS as u8).contains(&e.ch))
            .fold(0, |m, e| m | (1 << e.ch))
    }
}

/// The strongest heard channel outside `avoid` (bit `ch`), else the strongest heard at all: an avoided
/// channel is a preference, never a cage.
pub fn pick_channel(obs: &[Option<i8>; N_CHANNELS], avoid: u16) -> Option<(u8, i8)> {
    let strongest = |skip: u16| {
        let mut best: Option<(u8, i8)> = None;
        for (i, r) in obs.iter().enumerate() {
            let ch = i as u8 + 1;
            if let Some(dbm) = *r {
                if skip & (1 << ch) == 0 && best.map_or(true, |(_, b)| dbm > b) {
                    best = Some((ch, dbm));
                }
            }
        }
        best
    };
    strongest(avoid).or_else(|| strongest(0))
}
