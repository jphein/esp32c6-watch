pub mod panel;
pub mod co5300;
pub mod framebuffer;
pub mod qspi_bus;

// --- CYD-C5 (ST7789 + XPT2046 on one shared classic-SPI bus) ---------------
// Board-gated because these name C5-only board constants (`board::PIN_SCK`,
// `board::TOUCH_CAL`, ...) that exist only while `board-cyd-c5` selects
// `src/board/cyd_c5.rs`.
//
// Ported from ~/Projects/cyd-c5/watch-port/ (vesper, 2026-08-24) — a standalone
// crate written against `panel.rs`'s contract and verified on glass before it
// came anywhere near this tree. Per-file origin notes in each header.
#[cfg(feature = "board-cyd-c5")]
pub mod spi_bus;
#[cfg(feature = "board-cyd-c5")]
pub mod st7789;
#[cfg(feature = "board-cyd-c5")]
pub mod xpt2046;
/// WS2812 status LED encoder — SPIKE for smol#486 / #491. Board-gated because it
/// reads `board::WS2812_GPIO`, which only the CYD declares. Gives that constant its
/// first reader: it was one of the 10 pins the board-const audit found declared and
/// unread, and the least documented of them.
#[cfg(feature = "board-cyd-c5")]
pub mod ws2812;

/// Panel orientation. **CYD-only** — the CO5300 is fixed-orientation, so this
/// has no C6 meaning and is gated with the drivers that consume it.
///
/// The discriminant values ARE the MADCTL bit patterns, taken verbatim from the
/// vendor MicroPython rotation table
/// (`Demos/MicroPython/rotations/st7789py.py:154-158`, `_DISPLAY_240x320`):
///
/// ```text
/// (0x00, 240, 320, 0, 0, False)   # rot 0  portrait
/// (0x60, 320, 240, 0, 0, False)   # rot 1  landscape           MV|MX
/// (0xc0, 240, 320, 0, 0, False)   # rot 2  inverted portrait   MY|MX
/// (0xa0, 320, 240, 0, 0, False)   # rot 3  inverted landscape  MV|MY
/// ```
///
/// The colour-order bit is deliberately NOT part of these values — it is OR-ed
/// in from [`crate::board::MADCTL_COLOR_ORDER`] so the two stay separable:
/// rotation is a product decision, colour order is a wiring fact. Folding them
/// together is how a "just rotate the screen" edit silently swaps red and blue.
///
/// Note the fourth column of every row: `xstart = ystart = 0`. This die needs no
/// GRAM offset in any rotation, unlike the 240x240 / 135x240 entries in the same
/// vendor table and unlike the C6's CO5300 (`col_offset = 22`).
#[cfg(feature = "board-cyd-c5")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rotation {
    /// 240x320, MADCTL 0x00.
    Portrait,
    /// 320x240, MADCTL 0x60 (MV|MX).
    Landscape,
    /// 240x320, MADCTL 0xC0 (MY|MX).
    PortraitInverted,
    /// 320x240, MADCTL 0xA0 (MV|MY). **The board's shipped orientation** — with
    /// the BGR bit that is `0xA8` on the wire.
    LandscapeInverted,
}

#[cfg(feature = "board-cyd-c5")]
impl Rotation {
    /// MADCTL bits for this rotation, colour order excluded.
    pub const fn madctl(self) -> u8 {
        match self {
            Rotation::Portrait => 0x00,
            Rotation::Landscape => 0x60,
            Rotation::PortraitInverted => 0xC0,
            Rotation::LandscapeInverted => 0xA0,
        }
    }

    /// Logical `(width, height)` in this rotation.
    pub const fn size(self) -> (u16, u16) {
        match self {
            Rotation::Portrait | Rotation::PortraitInverted => {
                (crate::board::PANEL_NATIVE_W, crate::board::PANEL_NATIVE_H)
            }
            Rotation::Landscape | Rotation::LandscapeInverted => {
                (crate::board::PANEL_NATIVE_H, crate::board::PANEL_NATIVE_W)
            }
        }
    }

    /// True when the long axis is horizontal (the 320x240 orientations).
    pub const fn is_landscape(self) -> bool {
        matches!(self, Rotation::Landscape | Rotation::LandscapeInverted)
    }
}
