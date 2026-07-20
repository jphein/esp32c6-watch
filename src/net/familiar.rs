//! The smol Mesh Familiar (fleet issue #57) — watch port.
//!
//! One living creature inhabits **exactly one board at a time** and migrates
//! across the ESP-NOW mesh. Only the current holder beats `SMOLv1 FAM`
//! (~1.5 s); every other node reconstructs a Weasley-clock pointer from the
//! last beat it heard. Wire format and holder-arbitration rules are ported
//! byte-for-byte from the C3 fleet reference (`rust/clock/src/familiar/mod.rs`
//! in jphein/smol) — the fleet is the compatibility target:
//!
//!   - exactly-one invariant: only the holder beats; a holder that hears a
//!     newer `seq` (or equal `seq` + lower holder id) yields and adopts;
//!   - migration: a type-`X` handoff names a destination, then silence; the
//!     destination takes up the heartbeat at `seq+1`; a dropped handoff
//!     self-heals (the old holder resumes after `HANDOFF_TIMEOUT_MS`);
//!   - orphan re-election: no beat for `FAM_LOST_MS` (~12 s) → survivors
//!     re-birth the creature from the CACHED seed/birth (same creature, same
//!     age) on an RSSI-weighted + id-staggered claim window;
//!   - cold-mesh first-birth: a node that never heard a familiar mints a new
//!     one after a boot grace, id-staggered so the fleet births exactly one.
//!
//! Watch-sizing differences vs. the reference (state machine, not wire):
//! the C3 roster (RSSI-sorted `RosterView`) is replaced by the flat live-peer
//! id list from [`crate::net::smol_mesh::SmolMesh`]; the node-join greeting
//! (`seen_ids` set) and the feed/call UI hooks are omitted. Inbound CALL
//! frames are still honoured (wander bias toward the caller) so C3 nodes can
//! summon the creature off the watch.

use esp_println::println;

// ===========================================================================
// Tuning — values identical to the C3 reference.
// ===========================================================================

/// Heartbeat cadence (ms). Only the holder beats.
const HEARTBEAT_MS: u64 = 1_500;
/// Broadcast phase spread (matches the fleet's snake netcode `PHASE_NMAX`).
const PHASE_NMAX: u8 = 16;
/// A botched handoff self-heals: destination silent past this ⇒ we resume.
const HANDOFF_TIMEOUT_MS: u64 = 4_000;
/// No heartbeat heard for this long ⇒ holder presumed dead ⇒ takeover.
const FAM_LOST_MS: u64 = 12_000;
/// Cold-mesh grace from boot before a node may mint a NEW creature.
const FIRST_BIRTH_GRACE_MS: u64 = 8_000;
/// Per-`id % 8` claim stagger (ms) — the final tiebreak.
const ID_STAGGER_MS: u64 = 200;
/// Per-RSSI-bucket claim stagger (ms) — near survivors adopt first.
const RSSI_STAGGER_MS: u64 = 500;
/// Base wander period + per-id jitter span: the holder walks the creature to
/// a neighbour every ~2.5–5 min (this is how the watch relinquishes).
const WANDER_BASE_MS: u64 = 150_000;
const WANDER_JITTER_SPAN_MS: u64 = 150_000;
/// A CALL biases + expedites the holder's next wander to within this window.
const GREET_BIAS_MS: u64 = 15_000;

/// Growth-stage thresholds (age = `unix_now − birth_unix`).
const EGG_MAX_S: u32 = 300; // < 5 min
const HATCHLING_MAX_S: u32 = 7_200; // < 2 h
const JUVENILE_MAX_S: u32 = 86_400; // < 24 h

/// Hunger thresholds (`now − last_fed_unix`).
const FULL_MAX_S: u32 = 600; // < 10 min
const PECKISH_MAX_S: u32 = 3_600; // < 1 h

/// A recent migration/arrival shows as `Happy` for this long.
const HAPPY_WINDOW_S: u32 = 15;

/// Local night window → the creature sleeps. The fleet derives the hour from
/// `unix + TZ`; the watch uses fixed PST (no DST) — cosmetic-only (mood is
/// holder-computed and carried on the wire, so the fleet always agrees).
const NIGHT_START_H: u32 = 23;
const NIGHT_END_H: u32 = 6;
const TZ_OFFSET_SECONDS: i64 = -8 * 3600;

