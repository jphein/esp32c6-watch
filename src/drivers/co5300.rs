// CO5300 AMOLED display driver
// Translated from Arduino_CO5300.h/.cpp
// Resolution: 410x502, col_offset=22, RGB565

use embedded_graphics_core::draw_target::DrawTarget;
use embedded_graphics_core::geometry::{OriginDimensions, Size};
use embedded_graphics_core::pixelcolor::raw::RawU16;
use embedded_graphics_core::pixelcolor::Rgb565;
use embedded_graphics_core::prelude::*;
use embedded_graphics_core::primitives::Rectangle;

use esp_hal::delay::Delay;
use esp_hal::gpio::Output;

use crate::board;
use crate::drivers::qspi_bus::QspiBus;

use embedded_graphics_core::geometry::Point;

// CO5300 commands (from Arduino_CO5300.h)
const CMD_SWRESET: u8 = 0x01;
const CMD_SLPOUT: u8 = 0x11;
const CMD_INVOFF: u8 = 0x20;
const CMD_INVON: u8 = 0x21;
const CMD_DISPOFF: u8 = 0x28;
const CMD_DISPON: u8 = 0x29;
const CMD_CASET: u8 = 0x2A;
const CMD_PASET: u8 = 0x2B;
const CMD_RAMWR: u8 = 0x2C;
const CMD_MADCTL: u8 = 0x36;
const CMD_PIXFMT: u8 = 0x3A;
const CMD_SPIMODECTL: u8 = 0xC4;
const CMD_WCTRLD1: u8 = 0x53;
const CMD_BRIGHTNESS: u8 = 0x51;
const CMD_BRIGHTNESS_HBM: u8 = 0x63;
const CMD_WCE: u8 = 0x58;

// MADCTL flags
const MADCTL_RGB: u8 = 0x00;

// Delays
const RST_DELAY_MS: u32 = 200;
const SLPOUT_DELAY_MS: u32 = 120;
const SLPIN_DELAY_MS: u32 = 120;

pub struct Co5300Display<'d> {
    bus: QspiBus<'d>,
    reset: Output<'d>,
    delay: Delay,
    width: u16,
    height: u16,
    col_offset: u16,
    row_offset: u16,
}

#[derive(Debug)]
pub enum DisplayError {
    BusError,
}

impl<'d> Co5300Display<'d> {
    pub fn new(bus: QspiBus<'d>, reset: Output<'d>) -> Self {
        Self {
            bus,
            reset,
            delay: Delay::new(),
            width: board::LCD_WIDTH,
            height: board::LCD_HEIGHT,
            col_offset: board::LCD_COL_OFFSET,
            row_offset: board::LCD_ROW_OFFSET,
        }
    }

    /// Initialize the display. Must be called before any drawing.
    /// Follows the exact init sequence from Arduino_CO5300.h/cpp.
    pub fn init(&mut self) {
        // Hardware reset
        self.reset.set_high();
        self.delay.delay_millis(10);
        self.reset.set_low();
        self.delay.delay_millis(RST_DELAY_MS);
        self.reset.set_high();
        self.delay.delay_millis(RST_DELAY_MS);

        // Sleep Out
        self.bus.write_command(CMD_SLPOUT);
        self.delay.delay_millis(SLPOUT_DELAY_MS);

        // Init sequence from co5300_init_operations[]
        self.bus.write_c8d8(0xFE, 0x00); // Vendor register access
        self.bus.write_c8d8(CMD_SPIMODECTL, 0x80); // SPI mode control
        self.bus.write_c8d8(CMD_PIXFMT, 0x55); // 16-bit RGB565
        self.bus.write_c8d8(CMD_WCTRLD1, 0x20); // Write CTRL Display
        self.bus.write_c8d8(CMD_BRIGHTNESS_HBM, 0xFF); // HBM brightness max
        self.bus.write_command(CMD_DISPON); // Display ON
        self.bus.write_c8d8(CMD_BRIGHTNESS, 0xD0); // Normal brightness
        self.bus.write_c8d8(CMD_WCE, 0x00); // Contrast enhancement off

        // Set MADCTL for correct color order (RGB, no rotation)
        self.bus.write_c8d8(CMD_MADCTL, MADCTL_RGB);

        self.delay.delay_millis(10);

        // Inversion off (standard for this panel)
        self.bus.write_command(CMD_INVOFF);
    }

    /// Set the address window for pixel writes.
    pub fn set_addr_window(&mut self, x: u16, y: u16, w: u16, h: u16) {
        let x_start = x + self.col_offset;
        let x_end = x_start + w - 1;
        let y_start = y + self.row_offset;
        let y_end = y_start + h - 1;

        self.bus.write_c8d16d16(CMD_CASET, x_start, x_end);
        self.bus.write_c8d16d16(CMD_PASET, y_start, y_end);
        self.bus.write_command(CMD_RAMWR);
    }

    /// Fill the entire screen with a single color.
    pub fn fill_screen(&mut self, color: Rgb565) {
        let raw: u16 = RawU16::from(color).into_inner();
        self.set_addr_window(0, 0, self.width, self.height);
        let total = self.width as u32 * self.height as u32;
        self.bus.write_repeat(raw, total);
    }

