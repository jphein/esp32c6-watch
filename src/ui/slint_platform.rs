// Slint platform glue for the CO5300 AMOLED: embassy-clocked Platform and a
// line-streaming flusher. No framebuffer — the software renderer paints into
// a 2-line RGB565 strip (410 x 2 x 2 B) streamed to the panel's GRAM.
// Two lines per flush because the CO5300 requires a min 2-row address window.
//
// PARTIAL RENDERING (#18): the window uses RepaintBufferType::ReusedBuffer, so
// the software renderer only re-renders the BOUNDING BOX of what changed since
// the last frame (the panel's GRAM persists the rest). `render_by_line` then
// hands the flusher just the dirty lines + a constant dirty x-range, and only
// that sub-rectangle is streamed over QSPI — an idle clock tick or a slider drag
// repaints a few px-rows instead of all 502. A full repaint (page swap, overlay
// open, theme switch, or a forced request_redraw after a game/AOD bypass) still
// dirties the whole screen and collapses back to the original full-frame stream.

extern crate alloc;

use alloc::boxed::Box;
use alloc::rc::Rc;

use slint::platform::software_renderer::{
    MinimalSoftwareWindow, RepaintBufferType, Rgb565Pixel,
};
use slint::platform::{Platform, WindowAdapter};

use crate::board;
use crate::drivers::co5300::Co5300Display;

pub const WIDTH: usize = board::LCD_WIDTH as usize; // 410
pub const HEIGHT: usize = board::LCD_HEIGHT as usize; // 502

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
    // REVERTED to NewBuffer (#18 attempt 1): ReusedBuffer partial rendering
    // produced strip artifacts + janky launcher scrolling on glass — the dirty
    // region Slint feeds render_by_line doesn't match the flusher's single
    // bounding-box assumption on this scene. NewBuffer marks the full frame
    // dirty, so the (kept) dirty-box flusher degrades gracefully to exactly the
    // pre-#18 full-frame streaming. Re-attempt in #18 with per-line dirty spans.
    let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    window.set_size(slint::PhysicalSize::new(WIDTH as u32, HEIGHT as u32));
    slint::platform::set_platform(Box::new(EspPlatform {
        window: window.clone(),
    }))
    .expect("set_platform failed (init_platform called twice?)");
    window
}

/// LineBufferProvider that streams the renderer's dirty region to the panel two
/// rows per QSPI write.
///
/// Under `RepaintBufferType::ReusedBuffer`, Slint renders only the bounding box of
/// what changed since the last frame, so `render_by_line` feeds us just the dirty
/// lines (consecutive, top→bottom) and a CONSTANT dirty x-range (the box's
/// horizontal extent). We flush only that sub-rectangle `[x0, y, w, 2]`, leaving
/// the rest of the panel's GRAM untouched. On a full repaint the box is the whole
/// 410 x 502 screen (x0=0, w=410, 502 even rows), so this collapses to the exact
/// full-frame strip stream the old flusher produced — byte-for-byte identical.
///
/// The CO5300 needs a ≥2-row write window, so lines are batched in pairs. An
/// odd-height dirty box leaves one trailing line: it's flushed together with the
/// row ABOVE it — still valid in the other buffer half from the last pair — so
/// every panel write stays ≥2 rows WITHOUT painting a stale/duplicate row over
/// non-dirty content (the correctness trap a naive "duplicate the last line
/// downward" hits once rendering is partial rather than always full-frame).
pub struct TwoLineFlusher<'a, 'd> {
    display: &'a mut Co5300Display<'d>,
    /// 2 x WIDTH pixels: the staged line in the first half, its pair partner in
    /// the second. Only the dirty `[x0, x0+w)` columns of each half are written.
    buf: &'a mut [Rgb565Pixel],
    /// Raw u16 staging for the QSPI bus (holds up to 2 x WIDTH pixels; a strip
    /// sends `w * 2` of them).
    scratch: &'a mut [u16],
    /// y of the line staged in the FIRST half of `buf`, awaiting its pair partner.
    pending: Option<usize>,
    /// y of the line currently valid in the SECOND half of `buf` THIS frame (set
    /// after each flushed pair). Lets an odd trailing line flush safely as
    /// `[y-1, y]`; `None` until the first pair completes, so a 1-row dirty box
    /// (no valid neighbour) falls back to a clamped duplicate instead of blitting
    /// last-frame garbage over the row above.
    second_y: Option<usize>,
    /// Dirty-box x-origin + width (logical px), captured from the first line and
    /// constant across the frame (Slint feeds the bounding box).
    x0: usize,
    w: usize,
}

