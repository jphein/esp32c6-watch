// Slint platform glue for the CO5300 AMOLED: embassy-clocked Platform and a
// line-streaming flusher. No framebuffer — the software renderer paints into
// a 2-line RGB565 strip (410 x 2 x 2 B) streamed to the panel's GRAM.
//
// PARTIAL RENDERING (#18, attempt 2): the window uses
// RepaintBufferType::ReusedBuffer (the panel's GRAM is the persistent "reused
// buffer"), so Slint re-renders only the dirty region each frame. The dirty
// region is up to THREE disjoint rectangles (PhysicalRegion/DirtyRegion,
// MAX_COUNT=3) — `process_line` is called once per line PER SPAN, so the same
// line can arrive multiple times with different x-ranges, and ranges vary
// between lines. Attempt 1 assumed a single bounding box with one constant
// x-range and died of exactly that (strip artifacts + smeared rows on glass).
//
// HARDWARE CONTRACT (CO5300 datasheet §7.5.21/§7.5.22): CASET/RASET windows
// need start AND extent divisible by 2 on BOTH axes. With only a 2-line strip
// (no framebuffer), every flushed pixel must also be freshly rendered this
// frame — stale strip bytes must never reach GRAM. Slint 1.17 exposes no
// dirty-region rounding hook (LVGL-rounder equivalent), so the vendored
// renderer fork (crates/i-slint-renderer-software, `[patch.crates-io]`)
// aligns the dirty region to the even-pixel grid BEFORE item filtering and
// span emission. That guarantees, by construction:
//   1. every span handed to `process_line` has even start + even length;
//   2. dirty lines arrive in complete even/odd row PAIRS (rect y-edges are
//      even), and both lines of a pair carry IDENTICAL span lists (the
//      per-line range set only changes at rect y-edges — see
//      `region_line_ranges` in the vendored crate).
// The flusher below leans on those guarantees but never trusts them blindly:
// a violated pair is SKIPPED (pixels stay stale on the panel — visible but
// bounded, and logged) rather than flushed wrong (corruption).
//
// A full repaint (page swap, overlay open, theme switch, or a forced
// request_redraw after a game/AOD panel bypass) dirties the whole screen and
// collapses to the original full-frame strip stream, byte-for-byte.

extern crate alloc;

use alloc::boxed::Box;
use alloc::rc::Rc;

use esp_println::println;
use slint::platform::software_renderer::{
    MinimalSoftwareWindow, RepaintBufferType, Rgb565Pixel,
};
use slint::platform::{Platform, WindowAdapter};

use crate::board;
use crate::drivers::ActivePanel;

pub const WIDTH: usize = board::LCD_WIDTH as usize; // 410
pub const HEIGHT: usize = board::LCD_HEIGHT as usize; // 502

/// Max spans staged per line: the dirty region has at most 3 rectangles, so a
/// line intersects at most 3 disjoint x-ranges (+1 slack for future-proofing).
const MAX_SPANS: usize = 4;

struct EspPlatform {
    window: Rc<MinimalSoftwareWindow>,
}

impl Platform for EspPlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
        Ok(self.window.clone())
    }

    fn duration_since_start(&self) -> core::time::Duration {
        core::time::Duration::from_micros(embassy_time::Instant::now().as_micros())
    }
}

/// Create the window, register the platform. Call exactly once per boot —
/// `slint::platform::set_platform` panics on a second call.
pub fn init_platform() -> Rc<MinimalSoftwareWindow> {
    // ReusedBuffer = partial rendering (#18 attempt 2): only the dirty region
    // is re-rendered and flushed each frame. Correct this time because the
    // vendored renderer fork even-aligns the dirty region pre-render — see the
    // module header for the full contract.
    let window = MinimalSoftwareWindow::new(RepaintBufferType::ReusedBuffer);
    window.set_size(slint::PhysicalSize::new(WIDTH as u32, HEIGHT as u32));
    slint::platform::set_platform(Box::new(EspPlatform {
        window: window.clone(),
    }))
    .expect("set_platform failed (init_platform called twice?)");
    window
}

