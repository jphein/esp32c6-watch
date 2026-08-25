// FT3168 Touch Controller driver
// Reference: Arduino_FT3x68.h - I2C address 0x38

use embedded_hal::i2c::I2c;

const FT3168_ADDR: u8 = 0x38;

// Registers
const REG_FINGER_NUM: u8 = 0x02;
const REG_X1_H: u8 = 0x03;
const REG_X1_L: u8 = 0x04;
const REG_Y1_H: u8 = 0x05;
const REG_Y1_L: u8 = 0x06;
const REG_POWER_MODE: u8 = 0xA5;
const REG_GESTURE_ID: u8 = 0xD3;

#[derive(Debug, Clone, Copy)]
pub struct TouchPoint {
    pub x: u16,
    pub y: u16,
    pub fingers: u8,
}

#[derive(Debug, Clone, Copy)]
pub enum Gesture {
    None,
    SwipeUp,
    SwipeDown,
    SwipeLeft,
    SwipeRight,
    SingleTap,
    DoubleTap,
    LongPress,
    Unknown(u8),
}

/// Detected swipe gesture with start/end coordinates
#[derive(Debug, Clone, Copy)]
pub struct SwipeEvent {
    pub direction: SwipeDirection,
    pub start_x: u16,
    pub start_y: u16,
    pub end_x: u16,
    pub end_y: u16,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SwipeDirection {
    Up,
    Down,
    Left,
    Right,
    Tap,
}

pub struct Ft3168Touch<I> {
    i2c: I,
    // Swipe tracking state
    tracking: bool,
    start_x: u16,
    start_y: u16,
    last_x: u16,
    last_y: u16,
}

impl<I: I2c> Ft3168Touch<I> {
    pub fn new(i2c: I) -> Self {
        Self {
            i2c,
            tracking: false,
            start_x: 0,
            start_y: 0,
            last_x: 0,
            last_y: 0,
        }
    }

    fn read_reg(&mut self, reg: u8) -> Result<u8, I::Error> {
        let mut buf = [0u8];
        self.i2c.write_read(FT3168_ADDR, &[reg], &mut buf)?;
        Ok(buf[0])
    }

    fn write_reg(&mut self, reg: u8, val: u8) -> Result<(), I::Error> {
        self.i2c.write(FT3168_ADDR, &[reg, val])
    }

    /// Initialize touch controller in monitor power mode.
    pub fn init(&mut self) -> Result<(), I::Error> {
        // Set power mode to monitor (triggers on touch)
        self.write_reg(REG_POWER_MODE, 0x01)?;
        Ok(())
    }

    /// Read current touch state. Returns None if no touch.
    pub fn read(&mut self) -> Result<Option<TouchPoint>, I::Error> {
        let fingers = self.read_reg(REG_FINGER_NUM)?;
        if fingers == 0 {
            return Ok(None);
        }

        let x_h = self.read_reg(REG_X1_H)? as u16;
        let x_l = self.read_reg(REG_X1_L)? as u16;
        let y_h = self.read_reg(REG_Y1_H)? as u16;
        let y_l = self.read_reg(REG_Y1_L)? as u16;

        let x = ((x_h & 0x0F) << 8) | x_l;
        let y = ((y_h & 0x0F) << 8) | y_l;

        Ok(Some(TouchPoint {
            x,
            y,
            fingers,
        }))
    }

