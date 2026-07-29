// QSPI bus driver for the CO5300 AMOLED display — non-blocking DMA flush.
//
// The bulk pixel push (`write_pixels`, the Slint two-line strip flush that runs
// once per rendered line-pair) is the render loop's hottest CPU sink. Previously
// it went through `SpiDmaBus::half_duplex_write`, which DMAs the bytes but then
// BUSY-WAITS the CPU (`wait_for_idle`) until the transfer finishes — so the CPU
// is pinned for the whole flush and cannot poll touch. That is the "render
// starves touch" mechanism (perf-roadmap H2).
//
// This driver drops the blocking `SpiDmaBus` wrapper and drives the raw
// `SpiDma` + a single `DmaTxBuf` itself. For the common single-chunk pixel
// write it KICKS the DMA and returns immediately, leaving CS low and the
// transfer in flight: the CPU is free to render the next strip (and, once the
// loop wires it up, poll touch) while the panel is being fed by DMA. The
// transfer is reclaimed lazily — the next bus operation waits for completion
// and raises CS to terminate the pixel transaction. Because Slint renders the
// following two lines before it asks us to flush them, that render overlaps the
// previous strip's DMA for free, with no framebuffer and no double buffering
// (the DMA buffer is only ever touched again after the reclaim wait, so a
// single TX buffer is race-free).
//
// Byte-swapping is done straight into the DMA buffer, removing the extra
// heap scratch + the memcpy that `SpiDmaBus` did internally.
//
// ★ Descriptor sizing: the TX descriptors are sized by the `dma_tx_buffer!`
// macro, which derives the count from `esp_hal::dma::descriptor_count(...)`
// (never a hand-picked `[DmaDescriptor; N]`) and word-aligns the buffer. An
// oversized hand-sized descriptor array whose extra `.next` links were null and
// got walked by the DMA engine was the exact v0.6.0 crash (store to 0x8).

use esp_hal::dma::DmaTxBuf;
use esp_hal::dma_tx_buffer;
use esp_hal::gpio::Output;
use esp_hal::spi::master::{Address, Command, DataMode, SpiDma, SpiDmaTransfer};
use esp_hal::Blocking;

// Bytes per DMA transfer == size of the single TX DMA buffer. A Slint strip is
// WIDTH * 2 lines * 2 B = 1640 B (well under one chunk → single-shot flush);
// larger fills (fill_screen / write_repeat) are chunked synchronously.
//
// #75 (lucid): was 8000, which bought NOTHING on the hot path and cost 5,960 B
// of `.bss` — i.e. 5,960 B of stack, since `stack = _stack_start - _bss_end`
// (the #65 crash class). The Slint strip flush is 1,640 B and takes the
// single-shot deferred path either way; the only callers that ever exceed one
// chunk are `write_pixels_chunked` / `stream_pixels` / `write_repeat`, which
// already loop. So the buffer only has to be >= 1,640 B to preserve the
// performance-critical deferred flush; 2048 is the next power of two above it.
//
// Measured, identical tree otherwise (`size -A`):
//   DMA_CHUNK 8000 -> .bss 268,280  .stack 75,120  (floor margin +3,440)
//   DMA_CHUNK 2048 -> .bss 262,320  .stack 81,080  (floor margin +9,400)
//
// Cost: a framebuffer-game full-frame flush (205*251 px = 102,910 B) goes from
// 13 DMA kicks to 51. The per-pixel byteswap dominates that loop, so the extra
// ~38 setups are noise; the Slint path is byte-for-byte unchanged.
//
// The 5,960 B currently lands in the STACK (margin +3,440 -> +9,400, real #65
// insurance). To spend it on the heap instead, raise the MAIN `heap_allocator!`
// in main.rs by the same amount — that pushes `_bss_end` back up and returns the
// stack to where it was. Do NOT do both.
const DMA_CHUNK: usize = 2048;

/// Ownership state of the SPI peripheral + its TX DMA buffer.
enum Bus<'d> {
    /// The SPI and its TX buffer are ours to reuse.
    Idle(SpiDma<'d, Blocking>, DmaTxBuf),
    /// A deferred single-chunk pixel write is in flight. CS is held LOW and
    /// must be raised once the DMA completes — done in [`QspiBus::reclaim`].
    Flushing(SpiDmaTransfer<'d, Blocking, DmaTxBuf>),
    /// Transient hole while moving the SPI between the two states above. Never
    /// observed across a method boundary (all methods are `&mut self`).
    Between,
}

pub struct QspiBus<'d> {
    bus: Bus<'d>,
    cs: Output<'d>,
}

