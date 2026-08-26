//! Scene-cast core (#36's `cast`, watch edition) — the PURE half of mirroring
//! the watch's screen onto a WLED LED matrix over UDP.
//!
//! smol's `net/cast.rs` casts a 72x40 1-BIT OLED; the watch renders COLOR
//! through a LINE flusher with no full framebuffer, so this is a redesign on
//! smol's protocol rather than a vendor:
//!
//!  - [`RowMap`]: which SOURCE rows matter. The flusher hook samples only
//!    lines that hit a target cell's center row — ~M rows out of 502, so a
//!    non-cast frame costs one array lookup per flushed line and a cast
//!    frame samples N pixels on M lines instead of touching every pixel.
//!  - [`Mirror`]: the NxM RGB565 accumulator (caller-owned storage, the OTA
//!    window-buffer discipline — heap per cast session, never .bss).
//!  - [`encode_dnrgb`]: WLED realtime protocol 4 (DNRGB) packets, chunked to
//!    stay useful with small UDP TX buffers, RGB565 → RGB888 expansion at the
//!    wire (identical rounding to the panel's own gamma-naive expansion).
//!
//! The IMPURE half (the flusher tap + the embassy-net UDP send + the console
//! `cast` command) wires in firmware behind a `cast` feature — designed, not
//! yet landed; this crate exists so that wiring is arithmetic-free.
#![no_std]
#![forbid(unsafe_code)]

/// WLED's realtime UDP port.
pub const WLED_PORT: u16 = 21324;
/// DNRGB protocol id (start-index addressed, 16-bit index — works on any size).
pub const DNRGB: u8 = 4;
/// LEDs per packet: 2 + 2 + 128*3 = 388 B, under a 512 B UDP TX buffer.
pub const MAX_LEDS_PER_PKT: usize = 128;
/// Seconds WLED holds the realtime override after the last packet — the
/// auto-release when a cast stops (or a watch dies mid-cast).
pub const DEFAULT_TIMEOUT_S: u8 = 2;
/// Matrix dimension caps (storage = W*H u16; 32x32 = 2 KiB).
pub const MAX_W: usize = 32;
pub const MAX_H: usize = 32;

/// Which source row feeds each target row (cell centers), as a dense map the
/// flusher indexes per line: `map[src_y] == Some(target_row)`.
pub struct RowMap {
    /// One entry per SOURCE line; u8::MAX = not sampled.
    map: [u8; 512],
    src_h: usize,
}

impl RowMap {
    /// `src_h` panel lines feeding `dst_h` target rows (center sampling).
    pub fn new(src_h: usize, dst_h: usize) -> Self {
        let mut map = [u8::MAX; 512];
        if src_h > 0 && src_h <= 512 && dst_h > 0 && dst_h <= MAX_H {
            for ty in 0..dst_h {
                // the cell's center source row
                let sy = (ty * src_h + src_h / 2) / dst_h;
                if sy < src_h {
                    map[sy] = ty as u8;
                }
            }
        }
        Self { map, src_h }
    }

    /// The target row this source line feeds, if any.
    #[inline]
    pub fn target_row(&self, src_y: usize) -> Option<usize> {
        if src_y >= self.src_h {
            return None;
        }
        let t = self.map[src_y];
        (t != u8::MAX).then_some(t as usize)
    }
}

/// The NxM RGB565 accumulator. Storage is CALLER-owned (`&mut [u16]`,
/// `w * h` long) — heap per cast session, the OTA window-buffer discipline.
/// Copy: the geometry is two usizes, handed to the flusher sink each frame
/// while the owner keeps its copy.
#[derive(Clone, Copy)]
pub struct Mirror {
    pub w: usize,
    pub h: usize,
}

impl Mirror {
    pub fn new(w: usize, h: usize) -> Option<Self> {
        (w > 0 && w <= MAX_W && h > 0 && h <= MAX_H).then_some(Self { w, h })
    }

    pub fn cells(&self) -> usize {
        self.w * self.h
    }

    /// Sample a rendered source SPAN (one panel line's pixels, starting at
    /// `span_x`) into target row `ty`: for each target column whose center
    /// falls inside the span, store that pixel. `src_w` is the panel width.
    pub fn sample_span(
        &self,
        store: &mut [u16],
        ty: usize,
        src_w: usize,
        span_x: usize,
        span: &[u16],
    ) {
        self.sample_span_with(store, ty, src_w, span_x, span.len(), |i| span[i]);
    }

    /// As [`sample_span`](Self::sample_span) but the span pixels come from a
    /// closure `get(i) -> u16` over `0..span_len` — so a caller whose render
    /// buffer is not a `&[u16]` (the watch flusher's `Rgb565Pixel`) taps it
    /// with no intermediate copy. `span_x` is the span's first column.
    pub fn sample_span_with<F: Fn(usize) -> u16>(
        &self,
        store: &mut [u16],
        ty: usize,
        src_w: usize,
        span_x: usize,
        span_len: usize,
        get: F,
    ) {
        if ty >= self.h || src_w == 0 {
            return;
        }
        for tx in 0..self.w {
            let sx = (tx * src_w + src_w / 2) / self.w;
            if sx >= span_x && sx < span_x + span_len {
                store[ty * self.w + tx] = get(sx - span_x);
            }
        }
    }
}

/// RGB565 → (r, g, b) with the standard bit-replication expansion.
#[inline]
pub fn rgb565_to_888(p: u16) -> (u8, u8, u8) {
    let r5 = ((p >> 11) & 0x1F) as u8;
    let g6 = ((p >> 5) & 0x3F) as u8;
    let b5 = (p & 0x1F) as u8;
    ((r5 << 3) | (r5 >> 2), (g6 << 2) | (g6 >> 4), (b5 << 3) | (b5 >> 2))
}

/// Encode ONE DNRGB packet carrying `cells[start..]` (up to
/// [`MAX_LEDS_PER_PKT`]) into `out`. Returns (bytes_written, cells_consumed);
/// (0, 0) if `out` is too small or `start` is past the end.
pub fn encode_dnrgb(
    cells: &[u16],
    start: usize,
    timeout_s: u8,
    out: &mut [u8],
) -> (usize, usize) {
    if start >= cells.len() {
        return (0, 0);
    }
    let n = (cells.len() - start).min(MAX_LEDS_PER_PKT);
    let need = 4 + n * 3;
    if out.len() < need {
        return (0, 0);
    }
    out[0] = DNRGB;
    out[1] = timeout_s;
    out[2] = (start >> 8) as u8;
    out[3] = (start & 0xFF) as u8;
    for (i, &c) in cells[start..start + n].iter().enumerate() {
        let (r, g, b) = rgb565_to_888(c);
        out[4 + i * 3] = r;
        out[4 + i * 3 + 1] = g;
        out[4 + i * 3 + 2] = b;
    }
    (need, n)
}