// ===========================================================================
// Wire frame — SMOLv1 FAM. Byte-for-byte the fleet layout (29 B):
//   [0..11)  "SMOLv1 FAM "   ASCII prefix (byte 7 = 'F', unique vs TIME/OTA)
//   [11]     kind   u8       'H' heartbeat · 'X' handoff · 'C' call
//   [12]     holder u8       the node currently hosting the familiar
//   [13]     target u8       handoff dest (X) / caller id (C); else 0
//   [14..16] seq    u16 LE   monotonic authority counter
//   [16..20] seed   u32 LE   creature identity (nonzero; 0 = no creature)
//   [20..24] birth  u32 LE   mesh-Unix birth time → age / growth stage
//   [24..28] fed    u32 LE   last-fed mesh-Unix → hunger
//   [28]     mood   u8       holder-computed mood (cosmetic)
// ===========================================================================

pub const FAM_PREFIX: &[u8; 11] = b"SMOLv1 FAM ";
pub const FAM_FRAME_LEN: usize = 29;

pub const FAM_HEARTBEAT: u8 = b'H';
pub const FAM_HANDOFF: u8 = b'X';
pub const FAM_CALL: u8 = b'C';

/// Holder-computed mood tokens (byte 28 on the wire).
pub const MOOD_IDLE: u8 = 0;
pub const MOOD_HAPPY: u8 = 1;
pub const MOOD_HUNGRY: u8 = 2;
pub const MOOD_SLEEPING: u8 = 3;

/// A decoded SMOLv1 FAM frame. `Copy` scalar-only, no heap.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct FamFrame {
    pub kind: u8,
    pub holder: u8,
    pub target: u8,
    pub seq: u16,
    pub seed: u32,
    pub birth: u32,
    pub fed: u32,
    pub mood: u8,
}

/// Encode a [`FamFrame`] into `out` (29 B), returning the length, or `None`
/// if `out` is too small. Fixed-size, allocation-free, panic-free.
pub fn encode_fam(f: &FamFrame, out: &mut [u8]) -> Option<usize> {
    if out.len() < FAM_FRAME_LEN {
        return None;
    }
    out[..FAM_PREFIX.len()].copy_from_slice(FAM_PREFIX);
    out[11] = f.kind;
    out[12] = f.holder;
    out[13] = f.target;
    out[14..16].copy_from_slice(&f.seq.to_le_bytes());
    out[16..20].copy_from_slice(&f.seed.to_le_bytes());
    out[20..24].copy_from_slice(&f.birth.to_le_bytes());
    out[24..28].copy_from_slice(&f.fed.to_le_bytes());
    out[28] = f.mood;
    Some(FAM_FRAME_LEN)
}

/// Parse a SMOLv1 FAM frame, or `None` if too short / wrong prefix / an
/// unrecognised kind. A `seed == 0` frame is rejected (0 = uninitialised).
pub fn parse_fam(buf: &[u8]) -> Option<FamFrame> {
    if buf.len() < FAM_FRAME_LEN {
        return None;
    }
    if &buf[..FAM_PREFIX.len()] != FAM_PREFIX.as_slice() {
        return None;
    }
    let kind = buf[11];
    if kind != FAM_HEARTBEAT && kind != FAM_HANDOFF && kind != FAM_CALL {
        return None;
    }
    let seq = u16::from_le_bytes([buf[14], buf[15]]);
    let seed = u32::from_le_bytes([buf[16], buf[17], buf[18], buf[19]]);
    if seed == 0 {
        return None;
    }
    let birth = u32::from_le_bytes([buf[20], buf[21], buf[22], buf[23]]);
    let fed = u32::from_le_bytes([buf[24], buf[25], buf[26], buf[27]]);
    Some(FamFrame {
        kind,
        holder: buf[12],
        target: buf[13],
        seq,
        seed,
        birth,
        fed,
        mood: buf[28],
    })
}

/// Wrap-aware (RFC 1982) "is `a` newer than `b`" for the `seq:u16` counter:
/// forward distance `a − b (mod 2^16)` in `1..=0x7FFF`.
#[inline]
fn seq_newer(a: u16, b: u16) -> bool {
    let d = a.wrapping_sub(b);
    d != 0 && d < 0x8000
}

// ===========================================================================
// Creature — travelling identity + counters (survive migration on the wire).
// ===========================================================================