/// Byte-swap RGB565 pixels (big-endian on the wire) straight into `dst`.
/// `dst` must hold at least `pixels.len() * 2` bytes.
#[inline]
fn byteswap_into(pixels: &[u16], dst: &mut [u8]) {
    for (i, &px) in pixels.iter().enumerate() {
        dst[i * 2] = (px >> 8) as u8;
        dst[i * 2 + 1] = px as u8;
    }
}

impl<'d> QspiBus<'d> {
    pub fn new(spi: SpiDma<'d, Blocking>, cs: Output<'d>) -> Self {
        // Descriptors sized via `descriptor_count(...)`, buffer word-aligned —
        // both handled by the macro (see the ★ note above).
        let tx = dma_tx_buffer!(DMA_CHUNK).expect("QSPI DMA TX buffer");
        Self {
            bus: Bus::Idle(spi, tx),
            cs,
        }
    }

    /// If a deferred pixel DMA is in flight, block until it completes, raise CS
    /// to terminate that pixel transaction, and return to `Idle`. No-op (and no
    /// CS change) when already idle. Cheap when the DMA already finished during
    /// the caller's intervening CPU work — that overlap is the whole point.
    fn reclaim(&mut self) {
        if matches!(self.bus, Bus::Flushing(_)) {
            let Bus::Flushing(xfer) = core::mem::replace(&mut self.bus, Bus::Between) else {
                unreachable!()
            };
            let (spi, buf) = xfer.wait();
            self.cs.set_high(); // end the deferred pixel-write transaction
            self.bus = Bus::Idle(spi, buf);
        }
    }

    /// Take the idle SPI + TX buffer by value. Caller MUST have already
    /// [`reclaim`](Self::reclaim)ed (i.e. the bus is `Idle`).
    fn take_idle(&mut self) -> (SpiDma<'d, Blocking>, DmaTxBuf) {
        match core::mem::replace(&mut self.bus, Bus::Between) {
            Bus::Idle(spi, buf) => (spi, buf),
            _ => unreachable!("take_idle: bus must be reclaimed to Idle first"),
        }
    }

    /// Kick a half-duplex write of `n` bytes (already staged in `buf`), block
    /// until it completes, and restore the `Idle` state. Does not touch CS.
    fn kick_wait(
        &mut self,
        spi: SpiDma<'d, Blocking>,
        buf: DmaTxBuf,
        mode: DataMode,
        cmd: Command,
        addr: Address,
        n: usize,
    ) {
        let (spi, buf) = match spi.half_duplex_write(mode, cmd, addr, 0, n, buf) {
            Ok(xfer) => xfer.wait(),
            Err((_e, spi, buf)) => (spi, buf),
        };
        self.bus = Bus::Idle(spi, buf);
    }

    /// A complete, self-contained command/address transaction: reclaim any
    /// pending flush, pulse CS low → sync write → CS high. Matches the byte
    /// layout the CO5300 expects (`0x02` cmd, 24-bit reg address).
    fn cmd_write(&mut self, mode: DataMode, cmd: Command, addr: Address, data: &[u8]) {
        self.reclaim();
        self.cs.set_low();
        let (spi, mut buf) = self.take_idle();
        let n = data.len();
        if n > 0 {
            buf.as_mut_slice()[..n].copy_from_slice(data);
        }
        self.kick_wait(spi, buf, mode, cmd, addr, n);
        self.cs.set_high();
    }

    pub fn write_command(&mut self, reg: u8) {
        self.cmd_write(
            DataMode::Single,
            Command::_8Bit(0x02, DataMode::Single),
            Address::_24Bit((reg as u32) << 8, DataMode::Single),
            &[],
        );
    }

    pub fn write_c8d8(&mut self, reg: u8, data: u8) {
        self.cmd_write(
            DataMode::Single,
            Command::_8Bit(0x02, DataMode::Single),
            Address::_24Bit((reg as u32) << 8, DataMode::Single),
            &[data],
        );
    }

    pub fn write_c8d16d16(&mut self, reg: u8, d1: u16, d2: u16) {
        let data = [(d1 >> 8) as u8, d1 as u8, (d2 >> 8) as u8, d2 as u8];
        self.cmd_write(
            DataMode::Single,
            Command::_8Bit(0x02, DataMode::Single),
            Address::_24Bit((reg as u32) << 8, DataMode::Single),
            &data,
        );
    }

    /// Open a streamed pixel transaction (cmd+address, no data), leaving CS LOW.
    /// Follow with [`stream_pixels`](Self::stream_pixels) and close with
    /// [`end_pixels`](Self::end_pixels).
    pub fn begin_pixels(&mut self) {
        self.reclaim();
        self.cs.set_low();
        let (spi, buf) = self.take_idle();
        self.kick_wait(
            spi,
            buf,
            DataMode::Quad,
            Command::_8Bit(0x32, DataMode::Single),
            Address::_24Bit(0x003C00, DataMode::Single),
            0,
        );
        // CS intentionally left LOW for the streamed data phase.
    }

