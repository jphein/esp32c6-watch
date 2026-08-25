//! #181 mesh-ledger FIRMWARE WIRING — binds the pure, host-tested L1/L2/L3 cores
//! ([`super::ledger`] #182 hash-chain · [`super::treehead`] #183 CT-Merkle anchor · [`super::sth`]
//! #184 ed25519 Signed-Tree-Head) into the running firmware. The cores are pure (sha256 + ed25519
//! *injected*, no HAL); this module supplies the concrete injections — `sha2` (the same crate the
//! OTA path uses) and `ed25519-compact` — and owns the per-node state.
//!
//! ## What a node does with the ledger
//! - **L1:** each node keeps its OWN tamper-evident append-only chain in `.bss` (resets at boot —
//!   it is a within-session provenance log, gossip/persistence is the HW-gated follow-up).
//! - **L2/L3 (crown):** the crown folds the head-set into a Merkle anchor and, IF it has an
//!   on-device ed25519 key ([provisioned at a JP-supervised USB touch](crate::ota) — never in CI,
//!   the repo, MQTT, or logs), signs it into a Signed-Tree-Head.
//! - **verify-what-you-sign:** before surfacing an STH the crown re-verifies its OWN signature AND
//!   an inclusion proof of its own head under the signed root — a self-consistency gate that also
//!   exercises the full verify half of the cores (so nothing is dead-code in this binary crate).
//!
//! ## Key provisioning (the #181-L3 gate — NO key generation here)
//! The signing key is a 32-byte ed25519 SEED read from NVS ([`crate::ota::resolve_ledger_key`]),
//! seeded ONLY by a deliberate USB-touch provisioning step. A node with no key runs verify-only /
//! publishes an UNSIGNED anchor — never fabricates a key. The seed never leaves the device.

use super::{ledger, sth, treehead};

/// L1 chain depth (records retained before the oldest is pruned into the checkpoint). 16 × ~53 B
/// ≈ 850 B `.bss` (study §L1). The chain is tamper-evident across pruning via the checkpoint.
pub const LEDGER_CAP: usize = 16;
/// Max L1 record payload (≤ [`ledger::MAX_PAYLOAD`]). A compact provenance digest fits well under.
pub const LEDGER_PAY: usize = 32;
/// Max nodes in the crown's head-set for the L2 anchor (fleet ≤ ~30; must be ≤ 2^treehead::MAX_DEPTH).
pub const LEDGER_NODES: usize = 32;

/// The injected SHA-256 (the L1/L2 hasher). Same primitive the OTA path already links, so the
/// ledger adds no new hash dependency.
pub fn sha256(data: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(data);
    let mut out = [0u8; 32];
    out.copy_from_slice(&h.finalize());
    out
}

/// Detached ed25519 sign of `msg` under the 32-byte `seed` (the injected L3 signer). Deterministic
/// (no RNG — `Noise` omitted), so it is CI/attestation-free. Derives the keypair from the seed each
/// call — a few ms, dwarfed by the anchor cadence.
#[cfg(not(target_arch = "xtensa"))]
fn ed25519_sign(seed: &[u8; 32], msg: &[u8]) -> [u8; 64] {
    use ed25519_compact::{KeyPair, Seed};
    let kp = KeyPair::from_seed(Seed::new(*seed));
    let sig = kp.sk.sign(msg, None);
    let mut out = [0u8; 64];
    out.copy_from_slice(sig.as_ref());
    out
}
// Per-arch backend (the ota-proto shim's sibling): ed25519-compact crashes
// the Xtensa backend at every opt level; dalek signs deterministically from
// the same seed → identical signatures on both arms (RFC 8032).
#[cfg(target_arch = "xtensa")]
fn ed25519_sign(seed: &[u8; 32], msg: &[u8]) -> [u8; 64] {
    use ed25519_dalek::{Signer, SigningKey};
    let sk = SigningKey::from_bytes(seed);
    sk.sign(msg).to_bytes()
}

/// Verify `sig` over `msg` against the public key DERIVED FROM `seed` (the injected L3 verifier for
/// the self-check — a node verifies its own STH with its own key). Peer-STH acceptance (verifying a
/// DIFFERENT crown's key) is the HW-gated L2-coordination follow-up; this local verify keeps the
/// cores' verify half exercised.
fn ed25519_verify(seed: &[u8; 32], msg: &[u8], sig: &[u8; 64]) -> bool {
    use ed25519_compact::{KeyPair, Seed, Signature};
    let kp = KeyPair::from_seed(Seed::new(*seed));
    let Ok(s) = Signature::from_slice(sig) else {
        return false;
    };
    kp.pk.verify(msg, &s).is_ok()
}

