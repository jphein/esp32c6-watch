// Slint platform glue for the CO5300 AMOLED: embassy-clocked Platform and a
// line-streaming flusher. No framebuffer — the software renderer paints into
// a 2-line RGB565 strip (410 x 2 x 2 B) streamed to the panel's GRAM.
// Two lines per flush because the CO5300 requires a min 2x2 address window.

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
    let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    window.set_size(slint::PhysicalSize::new(WIDTH as u32, HEIGHT as u32));
    slint::platform::set_platform(Box::new(EspPlatform {
        window: window.clone(),
    }))
    .expect("set_platform failed (init_platform called twice?)");
    window
}

/// LineBufferProvider that batches two rendered lines per panel write.
pub struct TwoLineFlusher<'a, 'd> {
    display: &'a mut Co5300Display<'d>,
    /// 2 x WIDTH pixels: line A in the first half, line B in the second.
    buf: &'a mut [Rgb565Pixel],
    /// Raw u16 staging for the QSPI bus.
    scratch: &'a mut [u16],
    /// y of the line waiting in the first half of `buf`, if any.
    pending: Option<usize>,
}

impl<'a, 'd> TwoLineFlusher<'a, 'd> {
    pub fn new(
        display: &'a mut Co5300Display<'d>,
        buf: &'a mut [Rgb565Pixel],
        scratch: &'a mut [u16],
    ) -> Self {
        // Size invariants: flush_two zips `scratch` against `buf` and sends
        // `scratch` as one QSPI write — a short scratch silently truncates
        // that write and desyncs the panel's GRAM address pointer.
        debug_assert_eq!(buf.len(), WIDTH * 2);
        debug_assert!(scratch.len() >= WIDTH * 2);
        Self {
            display,
            buf,
            scratch,
            pending: None,
        }
    }
}

impl TwoLineFlusher<'_, '_> {
    /// Send `buf` (two lines) to rows `y` and `y + 1`.
    fn flush_two(&mut self, y: usize) {
        for (dst, src) in self.scratch.iter_mut().zip(self.buf.iter()) {
            *dst = src.0;
        }
        self.display.set_addr_window(0, y as u16, WIDTH as u16, 2);
        self.display.bus_mut().write_pixels(self.scratch);
    }

    /// Flush a leftover single line by duplicating it into a 2-row window.
    /// (Never hit in practice: with a full-frame repaint all 502 lines come
    /// in consecutively and 502 is even.)
    pub fn flush_pending(&mut self) {
        if let Some(y) = self.pending.take() {
            let (first, second) = self.buf.split_at_mut(WIDTH);
            second.copy_from_slice(first);
            let y = y.min(HEIGHT - 2); // keep the 2-row window on the panel
            self.flush_two(y);
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
        // Decide which half of the strip this line goes into.
        let second_half = match self.pending {
            Some(p) if line == p + 1 => true,
            Some(_) => {
                // Non-consecutive line: emit the stragglers first.
                self.flush_pending();
                false
            }
            None => false,
        };

        let offset = if second_half { WIDTH } else { 0 };
        let dst = &mut self.buf[offset..offset + WIDTH];
        if range.start != 0 || range.end != WIDTH {
            // Partial dirty range: blank the rest of the strip line so we
            // never push stale pixels (full repaints make this a no-op).
            dst.fill(Rgb565Pixel(0));
        }
        render_fn(&mut dst[range]);

        if second_half {
            let y = self.pending.take().unwrap();
            self.flush_two(y);
        } else {
            self.pending = Some(line);
        }
    }
}
