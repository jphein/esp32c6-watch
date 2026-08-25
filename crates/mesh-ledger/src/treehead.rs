//! #183 L2 — the PURE crown-ordered **tree-head**: a CT-style (RFC 6962 lineage) Merkle
//! **anchor** over the per-node #182 chain heads.
//!
//! ## What this is (design of record: `docs/superpowers/research/mesh-ledger-study.md` §L2, #181)
//! #182 gives each node its own tamper-evident chain. L2 gives the *fleet* a single
//! **consistency anchor**: hash the set of per-node chain heads into one root, so any node can
//! compare its view to the crown's in O(1) and detect divergence — the study's "crown-ordered
//! tree-head" (a Certificate-Transparency Signed-Tree-Head, minus the signature in v1). The
//! anchor also yields **inclusion proofs**: a node proves its head is under the anchor in
//! O(log n) sibling hashes instead of shipping every head — real airtime economy on a 250-B MTU.
//!
//! ## L2 scope (this module — the pure anchor-COMPUTATION core)
//! - Pure + host-testable (the `flood.rs`/`etx.rs`/`ledger.rs` pattern): no `esp-hal`/`esp-wifi`,
//!   no `std`, no alloc, **no hash-crate dep** — sha256 is **injected**. Host-tested in
//!   `experiments/treehead_verify`.
//! - **The "crown ORDERS it" coordination is OUT of scope** (gossiping the anchor via
//!   `dl_seq`+retained-MQTT and collecting peer heads over the mesh is the fw/radio layer,
//!   HW-gated). This module just *computes* the anchor + proofs from a head-set.
//! - **NOT wired into the radio path** — deliberately *not declared in `net.rs`* (inert; no
//!   dead-code under `-D warnings`, the #164 lesson), same as #182's `ledger.rs`.
//!
//! ## The Merkle construction (CT / RFC 6962)
//! Heads are kept in **canonical order by `node_id`** so every node computes the *identical*
//! anchor regardless of the order it learned them (the load-bearing invariant). Domain-separated
//! hashing prevents leaf/internal confusion: `leaf = H(0x00 ‖ node_id ‖ head ‖ seq)`,
//! `internal = H(0x01 ‖ left ‖ right)`, split at the largest power of two `< n` (balanced,
//! O(log n) proofs). 16-byte truncation (matches #182). Empty set → [`EMPTY_ROOT`].

/// Truncated hash width (matches #182's chain hash).
pub const HASH_LEN: usize = 16;
/// A truncated hash — a node's chain head, a Merkle node, or the anchor root.
pub type Hash = [u8; HASH_LEN];
/// The anchor of an empty head-set.
pub const EMPTY_ROOT: Hash = [0u8; HASH_LEN];
/// Max audit-path depth — supports up to `2^MAX_DEPTH` = 256 nodes (smol fleet is ≤ ~30).
pub const MAX_DEPTH: usize = 8;

const LEAF_PREFIX: u8 = 0x00;
const NODE_PREFIX: u8 = 0x01;

/// A head-set is full (a *new* node past capacity `N`; updating an existing node still works).
#[derive(Debug)]
pub struct Full;

/// One node's chain head: `(node_id, head_hash, seq)`.
#[derive(Clone, Copy)]
pub struct Head {
    pub(crate) node_id: u8,
    pub(crate) head: Hash,
    pub(crate) seq: u32,
}
impl Head {
    const EMPTY: Self = Self {
        node_id: 0,
        head: EMPTY_ROOT,
        seq: 0,
    };
}

/// An inclusion proof: the audit path (sibling hashes, deepest-first) plus the `index`/`size`
/// needed to reconstruct the anchor. `siblings[..len]` are meaningful.
#[derive(Clone, Copy)]
pub struct Proof {
    pub(crate) index: usize,
    pub(crate) size: usize,
    pub(crate) siblings: [Hash; MAX_DEPTH],
    pub len: usize,
}

fn truncate(full: [u8; 32]) -> Hash {
    let mut h = EMPTY_ROOT;
    h.copy_from_slice(&full[..HASH_LEN]);
    h
}

