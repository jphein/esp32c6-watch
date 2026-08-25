//! XPT2046 resistive touch controller, sharing the ST7789's SPI bus.
//!
//! # Port notes vs the watch's FT3168
//!
//! The watch's `peripherals::touch::Ft3168Touch` talks to a **capacitive**
//! controller over I2C that reports finger count and already-mapped coordinates.
//! The XPT2046 is a **resistive** 4-wire ADC: it reports raw 12-bit voltages and
//! nothing else. Everything the FT3168 did in silicon has to happen here in
//! software — debounce, pressure gating, axis calibration, orientation.
//!
//! What is kept identical on purpose, so app-level code ports unchanged:
//!   * [`TouchPoint`] has the same `{ x, y, fingers }` fields as `touch.rs:18`
//!     (`fingers` is 0 or 1 — this panel is single-touch by construction);
//!   * [`SwipeEvent`] / [`SwipeDirection`] mirror `touch.rs:39-54`;
//!   * [`Xpt2046::poll`] returns `(Option<TouchPoint>, Option<SwipeEvent>)` with
//!     the same lift-off semantics, including the watch's dominant-axis
//!     classification and its reasoning about the old 1.5x dead-zone.
//!
//! Three things are genuinely new:
//!   1. **No interrupt line.** `connections.md:18-20`'s touch table leaves the
//!      IRQ column empty — PENIRQ is not routed on this board. Touch is
//!      poll-only; there is no wake-on-touch path to build on later.
//!   2. **Pressure is the presence test.** With no finger-count register, "is
//!      something touching" is a Z-axis measurement compared against
//!      [`crate::board::TOUCH_PRESSURE_THRESHOLD`].
//!   3. **The bus is shared.** Every read borrows the display's
//!      [`SharedSpiBus`], which re-tunes the SPI clock from 20 MHz to 2 MHz and
//!      back around each conversion (see `spi_bus.rs`). Touch reads therefore
//!      cannot be interleaved *inside* a pixel transaction — poll between
//!      frames, not between strips.

use esp_hal::gpio::Input;

use crate::board;
use crate::drivers::spi_bus::SharedSpiBus;
use crate::drivers::Rotation;

// ---------------------------------------------------------------------------
// Control bytes
// ---------------------------------------------------------------------------
// Layout: S(1) A2 A1 A0 MODE SER/DFR PD1 PD0
//   S    = 1        start bit
//   A    = channel  (X=101, Y=001, Z1=011, Z2=100)
//   MODE = 0        12-bit conversion
//   /DFR = 0        differential reference (ratiometric — the accurate mode)
//   PD   = 00       power down between conversions
// These are the same four values TFT_eSPI's XPT2046 path uses.

/// X position, differential, 12-bit.
const CMD_READ_X: u8 = 0xD0;
/// Y position, differential, 12-bit.
const CMD_READ_Y: u8 = 0x90;
/// Z1 (touch pressure, first half of the bridge).
const CMD_READ_Z1: u8 = 0xB0;
/// Z2 (touch pressure, second half of the bridge).
const CMD_READ_Z2: u8 = 0xC0;

/// Samples per axis per read. Odd so the median is a real sample; 5 is the
/// smallest N that survives a single-sample spike (the dominant noise mode on a
/// resistive panel sharing a bus with a 20 MHz display clock) while costing only
/// ~10 conversions per poll.
const MEDIAN_N: usize = 5;

/// Consecutive below-threshold reads required before declaring a lift.
///
/// A capacitive controller reports `fingers == 0` cleanly; a resistive bridge
/// chatters as contact pressure drops through the threshold. Without this, a
/// single dropout mid-drag would end the gesture and emit a spurious swipe.
const RELEASE_DEBOUNCE: u8 = 2;

