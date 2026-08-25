//! ⚠️ LIFTED from feat/cyd-c5-gating @ a5f39e2 (morpheus's lane) so the S3
//! panel driver can ship from main before that branch merges. His branch
//! stays authoritative for this file until then — the merge unifies; do not
//! let a conflict resolve AGAINST his copy.
//! Shared classic-SPI bus for the NM-CYD-C5: ST7789 panel + XPT2046 touch on
//! one SPI2 (FSPI) peripheral.
//!
//! # Why this exists, and why its method names look like the watch's
//!
//! This is the CYD's answer to the watch's `drivers::qspi_bus::QspiBus`. The
//! public surface is deliberately name- and signature-compatible
//! (`write_command`, `write_c8d8`, `write_c8d16d16`, `begin_pixels`,
//! `stream_pixels`, `end_pixels`, `write_pixels`, `write_repeat`) so that
//! [`crate::drivers::st7789::St7789Display`] can be a line-for-line analogue of
//! `Co5300Display`, and so the watch's `ui::slint_platform::TwoLineFlusher` —
//! whose entire hardware contact is
//!
//! ```text
//! display.set_addr_window(x0, y_even, w, 2);
//! display.bus_mut().write_pixels(&scratch[..w * 2]);
//! ```
//!
//! — compiles against this stack with a type swap and nothing else.
//!
//! # Three differences from `QspiBus`, all forced by the hardware
//!
//! 1. **Framing.** The CO5300 is QSPI: a command is opcode `0x02` plus a 24-bit
//!    register address, pixels are opcode `0x32` plus address `0x003C00`, all
//!    on four data lines. The ST7789 is classic 4-wire SPI: a **DC pin** low
//!    for command bytes, high for data bytes, one data line. So every
//!    `write_c*` here lowers DC, ships one byte, raises DC, ships the payload.
//!
//! 2. **Two devices, two clock rates.** The panel wants 20 MHz and the XPT2046
//!    wants <= ~2 MHz for a settled conversion (see [`crate::board`] for the
//!    vendor citations). Running the touch chip at display speed does not
//!    error — it returns *plausible* garbage, the worst failure mode there is.
//!    [`SharedSpiBus`] therefore owns both chip selects and reapplies the SPI
//!    `Config` whenever the active device changes, skipping the reconfigure
//!    when it would be a no-op (the display path never pays for it).
//!
//! 3. **Blocking, not deferred.** `QspiBus` hand-rolls a non-blocking DMA kick
//!    so the CPU can render the next strip while the previous one is in flight
//!    — that was the fix for "render starves touch" on the watch. Here the bus
//!    is genuinely shared with the touch controller, so a transfer that is left
//!    in flight with CS low is a transfer that blocks touch polling anyway; the
//!    deferred trick buys nothing and costs a CS-ownership hazard. This driver
//!    uses `SpiDmaBus` (DMA'd, but synchronous). Revisit only if profiling on
//!    glass shows the strip flush dominating, and only with a plan for who owns
//!    CS during the overlap.
//!
//! # ⚠️ The invariant that makes DC framing safe — do not break it
//!
//! Every `write_c*` here raises DC immediately after shipping the opcode byte.
//! That is only correct because `SpiDmaBus::write` is **fully synchronous down
//! to the last clock edge**: it ends each chunk with `wait_for_idle()`, whose
//! `is_done()` checks `driver().busy()` — the SPI peripheral's own busy flag —
//! *before* looking at the DMA channel (esp-hal 1.1.2,
//! `src/spi/master/dma.rs:343-369`). So when `write` returns, the byte has been
//! clocked out, not merely handed to the DMA.
//!
//! If anyone later swaps this for a deferred/non-blocking flush (the
//! `QspiBus::write_pixels` trick), **DC and CS transitions must move behind an
//! explicit completion wait**. Raising DC while the opcode's final bits are
//! still on the wire makes the panel read them as pixel data: the failure looks
//! like intermittent colour corruption under load, not like a framing bug, and
//! it will not reproduce at low frame rates.
//!
//! Also audited for the esp-hal 1.1 "defaults are not 1.0-rc.0's defaults"
//! hazard called out in `PORT-SCOPING.md`: `spi::master::Config::default()` is
//! 1 MHz / `Mode::_0` / MSB-first on both directions
//! (`src/spi/master/mod.rs:478-490`). Frequency and mode are set explicitly
//! below; MSB-first is what both the ST7789 and the XPT2046 want, so no default
//! is being silently inherited here.

