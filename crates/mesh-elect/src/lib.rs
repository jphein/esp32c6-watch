//! Fleet-wide deterministic ESP-NOW **channel** + gateway election.
//!
//! # The constraint that makes this tractable
//!
//! The ESP32 has ONE radio, and ESP-NOW transmits/receives on whatever channel
//! the WiFi STA currently sits on. Therefore **choosing the AP *is* choosing the
//! ESP-NOW channel**, and the whole mesh-partition problem reduces to: make the
//! fleet agree on one channel. Two nodes on the same channel can always hear
//! each other; two on different channels never can, no matter what the mesh
//! code does.
//!
//! # Channel, not BSSID — the one place this departs from the spec
//!
//! The design spec scores per-AP (per-BSSID) and elects a BSSID. This crate
//! elects a **channel** instead, for three reasons:
//!
//! 1. **Only the channel is load-bearing.** Any AP on the elected channel puts
//!    us on the elected channel, which is all the mesh needs. Enforcing a BSSID
//!    buys nothing extra and costs the driver its roaming freedom — which is
//!    precisely the pinning JP asked to remove.
//! 2. **Per-BSSID scoring picks wrong on a roaming SSID.** With one SSID across
//!    a dozen APs, node A's strongest ch6 AP is often a *different* BSSID from
//!    node B's strongest ch6 AP. Summed per-BSSID, that shared-channel majority
//!    is invisible: a weak AP that both happen to see can outscore a channel
//!    both see strongly. Scoring per channel aggregates exactly the agreement we
//!    care about. This is our network (see `docs/`: one roaming SSID, ~12 APs).
//! 3. **It makes the wire frame fixed-size.** 13 channels is a constant, so the
//!    ELECT frame carries a fixed 13-slot weight vector with no variable
//!    candidate list — nothing for a hostile broadcaster to grow. The spec's
//!    "cap the candidate list to 6-8 entries" concern disappears by construction.
//!
//! # Scoring: count first, then signal
//!
//! The spec proposes `score = SUM of per-node RSSI weights`, saturating so one
//! close node cannot outvote the fleet. Saturation alone does not deliver that
//! property. With a plain sum over a 0..=51 weight scale, one node at -35 dBm
//! (51) beats three nodes at -70 dBm (16 each = 48) — the exact inversion the
//! spec set out to prevent. So the key is **lexicographic**:
//!
//! ```text
//! score(ch) = ( number of nodes that can USE ch , sum of their weights )
//! winner    = max score, tie-broken by LOWEST channel number
//! ```
//!
//! Count strictly dominates, so the winner is always the channel the most nodes
//! can join — which is literally what "all nodes that see any other node should
//! join the same mesh" asks for. Signal strength only breaks count ties, and the
//! channel number breaks those, giving a **total order with no ties at all**.
//! "Can USE" is gated by [`USABLE_MIN_DBM`], so a channel the fleet can only
//! barely hear cannot win on headcount alone.
//!
//! Because the result is a pure function of the observation *set*, it is
//! **permutation invariant**: every node that knows the same observations
//! computes the same winner, with no agreement rounds. Convergence needs only
//! information spread. (See `tests/consensus.rs::permutation_invariance`.)
//!
//! # Where convergence actually comes from
//!
//! Worth stating plainly, because it is stronger than the spec implies: a node
//! seeds its own observation from its **own WiFi scan**. Two nodes in the same
//! house scanning the same physical APs therefore compute the same winner
//! *without ever having heard each other*. That is what makes cold boot fast —
//! there is no rendezvous problem in the common case. Peer ELECT frames matter
//! for the harder cases: asymmetric visibility, and agreeing on epoch/gateway.
//!
//! Conversely — and this is the honest limit of determinism — ELECT frames can
//! only be *received* from nodes already on our channel. Determinism does not
//! merge a partition; only information crossing channels does. The escape
//! hatches are the shared scan above, and [`Elector::note_barren`] driving a
//! listen sweep.
//!
//! # Bounded by construction
//!
//! Every table here is a fixed-size array and every ingest path is capacity
//! checked. An inbound frame can neither allocate nor evict a live peer
//! ([`Elector::observe_peer`] refuses a full table rather than making room).
//! This is deliberate: the sibling `SmolMesh.peers` is an append-only `Vec` fed
//! straight from unauthenticated broadcasts, which is an OOM waiting to happen.
//! This crate does not add a second instance of that mistake.
//!
//! **Security, recorded not discovered:** ELECT frames are unauthenticated
//! (ESP-NOW here runs `lmk: None`). A hostile on-channel broadcaster can forge
//! observations and steer the fleet's channel. Three cheap mitigations are built
//! in — per-node dedupe by id (one forged id is one vote, not many), saturating
//! weights, and [`Elector::observe_peer`] refusing to adopt a channel our own
//! scan found no usable AP on — but a determined attacker with forged ids can
//! still influence the outcome. Accepted for a home fleet; it is not fixable
//! without a shared key.