    /// Poll touch and detect swipe gestures.
    /// Returns Some(SwipeEvent) when a finger is lifted after movement.
    /// Returns current touch position for live tracking.
    pub fn poll(&mut self) -> Result<(Option<TouchPoint>, Option<SwipeEvent>), I::Error> {
        let point = self.read()?;

        match point {
            Some(tp) => {
                if !self.tracking {
                    // New touch started
                    self.tracking = true;
                    self.start_x = tp.x;
                    self.start_y = tp.y;
                }
                self.last_x = tp.x;
                self.last_y = tp.y;
                Ok((Some(tp), None))
            }
            None => {
                if self.tracking {
                    // Finger lifted - determine swipe
                    self.tracking = false;
                    let dx = self.last_x as i32 - self.start_x as i32;
                    let dy = self.last_y as i32 - self.start_y as i32;
                    let abs_dx = dx.unsigned_abs();
                    let abs_dy = dy.unsigned_abs();

                    // Classify the lift-off gesture. It's a directional swipe once
                    // the DOMINANT axis travels at least SWIPE_MIN logical px; the
                    // direction is simply that larger axis. Otherwise it's a Tap.
                    //
                    // This deliberately drops the old "dominant axis must beat the
                    // other by 1.5x, else fall back to Tap" rule. That rule created
                    // a dead-zone that silently swallowed any swipe whose axes were
                    // within 1.5x of each other — a 100x80 px drag, or the slightly
                    // diagonal swipes people actually make — turning a deliberate
                    // navigation gesture into a stray tap. Dominant-axis is both
                    // more reliable (no dropped swipes) and identical on every
                    // screen (page carousel, launcher-close, every overlay close).
                    // SWIPE_MIN (~10% of the 410px panel) is a hair above the old
                    // 30px tap cutoff, so a jittery tap that slides a little still
                    // reads as a tap rather than an accidental swipe.
                    const SWIPE_MIN: u32 = 36;
                    let direction = if abs_dx.max(abs_dy) < SWIPE_MIN {
                        SwipeDirection::Tap
                    } else if abs_dx >= abs_dy {
                        if dx > 0 { SwipeDirection::Right } else { SwipeDirection::Left }
                    } else if dy > 0 {
                        SwipeDirection::Down
                    } else {
                        SwipeDirection::Up
                    };

                    let event = SwipeEvent {
                        direction,
                        start_x: self.start_x,
                        start_y: self.start_y,
                        end_x: self.last_x,
                        end_y: self.last_y,
                    };
                    Ok((None, Some(event)))
                } else {
                    Ok((None, None))
                }
            }
        }
    }

    /// Read gesture ID.
    pub fn read_gesture(&mut self) -> Result<Gesture, I::Error> {
        let id = self.read_reg(REG_GESTURE_ID)?;
        Ok(match id {
            0x00 => Gesture::None,
            0x01 => Gesture::SwipeUp,
            0x02 => Gesture::SwipeDown,
            0x03 => Gesture::SwipeLeft,
            0x04 => Gesture::SwipeRight,
            0x05 => Gesture::SingleTap,
            0x0B => Gesture::DoubleTap,
            0x0C => Gesture::LongPress,
            other => Gesture::Unknown(other),
        })
    }
}

// ============================================================================
// Null stubs (#cyd-c5) — boards without the FT3168 (`has-cap-touch` off).
//
// These exist so the ~60 touch consumer sites in main.rs compile UNCHANGED on a
// board whose touch arrives later through a different driver (the CYD's
// XPT2046, via drivers/panel.rs). The semantics are honest, not emulated:
// NullTouch never reports a contact, and NullInput's falling-edge future never
// resolves — inside a `select` that is exactly "this board has no touch IRQ
// line", so the timer arm wins every race, which is the correct behaviour for
// poll-only hardware.
// ============================================================================

/// A touch controller that is not there. `read()` is `Ok(None)` forever.
pub struct NullTouch;

impl NullTouch {
    pub fn read(&mut self) -> Result<Option<TouchPoint>, core::convert::Infallible> {
        Ok(None)
    }
    pub fn read_gesture(&mut self) -> Result<Gesture, core::convert::Infallible> {
        Ok(Gesture::None)
    }
    pub fn init(&mut self) -> Result<(), core::convert::Infallible> {
        Ok(())
    }
    pub fn poll(
        &mut self,
    ) -> Result<(Option<TouchPoint>, Option<SwipeEvent>), core::convert::Infallible> {
        Ok((None, None))
    }
}

/// An interrupt line that is not wired. Mirrors the `esp_hal::gpio::Input`
/// surface main.rs actually uses.
pub struct NullInput;

impl NullInput {
    pub fn is_low(&self) -> bool {
        false
    }
    pub fn is_high(&self) -> bool {
        true
    }
    pub fn wakeup_enable(
        &mut self,
        _enable: bool,
        _event: esp_hal::gpio::WakeEvent,
    ) -> Result<(), core::convert::Infallible> {
        Ok(())
    }
    /// Never resolves: no IRQ line exists to fall.
    pub async fn wait_for_falling_edge(&mut self) {
        core::future::pending::<()>().await
    }
}

// ============================================================================
// CYD-C5: the XPT2046 behind the same surface main.rs already calls (#cyd-c5)
// ============================================================================

/// Adapts vesper's [`Xpt2046`](crate::drivers::xpt2046::Xpt2046) to the surface
/// `main.rs` uses for the FT3168, and converts its types to this module's.
///
/// # Why the bus is a PARAMETER here and not a field
///
/// On this board the touch controller and the panel share one SPI peripheral,
/// and [`St7789Display`](crate::drivers::st7789::St7789Display) owns it —
/// deliberately, so the strip-flush hot path keeps a plain `&mut` instead of
/// paying a `RefCell` borrow per strip. So a touch read has to reach the bus
/// through the display, and the honest way to say that is in the signature.
///
/// ★ **The payoff is that the bus-sharing rule stops being a rule.** Passing
/// `display.bus_mut()` takes a `&mut` on the display for exactly the duration of
/// the call, and a live flusher also holds `&mut display` — so "never poll touch
/// inside a pixel transaction" becomes a borrow-checker error rather than a
/// comment someone has to remember. The two failure modes it prevents are not
/// subtle: a command clocked into an open RAMWR stream lands in GRAM as pixels,
/// and touch CS asserted under a low LCD CS puts two devices on one MISO line.
///
/// The FT3168 needs no such parameter (it is on I2C, behind its own
/// `RefCellDevice`), which is why the C5 call sites carry the extra argument and
/// the C6 ones do not. That asymmetry is the hardware's, not an abstraction leak.
#[cfg(feature = "board-cyd-c5")]
pub struct XptTouch<'d> {
    inner: crate::drivers::xpt2046::Xpt2046<'d>,
}