#[derive(Clone, Copy)]
pub struct Creature {
    pub seed: u32,
    pub birth_unix: u32,
    pub last_fed_unix: u32,
}

impl Creature {
    /// Growth stage 0..=3 (egg/hatchling/juvenile/adult) from age. Everyone
    /// computes the same stage from `birth_unix`.
    pub fn stage_level(&self, unix_now: u32) -> u8 {
        let age = unix_now.saturating_sub(self.birth_unix);
        if age < EGG_MAX_S {
            0
        } else if age < HATCHLING_MAX_S {
            1
        } else if age < JUVENILE_MAX_S {
            2
        } else {
            3
        }
    }

    /// Hunger 0..=2 (full/peckish/hungry) from time-since-fed.
    pub fn hunger_level(&self, unix_now: u32) -> u8 {
        let since = unix_now.saturating_sub(self.last_fed_unix);
        if since < FULL_MAX_S {
            0
        } else if since < PECKISH_MAX_S {
            1
        } else {
            2
        }
    }
}

/// True if the mesh-Unix time falls in the local night window.
fn is_night(unix_now: u32) -> bool {
    let sod = ((unix_now as i64) + TZ_OFFSET_SECONDS).rem_euclid(86_400) as u32;
    let hour = sod / 3_600;
    // Night = the wrap-around window [23:00, 06:00).
    !(NIGHT_END_H..NIGHT_START_H).contains(&hour)
}

/// A UI snapshot of the familiar for the Slint clock cluster, rebuilt from
/// [`FamState`] each tick. Mirrors the discrete semantics the old embedded-
/// graphics watchface drew (deleted in task 13): `holding` draws the live
/// creature, `known && !holding` the "away" marker. Derives `PartialEq` so the
/// main loop only pushes on change.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub struct FamUi {
    /// A familiar exists somewhere on the mesh (heard or hosted).
    pub known: bool,
    /// This watch is the current holder — draw the live creature.
    pub holding: bool,
    /// Mood token (cosmetic color): 0 idle, 1 happy, 2 hungry, 3 sleeping.
    pub mood: u8,
    /// Hunger 0..=2 (full / peckish / hungry) — a 3-step bar.
    pub hunger: u8,
    /// Growth stage 0..=3 (egg / hatchling / juvenile / adult) — scales the body.
    pub stage: u8,
}

// ===========================================================================
// FamState — holder / arbitration / migration state machine (always-on).
// ===========================================================================

pub struct FamState {
    /// This node's logical id (holder comparisons + election).
    node_id: u8,
    /// Are WE the current holder (the one that beats)?
    is_holder: bool,
    /// The authoritative sequence counter (++ each beat/handoff/claim).
    seq: u16,
    /// Latest known holder id (== `node_id` while `is_holder`).
    holder_id: u8,
    /// Whether we've ever heard/hosted a familiar (false = cold mesh).
    known: bool,
    /// The creature's travelling identity + counters (valid once `known`).
    creature: Creature,
    /// The mood the holder last computed / we last heard.
    mood: u8,
    /// Monotonic ms of the last heartbeat we HEARD (orphan-takeover timing).
    last_beat_ms: u64,
    /// RSSI (dBm) of the last holder frame — nearer holder ⇒ adopt sooner.
    last_holder_rssi: i8,
    /// Our phase-jittered slot within `HEARTBEAT_MS` (`(id % 16) * period/16`,
    /// the fleet's `phase_offset_ms` idiom).
    hb_phase: u64,
    /// Monotonic ms at/after which the next heartbeat fires (slot-aligned).
    /// Interval-based rather than the C3 subtick edge detector: the watch main
    /// loop cadence is variable, so we schedule the next slot explicitly.
    next_beat_ms: u64,
    /// A handoff we initiated: deadline by which the destination must have
    /// taken up the heartbeat, else we resume as holder.
    handoff_until_ms: Option<u64>,
    /// One-shot: emit a heartbeat on the very next tick.
    pending_beat: bool,
    /// Monotonic ms of the next scheduled wander (holder migrates the pet).
    next_wander_ms: u64,
    /// mesh-Unix of the last time we became holder (arrival → Happy).
    last_arrival_unix: u32,
    /// A pending wander bias toward a specific id (a CALL-er).
    bias_to: Option<u8>,
}