/// Minimum dominant-axis travel (in logical pixels) for a lift-off to count as
/// a swipe rather than a tap.
///
/// The watch uses 36 on a 410 px-wide panel — "a hair above the old 30 px tap
/// cutoff", ~9 % of the width. 32 is the same fraction of this panel's 320 px.
/// The classification rule below is otherwise the watch's verbatim, including
/// its deliberate rejection of the "dominant axis must beat the other by 1.5x"
/// rule that silently swallowed slightly-diagonal swipes.
const SWIPE_MIN: u32 = 32;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// One raw, uncalibrated conversion set straight off the ADC.
///
/// This is what milestone (c)'s console dump prints, and what the four corner
/// touches must be read from to replace [`crate::board::TOUCH_CAL`]'s
/// placeholder bounds.
#[derive(Debug, Clone, Copy)]
pub struct RawSample {
    /// Raw 12-bit X (0..=4095), median-filtered.
    pub x: u16,
    /// Raw 12-bit Y (0..=4095), median-filtered.
    pub y: u16,
    /// Pressure proxy, `z1 + 4095 - z2`. Higher = firmer press.
    pub z: u16,
}

/// A calibrated, screen-space touch.
///
/// Field-compatible with the watch's `peripherals::touch::TouchPoint`
/// (`touch.rs:18`) so consumers move across unchanged.
#[derive(Debug, Clone, Copy)]
pub struct TouchPoint {
    pub x: u16,
    pub y: u16,
    /// Always 1 when present — the XPT2046 cannot distinguish multiple contacts
    /// (a second finger just moves the resistive centroid). Kept for source
    /// compatibility with the FT3168's finger count.
    pub fingers: u8,
}

/// Mirrors `peripherals::touch::SwipeDirection` (`touch.rs:48-54`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwipeDirection {
    Up,
    Down,
    Left,
    Right,
    Tap,
}

/// Mirrors `peripherals::touch::SwipeEvent` (`touch.rs:39-46`).
#[derive(Debug, Clone, Copy)]
pub struct SwipeEvent {
    pub direction: SwipeDirection,
    pub start_x: u16,
    pub start_y: u16,
    pub end_x: u16,
    pub end_y: u16,
}

/// Maps the ADC's raw span onto the panel.
///
/// Which of the XPT2046's two ADC channels is meant.
///
/// The chip's own channel names (`0xD0` = "X", `0x90` = "Y") say nothing about
/// how the film is oriented on any particular panel, so the mapping from channel
/// to panel axis has to be stated per board.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RawAxis {
    /// Channel `0xD0`, reported as [`RawSample::x`].
    X,
    /// Channel `0x90`, reported as [`RawSample::y`].
    Y,
}

/// Maps the digitizer's raw ADC readings onto the panel's **mechanical** frame.
///
/// # The frame this is expressed in — and why that used to be the bug
///
/// This describes raw → *panel-fixed* coordinates: the **short** axis (240 px of
/// source/column lines) and the **long** axis (320 px of gate/row lines) of the
/// bare die. It is a **wiring fact and nothing else** — it does not change when
/// the display is rotated, and it mentions no screen direction at all.
/// [`Xpt2046::map`] then applies the rotation separately, deriving it from the
/// same MADCTL semantics the display driver uses.
///
/// That separation is the whole point of this type's shape. The previous version
/// carried a `swap_xy` flag plus two `invert_raw_*` flags whose documented frame
/// was "the default rotation" while the arithmetic actually treated them as
/// relative to the non-inverted base — a discrepancy that turned a single axis
/// flip into a two-scenario puzzle during on-glass calibration (2026-08-24), and
/// which also hid a real bug: `swap_xy` was silently doing the *rotation's*
/// transpose, so `set_rotation(Portrait)` produced garbage because nothing else
/// supplied one. Splitting "how is the film wired" from "how is the panel
/// rotated" fixes both, because each question now has exactly one home.
///
/// # Fields
///
/// `x_min`/`x_max`/`y_min`/`y_max` are the raw spans of the chip's own channels
/// (never 0..4095 — the film's active area is inset from the ADC rails).
/// `short_axis` says which channel runs along the die's 240 px axis. The two
/// inverts fix the direction of each panel axis, and — note — they are named for
/// the **panel** axis they affect, not the ADC channel they came from, so
/// "mirror the short axis" is one flag and reads as what it does.
///
/// The board's values live in [`crate::board::TOUCH_CAL`].
#[derive(Debug, Clone, Copy)]
pub struct Calibration {
    /// Raw span of channel X (`0xD0`).
    pub x_min: u16,
    /// Raw span of channel X (`0xD0`).
    pub x_max: u16,
    /// Raw span of channel Y (`0x90`).
    pub y_min: u16,
    /// Raw span of channel Y (`0x90`).
    pub y_max: u16,
    /// Which raw channel runs along the panel's **short** (240 px) axis. The
    /// other channel necessarily runs along the long (320 px) axis.
    pub short_axis: RawAxis,
    /// Mirror the panel's short (240 px) axis.
    pub invert_short: bool,
    /// Mirror the panel's long (320 px) axis.
    pub invert_long: bool,
}

