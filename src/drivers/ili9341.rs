//! ILI9341V panel driver — the ES3C28P (ESP32-S3 CYD, eldritch-insignia).
//!
//! Skeleton and discipline from morpheus's glass-proven `st7789.rs`
//! (feat/cyd-c5-gating): same [`SharedSpiBus`] underneath, same DCS command
//! set (CASET/RASET/RAMWR/MADCTL/COLMOD), same no-reset-pin SWRESET story.
//! The chip-specific difference is deliberate ABSENCE: the ST7789's
//! voltage/gamma block (PORCTRL/GCTRL/VCOMS/…) is that silicon's register
//! map, not this one's — the ILI9341V runs its power-on defaults, which is
//! what mipidsi 0.10 ships and what two sibling units run on glass
//! (burrito-fw, ember-satellite). If the bench finds the defaults washed
//! out, gamma tuning is a measured follow-up, not a guessed init line.
//!
//! Every panel fact here reads from `board::*`, lifted from smol's
//! `targets/s3-cyd/board-staging/board_es3c28p.rs` (the single source):
//! MADCTL 0x28 (human-verified; 0x68 is the mirror trap), BGR order,
//! INVERSION ON (where the ST7789 wants it OFF), 320x240 landscape with no
//! GRAM offsets, backlight GPIO45 active-high.
//!
//! The inherent surface mirrors `Co5300Display` exactly — `ActivePanel` in
//! `drivers/mod.rs` aliases one or the other per board, and main.rs compiles
//! against whichever is selected (the panel.rs structural-satisfaction
//! contract).

use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::prelude::*;
use esp_hal::delay::Delay;
use esp_hal::gpio::Output;

use crate::board;
use crate::drivers::spi_bus::SharedSpiBus;

const CMD_SWRESET: u8 = 0x01;
const CMD_SLPOUT: u8 = 0x11;
const CMD_INVOFF: u8 = 0x20;
const CMD_INVON: u8 = 0x21;
const CMD_NORON: u8 = 0x13;
const CMD_DISPOFF: u8 = 0x28;
const CMD_DISPON: u8 = 0x29;
const CMD_CASET: u8 = 0x2A;
const CMD_RASET: u8 = 0x2B;
const CMD_MADCTL: u8 = 0x36;
const CMD_COLMOD: u8 = 0x3A;

/// Datasheet: 5 ms after SWRESET before the next command, 120 ms out of
/// sleep. The generous values match the proven sibling inits.
const RST_DELAY_MS: u32 = 150;
const SLPOUT_DELAY_MS: u32 = 120;

/// PWM backlight over LEDC (#482): the brightness slider becomes a real dim
/// instead of a >0 threshold, and AOD's low value is genuinely low.
///
/// Gamma-squared mapping: LEDs are perceptually loud at low duty, so the
/// UI's linear 0-255 maps through `(b/255)^2` — AOD's 0x18 lands near 1%
/// duty instead of a linear 9%. 0 is EXACTLY 0 duty: the c992cb6 defect
/// class (a floor turning "off" into 16 = ON on a threshold backlight)
/// cannot re-enter through this path.
pub struct LedcBacklight {
    channel: esp_hal::ledc::channel::Channel<'static, esp_hal::ledc::LowSpeed>,
    /// Last non-zero percent, so `display_on` restores the user's level
    /// rather than blasting to 100%.
    last_on_pct: u8,
}

impl LedcBacklight {
    pub fn new(channel: esp_hal::ledc::channel::Channel<'static, esp_hal::ledc::LowSpeed>) -> Self {
        Self {
            channel,
            last_on_pct: 100,
        }
    }

    fn set_pct(&mut self, pct: u8) {
        use esp_hal::ledc::channel::ChannelIFace as _;
        // A failed duty write leaves the previous level — same degrade-not-
        // panic stance as every other status output on this firmware.
        let _ = self.channel.set_duty(pct.min(100));
        if pct > 0 {
            self.last_on_pct = pct.min(100);
        }
    }

    /// UI byte (0-255) -> gamma-squared duty percent (0-100).
    fn pct_for(brightness: u8) -> u8 {
        let b = brightness as u32;
        ((b * b * 100 + (255 * 255) / 2) / (255 * 255)) as u8
    }
}

