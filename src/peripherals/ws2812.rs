//! WS2812 status pixel over RMT (`has-ws2812`) — the GUI flavor's port of the
//! fleet's peer-state light (#491, parity with `rust/clock/src/led.rs`).
//!
//! Semantics match the fleet ladder the README documents: **off** = mesh not
//! running, **blink** = mesh up with no peers, **solid** = at least one peer.
//! The fleet proved on glass that a plain GPIO level is invisible to a WS2812
//! (#398), so this drives the pixel with real 800 kHz frames: RMT at 80 MHz,
//! divider 1 → 12.5 ns/tick.
//!
//! WS2812s LATCH — the pixel holds its color until told otherwise — so a frame
//! goes on the wire only when the logical state CHANGES, never per tick. A
//! failed RMT setup or transmit degrades to a dark LED, deliberately: a status
//! light must never be able to brick the node.

use esp_hal::gpio::Level;
use esp_hal::rmt::{Channel, PulseCode, Tx};
use esp_hal::Blocking;

/// 24 bit pulses + the >=50 µs latch + the end marker.
const FRAME_LEN: usize = 26;

/// Lit color, deliberately dim (status light, not a flashlight). GRB order,
/// green ~8% — same value the fleet ships, so both flavors look identical on
/// the same desk.
const LIT_GRB: [u8; 3] = [0x14, 0x00, 0x00];

/// Blink half-period. 500 ms on / 500 ms off mirrors the fleet's cadence.
const BLINK_HALF_MS: u64 = 500;

fn frame(grb: [u8; 3]) -> [PulseCode; FRAME_LEN] {
    // WS2812B datasheet timings at 12.5 ns/tick:
    //   0-bit: 0.40 µs high + 0.85 µs low -> 32 + 68 ticks
    //   1-bit: 0.80 µs high + 0.45 µs low -> 64 + 36 ticks
    let bit0 = PulseCode::new(Level::High, 32, Level::Low, 68);
    let bit1 = PulseCode::new(Level::High, 64, Level::Low, 36);
    let mut f = [PulseCode::end_marker(); FRAME_LEN];
    let mut i = 0;
    for byte in grb {
        for bit in (0..8).rev() {
            f[i] = if (byte >> bit) & 1 == 1 { bit1 } else { bit0 };
            i += 1;
        }
    }
    // >= 50 µs latch. Both halves non-zero so it is not read as an end marker.
    f[24] = PulseCode::new(Level::Low, 2000, Level::Low, 2000);
    // f[25] stays the end marker.
    f
}

/// Logical peer-state ladder, computed by the caller from mesh facts.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum LedState {
    /// Mesh not running (or the board wants the light dark).
    Off,
    /// Mesh up, zero peers — searching.
    Blink,
    /// At least one live peer.
    Solid,
}

pub struct Ws2812 {
    /// `None` while a frame's `transmit()` owns it, and `None` for good if
    /// RMT setup failed at boot (dark LED, node unaffected).
    channel: Option<Channel<'static, Blocking, Tx>>,
    /// Last pixel state actually transmitted (the pixel latches).
    last_lit: Option<bool>,
}

impl Ws2812 {
    /// Wrap a configured RMT TX channel already bound to the board's
    /// `WS2812_GPIO`. `None` (setup failed) is a legal dark LED, not an error.
    pub fn new(channel: Option<Channel<'static, Blocking, Tx>>) -> Self {
        Self {
            channel,
            last_lit: None,
        }
    }

    /// One WS2812 frame over RMT, only on CHANGE. The blocking `wait()` is
    /// ~80 µs of wire time on the rare change tick. Any error hands the
    /// channel back and drops the frame; the next change retries.
    fn set_lit(&mut self, lit: bool) {
        if self.last_lit == Some(lit) {
            return;
        }
        let Some(channel) = self.channel.take() else {
            return;
        };
        let grb = if lit { LIT_GRB } else { [0, 0, 0] };
        let f = frame(grb);
        match channel.transmit(&f) {
            Ok(txn) => match txn.wait() {
                Ok(ch) => {
                    self.channel = Some(ch);
                    self.last_lit = Some(lit);
                }
                Err((_, ch)) => self.channel = Some(ch),
            },
            Err((_, ch)) => self.channel = Some(ch),
        }
    }

    /// Drive the pixel for `state` at monotonic `now_ms`. Cheap enough to
    /// call every main-loop tick: it no-ops unless the blink phase or the
    /// state actually changed the lit flag.
    pub fn service(&mut self, state: LedState, now_ms: u64) {
        let lit = match state {
            LedState::Off => false,
            LedState::Solid => true,
            LedState::Blink => (now_ms / BLINK_HALF_MS) % 2 == 0,
        };
        self.set_lit(lit);
    }
}