pub struct Xpt2046<'d> {
    cal: Calibration,
    /// Optional PENIRQ input (GPIO3). Active LOW: the XPT2046 pulls it down
    /// while the panel is being pressed, and a 10k pull-up holds it high
    /// otherwise.
    ///
    /// ✅ The pin is **confirmed wired** (on glass 2026-08-24: LOW during every
    /// pressed sample, HIGH idle), so the original "is it even connected"
    /// caveat is retired — see [`with_irq`](Self::with_irq).
    ///
    /// ⚠️ Still `None` by default, but now for an **ownership** reason rather
    /// than a doubt: GPIO3 is one physical wire and esp-hal's pin singleton
    /// admits one owner. In the watch firmware integration the *firmware* keeps
    /// it, as its `touch_int` feeding `wait_for_falling_edge` — which sleeps the
    /// executor instead of spinning, so it is a strictly better use of the same
    /// PENIRQ signal than this driver's poll-time fast path. That integration
    /// therefore constructs `Xpt2046` **without** `with_irq`, and pays two ADC
    /// conversions per idle poll for it.
    ///
    /// Enable it for **standalone** use (the smoke test does) or if a future
    /// firmware hands the pin back. Both arrangements are correct; what would be
    /// wrong is two owners.
    irq: Option<Input<'d>>,
    /// The display rotation this driver maps into. Keep in step with
    /// [`crate::drivers::st7789::St7789Display::set_rotation`] or touches land
    /// transposed.
    rotation: Rotation,
    threshold: u16,
    /// DIAGNOSTIC (feature `touch-telemetry`). A zero-sized no-op when the
    /// feature is off, so every call site below stays cfg-free — see
    /// [`TouchTelemetry`].
    tel: TouchTelemetry,
    // Swipe tracking — same state the watch's Ft3168Touch carries.
    tracking: bool,
    releasing: u8,
    start_x: u16,
    start_y: u16,
    last_x: u16,
    last_y: u16,
}

impl<'d> Xpt2046<'d> {
    pub fn new(cal: Calibration, rotation: Rotation) -> Self {
        Self {
            cal,
            irq: None,
            rotation,
            threshold: board::TOUCH_PRESSURE_THRESHOLD,
            tel: TouchTelemetry::new(),
            tracking: false,
            releasing: 0,
            start_x: 0,
            start_y: 0,
            last_x: 0,
            last_y: 0,
        }
    }

    /// Override the pressure gate (builder style). Raise it if bus noise
    /// registers as phantom taps; lower it if light presses are dropped.
    pub fn with_threshold(mut self, threshold: u16) -> Self {
        self.threshold = threshold;
        self
    }

    /// Enable the PENIRQ fast path on GPIO3.
    ///
    /// When set, an idle poll costs **zero** SPI conversions instead of two: a
    /// high IRQ line means nothing is touching, so [`read_raw`](Self::read_raw)
    /// returns immediately without taking the shared bus away from the display
    /// at all. On a bus this contended that is the difference between touch
    /// polling being free and touch polling costing display bandwidth.
    ///
    /// ✅ **CONFIRMED ON HARDWARE 2026-08-24.** The schematic was right and every
    /// vendor software config (`-DCYD28_TouchR_IRQ=-1`) was merely a choice not
    /// to use it: `smoke.rs` stage 6 observed GPIO3 reading LOW during every
    /// single `PRESSED` sample and HIGH whenever idle. Enabling this is now the
    /// recommended default rather than an experiment.
    ///
    /// Pass the pin configured as an input with a pull-up.
    pub fn with_irq(mut self, irq: Input<'d>) -> Self {
        self.irq = Some(irq);
        self
    }

