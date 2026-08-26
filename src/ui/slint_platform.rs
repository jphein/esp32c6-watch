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

/// Post-rotation panel width, from the selected board module (410 on the C6,
/// 320 on the CYD). Parametric since #cyd-c5 — the trailing dimension comments
/// that used to live here were C6-only and went stale the moment a second board
/// existed.
pub const WIDTH: usize = board::LCD_WIDTH as usize;
/// Post-rotation panel height (502 on the C6, 240 on the CYD).
pub const HEIGHT: usize = board::LCD_HEIGHT as usize;

/// Pixels a flusher's strip buffers must hold: `WIDTH * board::FLUSH_STRIP_LINES`
/// (two lines on the CO5300, one on the ST7789). Sized here so `ShellUi`'s two
/// `Vec`s and the flusher's own `debug_assert`s cannot drift apart.
pub const STRIP_PX: usize = WIDTH * board::FLUSH_STRIP_LINES;

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
pub struct TwoLineFlusher<'a, 'd> {
    display: &'a mut board::BoardDisplay<'d>,
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
    /// Lines this flusher stages per window. Paired with
    /// `board::FLUSH_STRIP_LINES` by the const assert at the bottom of this
    /// module — see there for why that pairing needs enforcing.
    pub const LINES: usize = 2;

