//! ST7789 driver for the NM-CYD-C5's 2.8" 320x240 IPS panel, classic 4-wire SPI.
//!
//! # Parity contract with the watch's CO5300 driver
//!
//! This type is a deliberate analogue of `esp32c6-watch`'s
//! `drivers::co5300::Co5300Display`. The names and signatures below are chosen
//! so that the watch's renderer seam — `ui::slint_platform::TwoLineFlusher`,
//! whose only hardware contact is
//!
//! ```text
//! display.set_addr_window(x0, y_even, w, 2);
//! display.bus_mut().write_pixels(&scratch[..w * 2]);
//! ```
//!
//! — works here with a type swap and no edits. Same MIPI-DCS command family
//! (CASET `0x2A` / RASET `0x2B` / RAMWR `0x2C` / MADCTL `0x36` / COLMOD `0x3A`),
//! same RGB565 big-endian wire format, same `DrawTarget<Color = Rgb565>`.
//!
//! # ★ The alignment restriction does NOT carry over — and that matters upstream
//!
//! The watch's `slint_platform.rs:14-22` documents the CO5300's hardware
//! contract as "CASET/RASET windows need start AND extent divisible by 2 on
//! BOTH axes" (CO5300 datasheet §7.5.21/§7.5.22), and `co5300.rs` enforces a
//! matching "minimum 2x2 pixel write" everywhere. That restriction is the
//! entire reason the watch vendors a fork of Slint's software renderer
//! (`crates/i-slint-renderer-software`, wired in via `[patch.crates-io]`) to
//! even-align the dirty region before span emission.
//!
//! **The ST7789 has no such restriction.** Its CASET/RASET accept any start and
//! end, and 1x1 windows are legal. Evidence from the vendor's own sources:
//! `Demos/MicroPython/rotations/st7789py.py` sets 1-pixel-tall windows in
//! `hline()`, 1-pixel-wide windows in `vline()`, and 1x1 windows in `pixel()`,
//! unconditionally and with no rounding; and TFT_eSPI's ST7789 path ships no
//! LVGL-style rounder.
//!
//! Consequences, in order of usefulness:
//!   * [`DrawTarget::draw_iter`] writes true 1x1 windows instead of the
//!     CO5300 driver's 2x2 blocks;
//!   * [`DrawTarget::fill_contiguous`] drops the CO5300's "if height is 1,
//!     double it and duplicate each row" workaround entirely;
//!   * a one-window-per-line flusher becomes possible — see
//!     [`crate::seam::SingleLineFlusher`].
//!
//! ⚠️ **AMENDED 2026-08-24 by the watch session — the earlier version of this
//! note drew the wrong conclusion, and it is worth stating why.** It said the
//! vendored `crates/i-slint-renderer-software` fork was therefore unnecessary
//! on this board. That inference was wrong because it assumed the fork contains
//! only the alignment patch. It does not: per the contract at
//! `esp32c6-watch:src/drivers/panel.rs`, the fork *also* carries the scene
//! pooling and `pool_capacities()` instrumentation that the whole `[POOL]`
//! heap-attribution stack reads (#75). Putting the CYD on the stock renderer
//! would silently blind that instrumentation on one board of the fleet.
//!
//! **So the fork stays on every board.** The alignment freedom documented above
//! is still real and still exercised by this driver — even-aligned windows are
//! a legal subset, so the fork costs the CYD nothing. The lesson for anyone
//! reading this file later: "this board doesn't need constraint X" is not the
//! same claim as "this board doesn't need the component that provides X".
//!
//! # One invisible divergence: where RAMWR is sent
//!
//! `Co5300Display::set_addr_window` ends by sending RAMWR, because on QSPI the
//! subsequent pixel push re-opens its own transaction with the `0x32` opcode.
//! On classic SPI a memory write must stay inside one CS-low transaction, so
//! [`set_addr_window`](Self::set_addr_window) instead *arms* the bus
//! ([`SharedSpiBus::arm_ramwr`](super::spi_bus::SharedSpiBus::arm_ramwr)) and
//! the pixel call emits RAMWR itself — `RAMWR_CONT` (`0x3C`) for continuations,
//! so a chunked push still lands contiguously. Callers cannot observe the
//! difference; the call sequence they write is identical.

use embedded_graphics_core::draw_target::DrawTarget;
use embedded_graphics_core::geometry::{OriginDimensions, Point, Size};
use embedded_graphics_core::pixelcolor::raw::RawU16;
use embedded_graphics_core::pixelcolor::Rgb565;
use embedded_graphics_core::prelude::*;
use embedded_graphics_core::primitives::Rectangle;