    /// PENIRQ state, for diagnostics: `Some(true)` = line asserted (a contact is
    /// present), `Some(false)` = idle, `None` = no IRQ pin configured.
    ///
    /// Active LOW at the pin; this inverts so callers read it as "pressed".
    pub fn irq_active(&self) -> Option<bool> {
        self.irq.as_ref().map(|p| p.is_low())
    }

    /// True when the IRQ fast path says "definitely nothing pressed".
    ///
    /// Returns `false` when no IRQ pin is configured, so the caller falls
    /// through to the ADC path — the safe direction: a missing or miswired IRQ
    /// costs performance, never correctness.
    fn irq_says_idle(&self) -> bool {
        match self.irq.as_ref() {
            Some(pin) => pin.is_high(),
            None => false,
        }
    }

    /// Replace the calibration at runtime — the endpoint an on-glass
    /// calibration routine would write to.
    pub fn set_calibration(&mut self, cal: Calibration) {
        self.cal = cal;
    }

    pub fn calibration(&self) -> Calibration {
        self.cal
    }

    /// Keep in step with the display's orientation.
    ///
    /// All four rotations are now correct. Before the 2026-08-25 split of the
    /// rotation transform out of [`Calibration`], only the two landscape
    /// orientations worked: the transpose lived in the calibration's old
    /// `swap_xy` flag, so a portrait rotation had nothing to supply one and
    /// returned transposed coordinates scaled by the wrong extents.
    pub fn set_rotation(&mut self, rotation: Rotation) {
        self.rotation = rotation;
    }

    /// The rotation currently in effect.
    pub fn rotation_of_record(&self) -> Rotation {
        self.rotation
    }

    // -- raw acquisition ----------------------------------------------------

    /// Median of [`MEDIAN_N`] conversions on one channel.
    fn median(bus: &mut SharedSpiBus, cmd: u8) -> u16 {
        let mut s = [0u16; MEDIAN_N];
        for slot in s.iter_mut() {
            *slot = bus.touch_read(cmd);
        }
        // Insertion sort — N is 5, so this beats anything cleverer and needs no
        // allocation or comparator plumbing.
        for i in 1..MEDIAN_N {
            let v = s[i];
            let mut j = i;
            while j > 0 && s[j - 1] > v {
                s[j] = s[j - 1];
                j -= 1;
            }
            s[j] = v;
        }
        s[MEDIAN_N / 2]
    }

    /// One debounced raw sample, or `None` if nothing is pressing hard enough.
    ///
    /// The Z reads come first and gate the (more expensive) X/Y median passes,
    /// so an idle poll costs 2 conversions rather than 12.
    pub fn read_raw(&mut self, bus: &mut SharedSpiBus) -> Option<RawSample> {
        // Free rejection when PENIRQ is wired and enabled — no bus traffic at
        // all. Falls through when it is not configured (see `irq_says_idle`).
        if self.irq_says_idle() {
            return None;
        }
        let z1 = bus.touch_read(CMD_READ_Z1);
        let z2 = bus.touch_read(CMD_READ_Z2);

        // z1 == 0 means the bridge is open: nothing is touching, and the
        // `z1 + 4095 - z2` form would otherwise read as maximum pressure when
        // z2 is also 0. Check it explicitly rather than relying on the
        // threshold to catch it.
        if z1 == 0 {
            self.tel.note_open();
            return None;
        }
        let z = (z1 as u32 + 4095 - z2 as u32).min(u16::MAX as u32) as u16;
        if z < self.threshold {
            // The truncation signal: a below-gate read WHILE tracking is what
            // ends a gesture early and turns a swipe into a tap.
            self.tel.note_reject(z);
            return None;
        }
        self.tel.note_accept(z);

        Some(RawSample {
            x: Self::median(bus, CMD_READ_X),
            y: Self::median(bus, CMD_READ_Y),
            z,
        })
    }

