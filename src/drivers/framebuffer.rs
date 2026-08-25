// SRAM framebuffer for the CO5300 on ESP32-C6 (no PSRAM).
// The S3 firmware keeps two full RGB565 buffers (823KB) in PSRAM; the C6 has
// 512KB of SRAM total. A full-res RGB332 frame (410*502 = ~201KB) can't coexist
// with the Slint scene + WiFi/BLE/mesh in the one main heap region, so the
// backing store is HALF-RES RGB332 (205*251 = ~51KB) and is nearest-neighbor
// upscaled 2x to the panel during flush. Apps still draw at FULL 410x502 via
// DrawTarget (`size()` reports full res); writes map their coords down by 2.
// Colors quantize to 8 levels of red/green and 4 of blue; games are grid-based
// so the half-res downscale is barely visible on them.

use embedded_graphics_core::draw_target::DrawTarget;
use embedded_graphics_core::geometry::{OriginDimensions, Point, Size};
use embedded_graphics_core::pixelcolor::raw::RawU16;
use embedded_graphics_core::pixelcolor::Rgb565;
use embedded_graphics_core::prelude::*;
use embedded_graphics_core::primitives::Rectangle;

use crate::board;
use crate::drivers::co5300::DisplayError;
use crate::drivers::ActivePanel;

use alloc::vec::Vec;

// Panel (full) resolution — what apps draw against.
const WIDTH: usize = board::LCD_WIDTH as usize;
const HEIGHT: usize = board::LCD_HEIGHT as usize;
// Backing-store (half) resolution — what actually lives in SRAM. LCD dims are
// even (410x502) so the /2 is exact.
const FB_WIDTH: usize = WIDTH / 2;
const FB_HEIGHT: usize = HEIGHT / 2;
const FB_PIXEL_COUNT: usize = FB_WIDTH * FB_HEIGHT;

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
    /// Half-res RGB332 backing store (FB_WIDTH * FB_HEIGHT bytes).
    buf: Vec<u8>,
    /// Scratch for one FULL-width (upscaled) RGB565 panel row during flush.
    row: Vec<u16>,
}

impl Framebuffer {
    /// Allocate without aborting on OOM: a game grabs ~51KB on entry and the
    /// shell reclaims it on exit. `None` = the heap can't fit it right now.
    pub fn try_new() -> Option<Self> {
        let mut buf: Vec<u8> = Vec::new();
        buf.try_reserve_exact(FB_PIXEL_COUNT).ok()?;
        buf.resize(FB_PIXEL_COUNT, 0);
        let mut row: Vec<u16> = Vec::new();
        row.try_reserve_exact(WIDTH).ok()?;
        row.resize(WIDTH, 0);
        Some(Self { buf, row })
    }

    /// Flush the whole frame to the panel: expand RGB332 -> RGB565 and
    /// nearest-neighbor upscale 2x (each half-res pixel -> a 2x2 panel block).
    /// Each half-res row feeds two consecutive panel rows.
    pub fn flush(&mut self, display: &mut ActivePanel) {
        display.set_addr_window(0, 0, WIDTH as u16, HEIGHT as u16);
        display.bus_mut().begin_pixels();
        for y in 0..HEIGHT {
            let fb_y = y / 2; // 0..FB_HEIGHT (exact: HEIGHT is even)
            let src = &self.buf[fb_y * FB_WIDTH..(fb_y + 1) * FB_WIDTH];
            // Double each source pixel horizontally into the full-width row.
            for (fx, &c) in src.iter().enumerate() {
                let px = LUT_332_TO_565[c as usize];
                self.row[2 * fx] = px;
                self.row[2 * fx + 1] = px;
            }
            display.bus_mut().stream_pixels(&self.row);
        }
        display.bus_mut().end_pixels();
    }
}

impl OriginDimensions for Framebuffer {
    fn size(&self) -> Size {
        // Report FULL resolution: apps lay out against 410x502; the /2 mapping
        // to the half-res store happens transparently in the write path.
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
                let fx = coord.x as usize / 2;
                let fy = coord.y as usize / 2;
                self.buf[fy * FB_WIDTH + fx] = rgb565_to_332(RawU16::from(color).into_inner());
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
        // writing only in-bounds pixels (mapped down to the half-res store).
        let x0 = area.top_left.x;
        let y0 = area.top_left.y;
        let w = area.size.width as i32;
        let mut i = 0i32;
        for color in colors.into_iter() {
            let x = x0 + (i % w);
            let y = y0 + (i / w);
            if x >= 0 && x < WIDTH as i32 && y >= 0 && y < HEIGHT as i32 {
                let fx = x as usize / 2;
                let fy = y as usize / 2;
                self.buf[fy * FB_WIDTH + fx] = rgb565_to_332(RawU16::from(color).into_inner());
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
        let y_end = y + area.size.height as usize;
        // Map the full-res rect onto the half-res store, covering every touched
        // backing pixel (div_ceil on the exclusive ends).
        let fbx0 = x / 2;
        let fbx1 = x_end.div_ceil(2).min(FB_WIDTH);
        let fby0 = y / 2;
        let fby1 = y_end.div_ceil(2).min(FB_HEIGHT);
        for fbrow in fby0..fby1 {
            self.buf[fbrow * FB_WIDTH + fbx0..fbrow * FB_WIDTH + fbx1].fill(c);
        }
        Ok(())
    }
}