use esp_hal::delay::Delay;
use esp_hal::gpio::Output;

use crate::board;
use crate::drivers::spi_bus::SharedSpiBus;
use crate::drivers::Rotation;

// ---------------------------------------------------------------------------
// ST7789 commands (MIPI-DCS; same family the CO5300 driver uses)
// ---------------------------------------------------------------------------
const CMD_SWRESET: u8 = 0x01;
const CMD_SLPOUT: u8 = 0x11;
const CMD_NORON: u8 = 0x13;
const CMD_INVOFF: u8 = 0x20;
const CMD_INVON: u8 = 0x21;
const CMD_DISPOFF: u8 = 0x28;
const CMD_DISPON: u8 = 0x29;
const CMD_CASET: u8 = 0x2A;
const CMD_RASET: u8 = 0x2B;
const CMD_MADCTL: u8 = 0x36;
const CMD_COLMOD: u8 = 0x3A;
const CMD_SLPIN: u8 = 0x10;

// Panel-tuning registers. Values below are the vendor's, not generic defaults.
const CMD_DISPFN: u8 = 0xB6; // display function control
const CMD_PORCTRL: u8 = 0xB2; // porch control
const CMD_GCTRL: u8 = 0xB7; // gate control
const CMD_VCOMS: u8 = 0xBB; // VCOM setting
const CMD_LCMCTRL: u8 = 0xC0; // LCM control
const CMD_VDVVRHEN: u8 = 0xC2; // VDV/VRH enable
const CMD_VRHS: u8 = 0xC3; // VRH set
const CMD_VDVS: u8 = 0xC4; // VDV set
const CMD_FRCTRL2: u8 = 0xC6; // frame rate control (normal mode)
const CMD_PWCTRL1: u8 = 0xD0; // power control 1
const CMD_PVGAMCTRL: u8 = 0xE0; // positive gamma
const CMD_NVGAMCTRL: u8 = 0xE1; // negative gamma

/// Delay after a reset (hardware or software) before the panel accepts commands.
const RST_DELAY_MS: u32 = 150;
const SLPOUT_DELAY_MS: u32 = 120;
const SLPIN_DELAY_MS: u32 = 120;

#[derive(Debug)]
pub enum DisplayError {
    BusError,
}

pub struct St7789Display<'d> {
    bus: SharedSpiBus<'d>,
    /// Panel reset.
    ///
    /// `None` on the NM-CYD-C5 — there is no reset GPIO; the panel's RESET is
    /// tied to the SoC's own RESET line (`connections.md:9` gives the TFT_RST
    /// column as "C5 RST", and `User_Setup-NM-CYD-C5.h:220`, `pins_arduino.h:92`
    /// and `platformio.ini:38` all say `TFT_RST = -1`). See [`crate::board`] for
    /// why the vendor MicroPython demo's `reset=Pin(0)` must NOT be copied.
    ///
    /// Kept as an `Option` rather than deleted so a board rev that wires one up
    /// is a constructor change, not a driver change.
    reset: Option<Output<'d>>,
    /// Backlight (GPIO25). Plain on/off — the vendor drives it with
    /// `analogWrite` (a soft LEDC channel) in `interface.cpp:85`, so real
    /// dimming needs an LEDC channel this driver deliberately does not claim.
    backlight: Output<'d>,
    delay: Delay,
    rotation: Rotation,
    width: u16,
    height: u16,
    col_offset: u16,
    row_offset: u16,
}

impl<'d> St7789Display<'d> {
    /// `reset` is `None` on this board — see the field docs.
    pub fn new(bus: SharedSpiBus<'d>, reset: Option<Output<'d>>, backlight: Output<'d>) -> Self {
        let rotation = board::DEFAULT_ROTATION;
        let (width, height) = rotation.size();
        Self {
            bus,
            reset,
            backlight,
            delay: Delay::new(),
            rotation,
            width,
            height,
            col_offset: board::LCD_COL_OFFSET,
            row_offset: board::LCD_ROW_OFFSET,
        }
    }

