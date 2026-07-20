// SRAM framebuffer for the CO5300 on ESP32-C6 (no PSRAM).
// The S3 firmware keeps two full RGB565 buffers (823KB) in PSRAM; the C6 has
// 512KB of SRAM total, so we store the frame as RGB332 (1 byte/pixel,
// 410*502 = ~201KB) and expand to RGB565 through a 256-entry LUT while
// streaming to the panel. Apps still draw in Rgb565 via DrawTarget; colors
// quantize to 8 levels of red/green and 4 of blue.

use embedded_graphics_core::draw_target::DrawTarget;
use embedded_graphics_core::geometry::{OriginDimensions, Point, Size};
use embedded_graphics_core::pixelcolor::raw::RawU16;
use embedded_graphics_core::pixelcolor::Rgb565;
use embedded_graphics_core::prelude::*;
use embedded_graphics_core::primitives::Rectangle;

use crate::board;
use crate::drivers::co5300::{Co5300Display, DisplayError};

use alloc::vec::Vec;

const WIDTH: usize = board::LCD_WIDTH as usize;
const HEIGHT: usize = board::LCD_HEIGHT as usize;
const PIXEL_COUNT: usize = WIDTH * HEIGHT;

#[inline(always)]
fn rgb565_to_332(raw: u16) -> u8 {
    // rrrrrggggggbbbbb -> rrrgggbb
    (((raw >> 13) as u8) << 5) | ((((raw >> 8) & 0x07) as u8) << 2) | (((raw >> 3) & 0x03) as u8)
}

const fn expand_332_to_565(c: u8) -> u16 {
    let r3 = (c >> 5) as u16;
    let g3 = ((c >> 2) & 0x07) as u16;
    let b2 = (c & 0x03) as u16;
    let r5 = (r3 << 2) | (r3 >> 1);
    let g6 = (g3 << 3) | g3;
    let b5 = (b2 << 3) | (b2 << 1) | (b2 >> 1);
    (r5 << 11) | (g6 << 5) | b5
}

const LUT_332_TO_565: [u16; 256] = {
    let mut lut = [0u16; 256];
    let mut i = 0;
    while i < 256 {
        lut[i] = expand_332_to_565(i as u8);
        i += 1;
    }
    lut
};

pub struct Framebuffer {
    buf: Vec<u8>,
    row: Vec<u16>, // scratch row for LUT expansion during flush
}

impl Framebuffer {
    /// Allocate without aborting on OOM: games grab ~201KB on entry and the
    /// shell reclaims it on exit. `None` = the heap can't fit a frame right now
    /// (e.g. a WiFi window is holding the RAM), so the caller stays in the shell.
    pub fn try_new() -> Option<Self> {
        let mut buf: Vec<u8> = Vec::new();
        buf.try_reserve_exact(PIXEL_COUNT).ok()?;
        buf.resize(PIXEL_COUNT, 0);
        let mut row: Vec<u16> = Vec::new();
        row.try_reserve_exact(WIDTH).ok()?;
        row.resize(WIDTH, 0);
        Some(Self { buf, row })
    }

    /// Flush the whole frame to the display, expanding RGB332 -> RGB565 row by row.
    pub fn flush(&mut self, display: &mut Co5300Display) {
        display.set_addr_window(0, 0, WIDTH as u16, HEIGHT as u16);
        display.bus_mut().begin_pixels();
        for y in 0..HEIGHT {
            let src = &self.buf[y * WIDTH..(y + 1) * WIDTH];
            for (dst, &c) in self.row.iter_mut().zip(src) {
                *dst = LUT_332_TO_565[c as usize];
            }
            display.bus_mut().stream_pixels(&self.row);
        }
        display.bus_mut().end_pixels();
    }

