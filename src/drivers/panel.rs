//! The DRIVER CONTRACT a board's display + touch must satisfy (#cyd-c5).
//!
//! Extracted from the two pixel paths that actually exist, not invented:
//!
//!   * `ui/slint_shell.rs::render` — Slint's software renderer streams RGB565
//!     line pairs through `TwoLineFlusher`, which calls
//!     `set_addr_window(x, y, w, h)` then `bus begin_pixels()` + raw writes.
//!     The vendored renderer aligns dirty regions to an EVEN 2x2 grid because
//!     the CO5300 requires it; ST77xx does not require it but tolerates it, so
//!     the alignment stays board-independent.
//!   * `drivers/framebuffer.rs::flush` — games write the whole panel the same
//!     way: one full-screen window, then rows.
//!
//! Both paths reduce to this surface. Satisfaction is STRUCTURAL, not
//! trait-bound: the driver workstream mirrors `Co5300Display`/`QspiBus`'s
//! method names exactly, and the shell consumes whichever concrete type the
//! board feature selects (`BoardDisplay` alias) — so `TwoLineFlusher` compiles
//! against either with zero hot-path refactor. The traits below are the
//! NORMATIVE listing of that surface: implement them too (they are free), but
//! it is the method-name compatibility that the existing call sites bind to.
//! A static seam, never `dyn` — these are the hottest calls in the firmware
//! (paint budget 30 ms, measured worsts riding 26-30 ms).
//!
//! ## The vendored renderer STAYS on every board
//!
//! The CO5300's even-2x2 window alignment is the documented reason for the
//! `crates/i-slint-renderer-software` fork, and the ST7789 does not need it —
//! but the fork is NOT just the alignment patch. It also carries the scene
//! pooling and `pool_capacities()` instrumentation that the entire `[POOL]`
//! heap-attribution stack reads (#75). Swapping the CYD to the stock renderer
//! would silently blind that instrumentation on one board. Even windows are a
//! legal subset on ST77xx, so the fork costs the CYD nothing and keeps the
//! fleet's instruments identical.
//!
//! ## Touch
//!
//! [`TouchDriver`] is the read-side contract. `TouchPoint` is in PANEL
//! coordinates after the driver applies rotation + calibration — consumers
//! (Slint dispatch, the mid-playback hit-test in main.rs) never see raw ADC.
//! A resistive panel (XPT2046) must debounce and pressure-threshold INSIDE the
//! driver; `fingers` is 1 while pressed. The FT3168 reports up to 2.
//!
//! ## What is deliberately NOT in the contract
//!
//! Brightness (`set_brightness`) and power (`display_on/off`) stay board
//! methods outside the trait: the CO5300 does brightness by command, the CYD
//! by a backlight GPIO the display driver does not own. Forcing them into one
//! trait would hand the ST7789 driver a GPIO it has no business holding.
//!
//! ## ⚠️ The contract does NOT make the UI fit
//!
//! The entire Slint layout is absolute-positioned for 410x502 PORTRAIT
//! (`Theme.safe-side`, every `y:`, the hit-test rectangles in main.rs). The
//! CYD is 320x240 LANDSCAPE, and the software renderer does not reflow. A
//! working CYD build needs its own layout set (or a deliberately reduced
//! shell); satisfying these traits gets pixels on glass, not the watch UI.

use embedded_graphics::pixelcolor::Rgb565;

/// Display contract. `WIDTH`/`HEIGHT` are post-rotation panel dimensions and
/// must equal the board module's `LCD_WIDTH`/`LCD_HEIGHT`.
pub trait PanelDriver {
    /// Bring the panel out of reset to "ready for windows + pixels".
    fn init(&mut self);
    /// Restrict subsequent pixel writes to the rect (panel coordinates; the
    /// driver applies its own column/row offsets).
    fn set_addr_window(&mut self, x: u16, y: u16, w: u16, h: u16);
    /// Begin one raw pixel stream into the current window...
    fn begin_pixels(&mut self);
    /// ...and push LOGICAL RGB565 pixels into it. Callers may push a window's
    /// pixels across several calls; the driver must not re-issue the command
    /// preamble between them.
    ///
    /// `&[u16]`, not `&[u8]`, and the byte order is the DRIVER'S problem — this
    /// was an explicit decision (2026-08-24): both live flushers hold u16 pixel
    /// buffers and the byteswap currently lives in qspi_bus. Panel byte order is
    /// a per-panel electrical fact (the CO5300 wants big-endian over QSPI; the
    /// CYD's ST7789 is BGR with its own order), so pushing the swap into each
    /// driver keeps callers panel-agnostic and keeps the swap next to the thing
    /// that requires it. A &[u8] contract would have forced every caller to know
    /// every panel's byte order, which is the seam leaking.
    ///
    /// Data point for any future revisit (measured on the CYD, 2026-08-24): on
    /// the ST7789 a &[u8] path is CHEAPER — no byteswap, straight into DMA. The
    /// u16 ruling stands because caller panel-agnosticism outranks one panel's
    /// copy, but if a profile ever shows the swap on a hot path, that is the
    /// trade to reopen — with numbers, per house rules.
    fn push_pixels(&mut self, pixels: &[u16]);
    /// Close the pixel stream opened by [`Self::begin_pixels`]. REQUIRED, not
    /// defaulted, and the omission was a found bug in this contract's first
    /// draft: the underlying C6 bus always had `end_pixels` (it raises CS), but
    /// the trait didn't carry it — so a conforming caller could open a stream
    /// and never close it. On a dedicated bus that leaks a transaction; on the
    /// CYD's SHARED bus it is two silent failures: the next command gets
    /// clocked into the still-open RAMWR stream and lands in GRAM as pixels,
    /// and a following touch read asserts touch CS while LCD CS is still low —
    /// two devices driving one MISO line. Every begin/push sequence ends here,
    /// and a shared-bus driver's device-select should ALSO raise all chip
    /// selects first (defence in depth — the CYD driver does both).
    ///
    /// Better still, make the misuse unrepresentable when wiring TouchDriver:
    /// the CYD's `DisplayBoundTouch` construction turns "don't poll touch
    /// inside a pixel transaction" into a borrow-checker error instead of a
    /// comment. Prefer that shape over discipline.
    fn end_pixels(&mut self);
    /// Whole-panel solid fill (boot clear, game teardown).
    fn fill_screen(&mut self, color: Rgb565);
}

/// One touch sample. Panel coordinates, post-rotation, post-calibration.
#[derive(Debug, Clone, Copy)]
pub struct PanelTouch {
    pub x: u16,
    pub y: u16,
    /// Contacts down. Resistive hardware reports at most 1.
    pub fingers: u8,
}

/// Touch contract. `Ok(None)` = nothing pressed (already debounced).
pub trait TouchDriver {
    type Error;
    fn read(&mut self) -> Result<Option<PanelTouch>, Self::Error>;
}