/// Per-node ledger state: the L1 chain, the crown's L2 head-set view, a monotonic STH freshness
/// epoch, and the optional on-device signing seed.
pub struct LedgerLink {
    chain: ledger::Ledger<LEDGER_CAP, LEDGER_PAY>,
    heads: treehead::HeadSet<LEDGER_NODES>,
    epoch: u32,
    signing_key: Option<[u8; 32]>,
}

impl LedgerLink {
    /// An empty link (no key until [`set_signing_key`](Self::set_signing_key)). `const` → lives in
    /// `.bss` with no runtime init.
    pub const fn new() -> Self {
        Self {
            chain: ledger::Ledger::new(),
            heads: treehead::HeadSet::new(),
            epoch: 0,
            signing_key: None,
        }
    }

    /// Install (or clear) the on-device ed25519 signing seed read from NVS at boot. A `Some` key
    /// makes this node a signing crown; `None` publishes an unsigned anchor.
    pub fn set_signing_key(&mut self, key: Option<[u8; 32]>) {
        self.signing_key = key;
    }

    /// True once a signing key is installed (surfaced so the fleet can see which nodes can sign,
    /// without ever exposing the key).
    pub fn can_sign(&self) -> bool {
        self.signing_key.is_some()
    }

    /// L1: append a provenance record to this node's chain, returning the new tip. Payload is
    /// clamped to [`LEDGER_PAY`] by the core.
    pub fn append(&mut self, payload: &[u8]) -> ledger::Hash {
        self.chain.append(payload, sha256)
    }

    /// A compact chain summary for the DIAG record: `(tip, retained_len, verify_ok)`. Re-verifies
    /// the retained chain (cheap for `CAP=16`) so `verify_ok=false` in telemetry is an immediate
    /// tamper flag.
    pub fn chain_summary(&self) -> (ledger::Hash, usize, bool) {
        (self.chain.tip(), self.chain.len(), self.chain.verify(sha256).is_ok())
    }

    /// L2: fold this node's current head into the head-set and return the Merkle anchor over it.
    /// Used both directly (unsigned-anchor path) and inside [`sign_and_selfcheck`](Self::sign_and_selfcheck).
    pub fn anchor(&mut self, own_id: u8) -> treehead::Hash {
        // seq = retained record count (0 for an empty chain) — the freshness the leaf hash binds.
        let _ = self.heads.upsert(own_id, self.chain.tip(), self.chain.len() as u32);
        self.heads.root(sha256)
    }

    /// L3 (crown): fold in this node's head, sign the anchor into a Signed-Tree-Head, and
    /// VERIFY-WHAT-YOU-SIGN — re-check the STH signature AND an inclusion proof of this node's head
    /// under the signed root before returning it. `None` when there is no signing key, or if the
    /// self-check fails (never surface an STH that doesn't verify). Bumps the freshness epoch.
    pub fn sign_and_selfcheck(&mut self, own_id: u8) -> Option<sth::SignedTreeHead> {
        let key = self.signing_key?;
        let own_head = self.chain.tip();
        let own_seq = self.chain.len() as u32;
        let root = self.anchor(own_id);
        let size = self.heads.len() as u32;
        let epoch = self.epoch;
        self.epoch = self.epoch.wrapping_add(1);

        let head = sth::sign_sth(root, size, epoch, |m| ed25519_sign(&key, m));

        // Self-consistency gate: the signature must verify under our own key AND our head must be
        // provably included under the signed root (exercises verify_sth + verify_inclusion + accept).
        let proof = self.heads.inclusion_proof(own_id, sha256)?;
        let inclusion_ok =
            treehead::verify_inclusion(head.root(), own_id, own_head, own_seq, &proof, sha256);
        if sth::accept(&head, inclusion_ok, |m, s| ed25519_verify(&key, m, s)) {
            Some(head)
        } else {
            log::warn!("smol #181: STH self-check FAILED — not surfacing");
            None
        }
    }
}

impl Default for LedgerLink {
    fn default() -> Self {
        Self::new()
    }
}