#![no_std]

pub mod wire;

/// 2.4 GHz channels considered, `1..=13`. Fixed, so every table and the wire
/// frame are fixed-size.
pub const N_CHANNELS: usize = 13;

/// Observation slots: our own plus peers. The fleet is a handful of nodes; a
/// linear scan over this is free and the bound is what keeps an unauthenticated
/// frame from growing state.
pub const MAX_NODES: usize = 8;

/// An AP weaker than this does not count as "usable" — a channel we can only
/// barely hear must not win on headcount. Matches smol's proven
/// `coexist::AP_USABLE_MIN`, so both repos judge usability identically.
pub const USABLE_MIN_DBM: i8 = -82;

/// Weight saturates here at the strong end: everything at/above this counts the
/// same, so standing next to an AP cannot buy extra votes.
pub const WEIGHT_CEIL_DBM: i8 = -35;

/// An observation older than this stops counting. A node that walked away must
/// not keep voting for the channel it used to see. Peers tick every ~2 s, so
/// this is ~30 missed frames.
///
/// **Must exceed [`SETTLE_MS`] and [`PROBATION_MS`]** — see [`_WINDOW_ORDERING`].
pub const OBS_STALE_MS: u64 = 60_000;

/// A challenger must hold its lead this long before we spend an epoch on it.
/// Flapping costs an association and tears the mesh down, so it is strictly
/// worse than a suboptimal-but-stable channel.
pub const SETTLE_MS: u64 = 30_000;

/// Floor for the margin a challenger must beat the incumbent by (in summed
/// weight). See [`margin_for`].
pub const MARGIN_FLOOR: u32 = 12;

/// After adopting someone else's higher-epoch decision, how long we tolerate
/// hearing nothing on it before concluding the epoch is stale and re-electing.
/// Without this, one node with a high persisted epoch can wedge the whole fleet
/// onto a dead channel permanently — the failure mode a monotonic epoch invites
/// and the spec does not address.
pub const PROBATION_MS: u64 = 45_000;

/// Rendezvous channel of last resort, used only when there is nothing to elect
/// (no AP visible anywhere). Matches smol's historical `ESP_NOW_FIXED_CHANNEL`
/// so a fleet with no infrastructure still meets. This is a DEFAULT, not a pin:
/// it is abandoned the moment a real candidate appears.
pub const RENDEZVOUS_CHANNEL: u8 = 6;

/// Staleness must outlive both decision windows, or the machine can never
/// finish making up its mind: a challenger's supporting observations would
/// expire before its settle window closed, so `winner()` would flip back and the
/// timer would re-arm forever. Same for probation. Found by
/// `tests/consensus.rs::decisive_challenger_waits_for_settle_window`, which hung
/// at exactly `SETTLE_MS == OBS_STALE_MS`. Asserted at compile time so a future
/// tuning pass cannot silently reintroduce it.
const _WINDOW_ORDERING: () = {
    assert!(OBS_STALE_MS > SETTLE_MS);
    assert!(OBS_STALE_MS > PROBATION_MS);
};

/// Map dBm to a saturating vote weight. Monotone, clamped at both ends, and
/// `0` for anything below [`USABLE_MIN_DBM`] (not usable = no vote).
///
/// Range is `1..=48` for usable signal: the `+1` floor means bare usable
/// visibility still counts for something, and the ceiling means proximity
/// saturates.
#[must_use]
pub const fn weight(rssi_dbm: i8) -> u32 {
    if rssi_dbm < USABLE_MIN_DBM {
        return 0;
    }
    let capped = if rssi_dbm > WEIGHT_CEIL_DBM {
        WEIGHT_CEIL_DBM
    } else {
        rssi_dbm
    };
    // capped ∈ [-82, -35] → 1..=48
    (capped as i32 - USABLE_MIN_DBM as i32 + 1) as u32
}

