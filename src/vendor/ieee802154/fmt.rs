//! Minimal logging + `unwrap!` shim for the vendored 802.15.4 driver.
//!
//! esp-radio provides these crate-globally via its own `fmt.rs` (a feature-gated
//! defmt / log_04 shim + a `Try`-based `unwrap!`). Rather than drag that whole
//! machinery in, we map the log macros to the `log` crate the firmware already
//! uses, and `unwrap!` to a plain `.unwrap()` (the driver only ever calls it
//! single-arg on `Result`s whose error is `Debug`). `#[macro_use]`d first in
//! `mod.rs` so the sibling submodules (raw/hal/pib/frame/clocks) see them.
#![allow(unused_macros)]

macro_rules! trace { ($($t:tt)*) => { ::log::trace!($($t)*) }; }
macro_rules! debug { ($($t:tt)*) => { ::log::debug!($($t)*) }; }
macro_rules! info  { ($($t:tt)*) => { ::log::info!($($t)*) }; }
macro_rules! warn  { ($($t:tt)*) => { ::log::warn!($($t)*) }; }
macro_rules! error { ($($t:tt)*) => { ::log::error!($($t)*) }; }

macro_rules! unwrap {
    ($arg:expr $(,)?) => { ($arg).unwrap() };
    ($arg:expr, $($msg:tt)+) => { ($arg).expect($($msg)+) };
}