use esp_hal::gpio::Output;
use esp_hal::spi::master::{Config as SpiConfig, SpiDmaBus};
use esp_hal::time::Rate;
use esp_hal::Blocking;

use crate::board;

/// Bytes staged per DMA write.
///
/// Sized to exactly one watch-style two-line strip: `LCD_WIDTH * 2 lines * 2 B`
/// = 1280 B, so the hot path is a single staged copy and a single DMA. (The
/// watch's equivalent strip is 1640 B because its panel is 410 px wide; its
/// `DMA_CHUNK` is 2048, the next power of two above that.)
///
/// `SpiDmaBus::write` already re-chunks anything longer than the DMA TX buffer,
/// so this constant bounds the *staging* array, i.e. `.bss`. Bigger would only
/// help `write_repeat`, which is not on any latency-critical path.
///
/// # PSRAM: internal RAM is a preference here, not a requirement
///
/// ⚠️ **CORRECTED 2026-08-25. An earlier version of this comment claimed "DMA
/// cannot reach PSRAM on the ESP32-C5" and that a PSRAM-backed DMA buffer fails
/// with `DmaBufError::UnsupportedMemoryRegion`. That is FALSE — do not
/// reintroduce it.** `dma_can_access_psram` **is** set for the C5 under our exact
/// pin: `esp-metadata-generated 0.4.0` (the version esp-hal 1.1.2 resolves, per
/// our `Cargo.lock`) emits both `"dma_can_access_psram"` and
/// `"cargo:rustc-cfg=dma_can_access_psram"` at `src/_build_script_utils.rs:1837`
/// and `:2085`, inside the `esp32c5` block that spans lines 1696..2298. So
/// `dma/buffers.rs` takes the *enabled* arm and a PSRAM buffer validates fine.
///
/// The retracted claim came from confusing two similarly-named flags: the C5
/// genuinely lacks `ext_mem_configurable_block_size` — which only gates *tuning*
/// the external block size at runtime — and that was read as the C5 lacking
/// PSRAM-DMA capability altogether. Different questions, adjacent names.
///
/// And TX is the forgiving direction: `ExternalBurstConfig::min_psram_alignment`
/// (`dma/buffers.rs:174-195`) returns the burst size for `TransferDirection::In`
/// but **1** for `Out`, quoting the TRM — *"Size, length, and buffer address
/// pointer in transmit descriptors do not need to be aligned."* Display output is
/// TX, so **SPI TX straight out of PSRAM is legal and unaligned-safe here.**
///
/// So: prefer internal RAM for the `DmaRxBuf`/`DmaTxBuf` — it is faster and has
/// no cache-coherency surface — but treat that as a **performance choice**. It is
/// not a correctness constraint, and a PSRAM framebuffer may now be DMA'd from
/// directly rather than memcpy'd through internal RAM first.
///
/// ★ Scope, which is still worth stating because it is easy to get backwards:
/// whatever memory rules apply, they apply to the `DmaRxBuf`/`DmaTxBuf` the
/// **caller** builds and hands to `Spi::with_buffers()`
/// (`esp-hal-1.1.2/src/spi/master/dma.rs:590`) and which `SpiDmaBus` then owns
/// (`:846`, field at `:852`) — that is the memory the DMA engine reads. They do
/// **not** apply to [`SharedSpiBus`]'s own `stage` array below. Nothing DMAs out
/// of `stage`; the CPU copies from it into the `DmaTxBuf`, at `dma.rs:940`:
///
/// ```text
/// self.tx_buf.as_mut_slice()[..chunk.len()].copy_from_slice(chunk);
/// ```
///
/// The engine never sees the caller's slice. The word "staging" fits both
/// allocations, which is why a rule phrased as "SPI staging buffers must be
/// internal RAM" points at the one that cannot matter.
///
/// If a PSRAM region is ever added: `psram_allocator!` extends the **same global
/// heap** rather than creating a second allocator, and unqualified allocations
/// are first-fit in **insertion order** — so `heap_allocator!` must run *before*
/// `psram_allocator!` if internal RAM should be preferred by default. With the
/// correction above this is a *performance* ordering, not a correctness one:
/// getting it backwards makes default allocations land in PSRAM, which is slower
/// but works. Use `Vec::new_in(esp_alloc::InternalMemory)` where the speed
/// actually matters rather than relying on declaration order to enforce it.
pub const STAGE_BYTES: usize = board::LCD_WIDTH as usize * 2 * 2;