impl FamState {
    /// A fresh, creature-less state. No familiar exists until one is heard
    /// or first-birthed.
    pub fn new(node_id: u8) -> Self {
        Self {
            node_id,
            is_holder: false,
            seq: 0,
            holder_id: 0,
            known: false,
            creature: Creature {
                seed: 0,
                birth_unix: 0,
                last_fed_unix: 0,
            },
            mood: MOOD_IDLE,
            last_beat_ms: 0,
            last_holder_rssi: -90,
            hb_phase: (node_id % PHASE_NMAX) as u64 * (HEARTBEAT_MS / PHASE_NMAX as u64),
            next_beat_ms: 0,
            handoff_until_ms: None,
            pending_beat: false,
            next_wander_ms: 0,
            last_arrival_unix: 0,
            bias_to: None,
        }
    }

    pub fn is_holder(&self) -> bool {
        self.is_holder
    }

    pub fn known(&self) -> bool {
        self.known
    }

    pub fn mood(&self) -> u8 {
        self.mood
    }

    pub fn creature(&self) -> Creature {
        self.creature
    }

    /// True when the main loop must run at holder cadence (beats + the
    /// handoff-timeout watchdog) rather than at its idle pace.
    pub fn needs_fast_tick(&self) -> bool {
        self.is_holder || self.pending_beat || self.handoff_until_ms.is_some()
    }

    // ---- frame constructors ------------------------------------------------

    /// The current-state heartbeat/handoff frame. The caller bumps `seq` and
    /// computes `mood` first.
    fn state_frame(&self, kind: u8, target: u8) -> FamFrame {
        FamFrame {
            kind,
            holder: self.node_id,
            target,
            seq: self.seq,
            seed: self.creature.seed,
            birth: self.creature.birth_unix,
            fed: self.creature.last_fed_unix,
            mood: self.mood,
        }
    }

    /// Recompute the holder's mood, priority-ordered exactly like the fleet:
    /// Sleeping (clock) > Happy (fresh feed/arrival) > Hungry > Idle.
    fn compute_mood(&self, unix_now: u32) -> u8 {
        if is_night(unix_now) {
            return MOOD_SLEEPING;
        }
        let happy = unix_now.saturating_sub(self.creature.last_fed_unix) < HAPPY_WINDOW_S
            || unix_now.saturating_sub(self.last_arrival_unix) < HAPPY_WINDOW_S;
        if happy {
            return MOOD_HAPPY;
        }
        if self.creature.hunger_level(unix_now) >= 2 {
            return MOOD_HUNGRY;
        }
        MOOD_IDLE
    }

    /// Emit a heartbeat NOW: recompute mood, bump `seq`, schedule the next
    /// slot, return the frame.
    fn beat(&mut self, now_ms: u64, unix_now: u32) -> FamFrame {
        self.mood = self.compute_mood(unix_now);
        self.seq = self.seq.wrapping_add(1);
        // Next phase-aligned slot strictly after now.
        let base = now_ms.saturating_sub(self.hb_phase);
        self.next_beat_ms = (base / HEARTBEAT_MS + 1) * HEARTBEAT_MS + self.hb_phase;
        self.state_frame(FAM_HEARTBEAT, 0)
    }

    /// Schedule the next wander (per-id jittered so the fleet desynchronises).
    fn reschedule_wander(&mut self, now_ms: u64) {
        let jitter = (self.node_id as u64).wrapping_mul(7_919) % WANDER_JITTER_SPAN_MS;
        self.next_wander_ms = now_ms + WANDER_BASE_MS + jitter;
    }

    /// Adopt a heard creature's identity/state (a non-holder tracking "where
    /// + who", or a yielding holder taking the winner's creature).
    fn adopt(&mut self, f: &FamFrame) {
        self.holder_id = f.holder;
        self.seq = f.seq;
        self.creature = Creature {
            seed: f.seed,
            birth_unix: f.birth,
            last_fed_unix: f.fed,
        };
        self.mood = f.mood;
        self.known = true;
    }

    /// Become the holder: reset the wander timer + arrival greet.
    fn take_holdership(&mut self, now_ms: u64, unix_now: u32) {
        self.is_holder = true;
        self.holder_id = self.node_id;
        self.handoff_until_ms = None;
        self.last_arrival_unix = unix_now;
        self.reschedule_wander(now_ms);
    }