    /// Initialise the panel. Must be called before any drawing.
    ///
    /// The register values are lifted verbatim from the vendor's board-specific
    /// MicroPython init table (`Demos/MicroPython/rotations/tft_config.py`,
    /// `init_cmds`) — chosen over a generic ST7789 sequence precisely because it
    /// is tuned for *this* panel (its porch, gate, VCOM and gamma values are not
    /// the datasheet defaults).
    pub fn init(&mut self) {
        // Hardware reset if a pin exists (it does not on this board); otherwise
        // software reset. SWRESET is not optional here — without a reset line,
        // it is the only way to get the panel out of whatever state the ESP-Claw
        // factory firmware or a warm reboot left it in.
        match self.reset.as_mut() {
            Some(rst) => {
                rst.set_high();
                self.delay.delay_millis(10);
                rst.set_low();
                self.delay.delay_millis(RST_DELAY_MS);
                rst.set_high();
                self.delay.delay_millis(RST_DELAY_MS);
            }
            None => {
                self.bus.write_command(CMD_SWRESET);
                self.delay.delay_millis(RST_DELAY_MS);
            }
        }

        self.bus.write_command(CMD_SLPOUT);
        self.delay.delay_millis(SLPOUT_DELAY_MS);

        self.bus.write_command(CMD_NORON);

        // --- vendor init table (tft_config.py `init_cmds`) -------------------
        self.bus.write_c8dn(CMD_DISPFN, &[0x0A, 0x82]);
        self.bus.write_c8d8(CMD_COLMOD, 0x55); // 16 bpp, RGB565
        self.delay.delay_millis(10);
        self.bus.write_c8dn(CMD_PORCTRL, &[0x0C, 0x0C, 0x00, 0x33, 0x33]);
        self.bus.write_c8d8(CMD_GCTRL, 0x35);
        self.bus.write_c8d8(CMD_VCOMS, 0x28);
        self.bus.write_c8d8(CMD_LCMCTRL, 0x0C);
        self.bus.write_c8dn(CMD_VDVVRHEN, &[0x01, 0xFF]);
        self.bus.write_c8d8(CMD_VRHS, 0x10);
        self.bus.write_c8d8(CMD_VDVS, 0x20);
        self.bus.write_c8d8(CMD_FRCTRL2, 0x0F);
        self.bus.write_c8dn(CMD_PWCTRL1, &[0xA4, 0xA1]);
        self.bus.write_c8dn(
            CMD_PVGAMCTRL,
            &[
                0xD0, 0x00, 0x02, 0x07, 0x0A, 0x28, 0x32, 0x44, 0x42, 0x06, 0x0E, 0x12, 0x14, 0x17,
            ],
        );
        self.bus.write_c8dn(
            CMD_NVGAMCTRL,
            &[
                0xD0, 0x00, 0x02, 0x07, 0x0A, 0x28, 0x31, 0x54, 0x47, 0x0E, 0x1C, 0x17, 0x1B, 0x1E,
            ],
        );
        // ---------------------------------------------------------------------

        // Orientation + colour order. The vendor table omits MADCTL because
        // st7789py sends it from `rotation()` after the table runs; same here.
        self.apply_madctl();

        // Inversion OFF on this board — unusual for ST7789 and therefore stated
        // as a board constant with its citations. See `board::INVERT_COLORS`.
        if board::INVERT_COLORS {
            self.bus.write_command(CMD_INVON);
        } else {
            self.bus.write_command(CMD_INVOFF);
        }

        self.bus.write_command(CMD_DISPON);
        self.delay.delay_millis(120);
    }

    fn apply_madctl(&mut self) {
        self.bus
            .write_c8d8(CMD_MADCTL, self.rotation.madctl() | board::MADCTL_COLOR_ORDER);
    }

    /// Change orientation. Updates the reported [`size`](OriginDimensions::size)
    /// as well as the MADCTL bits, so `DrawTarget` clipping follows.
    ///
    /// No counterpart on the CO5300 (that panel is fixed-orientation).
    pub fn set_rotation(&mut self, rotation: Rotation) {
        self.rotation = rotation;
        let (w, h) = rotation.size();
        self.width = w;
        self.height = h;
        self.apply_madctl();
    }

    pub fn rotation(&self) -> Rotation {
        self.rotation
    }

    /// Set the address window for pixel writes.
    ///
    /// ★ The watch's flusher calls this with `h = 2` once per row-pair. Unlike
    /// the CO5300 there is no even-alignment requirement on `x`/`y`/`w`/`h`
    /// (see the module header) — any window is legal, down to 1x1.
    pub fn set_addr_window(&mut self, x: u16, y: u16, w: u16, h: u16) {
        let x_start = x + self.col_offset;
        let x_end = x_start + w - 1;
        let y_start = y + self.row_offset;
        let y_end = y_start + h - 1;

        self.bus.write_c8d16d16(CMD_CASET, x_start, x_end);
        self.bus.write_c8d16d16(CMD_RASET, y_start, y_end);
        // Deferred RAMWR — see the module header's "one invisible divergence".
        self.bus.arm_ramwr();
    }