/// Which device the SPI `Config` is currently tuned for.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tuned {
    /// Nothing applied yet — the first select must configure.
    None,
    Display,
    Touch,
}

/// ST7789 memory-write opcodes. `RAMWR` restarts the write at the window
/// origin; `RAMWR_CONT` resumes where the previous write stopped, which is what
/// makes a chunked pixel push across several transactions land contiguously.
const CMD_RAMWR: u8 = 0x2C;
const CMD_RAMWR_CONT: u8 = 0x3C;

pub struct SharedSpiBus<'d> {
    spi: SpiDmaBus<'d, Blocking>,
    dc: Output<'d>,
    lcd_cs: Output<'d>,
    touch_cs: Option<Output<'d>>,
    /// Held, never driven low. Its whole job is to keep the SD card off the bus
    /// — see [`crate::board::PIN_SD_CS`]. Owning it also makes it impossible
    /// for another part of the firmware to claim GPIO10 by accident.
    _sd_cs: Option<Output<'d>>,
    tuned: Tuned,
    /// RGB565 byteswap staging (see [`STAGE_BYTES`]).
    stage: [u8; STAGE_BYTES],
    /// Next memory-write opcode: `RAMWR` immediately after an address-window
    /// change, `RAMWR_CONT` for every continuation after that.
    next_ramwr: u8,
}

impl<'d> SharedSpiBus<'d> {
    /// Take ownership of the bus and all three chip selects.
    ///
    /// `sd_cs` is required rather than optional on purpose: an undeselected SD
    /// card clocks along with every display transaction and corrupts it, and a
    /// board with no card fitted still has a floating pin. Pass it even if you
    /// never intend to use the slot.
    pub fn new(
        spi: SpiDmaBus<'d, Blocking>,
        dc: Output<'d>,
        lcd_cs: Output<'d>,
        // Option-alized on main (S3 uses this bus with NOTHING else on it —
        // touch is I2C, no SD in play). The C5 passes Some for both, exactly
        // as before. morpheus: at merge, keep THIS signature and wrap your
        // call site's two pins in Some() — flagged in advance.
        touch_cs: Option<Output<'d>>,
        sd_cs: Option<Output<'d>>,
    ) -> Self {
        let mut sd_cs = sd_cs;
        if let Some(cs) = sd_cs.as_mut() {
            cs.set_high();
        }
        Self {
            spi,
            dc,
            lcd_cs,
            touch_cs,
            _sd_cs: sd_cs,
            tuned: Tuned::None,
            stage: [0u8; STAGE_BYTES],
            next_ramwr: CMD_RAMWR,
        }
    }

    // -- clock-rate arbitration -------------------------------------------

    fn tune(&mut self, want: Tuned, hz: u32) {
        if self.tuned == want {
            return;
        }
        let cfg = SpiConfig::default()
            .with_frequency(Rate::from_hz(hz))
            .with_mode(esp_hal::spi::Mode::_0);
        // A rate the peripheral cannot synthesise is a programming error, not a
        // runtime condition — both rates here are fixed constants.
        self.spi.apply_config(&cfg).expect("SPI apply_config");
        self.tuned = want;
    }