    /// Flush only a rectangular region (dirty rect optimization),
    /// expanded to the CO5300's even 2x2 alignment requirement.
    pub fn flush_region(&mut self, display: &mut Co5300Display, x: u16, y: u16, w: u16, h: u16) {
        if w == 0 || h == 0 {
            return;
        }
        let mut x0 = (x as usize).min(WIDTH.saturating_sub(1));
        let mut y0 = (y as usize).min(HEIGHT.saturating_sub(1));
        let mut x1 = (x as usize).saturating_add(w as usize).min(WIDTH);
        let mut y1 = (y as usize).saturating_add(h as usize).min(HEIGHT);

        x0 &= !1;
        y0 &= !1;
        if x1 & 1 != 0 && x1 < WIDTH {
            x1 += 1;
        }
        if y1 & 1 != 0 && y1 < HEIGHT {
            y1 += 1;
        }
        if x1 <= x0 {
            x1 = (x0 + 2).min(WIDTH);
        }
        if y1 <= y0 {
            y1 = (y0 + 2).min(HEIGHT);
        }

        let flush_w = (x1 - x0).max(2).min(WIDTH - x0);
        let flush_h = (y1 - y0).max(2).min(HEIGHT - y0);

        display.set_addr_window(x0 as u16, y0 as u16, flush_w as u16, flush_h as u16);
        display.bus_mut().begin_pixels();
        for row_y in y0..(y0 + flush_h) {
            let src = &self.buf[row_y * WIDTH + x0..row_y * WIDTH + x0 + flush_w];
            for (dst, &c) in self.row[..flush_w].iter_mut().zip(src) {
                *dst = LUT_332_TO_565[c as usize];
            }
            display.bus_mut().stream_pixels(&self.row[..flush_w]);
        }
        display.bus_mut().end_pixels();
    }
}

impl OriginDimensions for Framebuffer {
    fn size(&self) -> Size {
        Size::new(WIDTH as u32, HEIGHT as u32)
    }
}

impl DrawTarget for Framebuffer {
    type Color = Rgb565;
    type Error = DisplayError;

    fn draw_iter<I>(&mut self, pixels: I) -> Result<(), Self::Error>
    where
        I: IntoIterator<Item = Pixel<Self::Color>>,
    {
        for Pixel(coord, color) in pixels.into_iter() {
            if coord.x >= 0 && coord.x < WIDTH as i32 && coord.y >= 0 && coord.y < HEIGHT as i32 {
                self.buf[coord.y as usize * WIDTH + coord.x as usize] =
                    rgb565_to_332(RawU16::from(color).into_inner());
            }
        }
        Ok(())
    }

    fn fill_contiguous<I>(&mut self, area: &Rectangle, colors: I) -> Result<(), Self::Error>
    where
        I: IntoIterator<Item = Self::Color>,
    {
        let clipped = area.intersection(&Rectangle::new(
            Point::zero(),
            Size::new(WIDTH as u32, HEIGHT as u32),
        ));
        if clipped.size.width == 0 || clipped.size.height == 0 {
            return Ok(());
        }
        // Iterate the full (unclipped) area so the color iterator stays in sync,
        // writing only in-bounds pixels.
        let x0 = area.top_left.x;
        let y0 = area.top_left.y;
        let w = area.size.width as i32;
        let mut i = 0i32;
        for color in colors.into_iter() {
            let x = x0 + (i % w);
            let y = y0 + (i / w);
            if x >= 0 && x < WIDTH as i32 && y >= 0 && y < HEIGHT as i32 {
                self.buf[y as usize * WIDTH + x as usize] =
                    rgb565_to_332(RawU16::from(color).into_inner());
            }
            i += 1;
        }
        Ok(())
    }

    fn fill_solid(&mut self, area: &Rectangle, color: Self::Color) -> Result<(), Self::Error> {
        let area = area.intersection(&Rectangle::new(
            Point::zero(),
            Size::new(WIDTH as u32, HEIGHT as u32),
        ));
        if area.size.width == 0 || area.size.height == 0 {
            return Ok(());
        }
        let c = rgb565_to_332(RawU16::from(color).into_inner());
        let x = area.top_left.x as usize;
        let y = area.top_left.y as usize;
        let x_end = x + area.size.width as usize;
        for row in y..y + area.size.height as usize {
            self.buf[row * WIDTH + x..row * WIDTH + x_end].fill(c);
        }
        Ok(())
    }
}
