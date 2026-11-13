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
//!
//! **Pin constants (`*_GPIO…`) are documentation of record, not wiring** (#92).
//! esp-hal pins are typed peripheral fields (`peripherals.GPIO5`), and a
//! `const u8` cannot select one, so a use site names its pin as a field with a
//! `// board::NAME` comment beside it. The board file is still the place to read
//! the hardware; when a pin moves, change both. Every OTHER board constant
//! (addresses, clocks, flags, geometry) must have a real reader:
//! `tools/board-consts-audit.sh` lists any that do not, and exits 1.

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

// #92: HAS_AUDIO is the board file's statement; `has-audio-out` is what compiles
// the audio drivers. Before this assert nothing read HAS_AUDIO, so it could say
// either and the build would not notice. Now a board that flips one without the
// other does not compile.
const _: () = assert!(
    HAS_AUDIO == cfg!(feature = "has-audio-out"),
    "board::HAS_AUDIO disagrees with the board's has-audio-out feature"
);
// The battery-divider read in main.rs is compiled for board-esp32s3-cyd only
// (`#[cfg(feature = "board-esp32s3-cyd")] let (mut bat_adc, ..)`). A board that
// claims HAS_BATT_ADC without that path would report no battery, silently.
const _: () = assert!(
    HAS_BATT_ADC == cfg!(feature = "board-esp32s3-cyd"),
    "board::HAS_BATT_ADC is true only where main.rs compiles the battery-ADC path"
);
// main.rs implements ONE I2S clocking: MCLK driven from a pin (option B,
// a605ac2). The BCLK-derived fork does not exist, so a board may not ask for it.
const _: () = assert!(
    !AUDIO_BCLK_DERIVED,
    "board::AUDIO_BCLK_DERIVED = true needs the BCLK-derived I2S fork, which main.rs does not have"
);