/// `H(0x00 ‖ node_id ‖ head ‖ seq_le)` — a domain-separated leaf hash.
fn leaf_hash<F: Fn(&[u8]) -> [u8; 32]>(node_id: u8, head: &Hash, seq: u32, sha256: &F) -> Hash {
    let mut buf = [0u8; 1 + 1 + HASH_LEN + 4];
    buf[0] = LEAF_PREFIX;
    buf[1] = node_id;
    buf[2..2 + HASH_LEN].copy_from_slice(head);
    buf[2 + HASH_LEN..].copy_from_slice(&seq.to_le_bytes());
    truncate(sha256(&buf))
}

/// `H(0x01 ‖ left ‖ right)` — a domain-separated internal node.
fn internal<F: Fn(&[u8]) -> [u8; 32]>(left: &Hash, right: &Hash, sha256: &F) -> Hash {
    let mut buf = [0u8; 1 + 2 * HASH_LEN];
    buf[0] = NODE_PREFIX;
    buf[1..1 + HASH_LEN].copy_from_slice(left);
    buf[1 + HASH_LEN..].copy_from_slice(right);
    truncate(sha256(&buf))
}

/// Largest power of two strictly less than `n` (`n >= 2`).
fn split(n: usize) -> usize {
    let mut k = 1;
    while k << 1 < n {
        k <<= 1;
    }
    k
}

/// Merkle Tree Hash over already-computed leaf hashes (`leaves.len() >= 1`), RFC 6962 recursion.
fn mth<F: Fn(&[u8]) -> [u8; 32]>(leaves: &[Hash], sha256: &F) -> Hash {
    if leaves.len() == 1 {
        return leaves[0];
    }
    let k = split(leaves.len());
    internal(
        &mth(&leaves[..k], sha256),
        &mth(&leaves[k..], sha256),
        sha256,
    )
}

/// Audit path for leaf `m` in `leaves`: sibling subtree-roots, appended deepest-first. Returns
/// the number written. Depth ≤ `MAX_DEPTH` for `leaves.len() ≤ 256` (guarded on `HeadSet`).
fn audit_path<F: Fn(&[u8]) -> [u8; 32]>(
    leaves: &[Hash],
    m: usize,
    sha256: &F,
    out: &mut [Hash; MAX_DEPTH],
) -> usize {
    if leaves.len() == 1 {
        return 0;
    }
    let k = split(leaves.len());
    let (l, sib) = if m < k {
        (
            audit_path(&leaves[..k], m, sha256, out),
            mth(&leaves[k..], sha256),
        )
    } else {
        (
            audit_path(&leaves[k..], m - k, sha256, out),
            mth(&leaves[..k], sha256),
        )
    };
    if l < MAX_DEPTH {
        out[l] = sib;
    }
    l + 1
}

/// Reconstruct the root from a leaf hash + its audit path, mirroring [`audit_path`]. Never
/// panics on a malformed/tampered proof (returns a value that simply won't match the anchor).
fn root_from<F: Fn(&[u8]) -> [u8; 32]>(
    leaf: Hash,
    m: usize,
    n: usize,
    sibs: &[Hash],
    sha256: &F,
) -> Hash {
    if n <= 1 {
        return leaf;
    }
    if sibs.is_empty() {
        return EMPTY_ROOT; // malformed proof — cannot match a real anchor
    }
    let k = split(n);
    let sib = sibs[sibs.len() - 1];
    let rest = &sibs[..sibs.len() - 1];
    if m < k {
        internal(&root_from(leaf, m, k, rest, sha256), &sib, sha256)
    } else {
        internal(&sib, &root_from(leaf, m - k, n - k, rest, sha256), sha256)
    }
}

/// A bounded, canonically-ordered (by `node_id`) set of per-node chain heads → one Merkle anchor.
pub struct HeadSet<const N: usize> {
    pub(crate) heads: [Head; N],
    count: usize,
}