    /// Fill the entire screen with a single colour.
    pub fn fill_screen(&mut self, color: Rgb565) {
        let raw: u16 = RawU16::from(color).into_inner();
        self.set_addr_window(0, 0, self.width, self.height);
        let total = self.width as u32 * self.height as u32;
        self.bus.write_repeat(raw, total);
    }

    /// Fill a rectangular area with a solid colour.
    pub fn write_pixels_area(&mut self, x: u16, y: u16, w: u16, h: u16, color: Rgb565) {
        let raw: u16 = RawU16::from(color).into_inner();
        self.set_addr_window(x, y, w, h);
        self.bus.write_repeat(raw, w as u32 * h as u32);
    }

    /// Mutable access to the bus — the second half of the watch's flush seam,
    /// and how the touch driver reaches the shared SPI.
    pub fn bus_mut(&mut self) -> &mut SharedSpiBus<'d> {
        &mut self.bus
    }

    // -- `PanelDriver` surface ---------------------------------------------
    // The contract (`esp32c6-watch:src/drivers/panel.rs`) puts `begin_pixels`
    // and `push_pixels` on the DISPLAY, where the watch has them on the bus.
    // These forwarders make the display satisfy the contract directly, without
    // callers needing `bus_mut()` — while `bus_mut()` stays for `TwoLineFlusher`,
    // which binds to the watch's own `display.bus_mut().write_pixels(...)`
    // spelling. Both call shapes now work.

    /// Begin one raw pixel stream into the current window.
    pub fn begin_pixels(&mut self) {
        self.bus.begin_pixels();
    }

    /// Push **logical** RGB565 pixels into the open stream. Safe to call
    /// repeatedly for one window: no command preamble is re-issued between
    /// calls, as the contract requires.
    ///
    /// `&[u16]`, and the byte order is this driver's problem — decided upstream
    /// 2026-08-24 (`panel.rs`, rationale at the trait site). Panel byte order is
    /// a per-panel electrical fact, so the swap belongs next to the panel that
    /// requires it; a `&[u8]` contract would have forced every caller to know
    /// every panel's byte order, which is the seam leaking. The swap happens in
    /// `SharedSpiBus::stream_pixels`.
    pub fn push_pixels(&mut self, pixels: &[u16]) {
        self.bus.stream_pixels(pixels);
    }

    /// Close a pixel stream. Not in the contract (which has no close); calling
    /// it is tidier, omitting it is safe — see `SharedSpiBus::deselect_all`.
    pub fn end_pixels(&mut self) {
        self.bus.end_pixels();
    }

    /// Backlight control.
    ///
    /// Signature-compatible with `Co5300Display::set_brightness`, but this is a
    /// plain GPIO, not the CO5300's `0x51` brightness register: **0 = off,
    /// anything else = on**. Genuine dimming needs an LEDC channel on GPIO25
    /// (what the vendor's `interface.cpp:85` `analogWrite(TFT_BL, ...)` does);
    /// that is a deliberate non-goal here so this driver claims no timer
    /// peripheral the firmware may want.
    pub fn set_brightness(&mut self, brightness: u8) {
        if brightness == 0 {
            self.backlight.set_low();
        } else {
            self.backlight.set_high();
        }
    }

    /// Exit sleep + display ON. MIPI-DCS order: SLPOUT → 120 ms → DISPON → 20 ms.
    pub fn display_on(&mut self) {
        self.bus.write_command(CMD_SLPOUT);
        self.delay.delay_millis(SLPOUT_DELAY_MS);
        self.bus.write_command(CMD_DISPON);
        self.delay.delay_millis(20);
    }

    /// DISPOFF + enter sleep. MIPI-DCS order: DISPOFF → 20 ms → SLPIN → 120 ms.
    pub fn display_off(&mut self) {
        self.bus.write_command(CMD_DISPOFF);
        self.delay.delay_millis(20);
        self.bus.write_command(CMD_SLPIN);
        self.delay.delay_millis(SLPIN_DELAY_MS);
    }
}