    /// Mint a brand-new creature (cold-mesh first-birth). Seed mixing is the
    /// fleet's: `now_ms` low bits through the golden ratio + our id, forced
    /// non-zero.
    fn mint(&mut self, now_ms: u64, unix_now: u32) {
        let mixed = (now_ms as u32).wrapping_mul(2_654_435_761)
            ^ (self.node_id as u32).wrapping_mul(40_503).rotate_left(16);
        let seed = mixed | 1;
        self.creature = Creature {
            seed,
            birth_unix: unix_now,
            last_fed_unix: unix_now,
        };
        self.seq = 0;
        self.known = true;
    }

    // ---- inbound frame handling ---------------------------------------------

    /// Ingest a decoded FAM frame heard from a peer at `rssi`. Runs the
    /// exactly-one arbitration (dual-holder collapse), handoff take-up,
    /// orphan-view tracking, and call-bias capture. Never emits — any
    /// resulting beat rides the next tick.
    pub fn ingest(&mut self, f: &FamFrame, rssi: i32, now_ms: u64, unix_now: u32) {
        // A frame claiming our OWN id is a stray/echo — ignore.
        if f.holder == self.node_id && f.kind != FAM_CALL {
            return;
        }

        match f.kind {
            FAM_CALL => {
                // "Come here!" addressed to us as holder → bias + expedite
                // the next wander toward the caller.
                if self.is_holder && f.holder == self.node_id {
                    self.bias_to = Some(f.target);
                    self.next_wander_ms = self.next_wander_ms.min(now_ms + GREET_BIAS_MS);
                }
            }
            FAM_HEARTBEAT | FAM_HANDOFF => {
                if f.seed == 0 {
                    return;
                }
                // Freshness/liveness bookkeeping for every valid holder frame.
                self.last_beat_ms = now_ms;
                self.last_holder_rssi = rssi.clamp(-127, 0) as i8;

                let fresher =
                    seq_newer(f.seq, self.seq) || (f.seq == self.seq && f.holder < self.node_id);

                if self.is_holder {
                    // Dual-holder collapse: a strictly-fresher authority (or
                    // equal-seq + lower id) wins → we yield + adopt.
                    if fresher {
                        self.is_holder = false;
                        self.handoff_until_ms = None;
                        self.adopt(f);
                        println!("[FAM] yielded to id{:03} (seq {})", f.holder, f.seq);
                    }
                    // else: we're fresher-or-equal-higher-id → keep holding.
                } else {
                    // Non-holder: track the freshest view of where + who.
                    if !self.known || fresher || f.seq == self.seq {
                        self.adopt(f);
                    }
                }

                // A handoff addressed to US → become the new holder + beat at
                // once (the old holder confirms on hearing our +1 beat).
                if f.kind == FAM_HANDOFF && f.target == self.node_id {
                    self.adopt(f); // take the exact travelling state
                    self.take_holdership(now_ms, unix_now);
                    self.pending_beat = true;
                    println!("[FAM] handoff received from id{:03} - holding", f.holder);
                }
            }
            _ => {}
        }
    }

    // ---- the per-loop tick (may emit one frame to broadcast) ----------------

    /// Advance the state machine one main-loop pass. `peers` is the current
    /// live mesh peer id list (the watch's stand-in for the C3 roster).
    /// Returns a frame to broadcast this tick, if any.
    pub fn tick(&mut self, peers: &[u8], now_ms: u64, unix_now: u32) -> Option<FamFrame> {
        // An immediate become-holder beat (handoff take-up) fires first.
        if self.pending_beat {
            self.pending_beat = false;
            if self.is_holder {
                return Some(self.beat(now_ms, unix_now));
            }
        }

        // A handoff in flight: wait for the destination, else resume.
        if let Some(deadline) = self.handoff_until_ms {
            if now_ms >= deadline {
                // Timed out (destination asleep / lost the frame) → keep the
                // pet: resume heartbeating at seq+2.
                self.handoff_until_ms = None;
                self.is_holder = true;
                self.holder_id = self.node_id;
                self.last_arrival_unix = unix_now; // "it came back"
                self.reschedule_wander(now_ms);
                println!("[FAM] handoff timed out - resuming as holder");
                return Some(self.beat(now_ms, unix_now));
            }
            return None; // still waiting — no beat during a handoff
        }

        if self.is_holder {
            return self.holder_tick(peers, now_ms, unix_now);
        }
        self.claim_tick(now_ms, unix_now)
    }