    /// Raw sample with the pressure gate bypassed — for the calibration dump.
    ///
    /// Milestone (c) prints this so the raw span is visible even when the
    /// placeholder threshold is wrong, which is exactly the situation where a
    /// gated read would show nothing and look like a wiring fault.
    pub fn read_raw_ungated(&mut self, bus: &mut SharedSpiBus) -> RawSample {
        let z1 = bus.touch_read(CMD_READ_Z1);
        let z2 = bus.touch_read(CMD_READ_Z2);
        let z = (z1 as u32 + 4095 - z2 as u32).min(u16::MAX as u32) as u16;
        RawSample {
            x: Self::median(bus, CMD_READ_X),
            y: Self::median(bus, CMD_READ_Y),
            z: if z1 == 0 { 0 } else { z },
        }
    }

    // -- mapping ------------------------------------------------------------

    /// Normalise a raw reading against its calibrated span to 0..=4095.
    fn norm(v: u16, lo: u16, hi: u16) -> u32 {
        if hi <= lo {
            return 0;
        }
        let v = v.clamp(lo, hi) as u32;
        ((v - lo as u32) * 4095) / ((hi - lo) as u32)
    }

    /// Apply calibration + orientation to a raw sample.
    ///
    /// Two clearly separated stages, which is the whole design:
    ///
    /// 1. **Calibration** — raw ADC → the panel's *mechanical* frame `(s, g)`,
    ///    where `s` runs along the die's 240 px short axis and `g` along its
    ///    320 px long axis. Pure wiring; rotation-independent.
    /// 2. **Rotation** — `(s, g)` → logical screen coordinates, derived from the
    ///    same MADCTL semantics the display driver programs, so touch and pixels
    ///    cannot disagree about what a rotation means.
    ///
    /// # The rotation transforms, derived not guessed
    ///
    /// ST7789 MADCTL acts on GRAM addressing: `MV` (0x20) exchanges column/row,
    /// `MX` (0x40) mirrors the column address, `MY` (0x80) mirrors the row
    /// address. Logical `(x, y)` *is* `(col_addr, row_addr)`. Solving each of the
    /// four rotations for a physical point at `(s, g)`:
    ///
    /// | rotation | MADCTL | logical (x, y) | size |
    /// |---|---|---|---|
    /// | `Portrait` | 0x00 | `(s, g)` | 240x320 |
    /// | `Landscape` | 0x60 `MV\|MX` | `(MAX-g, s)` | 320x240 |
    /// | `PortraitInverted` | 0xC0 `MY\|MX` | `(MAX-s, MAX-g)` | 240x320 |
    /// | `LandscapeInverted` | 0xA0 `MV\|MY` | `(g, MAX-s)` | 320x240 |
    ///
    /// ★ Regression vectors (measured on this unit; see [`crate::board::TOUCH_CAL`]).
    /// The anchored press — a known physical top-right — must satisfy:
    ///
    /// ```text
    /// raw(3272, 459) @ LandscapeInverted -> (300,  29)   <- the anchor
    /// raw(3272, 459) @ Landscape         -> ( 18, 209)   180 deg away
    /// raw(3272, 459) @ Portrait          -> (209, 300)   in 240x320
    /// raw(3272, 459) @ PortraitInverted  -> ( 29,  18)   180 deg from Portrait
    /// ```
    ///
    /// The first line is byte-identical to what the pre-refactor code produced,
    /// so the shipped orientation's behaviour is provably unchanged; the other
    /// three are the ones that used to come out as garbage. `smoke.rs` asserts
    /// all four at boot — this crate cannot host-test them (it depends on
    /// `esp-hal`, which does not build for the host), so the firmware carries
    /// its own regression suite instead.
    pub fn map(&self, raw: &RawSample) -> TouchPoint {
        const MAX: u32 = 4095;

        // -- stage 1: raw -> panel mechanical frame (rotation-independent) ----
        let rx = Self::norm(raw.x, self.cal.x_min, self.cal.x_max);
        let ry = Self::norm(raw.y, self.cal.y_min, self.cal.y_max);
        let (mut s, mut g) = match self.cal.short_axis {
            RawAxis::X => (rx, ry),
            RawAxis::Y => (ry, rx),
        };
        if self.cal.invert_short {
            s = MAX - s;
        }
        if self.cal.invert_long {
            g = MAX - g;
        }

        // -- stage 2: panel frame -> logical screen frame (see the table) -----
        let (xn, yn) = match self.rotation {
            Rotation::Portrait => (s, g),
            Rotation::Landscape => (MAX - g, s),
            Rotation::PortraitInverted => (MAX - s, MAX - g),
            Rotation::LandscapeInverted => (g, MAX - s),
        };

        let (w, h) = self.rotation.size();
        TouchPoint {
            x: (xn * (w as u32 - 1) / MAX) as u16,
            y: (yn * (h as u32 - 1) / MAX) as u16,
            fingers: 1,
        }
    }

