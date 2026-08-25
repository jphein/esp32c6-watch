// Board pin definitions for the NM-CYD-C5 (ESP32-C5 "Cheap Yellow Display").
// Source: cyd-c5 bring-up session 2026-08-24 (ESP-Claw board json; vendor demos).
// Node id 176 is smol's ALLOCATION — note the watch fleet derives node ids from
// the efuse MAC fold (sigil-id), so the allocated and derived ids must be
// reconciled before this board joins the mesh. See the branch notes.
//
// ⚠️ SCAFFOLD. Pins below are from the bring-up session's recon, not yet proven
// by a driver on this branch. The display/touch drivers land against
// `src/drivers/panel.rs`'s contract (owned by the cyd driver workstream).

// === SPI Display (ST7789, 320x240 RGB565, classic SPI — NOT quad) ===
// SCK=GPIO6, MOSI=GPIO7, MISO=GPIO2, CS=GPIO23, DC=GPIO24. Landscape-native.
// Vendor-confirmed oddities (vesper-drivers, 2026-08-24): NO reset GPIO — the
// panel is tied to SoC RESET, so init is SWRESET + 150 ms; inversion OFF
// (unusual for ST7789); BGR order; zero GRAM offsets in all rotations.
// Bus: display at 20 MHz, touch at 2.5 MHz on ONE shared SPI with per-device
// apply_config.
pub const LCD_WIDTH: u16 = 320;
pub const LCD_HEIGHT: u16 = 240;
pub const LCD_COL_OFFSET: u16 = 0;
pub const LCD_ROW_OFFSET: u16 = 0;
pub const LCD_DC_GPIO: u8 = 24;

// === SD card CS — PARK HIGH, always ===
// The SD slot shares the SPI bus. Its CS (GPIO10) floats unless driven, and a
// floating SD CS corrupts display transactions. The BOARD seam owns parking it
// high at init, before the first display byte, whether or not SD is ever used.
pub const SD_CS_GPIO_PARK_HIGH: u8 = 10;

// === Backlight (plain GPIO, no PWM requirement) ===
pub const BACKLIGHT_GPIO: u8 = 25;

// === Touch (XPT2046 resistive, SHARED SPI bus with the display, own CS) ===
// Resistive: needs calibration + debounce; pressure threshold replaces the
// FT3168's finger count. POLL-ONLY — no IRQ line is wired, so there is no
// touch interrupt to arm; consumers must sample (the firmware already does:
// every touch read in main.rs is a poll). CS pin per vendor demo — confirm
// against the board json before first flash (memory `nm-cyd-c5-board`).

// === WS2812 status LED ===
pub const WS2812_GPIO: u8 = 27;

// === Flash / PSRAM ===
// 16 MB flash, 8 MB PSRAM (the C6 watch has 4 MB flash, NO PSRAM). The watch's
// 6 MB A/B OTA slots fit; partitions-cyd-c5.csv is the variant to flash with.
// PSRAM changes the entire heap story — the C6's reclaimed-pool scarcity and
// its 256-SceneTexture ceiling are C6 MEASUREMENTS and must not be inherited
// (the same measured-never-inherited rule as the stack floor).
