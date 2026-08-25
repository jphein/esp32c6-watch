//! #182 mesh-ledger L1 — the PURE per-node hash-chained append-only log (unsigned v1).
//!
//! ## What this is (design of record: `docs/superpowers/research/mesh-ledger-study.md`, #181)
//! The study's #1 ADOPT: give smol a **tamper-evident, replayable** history without a
//! blockchain, BFT, or a full CRDT. Each node keeps its **own** append-only log; every record
//! links the previous record's hash, so any insertion / deletion / reorder / byte-flip breaks
//! the chain and is caught by [`Ledger::verify`]. It finishes primitives smol already ships —
//! `dl_seq` (a per-source monotonic seq) and `boot_count` (a crash-safe monotonic head) — into
//! one structure. Enables fleet provenance (tamper-evident OTA/election/config history) and the
//! mesh-RPG world-state substrate.
//!
//! ## L1 scope (this module)
//! - **Unsigned, tamper-EVIDENT** (hash chain, sha256). *Authenticity/non-repudiation* (ed25519
//!   record signing) is **#184**, which rides ON TOP later — L1 is the substrate.
//! - **Pure + host-testable** (the `net/flood.rs` / `net/etx.rs` pattern): no `esp-hal`/`esp-wifi`,
//!   no `std`, no alloc, and — deliberately — **no hash crate dependency**. The sha256 is
//!   **injected** (`F: Fn(&[u8]) -> [u8; 32]`) so the core is dependency-free; the caller (the
//!   host verifier now, the firmware at #183) supplies it. Host-tested in `experiments/ledger_verify`.
//! - **NOT wired into the radio path** — gossip/relay of the chain is L2 (**#183**, HW-gated).
//!   This module is intentionally *not declared in `net.rs`* until then (it would otherwise be
//!   dead-code under `-D warnings`, the #164 lesson).
//!
//! ## The chain
//! `record_hash = truncate16( sha256( prev_hash(16) ‖ seq_le(4) ‖ payload ) )`. The first record
//! chains onto [`GENESIS`]. 16-byte truncation gives 2⁻¹²⁸ collision resistance — ample for a
//! 4–30 node fleet (study §3), at 16 B/record instead of 32.
//!
//! ## Bounded + crash-safe by construction
//! A fixed circular ring (`Ledger<CAP, PAY>`, no alloc, lives in `.bss`). When it overflows, the
//! oldest record is pruned and its hash becomes the **checkpoint** ([`Ledger::base`]) the retained
//! suffix chains onto — so tamper-evidence *survives pruning* (you can't rewrite the retained
//! chain without breaking from the checkpoint). RAM ≈ `CAP × (21 + PAY)` bytes (e.g. `<16, 32>` ≈
//! 850 B), well within the C3 budget.

/// Truncated chain-hash width in bytes (study §3: 16 B ⇒ 2⁻¹²⁸ collision).
pub const HASH_LEN: usize = 16;
/// A truncated chain hash.
pub type Hash = [u8; HASH_LEN];
/// The prev-hash the first record chains onto (an empty ledger's tip + base).
pub const GENESIS: Hash = [0u8; HASH_LEN];
/// Largest record payload that is hashed in one pass (bounds the stack scratch buffer). A
/// `Ledger<_, PAY>` requires `PAY <= MAX_PAYLOAD` (compile-time checked) so every stored byte is
/// covered by the chain hash — no un-hashed tail an attacker could tamper undetected.
pub const MAX_PAYLOAD: usize = 96;

/// Why [`Ledger::verify`] rejected a chain — with the record index (0 = oldest retained).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VerifyError {
    /// Record `index`'s recomputed hash ≠ its stored hash — a payload/hash/prev-link tamper.
    BadHash { index: usize },
    /// Record `index`'s seq is not contiguous with its predecessor — a reorder/insert/delete.
    BadSeq { index: usize },
}

/// One stored record. `Copy` so the ring lives inline in `.bss` (no heap). Fields are
/// `pub(crate)` so the `#[path]`-included host verifier can simulate on-wire tamper directly
/// (an attacker flipping a stored byte is exactly that) without a test-only production method.
#[derive(Clone, Copy)]
pub struct Rec<const PAY: usize> {
    pub(crate) seq: u32,
    pub(crate) len: u8,
    pub(crate) payload: [u8; PAY],
    pub(crate) hash: Hash,
}

impl<const PAY: usize> Rec<PAY> {
    const EMPTY: Self = Self {
        seq: 0,
        len: 0,
        payload: [0u8; PAY],
        hash: GENESIS,
    };
}

/// `prev ‖ seq ‖ payload` → truncated chain hash. Pure; `sha256` injected. Payload is already
/// clamped to `PAY <= MAX_PAYLOAD` by the caller, so the fixed scratch buffer always fits.
fn chain_hash<F: Fn(&[u8]) -> [u8; 32]>(prev: &Hash, seq: u32, payload: &[u8], sha256: &F) -> Hash {
    let mut buf = [0u8; HASH_LEN + 4 + MAX_PAYLOAD];
    buf[..HASH_LEN].copy_from_slice(prev);
    buf[HASH_LEN..HASH_LEN + 4].copy_from_slice(&seq.to_le_bytes());
    let n = payload.len().min(MAX_PAYLOAD);
    buf[HASH_LEN + 4..HASH_LEN + 4 + n].copy_from_slice(&payload[..n]);
    let full = sha256(&buf[..HASH_LEN + 4 + n]);
    let mut h = GENESIS;
    h.copy_from_slice(&full[..HASH_LEN]);
    h
}

