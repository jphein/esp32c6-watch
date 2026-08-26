// Board pin definitions for Waveshare ESP32-C6-Touch-AMOLED-2.06
// Source: waveshare/esp32_c6_touch_amoled_2_06 BSP component v2.0.0

// === QSPI Display (CO5300 AMOLED, 410x502 RGB565) ===
// SCLK=GPIO0, SDIO0..3=GPIO1..4, CS=GPIO5, RST=GPIO11 — wired in main.rs.
pub const LCD_WIDTH: u16 = 410;
pub const LCD_HEIGHT: u16 = 502;
pub const LCD_COL_OFFSET: u16 = 22;
pub const LCD_ROW_OFFSET: u16 = 0;

// === I2C Bus (SDA=GPIO8, SCL=GPIO7) ===
pub const I2C_FREQ_HZ: u32 = 400_000;

// === Touch (FT3168, INT=GPIO15, RST=GPIO10) ===
pub const TP_I2C_ADDR: u8 = 0x38;

// === IMU (QMI8658) ===
pub const IMU_I2C_ADDR: u8 = 0x6B;

// === RTC (PCF85063) ===
pub const RTC_I2C_ADDR: u8 = 0x51;

// === Audio (ES8311 codec over I2S) ===
// MCLK=GPIO19, SCLK=GPIO20, LRCK=GPIO22,
// ES8311 ASDOUT (ADC/mic data out, codec→SoC) = GPIO21  → SoC I2S RX DIN
// ES8311 DSDIN  (DAC data in,  SoC→codec)      = GPIO23  → SoC I2S TX DOUT
// (Per the V1.0 schematic page-1 pin table: I2S_ASDOUT=GPIO21, I2S_DSDIN=GPIO23.
// The old "DAC in=21/ADC out=23" was SWAPPED — reading GPIO23 for the mic got the
// playback line, hence exact-zero capture.)
// speaker amp enable=GPIO6 (keep LOW unless playing audio).

/// UI hit-geometry for THIS board's layout set (`ui/slint/`, 410x502 portrait).
///
/// These exist because Slint's event dispatch is dead while `play_chapter`
/// parks the main loop — the mid-playback touch path hit-tests raw panel
/// coordinates in Rust (main.rs), and hardcoding them there is how the numbers
/// silently diverge from the .slint the moment a layout moves. Every rect here
/// MUST mirror its `ui/slint/story.slint` tile exactly; the layout set and this
/// module change together or not at all. The CYD board carries its own values
/// for its own layout.
pub mod ui {
    /// story READ page, PAUSE tile: x0, x1, y0, y1 (inclusive band).
    pub const STORY_PAUSE_RECT: (u16, u16, u16, u16) = (22, 198, 378, 438);

    /// Switcher card stack (#31) — MUST match `ui/slint/switcher.slint`
    /// (slot i spans y `TOP + i*PITCH .. + H`).
    pub const SWITCHER_CARD_TOP: u16 = 110;
    pub const SWITCHER_CARD_H: u16 = 84;
    pub const SWITCHER_CARD_PITCH: u16 = 96;
    /// Visible card slots (the suspension list may be longer; overlay shows "+N").
    pub const SWITCHER_CARDS: usize = 4;

    /// Shade card stack (#32) — MUST match `ui/slint/shade.slint`.
    pub const SHADE_CARD_TOP: u16 = 76;
    pub const SHADE_CARD_H: u16 = 84;
    pub const SHADE_CARD_PITCH: u16 = 92;
    /// Visible shade cards (the ring holds up to 8; overlay shows "+N").
    pub const SHADE_CARDS: usize = 4;
}

// === Board identity for the UI (Luna's §1d — BOARD-FACT retirement) ===
// Rust formats, Slint displays: these feed the root properties board-chip /
// board-mem / backlight-dimmable / has-boot-key so no shared scene ever
// hardcodes a board fact again (the chip-text line was wrong TWICE that way).
pub const CHIP_NAME: &str = "ESP32-C6";
pub const MEM_SUMMARY: &str = "no PSRAM \u{00b7} 16 MB flash";
/// AMOLED brightness via the CO5300 command set — smoothly dimmable.
pub const BACKLIGHT_DIMMABLE: bool = true;
/// BOOT is a first-class input (#59 button map).
pub const HAS_BOOT_KEY: bool = true;

// Touch coordinate transform (peripherals/touch.rs applies these after the
// raw FocalTech read; identity on boards whose touch matches the panel).
/// the FT3168 reports panel-native portrait coordinates directly.
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
/// esp-idf's chip-id enum (ESP32-C6 = 13).
pub const ESP_IMAGE_CHIP_ID: u16 = 0x000D;
