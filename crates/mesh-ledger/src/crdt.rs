//! #185 mesh-ledger L4 — the PURE delta-state **OR-Set / G-Set** CRDT for multi-writer shared state.
//!
//! ## What this is (design of record: `docs/superpowers/research/mesh-ledger-study.md`, #181)
//! L1–L3 ([`super::ledger`] · [`super::treehead`] · [`super::sth`]) give a **single-writer**
//! tamper-evident history: each node's own chain, anchored and signed. RPG **loot** and
//! **gravestones** are the opposite shape — *multi-writer shared state* edited concurrently by
//! any node, over a lossy, order-scrambling, duplicating mesh with **no coordinator**. That is
//! the CRDT problem. This is a delta-state observed-remove set (OR-Set), which degenerates to a
//! grow-only set (**G-Set**) when nothing is ever removed (gravestones: append-only; loot: add +
//! remove-on-pickup).
//!
//! ## Why it converges (the CvRDT laws)
//! State is two **grow-only** sets: `adds` (each an `(elem, tag)` where `tag` is globally unique)
//! and `tombs` (removed tags). `merge` is their **union** — a join over a semilattice, hence
//! **commutative, idempotent, associative**. So any order / loss / duplication of gossip
//! converges every replica to the identical state (see `experiments/185_crdt_verify`). An element
//! is **live** iff it has some add-tag that is not tombed → **add-wins**: a concurrent add whose
//! fresh tag a remover never observed keeps the element alive.
//!
//! ## Delta-state (why not state-based)
//! A full state doesn't fit a 250 B ESP-NOW frame. Because the join is set-union, a **delta** is
//! just an `OrSet` fragment carrying only the entries a peer lacks ([`OrSet::delta_vs`]); merging
//! the delta is byte-for-byte equivalent to merging the whole state. Nodes gossip deltas; the same
//! [`OrSet::merge`] absorbs them.
//!
//! ## L4 scope (this module)
//! - **Pure + host-testable** (the `flood`/`etx`/`ledger`/`treehead`/`sth` pattern): no
//!   `esp-hal`/`esp-wifi`, no `std`, no alloc, **no hash-crate dep**. sha256 is **injected**
//!   (`F: Fn(&[u8]) -> [u8; 32]`) and powers only [`OrSet::digest`] — an order-independent
//!   live-set summary for O(1) convergence checks that also feeds the L2/L3 anchor/signature.
//! - **Bounded + const-constructible** — `OrSet<CAP>` is fixed arrays in `.bss` (`const fn new`),
//!   `Err(Error::Full)` past `CAP`. RAM ≈ `CAP × (16 + 5) + CAP × 5` ≈ `CAP × 26` B.
//! - **NOT wired into the radio path** — the world-state gossip/relay is a later HW-gated rung;
//!   this module is deliberately *not declared in `net.rs`* (else dead-code under `-D warnings`,
//!   the #164 lesson). Firmware build unaffected.
//! - **Known bound:** tombstones are grow-only (the classic OR-Set cost); `CAP` caps them. Causal
//!   compaction (dot-store / version-vector GC of tombs) is a documented v2 optimization.

/// Element-id width — loot/gravestone content is hashed to 16 B by the caller.
pub const ID_LEN: usize = 16;
/// A content-addressed element id (a hashed loot/gravestone identity).
pub type ElemId = [u8; ID_LEN];
/// Domain-separation prefix for the set digest (distinct from L1/L2 `0x00`/`0x01` and L3 `0x02`).
pub const DOMAIN: u8 = 0x04;

/// A globally-unique add-event identity: the node that added and its per-node monotonic seq.
/// Uniqueness is what makes add-wins work — a re-add mints a *new* tag a prior remover can't have
/// tombed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Tag {
    pub node: u8,
    pub seq: u32,
}

/// Capacity exhausted (a new add/merge would exceed `CAP`).
#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    Full,
}

/// A bounded delta-state OR-Set / G-Set. `adds` and `tombs` are both grow-only; an element is live
/// iff it holds an add-tag absent from `tombs`.
#[derive(Clone)]
pub struct OrSet<const CAP: usize> {
    adds: [Option<(ElemId, Tag)>; CAP],
    tombs: [Option<Tag>; CAP],
}

impl<const CAP: usize> Default for OrSet<CAP> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const CAP: usize> OrSet<CAP> {
    /// A fresh empty set (const-constructible — a node holds one in `.bss`).
    pub const fn new() -> Self {
        Self {
            adds: [None; CAP],
            tombs: [None; CAP],
        }
    }

