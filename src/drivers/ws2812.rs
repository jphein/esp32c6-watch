//! WS2812 status LED over RMT — SPIKE for smol#486 (scope unproven peripherals)
//! and #491 (GUI-wide LED driver).
//!
//! # What this is and is not
//!
//! It is a **bit encoder**, not a device driver. It turns one GRB pixel into the
//! `PulseCode` sequence the RMT peripheral clocks out, and nothing else. RMT setup
//! and the transmit live at the call site in `main()`, deliberately: esp-hal's
//! channel types carry several parameters that are inferred cleanly there and would
//! have to be spelled out (or boxed) to cross a module boundary. A spike should not
//! pay that cost to look more like a driver than it is.
//!
//! # Timing, derived rather than remembered
//!
//! The C5's RMT default clock source is **PLL 80 MHz** — cited, not assumed:
//! `esp-metadata-generated-0.4.0/src/_generated_esp32c5.rs:733` reads
//! `_for_each_inner_rmt_clock_source!((default(Pll80MHz)))`, and the C5's cfg list
//! carries `rmt_supports_pll80mhz_clock` (there is **no** `rmt_supports_apb_clock`
//! on this chip, so an APB-derived figure copied from another board would be
//! wrong).
//!
//! So `Rmt::new(peripherals.RMT, Rate::from_mhz(80))` divides by 1 — the C5 uses
//! the dividing `validate_clock` (`rmt.rs:2340`, gated `not(any(esp32, esp32s2))`),
//! not the exact-match variant — and a channel `clk_divider` of **8** gives
//! 80 MHz / 8 = 10 MHz, i.e. **100 ns per tick**. Every constant below is that tick
//! count, and the arithmetic is shown so a future reader can re-derive it instead of
//! trusting it:
//!
//! | phase | WS2812B target | ticks | actual | error | tolerance |
//! |-------|----------------|-------|--------|-------|-----------|
//! | T0H   | 400 ns         | 4     | 400 ns | 0     | ±150 ns   |
//! | T0L   | 850 ns         | 9     | 900 ns | +50   | ±150 ns   |
//! | T1H   | 800 ns         | 8     | 800 ns | 0     | ±150 ns   |
//! | T1L   | 450 ns         | 4     | 400 ns | −50   | ±150 ns   |
//!
//! Bit periods land at 1300 ns (zero) and 1200 ns (one) against a 1250 ns nominal
//! with ±600 ns of slack, so both are comfortably in spec.
//!
//! ⚠️ **If the channel divider changes, every constant here is wrong.** They are
//! tick counts, not durations, and nothing in the type system ties them to the
//! divider chosen at the call site. [`TICK_NS`] exists so the relationship is at
//! least written down, and the call site asserts against it.
//!
//! # What a dark LED does and does not prove
//!
//! This is unproven hardware. The pin is vendor-cited (`WS2812 GPIO 27`, vesper's
//! recon pin table, 2026-08-25) but **the LED's presence and count are not** — a
//! net can be routed with nothing populated, exactly the open question the BOOT-pin
//! note on this board records. So:
//!
//! * LED lights in the expected colours → the peripheral, the pin, the timing and
//!   the colour order are ALL confirmed at once.
//! * LED stays dark → **ambiguous**, and deliberately logged rather than guessed:
//!   no LED fitted, wrong pin, timing out of spec, or wrong channel. The probe
//!   prints what it sent so a dark LED can be told from firmware that never ran —
//!   silence and failure must not look alike.
//! * LED lights the WRONG colour → the wire order is wrong, not the timing.
//!   That is why the probe walks R, G and B **separately** instead of showing white:
//!   white is the one colour that cannot reveal a channel swap.

use esp_hal::gpio::Level;
use esp_hal::rmt::PulseCode;

/// Nanoseconds per RMT tick that the constants below assume. The call site must
/// configure the channel to match — see the module docs for the derivation.
pub const TICK_NS: u32 = 100;

/// RMT channel divider that yields [`TICK_NS`] from the C5's 80 MHz PLL source.
pub const CLK_DIVIDER: u8 = 8;

/// Source clock to request from `Rmt::new`, in MHz. The C5's default source is
/// PLL 80 MHz, so requesting 80 divides by 1.
pub const SRC_MHZ: u32 = 80;

// Bit phases, in ticks. See the table in the module docs for the error budget.
const T0H: u16 = 4;
const T0L: u16 = 9;
const T1H: u16 = 8;
const T1L: u16 = 4;

/// A pixel is 24 bits, plus the end marker RMT needs to stop cleanly.
pub const CODES_PER_PIXEL: usize = 25;

/// Encode one pixel into RMT pulse codes.
///
/// **Order is GRB, not RGB** — that is the WS2812's wire order, and taking `g`
/// first makes the call site state it rather than encode it silently. Most
/// confusion with these parts is a channel swap, so the signature is the place to
/// be loud about it.
///
/// Each colour byte goes out **MSB first**.
pub fn encode_grb(g: u8, r: u8, b: u8) -> [PulseCode; CODES_PER_PIXEL] {
    let mut out = [PulseCode::end_marker(); CODES_PER_PIXEL];
    let mut i = 0;
    for byte in [g, r, b] {
        for bit in (0..8).rev() {
            out[i] = if (byte >> bit) & 1 == 1 {
                PulseCode::new(Level::High, T1H, Level::Low, T1L)
            } else {
                PulseCode::new(Level::High, T0H, Level::Low, T0L)
            };
            i += 1;
        }
    }
    // out[24] stays the end marker from the initialiser above. Leaving it implicit
    // would be a landmine: without it RMT keeps driving the line and the next
    // transmit starts inside an unterminated frame.
    out
}

/// The spike's probe sequence: one dim colour per step, then off.
///
/// Dim on purpose (32/255). These parts are startlingly bright at full scale, this
/// one sits next to a screen JP looks at, and a spike should not be the reason
/// someone squints. Brightness proves nothing that visibility does not.
///
/// **Separate channels, never white** — see the module docs: white is the only
/// colour that cannot reveal a wire-order swap.
pub const PROBE: [(&str, u8, u8, u8); 4] = [
    // (label, g, r, b)
    ("RED", 0, 32, 0),
    ("GREEN", 32, 0, 0),
    ("BLUE", 0, 0, 32),
    ("OFF", 0, 0, 0),
];