pub struct Ili9341Display<'d> {
    bus: SharedSpiBus<'d>,
    /// LEDC PWM on GPIO45 (#482). Was a plain >0-threshold GPIO.
    backlight: LedcBacklight,
    delay: Delay,
    width: u16,
    height: u16,
    col_offset: u16,
    row_offset: u16,
}

impl<'d> Ili9341Display<'d> {
    pub fn new(bus: SharedSpiBus<'d>, backlight: LedcBacklight) -> Self {
        Self {
            bus,
            backlight,
            delay: Delay::new(),
            width: board::LCD_WIDTH,
            height: board::LCD_HEIGHT,
            col_offset: board::LCD_COL_OFFSET,
            row_offset: board::LCD_ROW_OFFSET,
        }
    }

    /// Initialize the panel. No reset line on this board
    /// (`HAS_LCD_RESET_PIN = false` in the board facts), so SWRESET carries
    /// the whole reset story — it is not optional.
    pub fn init(&mut self) {
        self.bus.write_command(CMD_SWRESET);
        self.delay.delay_millis(RST_DELAY_MS);

        self.bus.write_command(CMD_SLPOUT);
        self.delay.delay_millis(SLPOUT_DELAY_MS);

        self.bus.write_c8d8(CMD_COLMOD, 0x55); // 16 bpp RGB565
        self.bus.write_c8d8(
            CMD_MADCTL,
            board::MADCTL_LANDSCAPE
                | if board::LCD_COLOR_ORDER_BGR { 0x08 } else { 0x00 },
        );
        // The ILI9341V wants inversion ON where the ST7789 wants it OFF —
        // a board fact, not a driver choice.
        self.bus.write_command(if board::LCD_INVERT_COLORS {
            CMD_INVON
        } else {
            CMD_INVOFF
        });
        self.bus.write_command(CMD_NORON);
        self.bus.write_command(CMD_DISPON);
        self.delay.delay_millis(20);
        self.backlight.set_pct(100);
    }

    pub fn set_addr_window(&mut self, x: u16, y: u16, w: u16, h: u16) {
        let x0 = x + self.col_offset;
        let x1 = x0 + w - 1;
        let y0 = y + self.row_offset;
        let y1 = y0 + h - 1;
        self.bus.write_c8d16d16(CMD_CASET, x0, x1);
        self.bus.write_c8d16d16(CMD_RASET, y0, y1);
        // A new window means the next pixel push must restart at its origin
        // with RAMWR (0x2C), not resume the prior run with RAMWR_CONT (0x3C).
        // `SharedSpiBus` latches RAMWR_CONT after the first push, so without
        // re-arming here every strip after the first lands at the previous
        // GRAM pointer instead of this window — "chunks displaced/torn", colors
        // and signal intact (JP on-glass 2026-08-26). The ST7789 sibling's
        // set_addr_window arms it; the ILI9341 must too.
        self.bus.arm_ramwr();
    }

    pub fn fill_screen(&mut self, color: Rgb565) {
        let w = self.width;
        let h = self.height;
        self.write_pixels_area(0, 0, w, h, color);
    }

    pub fn write_pixels_area(&mut self, x: u16, y: u16, w: u16, h: u16, color: Rgb565) {
        self.set_addr_window(x, y, w, h);
        let raw: u16 = embedded_graphics::pixelcolor::raw::RawU16::from(color).into_inner();
        self.bus.begin_pixels();
        self.bus.write_repeat(raw, w as u32 * h as u32);
        self.bus.end_pixels();
    }

    pub fn bus_mut(&mut self) -> &mut SharedSpiBus<'d> {
        &mut self.bus
    }

    /// Real PWM dim (#482). 0 = duty 0 (hard off — no floor can turn "off"
    /// into ON); otherwise the UI byte maps through a gamma-squared curve so
    /// AOD's 0x18 is a genuine glow, not full brightness.
    pub fn set_brightness(&mut self, brightness: u8) {
        let pct = if brightness == 0 {
            0
        } else {
            LedcBacklight::pct_for(brightness).max(1)
        };
        self.backlight.set_pct(pct);
    }

    pub fn display_on(&mut self) {
        self.bus.write_command(CMD_DISPON);
        // Restore the user's level, not an unconditional 100%.
        let pct = self.backlight.last_on_pct;
        self.backlight.set_pct(pct);
    }

    pub fn display_off(&mut self) {
        self.backlight.set_pct(0);
        self.bus.write_command(CMD_DISPOFF);
    }
}