    pub fn new(
        display: &'a mut board::BoardDisplay<'d>,
        buf: &'a mut [Rgb565Pixel],
        scratch: &'a mut [u16],
    ) -> Self {
        // A full-frame strip is WIDTH*2 pixels; buf holds two full-width lines and
        // scratch stages the widest possible strip. Both must be exactly WIDTH*2 —
        // which is what `STRIP_PX` resolves to on the board that uses this flusher.
        debug_assert_eq!(buf.len(), WIDTH * 2);
        debug_assert_eq!(scratch.len(), WIDTH * 2);
        Self {
            display,
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

/// ★ **The CYD's flusher of record**: one window per line.
///
/// Ported from `~/Projects/cyd-c5/watch-port/src/seam.rs` (vesper, 2026-08-24),
/// blessed upstream the same day.
///
/// No row-pair staging, no span matching, no pairing contract and therefore **no
/// violation class** — because the ST7789 accepts a 1-pixel-tall window, which is
/// the single assumption [`TwoLineFlusher`] could not make. Half the state, half
/// the buffer, and none of the failure modes.
///
/// The decision that made a per-board flusher possible is that
/// [`crate::drivers::panel::PanelDriver`] stays *narrow* — `set_addr_window` plus
/// a pixel push, and nothing about row pairs. That deliberately keeps the
/// two-line apparatus a CO5300-private workaround instead of promoting one
/// panel's hardware quirk into a fleet-wide interface.
///
/// ⚠️ **The vendored renderer still runs on this board.** An earlier version of
/// this reasoning paired the single-line flusher with the *stock*
/// `i-slint-renderer-software`, on the grounds that the ST7789 does not need the
/// fork's even-alignment patch. That was wrong: the fork also carries the scene
/// pooling and `pool_capacities()` instrumentation the whole `[POOL]`
/// heap-attribution stack reads (#75), so dropping it on one board would blind
/// the fleet's instruments there. Even-aligned spans are simply a legal subset of
/// what this flusher accepts, so the fork costs the CYD nothing.
///
/// The two facts are independent, and conflating them is what produced the bad
/// recommendation: *the renderer stays* (instrumentation) **and** *the flusher is
/// per-board* (the contract is narrow enough not to care).
///
/// # ⚠️ Do not "optimise" this into larger strips
///
/// Throughput analysis for this panel favours fewer, larger pushes — a likely
/// sweet spot in the 16-48 row band. That conclusion is real and **it does not
/// apply here**, which matters because the numbers look like they license a
/// rewrite:
///
///   * **This path cannot batch.** Slint's software renderer hands out one line
///     at a time through `LineBufferProvider::process_line` and owns the buffer
///     between calls. Accumulating 16 rows means holding 16 rows of pixels
///     somewhere — which is a framebuffer, and the whole reason this flusher
///     exists is that there isn't one.
///   * **The row-band sweet spot belongs to the framebuffer path**
///     (`drivers/framebuffer.rs::flush`), where the caller already owns a full
///     backing store and genuinely chooses its own chunk size. Tune it there.
///
/// The general form, worth applying to any future throughput advice: **a
/// batching recommendation silently assumes the caller owns the memory being
/// batched.** Where the API hands you one unit at a time and reclaims it between
/// calls, batching is not a tuning knob — it is an architecture change.
pub struct SingleLineFlusher<'a, 'd> {
    display: &'a mut board::BoardDisplay<'d>,
    buf: &'a mut [Rgb565Pixel],
    scratch: &'a mut [u16],
    /// The open span-run: `(x0, w, next_line)` — the window currently accepting
    /// pixels, and the line number that would CONTINUE it. `None` = no stream
    /// open. See the type docs for why this batches windows and not pixels.
    run: Option<(u16, u16, usize)>,
}

impl<'a, 'd> SingleLineFlusher<'a, 'd> {
    /// Lines this flusher stages per window. See [`TwoLineFlusher::LINES`].
    pub const LINES: usize = 1;

    pub fn new(
        display: &'a mut board::BoardDisplay<'d>,
        buf: &'a mut [Rgb565Pixel],
        scratch: &'a mut [u16],
    ) -> Self {
        // One line, not two — `STRIP_PX` is `WIDTH * 1` on the board that uses
        // this flusher.
        debug_assert_eq!(buf.len(), WIDTH);
        debug_assert_eq!(scratch.len(), WIDTH);
        Self {
            display,
            buf,
            scratch,
            run: None,
        }
    }

    /// Close the open span-run, if any.
    ///
    /// ⚠️ This is no longer the no-op it was when every line closed its own
    /// window. A run deliberately outlives `process_line`, so the LAST run of a
    /// frame has nobody to close it — `flush_pending` is that nobody, and
    /// `slint_shell::render` already calls it after `render_by_line`.
    fn close_run(&mut self) {
        if self.run.take().is_some() {
            self.display.end_pixels();
        }
    }

    /// End-of-frame. Closes the final span-run.
    pub fn flush_pending(&mut self) {
        self.close_run();
    }
}

impl Drop for SingleLineFlusher<'_, '_> {
    /// Closing a span-run now depends on someone calling `flush_pending`, which
    /// is a discipline the old one-window-per-line version did not need. Rather
    /// than document that, make it unrepresentable: dropping the flusher closes
    /// the stream.
    ///
    /// `SharedSpiBus::deselect_all` would also rescue a leaked stream, but that
    /// is defence in depth — "survivable" and "correct" are different claims,
    /// and this is the one that makes the run's lifetime match the flusher's.
    fn drop(&mut self) {
        self.close_run();
    }
}

// ===========================================================================
// [RENDER-DBG] counters — DIAGNOSTIC, gated on `touch-telemetry`
// ===========================================================================
// The open question these exist to settle: property writes commit (a [SHELL-DBG]
// `PAGED 0 -> 4` is logged and the value reads back changed on the next gesture)
// and the glass does not move. Three layers could be responsible and they need
// completely different fixes, so guessing is expensive:
//
//   draw=false            -> the window was never marked dirty. Propagation.
//   draw=true, lines=0    -> dirty FLAG set, dirty REGION empty. The region
//                            computation lost it — prime suspect is the vendored
//                            renderer fork's even-pixel alignment pass, written
//                            for the CO5300's CASET/RASET constraint and running
//                            on an ST7789 that never asked for it.
//   draw=true, lines>0    -> pixels WERE pushed. Downstream: window, SPI, GRAM.
//
// `lines` counts `process_line` calls (one per span PER line); `spans` counts
// windows actually opened (a run that could not continue). A full-frame repaint
// on this panel should read lines=240 spans=1.
//
// Instrumented on the SingleLineFlusher only — the CYD is the board under
// investigation, and `touch-telemetry` is C5-only anyway.
#[cfg(feature = "touch-telemetry")]
pub static RDBG_LINES: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
#[cfg(feature = "touch-telemetry")]
pub static RDBG_SPANS: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

impl slint::platform::software_renderer::LineBufferProvider
    for &mut SingleLineFlusher<'_, '_>
{
    type TargetPixel = Rgb565Pixel;

    fn process_line(
        &mut self,
        line: usize,
        range: core::ops::Range<usize>,
        render_fn: impl FnOnce(&mut [Self::TargetPixel]),
    ) {
        if range.is_empty() {
            render_fn(&mut []);
            return;
        }
        #[cfg(feature = "touch-telemetry")]
        RDBG_LINES.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        debug_assert!(line < HEIGHT && range.end <= WIDTH);

        render_fn(&mut self.buf[range.clone()]);

        let (x0, w) = (range.start, range.len());
        for i in 0..w {
            self.scratch[i] = self.buf[x0 + i].0;
        }

        // ★ SPAN-RUN BATCHING. Consecutive lines sharing an x-span stream into
        // ONE window instead of one window each — the body of any rectangle is
        // exactly that shape, so a full repaint collapses from ~240 CASET/RASET/
        // RAMWR sequences to a handful.
        //
        // This batches WINDOWS, not pixels: no line is buffered, `render_fn`
        // still writes one line and it is pushed immediately. That distinction
        // is what keeps it clear of the "batching would invent a framebuffer"
        // objection — the memory the renderer owns between calls is untouched.
        //
        // ⚠️ THE CONSTRAINT THAT SHAPES THIS: a window's height is committed at
        // CASET/RASET time, and streaming past it WRAPS the ST7789's GRAM
        // pointer back to the window origin, overwriting what was just drawn.
        // So a run cannot be extended after opening. It instead declares the
        // largest extent it could need — to the bottom of the panel — and
        // simply under-fills it, which is legal: the GRAM pointer just stops.
        let continues = matches!(
            self.run,
            Some((rx, rw, next)) if rx as usize == x0 && rw as usize == w && next == line
        );
        if !continues {
            // Span changed, lines not consecutive, or nothing open: close the
            // previous run and start one here. Worst case — alternating spans,
            // which the renderer can emit when the dirty region has several
            // rects on one line — this degrades to exactly the old
            // one-window-per-line behaviour, never worse.
            self.close_run();
            // `HEIGHT - line` would UNDERFLOW in release if `line` ever went
            // out of range — the `debug_assert!` above is compiled out by
            // [profile.release], the same trap the flusher/board const-assert
            // exists for. A saturating floor of 1 keeps a bad line number to a
            // one-row window instead of a 65535-row one.
            let h = HEIGHT.saturating_sub(line).max(1) as u16;
            #[cfg(feature = "touch-telemetry")]
            RDBG_SPANS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            self.display.set_addr_window(x0 as u16, line as u16, w as u16, h);
            self.display.begin_pixels();
        }
        // `push_pixels` must not re-issue a preamble between calls — the
        // contract requires that, and `SharedSpiBus` honours it (RAMWR once,
        // RAMWR_CONT thereafter), which is what makes a multi-line run land
        // contiguously.
        self.display.push_pixels(&self.scratch[..w]);
        self.run = Some((x0 as u16, w as u16, line + 1));
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
            if self.pair_base == Some(line - 1) && self.span_staged(&range) {
                self.flush_span(line - 1, range);
                self.flushed_n += 1;
            } else {
                self.violations += 1;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The two per-board constants that must agree, made to agree at build time.
// ---------------------------------------------------------------------------
// `board::FLUSH_STRIP_LINES` sizes the strip buffers; `board::BoardFlusher`
// picks the algorithm that reads them. They are selected independently, in two
// different files, and nothing connected them.
//
// ⚠️ The buffer-length `debug_assert!`s in each `new()` do NOT cover this:
// `[profile.release]` compiles them out, so the one build that ships is the one
// build with no check. A future board pairing `TwoLineFlusher` with
// `FLUSH_STRIP_LINES = 1` would index past a half-length `buf` on the FIRST
// flush — a panic in the shipped image, found on glass.
//
// A const assert moves that to the compiler. Same spirit as `board/mod.rs`'s
// exactly-one-board check: state the invariant where it can be enforced rather
// than where it can be violated. (Found by vesper's conformance review of
// c658bc1 — the seam introduced the second constant without pairing it.)
const _: () = assert!(
    board::FLUSH_STRIP_LINES == board::BoardFlusher::LINES,
    "board::FLUSH_STRIP_LINES must equal the selected BoardFlusher's LINES — \
     the strip buffers are sized by one and indexed by the other"
);
