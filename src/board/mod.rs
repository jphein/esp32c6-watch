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

// Exactly one board, checked here rather than discovered as 200 duplicate-item
// errors. (Neither is also an error, but that one fails loudly on its own —
// every `board::` path breaks.)
#[cfg(all(feature = "board-waveshare-c6", feature = "board-cyd-c5"))]
compile_error!("exactly one board-* feature: board-waveshare-c6 XOR board-cyd-c5");
