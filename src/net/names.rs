//! Deterministic magical node names — re-exported from the `sigil-id` crate.
//!
//! v0.8.4 (#34) moved the verbatim smol/realm-sigil port that lived here into
//! `crates/sigil-id` so the corpus + index math are host-testable alongside
//! the new MAC-seeded per-device identity (see [`crate::net::sigil`]). One
//! pinned copy of the word tables serves both mesh peer names (id-seeded,
//! unchanged) and the device sigil (MAC-seeded). Call sites keep the
//! `crate::net::names::*` path.

pub use sigil_id::*;
