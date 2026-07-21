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