    /// Current calibrated touch, or `None`.
    pub fn read(&mut self, bus: &mut SharedSpiBus) -> Option<TouchPoint> {
        self.read_raw(bus).map(|raw| self.map(&raw))
    }

    /// Poll for position and lift-off gestures.
    ///
    /// Semantics match `Ft3168Touch::poll` (`touch.rs:120`): a live touch
    /// returns `(Some(point), None)`; the poll on which the finger leaves
    /// returns `(None, Some(event))`; idle returns `(None, None)`.
    ///
    /// The one addition is [`RELEASE_DEBOUNCE`] — a resistive bridge chatters
    /// through the pressure threshold as contact eases, and without it a single
    /// mid-drag dropout would terminate the gesture and emit a swipe the user
    /// never finished.
    pub fn poll(&mut self, bus: &mut SharedSpiBus) -> (Option<TouchPoint>, Option<SwipeEvent>) {
        match self.read(bus) {
            Some(tp) => {
                self.releasing = 0;
                if !self.tracking {
                    self.tracking = true;
                    self.start_x = tp.x;
                    self.start_y = tp.y;
                }
                self.last_x = tp.x;
                self.last_y = tp.y;
                (Some(tp), None)
            }
            None => {
                if !self.tracking {
                    return (None, None);
                }
                self.releasing += 1;
                if self.releasing < RELEASE_DEBOUNCE {
                    // Not convinced yet — report the last known position so a
                    // drag through a noisy patch stays live.
                    return (
                        Some(TouchPoint {
                            x: self.last_x,
                            y: self.last_y,
                            fingers: 1,
                        }),
                        None,
                    );
                }

                self.tracking = false;
                self.releasing = 0;

                let dx = self.last_x as i32 - self.start_x as i32;
                let dy = self.last_y as i32 - self.start_y as i32;
                let abs_dx = dx.unsigned_abs();
                let abs_dy = dy.unsigned_abs();

                // Dominant-axis classification, verbatim from the watch: a
                // directional swipe once the larger axis travels SWIPE_MIN, and
                // the direction is simply that axis. No 1.5x dominance test —
                // see SWIPE_MIN's docs and touch.rs:145-160 for why that rule
                // was removed there.
                let direction = if abs_dx.max(abs_dy) < SWIPE_MIN {
                    SwipeDirection::Tap
                } else if abs_dx >= abs_dy {
                    if dx > 0 {
                        SwipeDirection::Right
                    } else {
                        SwipeDirection::Left
                    }
                } else if dy > 0 {
                    SwipeDirection::Down
                } else {
                    SwipeDirection::Up
                };

                // DIAGNOSTIC: one line per gesture, on lift. dx/dy are the arc
                // the classifier actually saw. Compare `dom` against
                // SWIPE_MIN_X/Y: a Tap with a large `rej` count and a
                // `rej_zmax` close to `thr` is a TRUNCATED swipe, not a tap the
                // user meant. Compiles to nothing without `touch-telemetry`.
                self.tel
                    .report(direction, self.threshold, dx, dy, abs_dx.max(abs_dy));

                (
                    None,
                    Some(SwipeEvent {
                        direction,
                        start_x: self.start_x,
                        start_y: self.start_y,
                        end_x: self.last_x,
                        end_y: self.last_y,
                    }),
                )
            }
        }
    }
}