/// LineBufferProvider that streams the renderer's dirty spans to the panel in
/// even/odd row pairs, one QSPI write per (pair, span).
///
/// Even lines stage their spans in the first half of `buf`; each odd-line span
/// is rendered into the second half and immediately flushed as a 2-row window
/// `[x0, line-1, w, 2]` — both rows freshly rendered, window even-aligned on
/// both axes by the vendored renderer's region alignment. Spans whose pairing
/// guarantee does not hold are skipped and counted, never guessed at.
/// A live cast sample target handed to the flusher (feature `cast`): the
/// caller-owned RGB565 cell store, its Mirror geometry, and the source-row
/// map. process_line taps rendered spans into it — one RowMap lookup per
/// line when a cast is active, ZERO code when the feature is off.
#[cfg(feature = "cast")]
pub struct CastSink<'a> {
    pub rowmap: &'a cast_core::RowMap,
    pub mirror: cast_core::Mirror,
    pub store: &'a mut [u16],
}

pub struct TwoLineFlusher<'a, 'd> {
    display: &'a mut ActivePanel<'d>,
    #[cfg(feature = "cast")]
    cast: Option<CastSink<'a>>,
    /// 2 x WIDTH pixels: even line of the current pair in the first half, odd
    /// line in the second. Only rendered span columns are ever flushed.
    buf: &'a mut [Rgb565Pixel],
    /// Raw u16 staging for the QSPI bus (holds up to 2 x WIDTH pixels; a span
    /// flush sends `w * 2` of them).
    scratch: &'a mut [u16],
    /// Even line currently staged in the first half of `buf`.
    pair_base: Option<usize>,
    /// Spans rendered into the staged even line (start, end), left→right.
    staged: [(u16, u16); MAX_SPANS],
    staged_n: usize,
    /// Odd-line spans already matched+flushed for the current pair; the pair is
    /// complete (and its staging droppable) once this reaches `staged_n`.
    flushed_n: usize,
    /// Pairing-contract violations this frame (spans skipped, never flushed).
    violations: u32,
}

impl<'a, 'd> TwoLineFlusher<'a, 'd> {
    pub fn new(
        display: &'a mut ActivePanel<'d>,
        buf: &'a mut [Rgb565Pixel],
        scratch: &'a mut [u16],
    ) -> Self {
        // A full-frame strip is WIDTH*2 pixels; buf holds two full-width lines and
        // scratch stages the widest possible strip. Both must be exactly WIDTH*2.
        debug_assert_eq!(buf.len(), WIDTH * 2);
        debug_assert_eq!(scratch.len(), WIDTH * 2);
        Self {
            display,
            #[cfg(feature = "cast")]
            cast: None,
            buf,
            scratch,
            pair_base: None,
            staged: [(0, 0); MAX_SPANS],
            staged_n: 0,
            flushed_n: 0,
            violations: 0,
        }
    }

    /// Was `range` staged on the even line of the current pair? (Both lines of
    /// a pair carry identical span lists under the alignment contract.)
    fn span_staged(&self, range: &core::ops::Range<usize>) -> bool {
        self.staged[..self.staged_n]
            .iter()
            .any(|&(s, e)| s as usize == range.start && e as usize == range.end)
    }

    /// Stream the 2-row window `[x0, y_even, w, 2]` from the strip halves.
    fn flush_span(&mut self, y_even: usize, range: core::ops::Range<usize>) {
        let (x0, w) = (range.start, range.len());
        let (first, second) = self.buf.split_at(WIDTH);
        for i in 0..w {
            self.scratch[i] = first[x0 + i].0;
            self.scratch[w + i] = second[x0 + i].0;
        }
        self.display.set_addr_window(x0 as u16, y_even as u16, w as u16, 2);
        self.display.bus_mut().write_pixels(&self.scratch[..w * 2]);
    }