impl<'a, 'd> TwoLineFlusher<'a, 'd> {
    pub fn new(
        display: &'a mut Co5300Display<'d>,
        buf: &'a mut [Rgb565Pixel],
        scratch: &'a mut [u16],
    ) -> Self {
        // A full-frame strip is WIDTH*2 pixels; buf holds two full-width lines and
        // scratch stages the widest possible strip. Both must be exactly WIDTH*2.
        debug_assert_eq!(buf.len(), WIDTH * 2);
        debug_assert_eq!(scratch.len(), WIDTH * 2);
        Self {
            display,
            buf,
            scratch,
            pending: None,
            second_y: None,
            x0: 0,
            w: 0,
        }
    }
}

impl TwoLineFlusher<'_, '_> {
    /// Stream a 2-row window at `[x0, y, w, 2]`: `row0` from `buf`'s first half,
    /// `row1` from its second half, each clipped to the dirty x-range.
    fn flush_rows(&mut self, y: usize) {
        let (w, x0) = (self.w, self.x0);
        let (first, second) = self.buf.split_at(WIDTH);
        for i in 0..w {
            self.scratch[i] = first[x0 + i].0;
            self.scratch[w + i] = second[x0 + i].0;
        }
        self.display.set_addr_window(x0 as u16, y as u16, w as u16, 2);
        self.display.bus_mut().write_pixels(&self.scratch[..w * 2]);
    }

    /// Flush a leftover trailing line (odd-height dirty box). Pairs it with the
    /// still-valid row above when there is one, else duplicates it into a clamped
    /// 2-row window (the 1-row-box fallback — vanishingly rare, one adjacent row's
    /// dirty sub-range touched for a single frame).
    pub fn flush_pending(&mut self) {
        let Some(t) = self.pending.take() else { return; };
        let (w, x0) = (self.w, self.x0);
        if w == 0 {
            return; // nothing was rendered this frame
        }
        let (first, second) = self.buf.split_at(WIDTH);
        if t >= 1 && self.second_y == Some(t - 1) {
            // Row above (t-1) is still in the second half from the last pair and
            // belonged to THIS dirty box: flush [t-1, t] with both rows real
            // (row0 = second half = t-1, row1 = first half = t).
            for i in 0..w {
                self.scratch[i] = second[x0 + i].0;
                self.scratch[w + i] = first[x0 + i].0;
            }
            self.display.set_addr_window(x0 as u16, (t - 1) as u16, w as u16, 2);
        } else {
            // 1-row dirty box: no valid neighbour. Duplicate the single line into
            // a clamped 2-row window.
            let y = t.min(HEIGHT - 2);
            for i in 0..w {
                self.scratch[i] = first[x0 + i].0;
                self.scratch[w + i] = first[x0 + i].0;
            }
            self.display.set_addr_window(x0 as u16, y as u16, w as u16, 2);
        }
        self.display.bus_mut().write_pixels(&self.scratch[..w * 2]);
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
        // Capture the dirty box's x-extent once (constant per frame under the
        // bounding-box contract). Empty ranges (shouldn't occur for a dirty line)
        // are skipped so w never underflows in set_addr_window.
        if self.w == 0 && range.end > range.start {
            self.x0 = range.start;
            self.w = range.end - range.start;
        }
        // Decide which half of the strip this line goes into.
        let second_half = match self.pending {
            Some(p) if line == p + 1 => true,
            Some(_) => {
                // Non-consecutive line (defensive; a bounding box is contiguous):
                // emit the stragglers first, then start a fresh pair.
                self.flush_pending();
                false
            }
            None => false,
        };
        let offset = if second_half { WIDTH } else { 0 };
        let dst = &mut self.buf[offset..offset + WIDTH];
        render_fn(&mut dst[range]);
        if second_half {
            let y0 = self.pending.take().unwrap();
            self.flush_rows(y0);
            self.second_y = Some(y0 + 1);
        } else {
            self.pending = Some(line);
        }
    }
}