    /// Raise every chip select, guaranteeing a clean transaction boundary
    /// whatever state the previous caller left behind.
    ///
    /// ⚠️ This is load-bearing, and the reason is the *contract's* shape rather
    /// than this driver's: `PanelDriver` (the watch's `drivers/panel.rs`) has
    /// `begin_pixels` but **no `end_pixels`**. A conforming caller may therefore
    /// open a pixel stream and simply never close it, leaving LCD CS asserted
    /// indefinitely. Two things would then go wrong without this:
    ///
    ///   * a following command would be clocked into a still-open RAMWR stream
    ///     and land in GRAM as pixel data;
    ///   * a following *touch* read would assert TOUCH CS while LCD CS is still
    ///     low — two devices driving one MISO line, which is a bus contention
    ///     fault, not merely a wrong answer.
    ///
    /// Making every select start from all-deasserted costs two GPIO writes and
    /// removes both failure modes structurally, so no caller has to remember.
    fn deselect_all(&mut self) {
        self.lcd_cs.set_high();
        if let Some(cs) = self.touch_cs.as_mut() {
            cs.set_high();
        }
    }

    fn select_display(&mut self) {
        self.deselect_all();
        self.tune(Tuned::Display, board::SPI_DISPLAY_HZ);
        self.lcd_cs.set_low();
    }

    fn select_touch(&mut self) {
        self.deselect_all();
        self.tune(Tuned::Touch, board::SPI_TOUCH_HZ);
        // A board with no SPI touch never calls touch_read (its touch driver
        // is a different bus entirely); reaching here with None is a wiring
        // bug, and selecting nothing is the fail-safe.
        if let Some(cs) = self.touch_cs.as_mut() {
            cs.set_low();
        }
    }

    // -- command / small-data writes (QspiBus parity) ----------------------

    /// One self-contained command transaction: CS low → DC low → opcode →
    /// (DC high → payload) → CS high.
    fn cmd_write(&mut self, reg: u8, data: &[u8]) {
        self.select_display();
        self.dc.set_low();
        let _ = self.spi.write(&[reg]);
        if !data.is_empty() {
            self.dc.set_high();
            let _ = self.spi.write(data);
        }
        self.lcd_cs.set_high();
    }

    /// Command with no parameters.
    pub fn write_command(&mut self, reg: u8) {
        self.cmd_write(reg, &[]);
    }

    /// Command with one parameter byte.
    pub fn write_c8d8(&mut self, reg: u8, data: u8) {
        self.cmd_write(reg, &[data]);
    }

    /// Command with two big-endian u16 parameters — the CASET/RASET shape.
    /// Same name and signature as `QspiBus::write_c8d16d16`.
    pub fn write_c8d16d16(&mut self, reg: u8, d1: u16, d2: u16) {
        self.cmd_write(
            reg,
            &[(d1 >> 8) as u8, d1 as u8, (d2 >> 8) as u8, d2 as u8],
        );
    }

    /// Command with an arbitrary parameter run (init tables: porch control,
    /// gamma curves, ...). No `QspiBus` equivalent — the CO5300's init table is
    /// all single-byte parameters, the ST7789's is not.
    pub fn write_c8dn(&mut self, reg: u8, data: &[u8]) {
        self.cmd_write(reg, data);
    }

    /// Called by [`super::st7789::St7789Display::set_addr_window`] once the new
    /// CASET/RASET have been sent: the next pixel push must restart at the
    /// window origin with `RAMWR`, not continue the previous run.
    pub(crate) fn arm_ramwr(&mut self) {
        self.next_ramwr = CMD_RAMWR;
    }

    /// Emit the pending memory-write opcode with CS already low and DC set for
    /// commands, then leave DC high ready for pixel data. Flips the pending
    /// opcode to `RAMWR_CONT` so any following chunk resumes rather than
    /// rewinds.
    fn open_ramwr(&mut self) {
        self.dc.set_low();
        let cmd = self.next_ramwr;
        let _ = self.spi.write(&[cmd]);
        self.next_ramwr = CMD_RAMWR_CONT;
        self.dc.set_high();
    }

    // -- pixel paths (QspiBus parity) --------------------------------------

    /// Open a streamed pixel transaction, leaving CS LOW. Follow with
    /// [`stream_pixels`](Self::stream_pixels), close with
    /// [`end_pixels`](Self::end_pixels).
    pub fn begin_pixels(&mut self) {
        self.select_display();
        self.open_ramwr();
    }

