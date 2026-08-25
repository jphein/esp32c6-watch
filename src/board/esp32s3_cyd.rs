//! Board constants — **ES3C28P** ("Hosyond 2.8in ESP32-S3 Touchscreen"),
//! smol fleet node 162, sigil **eldritch-insignia** (id ALLOCATED; MAC fold 150).
//!
//! ⚠️ **SINGLE SOURCE**: every hardware fact here is LIFTED from
//! `smol:targets/s3-cyd/board-staging/board_es3c28p.rs`, which carries the full
//! provenance chain (vendor schematic → emberboy config.h → ESPHome → Rust on
//! glass → spike M1 on this exact unit) and the per-const hazards. Corrections
//! flow back to THAT file first — never fork the pin table; this module carries
//! only the subset the watch's board contract (src/drivers/panel.rs) consumes,
//! each with its citation line. Values re-checked against staging 2026-08-25.
//!
//! ⛔ FLASHING: the physical unit (serial 14:C1:9F:D1:C8:10) is owned by the
//! s3-cyd session — its bus carries a sealed same-model board differing in two
//! octets. All flashing routes through their serial-pinned guard or their GO.
//!
//! Toolchain: Xtensa — espup `+esp` toolchain, `xtensa-esp32s3-none-elf`,
//! build-std, and **opt-level 2 under fat LTO** (s/z crash LLVM; PORT-SCOPING
//! §6.1). See tools/build-s3.sh.

// === Display: ILI9341V, 240x320 native, driven 320x240 landscape ============
// board_es3c28p.rs: LCD_WIDTH/LCD_HEIGHT — logical landscape dims.
pub const LCD_WIDTH: u16 = 320;
pub const LCD_HEIGHT: u16 = 240;
// board_es3c28p.rs: LCD_COL_OFFSET/LCD_ROW_OFFSET — ILI9341 has the full
// 240x320 GRAM, no window offset in either orientation.
pub const LCD_COL_OFFSET: u16 = 0;
pub const LCD_ROW_OFFSET: u16 = 0;
// board_es3c28p.rs: PIN_LCD_DC = 46 (schematic + emberboy, board-class-verified).
pub const LCD_DC_GPIO: u8 = 46;
// board_es3c28p.rs: PIN_LCD_BACKLIGHT = 45, active HIGH.
pub const BACKLIGHT_GPIO: u8 = 45;
// board_es3c28p.rs: MADCTL_LANDSCAPE = 0x28 (human-verified on a sibling unit;
// 0x68 is the mirror trap), BGR order, INVERT_COLORS = true — the ILI9341V
// wants inversion ON where the C5's ST7789 wanted it OFF. Driver-side facts,
// recorded here so the panel bring-up does not re-derive them.
pub const MADCTL_LANDSCAPE: u8 = 0x28;
pub const LCD_COLOR_ORDER_BGR: bool = true;
pub const LCD_INVERT_COLORS: bool = true;
// board_es3c28p.rs: WS2812 status LED on GPIO42.
pub const WS2812_GPIO: u8 = 42;
// No SD slot on the shared display bus contract here: the ES3C28P's SD is its
// own concern and nothing in the watch touches it — no park-high needed (the
// C5's SD_CS_GPIO_PARK_HIGH exists because ITS SD shares the display bus).

// === I2C: a REAL bus with a REAL touch controller ===========================
// Unlike the C5 (whose I2C consts configure a bus that talks to nothing), this
// board has FT6336U cap touch at 0x38 on SDA=16/SCL=15 — same controller
// family and address as the C6's FT3168, which is why has-cap-touch is the
// FIRST capability this board should earn. Until that driver seam is proven on
// this chip the capability stays off and these are bring-up parameters only.
// board_es3c28p.rs: I2C_HZ = 100_000 (ESPHome-proven; NOT the C6's 400k).
pub const I2C_FREQ_HZ: u32 = 100_000;
// board_es3c28p.rs: I2C_ADDR_TOUCH = 0x38.
pub const TP_I2C_ADDR: u8 = 0x38;
// No IMU, no external RTC on this board — these satisfy main.rs's
// unconditional bring-up references and address NOTHING (the C5 precedent;
// the fake-bus shim answers for them). Never read these as hardware facts.
pub const IMU_I2C_ADDR: u8 = 0x6B;
pub const RTC_I2C_ADDR: u8 = 0x51;

// === UI hit-geometry =========================================================
/// PLACEHOLDERS: the S3 CYD is 320x240 landscape like the C5, so Luna's
/// `ui/cyd/` layout set is the intended scene root — but no layout pass has
/// RUN against this panel, so every rect here is zeros/C5-values pending that
/// pass (same discipline as the C5's own placeholder phase).
pub mod ui {
    pub const STORY_PAUSE_RECT: (u16, u16, u16, u16) = (0, 0, 0, 0);
    // C5 values as stand-ins — same 320x240 landscape, same ui/cyd scene set.
    // Confirm against the S3's own layout pass before trusting a swipe-kill.
    pub const SWITCHER_CARD_TOP: u16 = 40;
    pub const SWITCHER_CARD_H: u16 = 52;
    pub const SWITCHER_CARD_PITCH: u16 = 58;
    pub const SWITCHER_CARDS: usize = 3;
    pub const SHADE_CARD_TOP: u16 = 38;
    pub const SHADE_CARD_H: u16 = 60;
    pub const SHADE_CARD_PITCH: u16 = 66;
    pub const SHADE_CARDS: usize = 3;
}

// === Board identity for the UI (Luna's §1d — BOARD-FACT retirement) ===
// Rust formats, Slint displays: these feed the root properties board-chip /
// board-mem / backlight-dimmable / has-boot-key so no shared scene ever
// hardcodes a board fact again (the chip-text line was wrong TWICE that way).
pub const CHIP_NAME: &str = "ESP32-S3";
pub const MEM_SUMMARY: &str = "8 MB PSRAM \u{00b7} 16 MB flash";
/// LEDC exists on this chip (board_es3c28p.rs: PIN_LCD_BACKLIGHT 45, active high).
/// The watch's LEDC driver is NOT written yet — this is the hardware fact; the
/// driver catching up is tracked bring-up work.
pub const BACKLIGHT_DIMMABLE: bool = true;
/// GPIO0 BOOT is the entire button budget (BOARD.md).
pub const HAS_BOOT_KEY: bool = true;

// ⚠️ PAINT BUDGET IS NOT INHERITED: ui/cyd/geom.slint's 117-row band and the
// rules built on it are the C5's SPI arithmetic (61.4 ms full frame at 20 MHz).
// This board clocks its panel at 40 MHz (SPI_DISPLAY_HZ in board_es3c28p.rs),
// so the number is different — measure it during bring-up before treating any
// row constant as this board's fact (measured, never inherited).