    // --- internal helpers --------------------------------------------------
    fn has_add(&self, e: &ElemId, t: &Tag) -> bool {
        self.adds
            .iter()
            .flatten()
            .any(|(ae, at)| ae == e && at == t)
    }
    fn tombed(&self, t: &Tag) -> bool {
        self.tombs.iter().flatten().any(|tt| tt == t)
    }
    fn has_tomb(&self, t: &Tag) -> bool {
        self.tombed(t)
    }
    fn push_add(&mut self, e: ElemId, t: Tag) -> Result<(), Error> {
        if self.has_add(&e, &t) {
            return Ok(()); // idempotent
        }
        let slot = self.adds.iter_mut().find(|s| s.is_none());
        match slot {
            Some(s) => {
                *s = Some((e, t));
                Ok(())
            }
            None => Err(Error::Full),
        }
    }
    fn push_tomb(&mut self, t: Tag) -> Result<(), Error> {
        if self.tombed(&t) {
            return Ok(());
        }
        let slot = self.tombs.iter_mut().find(|s| s.is_none());
        match slot {
            Some(s) => {
                *s = Some(t);
                Ok(())
            }
            None => Err(Error::Full),
        }
    }
    /// True iff the add at `i` is live AND is the first live occurrence of its element (so
    /// distinct-live iteration counts/hashes each element exactly once, even with concurrent adds).
    fn is_first_live(&self, i: usize) -> bool {
        let (e, t) = match self.adds[i] {
            Some(v) => v,
            None => return false,
        };
        if self.tombed(&t) {
            return false;
        }
        for j in 0..i {
            if let Some((ej, tj)) = self.adds[j] {
                if ej == e && !self.tombed(&tj) {
                    return false; // an earlier live tag already represents this element
                }
            }
        }
        true
    }

    // --- public API --------------------------------------------------------
    /// Add `elem` under a fresh tag `(node, seq)`. Idempotent for an identical `(elem, tag)`.
    /// `Err(Full)` if no add-slot remains. The caller supplies a per-node monotonic `seq`.
    pub fn add(&mut self, elem: ElemId, node: u8, seq: u32) -> Result<Tag, Error> {
        let t = Tag { node, seq };
        self.push_add(elem, t)?;
        Ok(t)
    }

    /// Observed-remove: tombstone every currently-live tag of `elem`. Returns the number of tags
    /// newly tombed (0 if the element is absent). Concurrent adds elsewhere (tags not yet observed
    /// here) are untouched → add-wins.
    pub fn remove(&mut self, elem: &ElemId) -> usize {
        let mut victims: [Option<Tag>; CAP] = [None; CAP];
        let mut n = 0;
        for slot in self.adds.iter().flatten() {
            let (ae, at) = slot;
            if ae == elem && !self.tombed(at) {
                victims[n] = Some(*at);
                n += 1;
            }
        }
        let mut tombed = 0;
        for v in victims.iter().flatten() {
            if self.push_tomb(*v).is_ok() {
                tombed += 1;
            }
        }
        tombed
    }

    /// True iff `elem` has at least one add-tag that is not tombed.
    pub fn contains(&self, elem: &ElemId) -> bool {
        self.adds
            .iter()
            .flatten()
            .any(|(ae, at)| ae == elem && !self.tombed(at))
    }

    /// Merge another set (full state OR a delta — same op). Union of `adds` and `tombs`.
    /// `Err(Full)` if absorbing new entries would exceed `CAP` (size `CAP` for the expected fleet).
    pub fn merge(&mut self, other: &Self) -> Result<(), Error> {
        for (e, t) in other.adds.iter().flatten() {
            self.push_add(*e, *t)?;
        }
        for t in other.tombs.iter().flatten() {
            self.push_tomb(*t)?;
        }
        Ok(())
    }

    /// The minimal fragment `other` is missing: `self`'s adds/tombs not already in `other`.
    /// Merging this delta into `other` is equivalent to merging all of `self` (anti-entropy).
    /// The result is a subset of `self`, so it always fits `CAP`.
    pub fn delta_vs(&self, other: &Self) -> Self {
        let mut d = Self::new();
        for (e, t) in self.adds.iter().flatten() {
            if !other.has_add(e, t) {
                let _ = d.push_add(*e, *t);
            }
        }
        for t in self.tombs.iter().flatten() {
            if !other.has_tomb(t) {
                let _ = d.push_tomb(*t);
            }
        }
        d
    }

    /// Order-independent digest of the **live** element set (domain-separated `0x04`). Two replicas
    /// that converged on the same live set produce the same digest — the O(1) "are we converged?"
    /// check, and the value L2/L3 anchor/sign. Computed as `truncate16( XOR over live elems of
    /// sha(0x04 ‖ elem) )`: XOR is commutative/associative (order-independent) and each *distinct*
    /// live element contributes exactly once. Empty set ⇒ all-zero.
    pub fn digest<F: Fn(&[u8]) -> [u8; 32]>(&self, sha: F) -> [u8; ID_LEN] {
        let mut acc = [0u8; 32];
        let mut buf = [0u8; 1 + ID_LEN];
        buf[0] = DOMAIN;
        for i in 0..CAP {
            if self.is_first_live(i) {
                if let Some((e, _)) = self.adds[i] {
                    buf[1..].copy_from_slice(&e);
                    let h = sha(&buf);
                    for (a, hb) in acc.iter_mut().zip(h.iter()) {
                        *a ^= *hb;
                    }
                }
            }
        }
        let mut out = [0u8; ID_LEN];
        out.copy_from_slice(&acc[..ID_LEN]);
        out
    }

    /// Count of distinct live elements.
    pub fn len(&self) -> usize {
        (0..CAP).filter(|&i| self.is_first_live(i)).count()
    }

    /// True iff no element is live.
    pub fn is_empty(&self) -> bool {
        !(0..CAP).any(|i| self.is_first_live(i))
    }
}