#[cfg(feature = "board-cyd-c5")]
impl<'d> XptTouch<'d> {
    /// Built from the board's calibration and the display's CURRENT rotation —
    /// passed in rather than read from `board::DEFAULT_ROTATION` so the two
    /// cannot drift if the display is ever rotated at runtime.
    pub fn new(rotation: crate::drivers::Rotation) -> Self {
        Self {
            // No `.with_irq()`: GPIO3 is one wire with one owner, and the
            // firmware keeps it as `touch_int` for `wait_for_falling_edge`,
            // which sleeps the executor instead of spinning. Costs two ADC
            // conversions on an idle poll; buys a strictly better use of the
            // same PENIRQ signal. See the driver's `irq` field docs.
            inner: crate::drivers::xpt2046::Xpt2046::new(crate::board::TOUCH_CAL, rotation),
        }
    }

    /// Nothing to bring up: the XPT2046 has no init sequence — it converts on
    /// demand and powers down between conversions. `Ok` keeps the call site
    /// shaped like the FT3168's.
    pub fn init(&mut self) -> Result<(), core::convert::Infallible> {
        Ok(())
    }

    /// Current contact, or `None`. `Infallible` because SPI has no
    /// acknowledgement — a read cannot fail, it can only report "nothing
    /// pressed".
    pub fn read(
        &mut self,
        bus: &mut crate::drivers::spi_bus::SharedSpiBus<'d>,
    ) -> Result<Option<TouchPoint>, core::convert::Infallible> {
        Ok(self.inner.read(bus).map(conv_point))
    }

    /// Position + lift-off gesture, matching `Ft3168Touch::poll`'s semantics: a
    /// live touch is `(Some(point), None)`, the poll on which the finger leaves
    /// is `(None, Some(event))`, idle is `(None, None)`.
    pub fn poll(
        &mut self,
        bus: &mut crate::drivers::spi_bus::SharedSpiBus<'d>,
    ) -> Result<(Option<TouchPoint>, Option<SwipeEvent>), core::convert::Infallible> {
        let (p, e) = self.inner.poll(bus);
        Ok((p.map(conv_point), e.map(conv_swipe)))
    }
}

#[cfg(feature = "board-cyd-c5")]
fn conv_point(p: crate::drivers::xpt2046::TouchPoint) -> TouchPoint {
    TouchPoint {
        x: p.x,
        y: p.y,
        fingers: p.fingers,
    }
}

#[cfg(feature = "board-cyd-c5")]
fn conv_swipe(e: crate::drivers::xpt2046::SwipeEvent) -> SwipeEvent {
    use crate::drivers::xpt2046::SwipeDirection as D;
    SwipeEvent {
        // Written out rather than transmuted: the two enums are declared in
        // different crates' worth of code and only happen to agree today.
        direction: match e.direction {
            D::Up => SwipeDirection::Up,
            D::Down => SwipeDirection::Down,
            D::Left => SwipeDirection::Left,
            D::Right => SwipeDirection::Right,
            D::Tap => SwipeDirection::Tap,
        },
        start_x: e.start_x,
        start_y: e.start_y,
        end_x: e.end_x,
        end_y: e.end_y,
    }
}
