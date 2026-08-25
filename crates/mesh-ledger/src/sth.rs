//! #184 mesh-ledger L3 — the PURE ed25519 **Signed-Tree-Head** over the L2 (#183) anchor.
//!
//! ## What this is (design of record: `docs/superpowers/research/mesh-ledger-study.md` §L3, #181)
//! L2 ([`super::treehead`]) produces a Merkle **anchor** over the per-node #182 chains — but that
//! anchor is *symmetric-unforgeable-free*: anyone with the head-set recomputes it, so a node
//! can't distinguish a genuine crown anchor from a forged one. L3 makes it **non-repudiable**:
//! the crown **signs** the anchor (root + size + epoch) with ed25519, yielding a Certificate-
//! Transparency **Signed-Tree-Head**. Now a peer accepts "my head is in the fleet's anchor" only
//! if the STH signature is the crown's *and* an inclusion proof checks against the signed root.
//!
//! ## L3 scope (this module — the pure sign/verify core)
//! - Pure + host-testable (the `flood.rs`/`etx.rs`/`ledger.rs`/`treehead.rs` pattern): no
//!   `esp-hal`/`esp-wifi`, no `std`, no alloc, **no crypto-crate dep** — the ed25519 sign/verify
//!   are **injected** (`sign: Fn(&[u8]) -> [u8;64]`, `verify: Fn(&[u8], &[u8;64]) -> bool`). The
//!   firmware injects the **verify** it already ships (`ota.rs`'s `ed25519-compact` OTA verifier);
//!   whoever authors the STH injects the **sign**. Host-tested in `experiments/sth_verify`.
//! - **The key PROVISIONING is OUT of scope** — putting an on-device signing key on the crown is
//!   the #181-L3 gate (JP's greenlight/defer, HW-gated). This core is *agnostic to where the key
//!   lives*: it composes whatever sign/verify it's handed.
//! - **Decoupled from the anchor's internals:** the STH signs the anchor as an opaque 16-byte
//!   root (consistent with L1/L2 each being self-contained over `[u8;16]`). The composition with
//!   an inclusion proof is [`accept`] — the caller runs [`super::treehead::verify_inclusion`]
//!   against [`SignedTreeHead::root`] and passes the result.
//! - **NOT wired into the radio path** — deliberately *not declared in `net.rs`* (inert; no
//!   dead-code under `-D warnings`, the #164/#182/#183 pattern). Firmware build unaffected.
//!
//! ## The signed message (domain-separated)
//! `sig = ed25519( 0x02 ‖ root(16) ‖ size_le(4) ‖ epoch_le(4) )`. The `0x02` prefix is distinct
//! from L2's `0x00`/`0x01` leaf/node prefixes, so an STH signature can never be confused with a
//! Merkle hash pre-image. **`epoch` is inside the signed message** — a monotonic freshness marker
//! (the `dl_seq`/`boot_count` lineage) so a stale STH can't be replayed as a current one; the
//! caller enforces "epoch strictly newer," the signature makes epoch unforgeable.

/// Anchor/root width (matches L1/L2).
pub const HASH_LEN: usize = 16;
/// A Merkle anchor root (from L2) — signed here as opaque bytes.
pub type Hash = [u8; HASH_LEN];
/// An ed25519 detached signature.
pub type Sig = [u8; 64];
/// Domain-separation prefix for the STH signed message (distinct from L2's `0x00`/`0x01`).
pub const STH_PREFIX: u8 = 0x02;
/// Length of the canonical STH message that gets signed: `prefix ‖ root ‖ size ‖ epoch`.
pub const STH_MSG_LEN: usize = 1 + HASH_LEN + 4 + 4;

/// A crown-signed tree-head: the L2 anchor (`root`) at fleet size `size` and freshness `epoch`,
/// plus the ed25519 signature over their canonical encoding. `Copy` (no heap). Fields are
/// `pub(crate)` so the `#[path]`-included host verifier can simulate tamper directly.
#[derive(Clone, Copy)]
pub struct SignedTreeHead {
    pub(crate) root: Hash,
    pub(crate) size: u32,
    pub(crate) epoch: u32,
    pub(crate) sig: Sig,
}

impl SignedTreeHead {
    /// The signed L2 anchor root (feed this to [`super::treehead::verify_inclusion`]).
    pub fn root(&self) -> Hash {
        self.root
    }
    /// The fleet size (node count) the anchor covered.
    pub fn size(&self) -> u32 {
        self.size
    }
    /// The freshness epoch (authenticated — see the module docs).
    pub fn epoch(&self) -> u32 {
        self.epoch
    }
    /// The ed25519 signature.
    pub fn sig(&self) -> Sig {
        self.sig
    }
}

/// The canonical bytes that get signed/verified: `0x02 ‖ root ‖ size_le ‖ epoch_le`.
pub fn sth_message(root: &Hash, size: u32, epoch: u32) -> [u8; STH_MSG_LEN] {
    let mut m = [0u8; STH_MSG_LEN];
    m[0] = STH_PREFIX;
    m[1..1 + HASH_LEN].copy_from_slice(root);
    m[1 + HASH_LEN..1 + HASH_LEN + 4].copy_from_slice(&size.to_le_bytes());
    m[1 + HASH_LEN + 4..].copy_from_slice(&epoch.to_le_bytes());
    m
}

/// Sign an anchor `(root, size, epoch)` into a [`SignedTreeHead`] using the injected `sign`.
pub fn sign_sth<S: Fn(&[u8]) -> [u8; 64]>(
    root: Hash,
    size: u32,
    epoch: u32,
    sign: S,
) -> SignedTreeHead {
    let msg = sth_message(&root, size, epoch);
    SignedTreeHead {
        root,
        size,
        epoch,
        sig: sign(&msg),
    }
}

/// Verify an STH's signature over its own `(root, size, epoch)` using the injected `verify`.
/// Recomputes the canonical message from the STH's fields, so tampering *any* field fails.
pub fn verify_sth<V: Fn(&[u8], &[u8; 64]) -> bool>(sth: &SignedTreeHead, verify: V) -> bool {
    let msg = sth_message(&sth.root, sth.size, sth.epoch);
    verify(&msg, &sth.sig)
}

/// The L3 acceptance rule: a claim is accepted iff **the STH is a valid crown signature** AND
/// **the head is included under the signed root**. `inclusion_ok` is the caller's
/// [`super::treehead::verify_inclusion`]`(sth.root(), node_id, head, seq, proof, sha256)` result —
/// kept as a parameter so this module stays decoupled from L2's internals. Signature checked
/// first (cheap short-circuit before trusting the root the proof is checked against).
pub fn accept<V: Fn(&[u8], &[u8; 64]) -> bool>(
    sth: &SignedTreeHead,
    inclusion_ok: bool,
    verify: V,
) -> bool {
    verify_sth(sth, verify) && inclusion_ok
}