impl<const N: usize> HeadSet<N> {
    /// `N` must fit the audit-path depth (≤ 2^MAX_DEPTH), evaluated when referenced in `upsert`.
    const DEPTH_OK: () = assert!(
        N <= (1usize << MAX_DEPTH),
        "HeadSet N must be <= 2^MAX_DEPTH"
    );

    /// An empty head-set. `const` so a crown holds one in `.bss`.
    pub const fn new() -> Self {
        Self {
            heads: [Head::EMPTY; N],
            count: 0,
        }
    }

    /// Insert-or-update a node's head, keeping canonical `node_id` order. `Err(Full)` only when a
    /// *new* node exceeds `N` (updating an existing node always succeeds).
    pub fn upsert(&mut self, node_id: u8, head: Hash, seq: u32) -> Result<(), Full> {
        let () = Self::DEPTH_OK;
        for i in 0..self.count {
            if self.heads[i].node_id == node_id {
                self.heads[i].head = head;
                self.heads[i].seq = seq;
                return Ok(());
            }
        }
        if self.count == N {
            return Err(Full);
        }
        let mut pos = self.count;
        while pos > 0 && self.heads[pos - 1].node_id > node_id {
            self.heads[pos] = self.heads[pos - 1];
            pos -= 1;
        }
        self.heads[pos] = Head { node_id, head, seq };
        self.count += 1;
        Ok(())
    }

    /// Number of nodes in the set.
    pub fn len(&self) -> usize {
        self.count
    }
    /// True if no nodes are tracked.
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// The Merkle **anchor** (tree-head) over the canonical head-set. Deterministic — identical
    /// for the same set regardless of upsert order. `EMPTY_ROOT` for an empty set.
    pub fn root<F: Fn(&[u8]) -> [u8; 32]>(&self, sha256: F) -> Hash {
        if self.count == 0 {
            return EMPTY_ROOT;
        }
        let mut leaves = [EMPTY_ROOT; N];
        for (leaf, h) in leaves[..self.count]
            .iter_mut()
            .zip(self.heads[..self.count].iter())
        {
            *leaf = leaf_hash(h.node_id, &h.head, h.seq, &sha256);
        }
        mth(&leaves[..self.count], &sha256)
    }

    /// An inclusion [`Proof`] that `node_id`'s head is under the anchor, or `None` if absent.
    pub fn inclusion_proof<F: Fn(&[u8]) -> [u8; 32]>(
        &self,
        node_id: u8,
        sha256: F,
    ) -> Option<Proof> {
        let mut index = None;
        for i in 0..self.count {
            if self.heads[i].node_id == node_id {
                index = Some(i);
                break;
            }
        }
        let index = index?;
        let mut leaves = [EMPTY_ROOT; N];
        for (leaf, h) in leaves[..self.count]
            .iter_mut()
            .zip(self.heads[..self.count].iter())
        {
            *leaf = leaf_hash(h.node_id, &h.head, h.seq, &sha256);
        }
        let mut siblings = [EMPTY_ROOT; MAX_DEPTH];
        let len = audit_path(&leaves[..self.count], index, &sha256, &mut siblings);
        Some(Proof {
            index,
            size: self.count,
            siblings,
            len,
        })
    }
}

impl<const N: usize> Default for HeadSet<N> {
    fn default() -> Self {
        Self::new()
    }
}

/// Verify that `(node_id, head, seq)` is included under `root` via `proof`. Recomputes the leaf
/// and replays the audit path; total (no panic) on any malformed/tampered proof.
pub fn verify_inclusion<F: Fn(&[u8]) -> [u8; 32]>(
    root: Hash,
    node_id: u8,
    head: Hash,
    seq: u32,
    proof: &Proof,
    sha256: F,
) -> bool {
    if proof.len > MAX_DEPTH || proof.index >= proof.size.max(1) {
        return false;
    }
    let leaf = leaf_hash(node_id, &head, seq, &sha256);
    root_from(
        leaf,
        proof.index,
        proof.size,
        &proof.siblings[..proof.len],
        &sha256,
    ) == root
}
