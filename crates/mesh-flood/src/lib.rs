//! SMOLv1 multihop protocol layer — the PURE half, vendored from smol.
//!
//! ⚠️ SINGLE SOURCE + SYNC CONTRACT: both modules are VERBATIM copies from
//! `jphein/smol` @ f2e6e1d (`rust/clock/src/net/wire.rs` and `net/flood.rs`),
//! the same vendoring shape `mesh-elect` used. smol's copies stay
//! authoritative for the PROTOCOL — a wire-format change lands THERE first
//! and re-vendors here (the #36 epic's no-fork rule). Long-term home is
//! `rust/smol-core` in smol's tree (the intake plan's `wire` module); this
//! crate is the bridge until the standalone repo can consume that directly.
//!
//! What this buys the watch (#64, #86's substrate): the UP2 generic uplink
//! envelope + RELAYACK2 flooded ACK codec, and the managed-flood decision
//! core (SeenSet / forward_decision / HopLatch) — a watch can act as a RELAY
//! for stranded leaves and escalate its own uplinks when stranded, with the
//! byte-identical-when-in-range invariant smol's canary proved.
#![no_std]
#![forbid(unsafe_code)]

pub mod cfgsched;
pub mod etx;
pub mod flood;
pub mod wire;
