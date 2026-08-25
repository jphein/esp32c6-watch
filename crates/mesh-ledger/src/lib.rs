//! smol's mesh-ledger stack L1–L4 — the PURE multi-writer shared-state layer,
//! vendored VERBATIM from `jphein/smol` @ 58b0dfa
//! (`rust/clock/src/net/{ledger,treehead,sth,crdt,ledger_link}.rs`), the
//! mesh-flood vendoring shape: smol stays authoritative, wire/format changes
//! land there first and re-vendor here (the #36 no-fork rule).
//!
//! L1 `ledger`: per-node hash-chained append-only log (tamper-evident).
//! L2 `treehead`: Merkle tree head over the log.
//! L3 `sth`: signed tree head exchange.
//! L4 `crdt`: delta-state OR-Set / G-Set for multi-writer shared state.
//! `ledger_link`: the glue binding L1–L3 into one advancing checkpoint.
//!
//! No consumer on the watch YET — this is the parity substrate: shared game
//! state (mesh_snake score boards), fleet config history, and the #185 line
//! all build on it. The hasher is injected everywhere, so the crate carries
//! zero deps and the firmware hands in its own sha2.
#![no_std]
#![forbid(unsafe_code)]

pub mod ledger;
pub mod treehead;
pub mod sth;
pub mod crdt;
#[cfg(feature = "link")]
pub mod ledger_link;