/// The margin a challenger must clear, scaled to the incumbent's score.
///
/// The spec suggests a flat ">= 6 dB aggregate", but aggregate score is a sum
/// across nodes, so a flat threshold gets *easier* to trip as the fleet grows
/// (with 5 reporters, 6 units of aggregate is barely 1 dB each). Scaling with
/// the incumbent keeps the hysteresis meaningful at any fleet size, with a floor
/// so it is never trivial when scores are small.
#[must_use]
pub const fn margin_for(incumbent_sum: u32) -> u32 {
    let scaled = incumbent_sum / 8;
    if scaled > MARGIN_FLOOR {
        scaled
    } else {
        MARGIN_FLOOR
    }
}

/// Channel `1..=13` to array index, or `None` if out of range. Every inbound
/// channel number goes through this — a frame claiming ch 0 or ch 99 is dropped,
/// never indexed with.
#[must_use]
pub const fn ch_index(channel: u8) -> Option<usize> {
    if channel >= 1 && channel as usize <= N_CHANNELS {
        Some(channel as usize - 1)
    } else {
        None
    }
}

/// The committed decision. Carried on every ELECT frame; higher epoch wins.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Decision {
    /// Elected ESP-NOW channel, `1..=13`.
    pub channel: u8,
    /// Monotonic epoch. Higher wins; a late joiner adopts rather than re-elects.
    pub epoch: u32,
    /// Elected gateway node id, or `0` for "none known".
    pub gateway: u8,
}

impl Decision {
    /// The boot decision before anything is known or restored.
    #[must_use]
    pub const fn bootstrap() -> Self {
        Self {
            channel: RENDEZVOUS_CHANNEL,
            epoch: 0,
            gateway: 0,
        }
    }

    /// Total order for partition merge: higher epoch wins; equal epoch breaks by
    /// larger member count, then by lower channel. Never returns "equal" for two
    /// differing decisions reached with the same member count, so a merge always
    /// resolves.
    #[must_use]
    pub fn supersedes(&self, members: u8, other: &Self, other_members: u8) -> bool {
        if self.epoch != other.epoch {
            return self.epoch > other.epoch;
        }
        if members != other_members {
            return members > other_members;
        }
        self.channel < other.channel
    }
}

/// Per-channel tally: how many nodes can use it, and their summed weight.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Tally {
    /// Number of distinct nodes that can USE this channel. Dominant term.
    pub voters: u8,
    /// Sum of those nodes' weights. Breaks voter ties.
    pub sum: u32,
}

impl Tally {
    /// Lexicographic: voters first, then sum. Callers break remaining ties by
    /// lowest channel index, which makes the overall order total.
    #[must_use]
    fn beats(&self, other: &Self) -> bool {
        (self.voters, self.sum) > (other.voters, other.sum)
    }
}

/// What [`Elector::observe_peer`] did with a frame — so the firmware can log
/// honestly instead of guessing, and so tests can assert the bounds hold.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Ingest {
    /// Recorded (new slot or refreshed existing one).
    Recorded,
    /// Recorded, and we adopted the sender's strictly-higher epoch.
    Adopted(Decision),
    /// Malformed: channel out of range, or the sender used our own id.
    Rejected,
    /// Table full of live peers — dropped rather than evicting one. The bound
    /// that keeps an unauthenticated broadcaster from displacing real nodes.
    Full,
    /// Higher epoch, but it named a channel we have no usable AP on and have
    /// heard no peer on. Refused: this is the cheap defence against being
    /// dragged onto a dead channel.
    RefusedUnusable(u8),
}

#[derive(Clone, Copy)]
struct Slot {
    used: bool,
    node_id: u8,
    last_ms: u64,
    /// Per-channel weight as this node reports it. `0` = not usable.
    w: [u8; N_CHANNELS],
}

impl Slot {
    const fn empty() -> Self {
        Self {
            used: false,
            node_id: 0,
            last_ms: 0,
            w: [0; N_CHANNELS],
        }
    }
}

/// The election state machine. One per node.
///
/// Fixed size, no allocation: `MAX_NODES` slots of `[u8; 13]` plus scalars —
/// about 190 bytes of `.bss` at `MAX_NODES = 8`, which matters on a firmware
/// whose stack margin is single-digit KB.
pub struct Elector {
    self_id: u8,
    obs: [Slot; MAX_NODES],
    decision: Decision,
    /// Channel currently accumulating a lead, `0` = none.
    challenger: u8,
    challenger_since: u64,
    /// Set when we adopted a peer's epoch; cleared once the channel proves out.
    probation_since: Option<u64>,
    /// Highest epoch we have ever seen, so re-electing always moves forward.
    seen_epoch: u32,
}

