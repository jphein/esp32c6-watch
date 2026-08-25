//! The vendored ledger stack, exercised at its laws: chain tamper-evidence
//! (L1) and CRDT merge properties (L4). smol verifies these in its own
//! harnesses; the vendored copy carries its own so a drifted re-vendor
//! cannot pass silently (the mesh-flood rule).

use mesh_ledger::crdt::OrSet;
use mesh_ledger::ledger::Ledger;
use sha2::{Digest, Sha256};

fn sha(data: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(data);
    h.finalize().into()
}

#[test]
fn ledger_chains_and_detects_tampering() {
    let mut l: Ledger<8, 32> = Ledger::new();
    assert!(l.is_empty());
    let t1 = l.append(b"first", sha);
    let t2 = l.append(b"second", sha);
    assert_ne!(t1, t2);
    assert_eq!(l.tip(), t2);
    assert_eq!(l.len(), 2);
    assert!(l.verify(sha).is_ok(), "an untampered chain verifies");
}

#[test]
fn ledger_ring_advances_base() {
    let mut l: Ledger<4, 32> = Ledger::new();
    let genesis_base = l.base();
    for i in 0..8u32 {
        l.append(&i.to_le_bytes(), sha);
    }
    assert!(l.verify(sha).is_ok(), "a wrapped ring still verifies from its base");
    assert_ne!(l.base(), genesis_base, "eviction advanced the checkpoint");
    assert_eq!(l.len(), 4);
}

#[test]
fn orset_add_remove_contains() {
    let mut s: OrSet<16> = OrSet::new();
    let apple = *b"apple___________";
    s.add(apple, 1, 1).unwrap();
    assert!(s.contains(&apple));
    assert_eq!(s.remove(&apple), 1);
    assert!(!s.contains(&apple), "observed-remove wins over the observed add");
}

#[test]
fn orset_merge_is_idempotent_and_commutative() {
    let x = *b"x_______________";
    let y = *b"y_______________";
    let mut a: OrSet<16> = OrSet::new();
    let mut b: OrSet<16> = OrSet::new();
    a.add(x, 1, 1).unwrap();
    b.add(y, 2, 1).unwrap();

    let mut ab = a.clone();
    ab.merge(&b).unwrap();
    let mut ba = b.clone();
    ba.merge(&a).unwrap();
    assert_eq!(ab.digest(sha), ba.digest(sha), "merge commutes");

    let before = ab.digest(sha);
    ab.merge(&b).unwrap();
    assert_eq!(ab.digest(sha), before, "merge is idempotent");
    assert!(ab.contains(&x) && ab.contains(&y));
}

#[test]
fn orset_concurrent_add_survives_remote_remove_of_older_tag() {
    // Add-wins bias for CONCURRENT ops: node 1 removes its OWN observed add;
    // node 2's concurrent add (a tag node 1 never observed) must survive merge.
    let x = *b"x_______________";
    let mut a: OrSet<16> = OrSet::new();
    a.add(x, 1, 1).unwrap();
    let mut b = a.clone();
    a.remove(&x); // node 1 removes what it observed
    b.add(x, 2, 7).unwrap(); // node 2 concurrently re-adds with its own tag
    a.merge(&b).unwrap();
    assert!(a.contains(&x), "the unobserved concurrent add survives");
}