impl OriginDimensions for St7789Display<'_> {
    fn size(&self) -> Size {
        Size::new(self.width as u32, self.height as u32)
    }
}

impl DrawTarget for St7789Display<'_> {
    type Color = Rgb565;
    type Error = DisplayError;

    fn draw_iter<I>(&mut self, pixels: I) -> Result<(), Self::Error>
    where
        I: IntoIterator<Item = Pixel<Self::Color>>,
    {
        // True 1x1 writes. The CO5300 driver has to paint a 2x2 block per pixel
        // because of its minimum-window rule; this panel does not.
        for Pixel(coord, color) in pixels.into_iter() {
            if coord.x >= 0
                && coord.x < self.width as i32
                && coord.y >= 0
                && coord.y < self.height as i32
            {
                let raw: u16 = RawU16::from(color).into_inner();
                self.set_addr_window(coord.x as u16, coord.y as u16, 1, 1);
                self.bus.write_pixels(&[raw]);
            }
        }
        Ok(())
    }

    fn fill_contiguous<I>(&mut self, area: &Rectangle, colors: I) -> Result<(), Self::Error>
    where
        I: IntoIterator<Item = Self::Color>,
    {
        let bounds = Rectangle::new(
            Point::zero(),
            Size::new(self.width as u32, self.height as u32),
        );
        let clipped = area.intersection(&bounds);
        if clipped.size.width == 0 || clipped.size.height == 0 {
            return Ok(());
        }

        // Fast path: the area is fully on-screen, so the colour iterator maps
        // 1:1 onto the GRAM window and can be streamed straight through. The
        // ST7789 auto-increments within the window, so there is no need to
        // respect row boundaries — one continuous run is correct. (The CO5300
        // driver rebuilds per-row because of its 2-line minimum; not needed.)
        if clipped == *area {
            self.set_addr_window(
                area.top_left.x as u16,
                area.top_left.y as u16,
                area.size.width as u16,
                area.size.height as u16,
            );
            self.bus.begin_pixels();
            let mut chunk = [0u16; FILL_CHUNK];
            let mut n = 0usize;
            // embedded-graphics only guarantees the iterator yields AT LEAST
            // `width * height` colours — it is explicitly allowed to be longer
            // (`Text` with a background is one such producer). Streaming past
            // the window would wrap the ST7789's GRAM pointer back to the
            // window origin and overwrite what was just drawn, so bound the run
            // by the window's own pixel count rather than by iterator
            // exhaustion.
            let mut budget = area.size.width as u64 * area.size.height as u64;
            for color in colors.into_iter() {
                if budget == 0 {
                    break;
                }
                budget -= 1;
                chunk[n] = RawU16::from(color).into_inner();
                n += 1;
                if n == FILL_CHUNK {
                    self.bus.stream_pixels(&chunk);
                    n = 0;
                }
            }
            if n > 0 {
                self.bus.stream_pixels(&chunk[..n]);
            }
            self.bus.end_pixels();
            return Ok(());
        }

        // Slow path: partially off-screen. Walk the full (unclipped) area so the
        // colour iterator stays in step, and place only in-bounds pixels.
        let x0 = area.top_left.x;
        let y0 = area.top_left.y;
        let w = area.size.width as i32;
        if w == 0 {
            return Ok(());
        }
        // Same over-long-iterator guard as the fast path above: stop at the
        // area's own pixel count, or a generous iterator would keep producing
        // rows below the area and scribble over in-bounds pixels that belong to
        // somebody else.
        let budget = area.size.width as usize * area.size.height as usize;
        for (i, color) in colors.into_iter().take(budget).enumerate() {
            let i = i as i32;
            let x = x0 + (i % w);
            let y = y0 + (i / w);
            if x >= 0 && x < self.width as i32 && y >= 0 && y < self.height as i32 {
                let raw: u16 = RawU16::from(color).into_inner();
                self.set_addr_window(x as u16, y as u16, 1, 1);
                self.bus.write_pixels(&[raw]);
            }
        }
        Ok(())
    }

    fn fill_solid(&mut self, area: &Rectangle, color: Self::Color) -> Result<(), Self::Error> {
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

/// Pixels staged per `fill_contiguous` flush. Small on purpose: it lives on the
/// stack, and `SharedSpiBus::stream_pixels` re-stages into the DMA buffer
/// anyway, so a bigger array would buy nothing but stack pressure (the watch's
/// `.bss`/stack accounting notes in `qspi_bus.rs` are the cautionary tale).
const FILL_CHUNK: usize = 64;