    /// Stream a continuation chunk of an already-open pixel transaction.
    ///
    /// RGB565 goes big-endian on the wire, identical to
    /// `qspi_bus::byteswap_into`.
    pub fn stream_pixels(&mut self, pixels: &[u16]) {
        if pixels.is_empty() {
            return;
        }
        let max_px = STAGE_BYTES / 2;
        let mut remaining = pixels;
        while !remaining.is_empty() {
            let n = remaining.len().min(max_px);
            for (i, &px) in remaining[..n].iter().enumerate() {
                self.stage[i * 2] = (px >> 8) as u8;
                self.stage[i * 2 + 1] = px as u8;
            }
            let _ = self.spi.write(&self.stage[..n * 2]);
            remaining = &remaining[n..];
        }
    }

    // NOTE: a raw `&[u8]` pass-through used to live here, for the period when
    // the contract declared `push_pixels(&[u8])`. It was cheaper (no byteswap,
    // no staging copy) but it is deliberately gone: the contract was resolved
    // to `&[u16]` on 2026-08-24 precisely so that byte order stays the driver's
    // secret. Re-adding a public byte path would re-open the leak, because a
    // caller holding bytes has necessarily already made a byte-order decision
    // that belongs here. If an internal fast path ever needs one, keep it
    // private.

    /// Close a streamed pixel transaction: raise CS.
    ///
    /// Not part of the `PanelDriver` contract (which has `begin_pixels` and no
    /// close). Calling it is still correct and slightly tidier; omitting it is
    /// safe because every subsequent bus operation re-establishes a clean
    /// transaction boundary — see [`deselect_all`](Self::deselect_all).
    pub fn end_pixels(&mut self) {
        self.lcd_cs.set_high();
    }

    /// Push a run of RGB565 pixels as one complete transaction.
    ///
    /// ★ THE HOT PATH. The watch's `TwoLineFlusher::flush_span` calls exactly
    /// this, once per (row-pair, span), with `w * 2` pixels — 640 px / 1280 B
    /// for a full-width strip, which fits [`STAGE_BYTES`] in a single staged
    /// copy and a single DMA.
    pub fn write_pixels(&mut self, pixels: &[u16]) {
        if pixels.is_empty() {
            return;
        }
        self.select_display();
        self.open_ramwr();
        self.stream_pixels(pixels);
        self.lcd_cs.set_high();
    }

    /// Fill `count` pixels with a single colour.
    pub fn write_repeat(&mut self, color: u16, count: u32) {
        if count == 0 {
            return;
        }
        self.select_display();
        self.open_ramwr();

        let hi = (color >> 8) as u8;
        let lo = color as u8;
        let max_px = STAGE_BYTES / 2;
        let n_first = (count as usize).min(max_px);
        for i in 0..n_first {
            self.stage[i * 2] = hi;
            self.stage[i * 2 + 1] = lo;
        }

        // The staging buffer holds one full chunk of the (constant) colour, so
        // every subsequent chunk re-sends the same bytes with no restaging.
        let mut remaining = count;
        while remaining > 0 {
            let n = remaining.min(max_px as u32) as usize;
            let _ = self.spi.write(&self.stage[..n * 2]);
            remaining -= n as u32;
        }
        self.lcd_cs.set_high();
    }

    // -- touch path ---------------------------------------------------------

    /// Run one XPT2046 conversion: full-duplex `[cmd, 0, 0]` out, three bytes
    /// in, at the touch clock rate with the touch CS asserted.
    ///
    /// Returns the raw 12-bit result. The XPT2046 clocks its answer out one bit
    /// after the command's last bit, so the 12 significant bits sit in
    /// `rx[1]:rx[2]` left-aligned — hence the `>> 3`.
    pub fn touch_read(&mut self, cmd: u8) -> u16 {
        let mut rx = [0u8; 3];
        let tx = [cmd, 0x00, 0x00];
        self.select_touch();
        let _ = self.spi.transfer(&mut rx, &tx);
        if let Some(cs) = self.touch_cs.as_mut() {
            cs.set_high();
        }
        (((rx[1] as u16) << 8) | rx[2] as u16) >> 3
    }
}