    /// Stream a continuation chunk of an already-open pixel transaction. Each
    /// chunk is written synchronously (same CS-low transaction, shared buffer).
    pub fn stream_pixels(&mut self, pixels: &[u16]) {
        if pixels.is_empty() {
            return;
        }
        let max_px = DMA_CHUNK / 2;
        let mut remaining = pixels;
        while !remaining.is_empty() {
            let n = remaining.len().min(max_px);
            // Mid-transaction: the bus is Idle between chunks (kick_wait blocks).
            let (spi, mut buf) = self.take_idle();
            byteswap_into(&remaining[..n], buf.as_mut_slice());
            self.kick_wait(spi, buf, DataMode::Quad, Command::None, Address::None, n * 2);
            remaining = &remaining[n..];
        }
    }

    /// Close a streamed pixel transaction: raise CS.
    pub fn end_pixels(&mut self) {
        self.reclaim();
        self.cs.set_high();
    }

    /// Push a run of RGB565 pixels to the panel. The single-chunk case (every
    /// Slint strip flush) is DEFERRED: the DMA is kicked and this returns with
    /// the transfer in flight and CS held low, freeing the CPU during the push.
    /// The completion is reclaimed by the next bus op (`set_addr_window` for the
    /// following strip), which overlaps this DMA with the intervening render.
    pub fn write_pixels(&mut self, pixels: &[u16]) {
        if pixels.is_empty() {
            return;
        }
        let n_bytes = pixels.len() * 2;
        if n_bytes > DMA_CHUNK {
            // Never hit by Slint strips (1640 B); kept correct for large writes.
            self.write_pixels_chunked(pixels);
            return;
        }

        self.reclaim();
        self.cs.set_low();
        let (spi, mut buf) = self.take_idle();
        byteswap_into(pixels, buf.as_mut_slice());
        match spi.half_duplex_write(
            DataMode::Quad,
            Command::_8Bit(0x32, DataMode::Single),
            Address::_24Bit(0x003C00, DataMode::Single),
            0,
            n_bytes,
            buf,
        ) {
            Ok(xfer) => {
                // Deferred: DMA runs in the background, CS stays LOW until
                // reclaim() waits for it and raises CS. CPU is free now.
                self.bus = Bus::Flushing(xfer);
            }
            Err((_e, spi, buf)) => {
                // Kick failed: nothing in flight — restore Idle and close CS.
                self.cs.set_high();
                self.bus = Bus::Idle(spi, buf);
            }
        }
    }

    /// Synchronous multi-chunk pixel write (fallback for runs larger than one
    /// DMA chunk; not the touch-critical path).
    fn write_pixels_chunked(&mut self, pixels: &[u16]) {
        self.reclaim();
        self.cs.set_low();
        let max_px = DMA_CHUNK / 2;
        let mut remaining = pixels;
        let mut first = true;
        while !remaining.is_empty() {
            let n = remaining.len().min(max_px);
            let (spi, mut buf) = self.take_idle();
            byteswap_into(&remaining[..n], buf.as_mut_slice());
            let (cmd, addr) = if first {
                (
                    Command::_8Bit(0x32, DataMode::Single),
                    Address::_24Bit(0x003C00, DataMode::Single),
                )
            } else {
                (Command::None, Address::None)
            };
            first = false;
            self.kick_wait(spi, buf, DataMode::Quad, cmd, addr, n * 2);
            remaining = &remaining[n..];
        }
        self.cs.set_high();
    }

    /// Fill `count` pixels with a single color, chunked and synchronous.
    pub fn write_repeat(&mut self, color: u16, count: u32) {
        if count == 0 {
            return;
        }
        let hi = (color >> 8) as u8;
        let lo = color as u8;
        let max_px = DMA_CHUNK / 2;
        self.reclaim();
        self.cs.set_low();
        let mut remaining = count;
        let mut first = true;
        while remaining > 0 {
            let n = remaining.min(max_px as u32) as usize;
            let (spi, mut buf) = self.take_idle();
            {
                let slice = buf.as_mut_slice();
                for i in 0..n {
                    slice[i * 2] = hi;
                    slice[i * 2 + 1] = lo;
                }
            }
            let (cmd, addr) = if first {
                (
                    Command::_8Bit(0x32, DataMode::Single),
                    Address::_24Bit(0x003C00, DataMode::Single),
                )
            } else {
                (Command::None, Address::None)
            };
            first = false;
            self.kick_wait(spi, buf, DataMode::Quad, cmd, addr, n * 2);
            remaining -= n as u32;
        }
        self.cs.set_high();
    }
}