impl Elector {
    #[must_use]
    pub fn new(self_id: u8) -> Self {
        Self {
            self_id,
            obs: [Slot::empty(); MAX_NODES],
            decision: Decision::bootstrap(),
            challenger: 0,
            challenger_since: 0,
            probation_since: None,
            seen_epoch: 0,
        }
    }

    /// Restore the persisted last-known-good decision at boot. This is the
    /// "fast, always" path: a fleet that converged yesterday is converged
    /// before it has heard anybody today.
    pub fn restore(&mut self, d: Decision) {
        if ch_index(d.channel).is_none() {
            return; // corrupt record — keep the bootstrap default
        }
        self.decision = d;
        self.seen_epoch = d.epoch;
        // Restored, not adopted from a live peer: no probation. If it is stale
        // the barren check will re-elect us off it soon enough.
        self.probation_since = None;
    }

    #[must_use]
    pub const fn decision(&self) -> Decision {
        self.decision
    }

    #[must_use]
    pub const fn self_id(&self) -> u8 {
        self.self_id
    }

    /// Live observation count (including our own), after staleness expiry.
    #[must_use]
    pub fn members(&self, now_ms: u64) -> u8 {
        let mut n = 0u8;
        for s in &self.obs {
            if s.used && !Self::stale(s, now_ms) {
                n = n.saturating_add(1);
            }
        }
        n
    }

    const fn stale(s: &Slot, now_ms: u64) -> bool {
        now_ms.saturating_sub(s.last_ms) > OBS_STALE_MS
    }

    /// Fold OUR OWN scan into the observation set. `best_rssi[i]` is the
    /// strongest AP for our SSID on channel `i+1`, or `None` if none was heard.
    ///
    /// This is the primary convergence engine, not a detail: two nodes scanning
    /// the same physical APs produce the same winner with no messages exchanged.
    pub fn observe_self(&mut self, now_ms: u64, best_rssi: &[Option<i8>; N_CHANNELS]) {
        let mut w = [0u8; N_CHANNELS];
        for (i, r) in best_rssi.iter().enumerate() {
            if let Some(dbm) = r {
                // weight() is 0..=48, fits u8 by construction.
                w[i] = weight(*dbm) as u8;
            }
        }
        let id = self.self_id;
        self.upsert(id, now_ms, w);
    }

    /// Our own weight vector as it will go on the wire.
    #[must_use]
    pub fn self_weights(&self) -> [u8; N_CHANNELS] {
        for s in &self.obs {
            if s.used && s.node_id == self.self_id {
                return s.w;
            }
        }
        [0; N_CHANNELS]
    }

    /// Ingest a peer's ELECT frame. Bounded and validating: a full table is
    /// refused rather than evicting a live peer, and a decision naming a channel
    /// we cannot use is not adopted.
    pub fn observe_peer(
        &mut self,
        now_ms: u64,
        node_id: u8,
        epoch: u32,
        channel: u8,
        gateway: u8,
        w: &[u8; N_CHANNELS],
    ) -> Ingest {
        if node_id == self.self_id || node_id == 0 {
            return Ingest::Rejected; // spoofed as us, or the "none" sentinel
        }
        if ch_index(channel).is_none() {
            return Ingest::Rejected;
        }
        // Clamp claimed weights to the honest ceiling so a forged 255 cannot
        // outvote the fleet even before saturation is considered.
        let ceil = weight(WEIGHT_CEIL_DBM) as u8;
        let mut clamped = [0u8; N_CHANNELS];
        for i in 0..N_CHANNELS {
            clamped[i] = if w[i] > ceil { ceil } else { w[i] };
        }
        if !self.upsert(node_id, now_ms, clamped) {
            return Ingest::Full;
        }
        if epoch > self.seen_epoch {
            // A strictly newer epoch: adopt rather than re-elect (this is what
            // "all nodes that see any other node join the same mesh" needs).
            // But refuse a channel we have no usable AP on — the cheap defence
            // against a forged frame dragging us somewhere dead.
            if !self.self_can_use(channel) && !self.peer_can_use(channel, now_ms) {
                return Ingest::RefusedUnusable(channel);
            }
            self.seen_epoch = epoch;
            self.decision = Decision {
                channel,
                epoch,
                gateway,
            };
            self.challenger = 0;
            self.probation_since = Some(now_ms);
            return Ingest::Adopted(self.decision);
        }
        if epoch == self.decision.epoch
            && self.decision.gateway == 0
            && gateway != 0
            && channel == self.decision.channel
        {
            // Same epoch, same channel, and they know a gateway we don't. Learn
            // it without spending an epoch — a gateway *claim* is what bumps.
            self.decision.gateway = gateway;
        }
        Ingest::Recorded
    }

