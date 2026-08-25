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

#[cfg(feature = "board-esp32s3-cyd")]
mod esp32s3_cyd;
#[cfg(feature = "board-esp32s3-cyd")]
pub use esp32s3_cyd::*;

// Exactly one board, checked here rather than discovered as 200 duplicate-item
// errors. (None enabled is also an error, but that one fails loudly on its
// own — every `board::` path breaks.) Pairwise, so the message names the clash.
#[cfg(all(feature = "board-waveshare-c6", feature = "board-cyd-c5"))]
compile_error!("exactly one board-* feature: board-waveshare-c6 AND board-cyd-c5 are both on");
#[cfg(all(feature = "board-waveshare-c6", feature = "board-esp32s3-cyd"))]
compile_error!("exactly one board-* feature: board-waveshare-c6 AND board-esp32s3-cyd are both on");
#[cfg(all(feature = "board-cyd-c5", feature = "board-esp32s3-cyd"))]
compile_error!("exactly one board-* feature: board-cyd-c5 AND board-esp32s3-cyd are both on");