/// A per-node hash-chained append-only log over a fixed ring of `CAP` records, each ≤ `PAY`
/// bytes. See the module docs. `CAP >= 1` and `PAY <= MAX_PAYLOAD` are compile-time enforced.
pub struct Ledger<const CAP: usize, const PAY: usize> {
    pub(crate) ring: [Rec<PAY>; CAP],
    /// Index of the oldest retained record (circular).
    pub(crate) head: usize,
    count: usize,
    next_seq: u32,
    tip: Hash,
    /// The hash the oldest RETAINED record chains onto: [`GENESIS`] until the first prune, then
    /// the pruned checkpoint's hash.
    base: Hash,
    /// Seq of the last pruned record (0 = nothing pruned ⇒ first retained seq is 1).
    base_seq: u32,
    pruned: u32,
}

impl<const CAP: usize, const PAY: usize> Ledger<CAP, PAY> {
    /// Compile-time invariants (evaluated when referenced in [`append`](Ledger::append)).
    const INVARIANTS: () = {
        assert!(CAP >= 1, "Ledger CAP must be >= 1");
        assert!(
            PAY <= MAX_PAYLOAD,
            "Ledger PAY must be <= MAX_PAYLOAD (else un-hashed tail)"
        );
    };

    /// An empty ledger (tip + base = [`GENESIS`], next seq = 1). `const` so a fleet of these
    /// initializes in `.bss` with no runtime work.
    pub const fn new() -> Self {
        Self {
            ring: [Rec::EMPTY; CAP],
            head: 0,
            count: 0,
            next_seq: 1,
            tip: GENESIS,
            base: GENESIS,
            base_seq: 0,
            pruned: 0,
        }
    }

    /// Append `payload` (clamped to `PAY`) as the next record, chaining it onto the current tip.
    /// Returns the new record's hash (the new tip). On overflow, prunes the oldest record and
    /// advances the [`base`](Ledger::base) checkpoint. `sha256` is the injected hasher.
    pub fn append<F: Fn(&[u8]) -> [u8; 32]>(&mut self, payload: &[u8], sha256: F) -> Hash {
        let () = Self::INVARIANTS; // force the compile-time CAP/PAY checks
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        let n = payload.len().min(PAY);
        let hash = chain_hash(&self.tip, seq, &payload[..n], &sha256);

        let mut rec = Rec::<PAY>::EMPTY;
        rec.seq = seq;
        rec.len = n as u8;
        rec.payload[..n].copy_from_slice(&payload[..n]);
        rec.hash = hash;

        if self.count == CAP {
            // Full → prune the oldest (ring[head]); its hash becomes the checkpoint.
            self.base = self.ring[self.head].hash;
            self.base_seq = self.ring[self.head].seq;
            self.pruned = self.pruned.saturating_add(1);
            self.ring[self.head] = rec;
            self.head = (self.head + 1) % CAP;
        } else {
            let idx = (self.head + self.count) % CAP;
            self.ring[idx] = rec;
            self.count += 1;
        }
        self.tip = hash;
        hash
    }

    /// Replay the retained chain from the [`base`](Ledger::base) checkpoint forward, recomputing
    /// each record's hash and checking seq contiguity. `Ok(())` if intact (including empty);
    /// `Err` at the first tampered record. Pure; `sha256` injected.
    pub fn verify<F: Fn(&[u8]) -> [u8; 32]>(&self, sha256: F) -> Result<(), VerifyError> {
        let mut prev = self.base;
        let mut expected_seq = self.base_seq.wrapping_add(1);
        for i in 0..self.count {
            let rec = &self.ring[(self.head + i) % CAP];
            if rec.seq != expected_seq {
                return Err(VerifyError::BadSeq { index: i });
            }
            let n = rec.len as usize;
            if chain_hash(&prev, rec.seq, &rec.payload[..n], &sha256) != rec.hash {
                return Err(VerifyError::BadHash { index: i });
            }
            prev = rec.hash;
            expected_seq = expected_seq.wrapping_add(1);
        }
        Ok(())
    }

    /// The newest record's hash (the chain tip), or [`GENESIS`] if empty.
    pub fn tip(&self) -> Hash {
        self.tip
    }
    /// The hash the oldest retained record chains onto ([`GENESIS`] until the first prune).
    pub fn base(&self) -> Hash {
        self.base
    }
    /// Retained record count (≤ `CAP`).
    pub fn len(&self) -> usize {
        self.count
    }
    /// True if no records are retained.
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }
    /// Total records pruned by ring overflow (the checkpoint has advanced this many times).
    pub fn pruned(&self) -> u32 {
        self.pruned
    }
}

impl<const CAP: usize, const PAY: usize> Default for Ledger<CAP, PAY> {
    fn default() -> Self {
        Self::new()
    }
}
