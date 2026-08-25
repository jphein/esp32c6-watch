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
    /// ...and push RGB565 big-endian bytes into it. Callers may push a window's
    /// pixels across several calls; the driver must not re-issue the command
    /// preamble between them.
    fn push_pixels(&mut self, data: &[u8]);
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