// ===========================================================================
// Touch telemetry (feature `touch-telemetry`) — DIAGNOSTIC, not a product
// ===========================================================================
// WHY THIS EXISTS: on the CYD, most of JP's swipes register as taps. The
// leading hypothesis is SAMPLING STARVATION, not a mis-set pressure gate — a
// 61.4 ms full-frame paint can swallow a fast swipe whole, leaving the
// classifier too few samples to see an arc. The span-run flusher (83c8491) is
// the suspected fix, so this instrumentation has to survive INTO the image that
// carries the flusher; otherwise the fix lands and nothing can confirm it from
// ordinary use.
//
// It deliberately does NOT tune anything. `TOUCH_PRESSURE_THRESHOLD` (400) and
// `SWIPE_MIN_*` are untouched: this port's rule is measured-never-inherited, and
// a blind nudge to either would destroy the evidence this exists to collect.
//
// SHAPE: per-GESTURE accounting, one println on lift — not per poll. At 62 Hz a
// per-sample log would itself perturb the timing it is measuring, which is
// exactly the quantity under test.
//
// The feature-off arm is a zero-sized type with empty method bodies, so the call
// sites in `raw_sample`/`classify` need no `#[cfg]` and cannot drift apart. When
// the swipe question is ANSWERED, delete this block, its feature, and the four
// `self.tel` calls — the compiler will name every one of them.

/// Per-gesture touch accounting. Diagnostic build only.
#[cfg(feature = "touch-telemetry")]
struct TouchTelemetry {
    /// Reads that passed the pressure gate and became samples.
    samples: u32,
    /// Reads REJECTED by the gate — the truncation signal when tracking.
    rejects: u32,
    /// Reads where the bridge was open (z1 == 0): genuinely no contact.
    open: u32,
    zmin: u16,
    zmax: u16,
    /// Highest pressure that still lost to the gate. Close to `thr` means the
    /// gate is the thing ending gestures early.
    rej_zmax: u16,
}

#[cfg(feature = "touch-telemetry")]
impl TouchTelemetry {
    const fn new() -> Self {
        Self {
            samples: 0,
            rejects: 0,
            open: 0,
            zmin: u16::MAX,
            zmax: 0,
            rej_zmax: 0,
        }
    }

    fn note_open(&mut self) {
        self.open += 1;
    }

    fn note_reject(&mut self, z: u16) {
        self.rejects += 1;
        if z > self.rej_zmax {
            self.rej_zmax = z;
        }
    }

    fn note_accept(&mut self, z: u16) {
        self.samples += 1;
        if z < self.zmin {
            self.zmin = z;
        }
        if z > self.zmax {
            self.zmax = z;
        }
    }

    fn report(&mut self, direction: SwipeDirection, thr: u16, dx: i32, dy: i32, dom: u32) {
        esp_println::println!(
            "[TOUCH-DBG] {:?} n={} rej={} open={} z={}..{} rej_zmax={} thr={} dx={} dy={} dom={}",
            direction,
            self.samples,
            self.rejects,
            self.open,
            if self.zmin == u16::MAX { 0 } else { self.zmin },
            self.zmax,
            self.rej_zmax,
            thr,
            dx,
            dy,
            dom,
        );
        *self = Self::new();
    }
}

/// Zero-sized stand-in when `touch-telemetry` is off. Every method is empty, so
/// the instrumented call sites compile away entirely.
#[cfg(not(feature = "touch-telemetry"))]
struct TouchTelemetry;

#[cfg(not(feature = "touch-telemetry"))]
impl TouchTelemetry {
    const fn new() -> Self {
        Self
    }
    #[inline(always)]
    fn note_open(&mut self) {}
    #[inline(always)]
    fn note_reject(&mut self, _z: u16) {}
    #[inline(always)]
    fn note_accept(&mut self, _z: u16) {}
    #[inline(always)]
    fn report(&mut self, _d: SwipeDirection, _thr: u16, _dx: i32, _dy: i32, _dom: u32) {}
}