    fn self_can_use(&self, channel: u8) -> bool {
        let Some(i) = ch_index(channel) else {
            return false;
        };
        for s in &self.obs {
            if s.used && s.node_id == self.self_id {
                return s.w[i] > 0;
            }
        }
        // No scan data at all — we cannot be pickier than that, so allow it.
        true
    }

    fn peer_can_use(&self, channel: u8, now_ms: u64) -> bool {
        let Some(i) = ch_index(channel) else {
            return false;
        };
        self.obs
            .iter()
            .any(|s| s.used && s.node_id != self.self_id && !Self::stale(s, now_ms) && s.w[i] > 0)
    }

    /// Insert or refresh. Returns `false` only when the table is full of live
    /// entries — the caller reports `Full` and drops the frame.
    fn upsert(&mut self, node_id: u8, now_ms: u64, w: [u8; N_CHANNELS]) -> bool {
        for s in self.obs.iter_mut() {
            if s.used && s.node_id == node_id {
                s.last_ms = now_ms;
                s.w = w;
                return true;
            }
        }
        // Prefer a genuinely free slot, then reclaim a stale one. Never evict a
        // live peer: that is the DoS an unauthenticated frame would otherwise get.
        if let Some(s) = self.obs.iter_mut().find(|s| !s.used) {
            *s = Slot {
                used: true,
                node_id,
                last_ms: now_ms,
                w,
            };
            return true;
        }
        let mut oldest: Option<(usize, u64)> = None;
        for (i, s) in self.obs.iter().enumerate() {
            if Self::stale(s, now_ms) && oldest.map_or(true, |(_, t)| s.last_ms < t) {
                oldest = Some((i, s.last_ms));
            }
        }
        match oldest {
            Some((i, _)) => {
                self.obs[i] = Slot {
                    used: true,
                    node_id,
                    last_ms: now_ms,
                    w,
                };
                true
            }
            None => false,
        }
    }

    /// Tally every channel over the live observation set. Pure — this is the
    /// function whose determinism the whole design rests on.
    #[must_use]
    pub fn tally(&self, now_ms: u64) -> [Tally; N_CHANNELS] {
        let mut out = [Tally::default(); N_CHANNELS];
        for s in &self.obs {
            if !s.used || Self::stale(s, now_ms) {
                continue;
            }
            for i in 0..N_CHANNELS {
                if s.w[i] > 0 {
                    out[i].voters = out[i].voters.saturating_add(1);
                    out[i].sum = out[i].sum.saturating_add(s.w[i] as u32);
                }
            }
        }
        out
    }

    /// The winning channel over the current observation set, or `None` when no
    /// channel is usable by anyone.
    ///
    /// Total order — `(voters, sum)` descending, then LOWEST channel number — so
    /// there are no ties to resolve and every node with the same observations
    /// picks the same channel. Independent of insertion order by construction.
    #[must_use]
    pub fn winner(&self, now_ms: u64) -> Option<u8> {
        let t = self.tally(now_ms);
        let mut best: Option<(usize, Tally)> = None;
        for (i, cand) in t.iter().enumerate() {
            if cand.voters == 0 {
                continue;
            }
            match best {
                // Strictly `beats`, and we walk ascending, so an equal tally
                // keeps the LOWER channel. That is the tie-break.
                Some((_, b)) if !cand.beats(&b) => {}
                _ => best = Some((i, *cand)),
            }
        }
        best.map(|(i, _)| i as u8 + 1)
    }