    /// Fill a rectangular area with a solid color.
    pub fn write_pixels_area(&mut self, x: u16, y: u16, w: u16, h: u16, color: Rgb565) {
        let raw: u16 = RawU16::from(color).into_inner();
        self.set_addr_window(x, y, w, h);
        self.bus.write_repeat(raw, w as u32 * h as u32);
    }

    /// Get mutable reference to bus (for framebuffer flush).
    pub fn bus_mut(&mut self) -> &mut QspiBus<'d> {
        &mut self.bus
    }

    /// Set display brightness (0x00 = off, 0xD0 = default, 0xFF = max).
    pub fn set_brightness(&mut self, brightness: u8) {
        self.bus.write_c8d8(CMD_BRIGHTNESS, brightness);
    }

    /// Turn display on (exit sleep + display ON).
    /// MIPI DCS order: SLPOUT -> 120ms -> DISPON -> 20ms.
    pub fn display_on(&mut self) {
        self.bus.write_command(CMD_SLPOUT);
        self.delay.delay_millis(SLPOUT_DELAY_MS);
        self.bus.write_command(CMD_DISPON);
        self.delay.delay_millis(20);
    }

    /// Turn display off (DISPOFF + enter sleep).
    /// MIPI DCS order: DISPOFF -> 20ms -> SLPIN -> 120ms.
    pub fn display_off(&mut self) {
        self.bus.write_command(CMD_DISPOFF);
        self.delay.delay_millis(20);
        self.bus.write_command(0x10); // SLPIN
        self.delay.delay_millis(SLPIN_DELAY_MS);
    }
}

impl OriginDimensions for Co5300Display<'_> {
    fn size(&self) -> Size {
        Size::new(self.width as u32, self.height as u32)
    }
}

impl DrawTarget for Co5300Display<'_> {
    type Color = Rgb565;
    type Error = DisplayError;

    fn draw_iter<I>(&mut self, pixels: I) -> Result<(), Self::Error>
    where
        I: IntoIterator<Item = Pixel<Self::Color>>,
    {
        // CO5300 requires minimum 2x2 pixel writes.
        // Draw each pixel as a 2x2 block.
        for Pixel(coord, color) in pixels.into_iter() {
            if coord.x >= 0
                && coord.x < self.width as i32
                && coord.y >= 0
                && coord.y < self.height as i32
            {
                let raw: u16 = RawU16::from(color).into_inner();
                // Write 2x2 block (4 pixels)
                self.set_addr_window(coord.x as u16, coord.y as u16, 2, 2);
                self.bus.write_pixels(&[raw, raw, raw, raw]);
            }
        }
        Ok(())
    }

    fn fill_contiguous<I>(
        &mut self,
        area: &Rectangle,
        colors: I,
    ) -> Result<(), Self::Error>
    where
        I: IntoIterator<Item = Self::Color>,
    {
        let area = area.intersection(&Rectangle::new(
            Point::zero(),
            Size::new(self.width as u32, self.height as u32),
        ));

        if area.size.width == 0 || area.size.height == 0 {
            return Ok(());
        }

        self.set_addr_window(
            area.top_left.x as u16,
            area.top_left.y as u16,
            area.size.width as u16,
            area.size.height as u16,
        );

        // CO5300 requires minimum 2-line writes.
        // If height is 1, double it and duplicate each row.
        let actual_h = if area.size.height < 2 { 2 } else { area.size.height as u16 };
        let needs_row_dup = area.size.height < 2;

        self.set_addr_window(
            area.top_left.x as u16,
            area.top_left.y as u16,
            area.size.width as u16,
            actual_h,
        );

        self.bus.begin_pixels();
        let w = area.size.width as usize;
        let mut row_buf = [0u16; 128]; // max width we support per row
        let mut col = 0usize;

        for color in colors.into_iter() {
            if col < 128 {
                row_buf[col] = RawU16::from(color).into_inner();
            }
            col += 1;

            // End of row
            if col >= w {
                let slice = &row_buf[..w.min(128)];
                self.bus.stream_pixels(slice);
                if needs_row_dup {
                    // Duplicate the row for minimum 2-line requirement
                    self.bus.stream_pixels(slice);
                }
                col = 0;
            }
        }
        // Flush remaining partial row
        if col > 0 {
            let slice = &row_buf[..col.min(128)];
            self.bus.stream_pixels(slice);
            if needs_row_dup {
                self.bus.stream_pixels(slice);
            }
        }
        self.bus.end_pixels();

        Ok(())
    }

    fn fill_solid(
        &mut self,
        area: &Rectangle,
        color: Self::Color,
    ) -> Result<(), Self::Error> {
        let area = area.intersection(&Rectangle::new(
            Point::zero(),
            Size::new(self.width as u32, self.height as u32),
        ));

        if area.size.width == 0 || area.size.height == 0 {
            return Ok(());
        }

        let raw: u16 = RawU16::from(color).into_inner();
        self.set_addr_window(
            area.top_left.x as u16,
            area.top_left.y as u16,
            area.size.width as u16,
            area.size.height as u16,
        );
        self.bus.write_repeat(raw, area.size.width * area.size.height);
        Ok(())
    }
}
