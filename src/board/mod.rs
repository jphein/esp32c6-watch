//! Board selection seam (#cyd-c5). Exactly one `board-*` feature is enabled
//! (the Cargo features enforce chip exclusivity); this module re-exports that
//! board's constants so consumers write `board::LCD_WIDTH` and never name a
//! board.
//!
//! What a board module must define is the CONTRACT listed in
//! `src/drivers/panel.rs` — the constants here, plus a display driver and a
//! touch driver satisfying the traits there. Capabilities the board lacks are
//! simply not compiled: gate on `#[cfg(feature = "has-pmu")]` etc., which the
//! board feature supplies (see Cargo.toml's BOARD TARGETS block for the model).

#[cfg(feature = "board-waveshare-c6")]
mod waveshare_c6;
#[cfg(feature = "board-waveshare-c6")]
pub use waveshare_c6::*;

#[cfg(feature = "board-cyd-c5")]
mod cyd_c5;
#[cfg(feature = "board-cyd-c5")]
pub use cyd_c5::*;

/// First-boot scaffold: a bus for the six I2C devices this board does not have.
/// **Fails closed by design** — see the module header before touching it.
#[cfg(feature = "board-cyd-c5")]
pub mod fake_i2c;

// Exactly one board, checked here rather than discovered as 200 duplicate-item
// errors. (Neither is also an error, but that one fails loudly on its own —
// every `board::` path breaks.)
#[cfg(all(feature = "board-waveshare-c6", feature = "board-cyd-c5"))]
compile_error!("exactly one board-* feature: board-waveshare-c6 XOR board-cyd-c5");

// ===========================================================================
// The display + touch seam (#cyd-c5)
// ===========================================================================
// `drivers/panel.rs` names a `BoardDisplay` alias as the thing the shell
// consumes; until now it existed only in that doc comment. These are it.
//
// ★ Why aliases and not `dyn PanelDriver`: these are the hottest calls in the
// firmware (30 ms paint budget, measured worsts riding 26-30 ms). A static seam
// costs nothing at runtime, and the contract is explicit that satisfaction is
// STRUCTURAL — the two drivers mirror each other's method names, so every one of
// the ~19 `display.` call sites in main.rs and the flusher's two-line hot path
// compile against either type with no edit at all. That property is what made
// this a type alias instead of a refactor, and it is worth not squandering: a
// method added to one driver and not the other silently un-ports the board.

/// The concrete display driver this board selects.
#[cfg(feature = "board-waveshare-c6")]
pub type BoardDisplay<'d> = crate::drivers::co5300::Co5300Display<'d>;
/// The concrete display driver this board selects.
#[cfg(feature = "board-cyd-c5")]
pub type BoardDisplay<'d> = crate::drivers::st7789::St7789Display<'d>;

/// The renderer flusher this board selects.
///
/// Per-board because the panels' window rules genuinely differ and the contract
/// is narrow enough not to care: the CO5300 needs even-aligned row PAIRS, the
/// ST7789 accepts a 1-pixel-tall window. Keeping the two-line apparatus a
/// CO5300-private workaround — rather than promoting one panel's hardware quirk
/// into a fleet-wide interface — is what makes this a two-line alias.
#[cfg(feature = "board-waveshare-c6")]
pub type BoardFlusher<'a, 'd> = crate::ui::slint_platform::TwoLineFlusher<'a, 'd>;
/// The renderer flusher this board selects.
#[cfg(feature = "board-cyd-c5")]
pub type BoardFlusher<'a, 'd> = crate::ui::slint_platform::SingleLineFlusher<'a, 'd>;