    /// End-of-frame check. Under the alignment contract nothing is ever left
    /// pending (pairs always complete); a straggler means the contract broke —
    /// its pixels are left stale on the panel (bounded, visible, logged) rather
    /// than flushed as a guessed 2-row window (corruption).
    /// Attach a live cast sink for this frame (feature `cast`). The store is
    /// the caller's cell buffer (w*h u16); the flusher fills it as it renders.
    #[cfg(feature = "cast")]
    pub fn with_cast(mut self, sink: CastSink<'a>) -> Self {
        self.cast = Some(sink);
        self
    }

    /// Sample the just-rendered span of `line` into the cast store, if a cast
    /// is active and this source row feeds a target cell row. One RowMap
    /// lookup per line; the sampled columns are the cell centers only.
    #[cfg(feature = "cast")]
    fn cast_tap(&mut self, line: usize, range: core::ops::Range<usize>) {
        let Some(sink) = self.cast.as_mut() else { return };
        let Some(ty) = sink.rowmap.target_row(line) else { return };
        // Even source lines rendered into buf[..WIDTH], odd into buf[WIDTH..].
        let half = if line % 2 == 0 {
            &self.buf[..WIDTH]
        } else {
            &self.buf[WIDTH..]
        };
        sink.mirror.sample_span_with(
            sink.store,
            ty,
            WIDTH,
            range.start,
            range.len(),
            |i| half[range.start + i].0,
        );
    }

    pub fn flush_pending(&mut self) {
        if self.pair_base.is_some() && self.flushed_n < self.staged_n {
            self.violations += (self.staged_n - self.flushed_n) as u32;
        }
        self.pair_base = None;
        self.staged_n = 0;
        self.flushed_n = 0;
        if self.violations > 0 {
            println!(
                "[RENDER] pairing contract violated: {} span(s) skipped (stale on panel)",
                self.violations
            );
        }
    }
}

impl slint::platform::software_renderer::LineBufferProvider for &mut TwoLineFlusher<'_, '_> {
    type TargetPixel = Rgb565Pixel;

    fn process_line(
        &mut self,
        line: usize,
        range: core::ops::Range<usize>,
        render_fn: impl FnOnce(&mut [Self::TargetPixel]),
    ) {
        // Defensive: empty spans can occur (region construction pads with empty
        // rects when an off-screen rect is clipped away). Nothing to render.
        if range.is_empty() {
            render_fn(&mut []);
            return;
        }
        // Even-alignment contract (vendored renderer patch). Violations are
        // rendered (Slint needs the pixels processed) but never flushed odd.
        debug_assert!(range.start % 2 == 0 && range.len() % 2 == 0, "odd span");
        debug_assert!(line < HEIGHT && range.end <= WIDTH);

        if line % 2 == 0 {
            // Even line: (re)stage. A new even line while staged spans are still
            // unmatched means the previous pair's odd partner never arrived —
            // contract violation; the old pair is dropped (stale on panel),
            // never guessed.
            if self.pair_base != Some(line) {
                if self.pair_base.is_some() && self.flushed_n < self.staged_n {
                    self.violations += (self.staged_n - self.flushed_n) as u32;
                }
                self.pair_base = Some(line);
                self.staged_n = 0;
                self.flushed_n = 0;
            }
            render_fn(&mut self.buf[..WIDTH][range.clone()]);
            #[cfg(feature = "cast")]
            self.cast_tap(line, range.clone());
            if self.staged_n < MAX_SPANS {
                self.staged[self.staged_n] = (range.start as u16, range.end as u16);
                self.staged_n += 1;
            } else {
                // >3 spans per line is impossible (3-rect region); treat as
                // violation so the unmatched odd span skips instead of flushing.
                self.violations += 1;
            }
        } else {
            // Odd line: render, then flush the pair window for this span.
            render_fn(&mut self.buf[WIDTH..][range.clone()]);
            #[cfg(feature = "cast")]
            self.cast_tap(line, range.clone());
            if self.pair_base == Some(line - 1) && self.span_staged(&range) {
                self.flush_span(line - 1, range);
                self.flushed_n += 1;
            } else {
                self.violations += 1;
            }
        }
    }
}
