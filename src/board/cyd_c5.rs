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
// 16 MB flash, 8 MB PSRAM. The C6 watch is ALSO 16 MB flash (partitions.csv's
// last partition ends at 0xC20000 — an earlier draft of this comment said 4 MB,
// which was the C6's ROM-REGION ceiling before widen_rom_region, not its flash
// size). So the OTA layout ports UNCHANGED: same partitions.csv, same 6 MB A/B
// slots, no board variant needed. What does NOT port is the C6's heap story —
// PSRAM changes it entirely, and the reclaimed-pool scarcity and the
// 256-SceneTexture ceiling are C6 MEASUREMENTS that must not be inherited
// (the same measured-never-inherited rule as the stack floor).

// === Constants with LIVE call sites in main.rs (compile-time requirement) ===
// The CYD has no I2C peripherals in use — touch is SPI, there is no PMU, IMU or
// RTC chip. These exist because main.rs's bring-up references them
// unconditionally until the capability-gating pass lands; the first-boot plan
// (mapper §5) satisfies the I2C drivers with a fake-bus shim, so these values
// configure a bus that talks to NOTHING. They must never be read as hardware
// facts about this board.
pub const I2C_FREQ_HZ: u32 = 400_000;
pub const TP_I2C_ADDR: u8 = 0x38;
pub const IMU_I2C_ADDR: u8 = 0x6B;
pub const RTC_I2C_ADDR: u8 = 0x51;

/// UI hit-geometry for the CYD layout set (`ui/cyd/`, 320x240 landscape).
/// PLACEHOLDER values pending the layout work — they mirror nothing yet, and
/// the C5 arm's story playback is gated off until they do.
pub mod ui {
    pub const STORY_PAUSE_RECT: (u16, u16, u16, u16) = (0, 0, 0, 0);

    /// Switcher card stack (#31) — MUST match `ui/cyd/switcher.slint`
    /// (landscape 320x240; geometry from that file's port header, landed at
    /// 42dd687). Slot i spans y `TOP + i*PITCH .. + H`.
    pub const SWITCHER_CARD_TOP: u16 = 40;
    pub const SWITCHER_CARD_H: u16 = 52;
    pub const SWITCHER_CARD_PITCH: u16 = 58;
    pub const SWITCHER_CARDS: usize = 3;

    /// Shade card stack (#32) — MUST match `ui/cyd/shade.slint`.
    pub const SHADE_CARD_TOP: u16 = 38;
    pub const SHADE_CARD_H: u16 = 60;
    pub const SHADE_CARD_PITCH: u16 = 66;
    pub const SHADE_CARDS: usize = 3;
}

// === Soft-douse contract (BINDING — set by the shipped power-menu caption) ===
// The CYD power menu reads: "screen and radios off · tap the glass to wake"
// (with a drawn tap-mark — the one caption a user reads to UNDO something).
// The Rust that implements soft douse (no deep sleep on this board: esp-hal
// 1.1.1 is radio XOR sleep) must therefore:
//   (a) wake on the TOUCH IRQ (XPT2046 /IRQ on GPIO3 — glass-verified), and
//   (b) bring the radios back WITH the screen — no further user action.
// If the relight path cannot restore radios, the caption changes BEFORE this
// firmware ships, not after. Whoever lands douse owns keeping that sentence
// true.

// === Board identity for the UI (Luna's §1d — BOARD-FACT retirement) ===
// Rust formats, Slint displays: these feed the root properties board-chip /
// board-mem / backlight-dimmable / has-boot-key so no shared scene ever
// hardcodes a board fact again (the chip-text line was wrong TWICE that way).
pub const CHIP_NAME: &str = "ESP32-C5";
pub const MEM_SUMMARY: &str = "8 MB PSRAM \u{00b7} 16 MB flash";
/// no LEDC in this HAL generation (see the backlight note above) — binary GPIO only.
pub const BACKLIGHT_DIMMABLE: bool = false;
/// PROVISIONAL: boot straps are 26/27/28 and no key is confirmed wired; the
/// board may be touch-only. Flip only with a measured press on glass.
pub const HAS_BOOT_KEY: bool = false;

// SPI clocks for the shared display/touch bus (drivers/spi_bus.rs reads
// these). MEASURED values from the CYD bring-up: ST7789 at 20 MHz, XPT2046 at
// 2.5 MHz on the same bus with per-select retuning.
pub const SPI_DISPLAY_HZ: u32 = 20_000_000;
pub const SPI_TOUCH_HZ: u32 = 2_500_000;

// Touch coordinate transform (peripherals/touch.rs applies these after the
// raw FocalTech read; identity on boards whose touch matches the panel).
/// UNUSED until morpheus's XPT2046 driver merges (NullTouch today); his
/// driver carries its own measured calibration transform.
pub const TOUCH_SWAP_XY: bool = false;
pub const TOUCH_INVERT_X: bool = false;
pub const TOUCH_INVERT_Y: bool = false;

/// FT6336U deaf-Monitor quirk is S3-CYD-only (see esp32s3_cyd.rs); this
/// board keeps the original FocalTech Monitor init.
pub const TOUCH_FT6336_ACTIVE_QUIRK: bool = false;

/// `chip_id` in the esp-idf app-image header (LE u16 at bytes 12..14) for
/// this board's SoC. Both OTA paths (WiFi + mesh) refuse a mismatch BEFORE
/// the first flash write — the wrong arm's image passes the 0xE9 magic check.
/// MEASURED by morpheus from real espflash images (his 1e41596); matches
/// esp-idf's chip-id enum (ESP32-C5 = 23).
pub const ESP_IMAGE_CHIP_ID: u16 = 0x0017;