    /// Holder path: maybe migrate (wander), else beat on cadence.
    fn holder_tick(&mut self, peers: &[u8], now_ms: u64, unix_now: u32) -> Option<FamFrame> {
        // Wander: hand the creature off to a neighbour when the timer elapses.
        if now_ms >= self.next_wander_ms {
            if let Some(dest) = self.pick_dest(peers) {
                self.seq = self.seq.wrapping_add(1);
                self.mood = self.compute_mood(unix_now);
                self.handoff_until_ms = Some(now_ms + HANDOFF_TIMEOUT_MS);
                self.bias_to = None;
                self.reschedule_wander(now_ms); // next attempt either way
                println!("[FAM] wandering - handoff to id{dest:03}");
                return Some(self.state_frame(FAM_HANDOFF, dest));
            }
            // Alone (no live peers) → stay put, try again later.
            self.reschedule_wander(now_ms);
        }

        // Heartbeat on our phase-jittered slot.
        if now_ms >= self.next_beat_ms {
            return Some(self.beat(now_ms, unix_now));
        }
        None
    }

    /// Non-holder path: take over a dead holder's creature, or first-birth a
    /// new one on a cold mesh — both on a staggered claim window.
    fn claim_tick(&mut self, now_ms: u64, unix_now: u32) -> Option<FamFrame> {
        if !self.known {
            // Cold mesh: nobody has a familiar. First-birth after a boot
            // grace so a late-heard existing holder wins first; id-staggered
            // so the lowest id mints and the others adopt. Watch extra gate:
            // never mint before mesh/NTP time is known (a birth at unix 0
            // would fake a years-old adult).
            if unix_now == 0 {
                return None;
            }
            let wait = FIRST_BIRTH_GRACE_MS + self.id_stagger();
            if now_ms < wait {
                return None;
            }
            self.mint(now_ms, unix_now);
            self.take_holdership(now_ms, unix_now);
            println!(
                "[FAM] first-birth: seed {:08x} born {}",
                self.creature.seed, unix_now
            );
            return Some(self.beat(now_ms, unix_now)); // seq 0 → 1
        }

        // Orphan takeover: no beat for FAM_LOST_MS ⇒ the holder is dead.
        // Claim on a window weighted by how strongly we heard the (now-dead)
        // holder — the nearest survivor claims first.
        if now_ms.saturating_sub(self.last_beat_ms) < FAM_LOST_MS {
            return None;
        }
        let wait = self.last_beat_ms + FAM_LOST_MS + self.rssi_stagger() + self.id_stagger();
        if now_ms < wait {
            return None;
        }
        // Re-birth from the CACHED state (same seed/birth ⇒ same creature,
        // same age). `beat()` bumps seq to cached+1, out-ranking any stale
        // non-holder; a same-seq rival is settled by the id tiebreak.
        self.take_holdership(now_ms, unix_now);
        println!("[FAM] orphan takeover (holder id{:03} silent)", self.holder_id);
        Some(self.beat(now_ms, unix_now))
    }

    /// Pick a wander destination from the live peers. A pending bias (a
    /// CALL-er) wins if that peer is still audible; otherwise rotate by seq
    /// so the creature wanders rather than pinning to one peer. (The C3
    /// reference RSSI-weights this pick; the watch peer list is unordered.)
    fn pick_dest(&self, peers: &[u8]) -> Option<u8> {
        if peers.is_empty() {
            return None;
        }
        if let Some(b) = self.bias_to {
            if peers.contains(&b) {
                return Some(b);
            }
        }
        Some(peers[(self.seq as usize) % peers.len()])
    }

    /// Per-`id % 8` stagger term (ms) — the final claim tiebreak.
    fn id_stagger(&self) -> u64 {
        (self.node_id as u64 % 8) * ID_STAGGER_MS
    }

    /// RSSI-bucketed stagger term (ms): a stronger (nearer) last-heard holder
    /// ⇒ fewer buckets ⇒ we claim sooner. ~10 dB buckets, capped at 6.
    fn rssi_stagger(&self) -> u64 {
        let mag = (-(self.last_holder_rssi as i32)).clamp(0, 99) as u64;
        (mag / 10).min(6) * RSSI_STAGGER_MS
    }
}