    /// Drive the epoch/hysteresis machine. Call on the mesh tick. Returns
    /// `Some(decision)` only when the committed decision CHANGED, so the caller
    /// can act (retune, persist) exactly on transitions.
    ///
    /// Converges and then stops: a challenger must beat the incumbent by
    /// [`margin_for`] and hold it for [`SETTLE_MS`] before an epoch is spent.
    pub fn step(&mut self, now_ms: u64) -> Option<Decision> {
        let t = self.tally(now_ms);
        let Some(win) = self.winner(now_ms) else {
            // Nothing usable anywhere. Hold what we have (or the rendezvous
            // default) — do not churn.
            return None;
        };
        if win == self.decision.channel {
            // Incumbent still wins: cancel any challenger and clear probation,
            // the decision is validated by our own observations.
            self.challenger = 0;
            self.probation_since = None;
            return None;
        }

        // ONE NODE IS NOT A QUORUM. A node that has heard nobody knows only what
        // its own antenna sees; that is enough to AGREE with a decision, never
        // enough to move the fleet off one. Without this a watch carried into
        // another building would elect its new local channel and abandon the
        // fleet it left behind — and, just as important, it is what makes this
        // change safe to deploy on one repo at a time: until smol also speaks
        // ELECT, the watch hears no peers, so it stays on the rendezvous channel
        // instead of unilaterally walking off it. The feature arms itself when
        // the fleet is ready.
        if self.members(now_ms) < 2 {
            self.challenger = 0;
            return None;
        }

        let cur_i = ch_index(self.decision.channel);
        let incumbent = cur_i.map_or(Tally::default(), |i| t[i]);
        let challenger = t[ch_index(win).expect("winner() returns 1..=13")];

        // A challenger with MORE voters is a connectivity win, not a signal
        // preference — that is the whole point of the election, so it only needs
        // the settle window, not the dB margin. Equal voters means we are
        // trading one channel for a similar one, which must clear the margin.
        let worth_it = if challenger.voters > incumbent.voters {
            true
        } else {
            challenger.sum > incumbent.sum + margin_for(incumbent.sum)
        };
        if !worth_it {
            self.challenger = 0;
            return None;
        }

        if self.challenger != win {
            self.challenger = win;
            self.challenger_since = now_ms;
            return None;
        }
        if now_ms.saturating_sub(self.challenger_since) < SETTLE_MS {
            return None; // holding the lead, not long enough yet
        }

        self.seen_epoch = self.seen_epoch.saturating_add(1);
        self.decision = Decision {
            channel: win,
            epoch: self.seen_epoch,
            // A channel change invalidates who we can reach; the gateway is
            // re-learned on the new channel rather than assumed to follow.
            gateway: 0,
        };
        self.challenger = 0;
        self.probation_since = None;
        Some(self.decision)
    }

    /// Tell the elector we have been on the committed channel this long with
    /// **nothing to show for it** — no peers heard and no association. Returns
    /// `Some(decision)` if that is enough to abandon an adopted-but-dead epoch.
    ///
    /// This is the anti-wedge escape hatch the spec omits. With a strictly
    /// monotonic epoch and unconditional "higher epoch wins", one node holding a
    /// high persisted epoch for a channel that no longer exists can pin the
    /// entire fleet onto a dead channel forever. Probation bounds that: an
    /// adopted decision that produces nothing for [`PROBATION_MS`] is discarded
    /// and we re-elect at a higher epoch.
    pub fn note_barren(&mut self, now_ms: u64) -> Option<Decision> {
        let since = self.probation_since?;
        if now_ms.saturating_sub(since) < PROBATION_MS {
            return None;
        }
        // Drop the dead channel from our own observation so the re-election
        // cannot immediately pick it again.
        if let Some(i) = ch_index(self.decision.channel) {
            let id = self.self_id;
            for s in self.obs.iter_mut() {
                if s.used && s.node_id == id {
                    s.w[i] = 0;
                }
            }
        }
        self.probation_since = None;
        let win = self.winner(now_ms)?;
        if win == self.decision.channel {
            return None;
        }
        self.seen_epoch = self.seen_epoch.saturating_add(1);
        self.decision = Decision {
            channel: win,
            epoch: self.seen_epoch,
            gateway: 0,
        };
        self.challenger = 0;
        Some(self.decision)
    }

    /// True while an adopted decision is still unproven.
    #[must_use]
    pub const fn on_probation(&self) -> bool {
        self.probation_since.is_some()
    }

    /// The gateway relinquish rule: a gateway that lost its uplink must stand
    /// down rather than black-hole traffic. Returns the new decision if we
    /// cleared a gateway we no longer believe in.
    pub fn relinquish_gateway(&mut self, id: u8) -> Option<Decision> {
        if id != 0 && self.decision.gateway == id {
            self.decision.gateway = 0;
            return Some(self.decision);
        }
        None
    }

    /// Claim the gateway role for `id` under the current epoch. Deterministic
    /// tie-break: the LOWEST node id wins, so two simultaneous claims resolve
    /// without a round trip.
    pub fn claim_gateway(&mut self, id: u8) -> Option<Decision> {
        if id == 0 {
            return None;
        }
        let cur = self.decision.gateway;
        if cur == 0 || id < cur {
            self.decision.gateway = id;
            return Some(self.decision);
        }
        None
    }
}
