// FocalTech capacitive touch driver — FT3168 (C6 watch) and FT6336U (S3 CYD)
// speak the same register map at the same 0x38 address; the struct keeps its
// original FT3168 name. Reference: Arduino_FT3x68.h.

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
/// ID_G_MODE — INT behaviour: 0x00 = polling (level-low while touched),
/// 0x01 = trigger (pulses at report rate). main.rs samples the INT *level*
/// (`touch_held`, `int_low || was_touching`), so boards on the quirk path
/// need 0x00. Datasheet-labelled; init reads it back and says what took.
const REG_G_MODE: u8 = 0xA4;
/// ID_G_CTRL — 0x00 = stay Active with no touch, 0x01 = auto-drop to
/// Monitor. The FT6336U drops on its own and its Monitor is deaf
/// (emberburrito, measured on this panel class).
const REG_CTRL: u8 = 0x86;

/// How many touch samples to log after boot (raw + transformed) — enough
/// for a four-corner calibration sweep plus a tap-around pass, then quiet.
/// Emberburrito's TOUCH_LOG_BUDGET pattern: samples exist exactly when
/// someone is standing at the glass asking why nothing happened.
const TOUCH_LOG_BUDGET: u8 = 40;

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
    /// Remaining raw-sample log budget (quirk boards; see TOUCH_LOG_BUDGET).
    log_budget: u8,
    /// Empty-read counter driving the periodic Monitor-mode reconcile on the
    /// quirk path (the FT6336U can re-enter its deaf Monitor even with
    /// CTRL written — emberburrito measured the race; reconcile is the
    /// backstop that can only shrink the deaf window).
    reconcile_tick: u16,
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
            log_budget: TOUCH_LOG_BUDGET,
            reconcile_tick: 0,
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

    /// Initialize the touch controller.
    ///
    /// Two paths, chosen by the board (`TOUCH_FT6336_ACTIVE_QUIRK`):
    /// - FT3168 (C6 watch): Monitor power mode, as always — byte-identical
    ///   to the original init.
    /// - FT6336U (S3 CYD): Monitor is DEAF on this part and it re-enters
    ///   Monitor by itself, so force Active + stay-Active + level INT.
    ///   Every write is read back and logged: a chip that ignores a
    ///   register says so on serial instead of leaving us believing it
    ///   was handled.
    pub fn init(&mut self) -> Result<(), I::Error> {
        if crate::board::TOUCH_FT6336_ACTIVE_QUIRK {
            for (reg, val, name) in [
                (REG_POWER_MODE, 0x00u8, "pmode-active"),
                (REG_CTRL, 0x00, "ctrl-stay-active"),
                (REG_G_MODE, 0x00, "gmode-level-int"),
            ] {
                self.write_reg(reg, val)?;
                let back = self.read_reg(reg)?;
                esp_println::println!(
                    "[TOUCH] {} 0x{:02X}<=0x{:02X} readback 0x{:02X}{}",
                    name,
                    reg,
                    val,
                    back,
                    if back == val { "" } else { " (IGNORED)" }
                );
            }
        } else {
            // Set power mode to monitor (triggers on touch)
            self.write_reg(REG_POWER_MODE, 0x01)?;
        }
        Ok(())
    }

    /// Read current touch state. Returns None if no touch.
    pub fn read(&mut self) -> Result<Option<TouchPoint>, I::Error> {
        let fingers = self.read_reg(REG_FINGER_NUM)?;
        if fingers == 0 {
            // Quirk-path backstop: the FT6336U can drop back into its deaf
            // Monitor mode on its own. Reconcile PMODE→Active every 64th
            // empty read (one extra I2C read per ~64 polls, nothing on the
            // touch hot path) and say so when it was caught — that count on
            // serial is the evidence for whether CTRL actually took.
            if crate::board::TOUCH_FT6336_ACTIVE_QUIRK {
                self.reconcile_tick = self.reconcile_tick.wrapping_add(1);
                if self.reconcile_tick % 64 == 0 {
                    let pmode = self.read_reg(REG_POWER_MODE)?;
                    if pmode != 0x00 {
                        self.write_reg(REG_POWER_MODE, 0x00)?;
                        esp_println::println!(
                            "[TOUCH] monitor re-arm caught (pmode=0x{:02X}), forced Active",
                            pmode
                        );
                    }
                }
            }
            return Ok(None);
        }

        let x_h = self.read_reg(REG_X1_H)? as u16;
        let x_l = self.read_reg(REG_X1_L)? as u16;
        let y_h = self.read_reg(REG_Y1_H)? as u16;
        let y_l = self.read_reg(REG_Y1_L)? as u16;

        let raw_x = ((x_h & 0x0F) << 8) | x_l;
        let raw_y = ((y_h & 0x0F) << 8) | y_l;
        let mut x = raw_x;
        let mut y = raw_y;

        // Board transform (identity on the C6): FocalTech parts report in the
        // PANEL-NATIVE frame, which is not always the frame the scene draws
        // in — the S3 CYD drives a 240x320-native panel in 320x240 landscape,
        // so raw axes swap and the (post-swap) vertical inverts. The consts
        // are board facts (tested-beats-derived; see each board module).
        if crate::board::TOUCH_SWAP_XY {
            core::mem::swap(&mut x, &mut y);
        }
        if crate::board::TOUCH_INVERT_X {
            x = (crate::board::LCD_WIDTH - 1).saturating_sub(x);
        }
        if crate::board::TOUCH_INVERT_Y {
            y = (crate::board::LCD_HEIGHT - 1).saturating_sub(y);
        }

        // Budgeted calibration evidence (quirk boards): raw vs transformed,
        // so a four-corner sweep on the bench verifies SWAP/INVERT against a
        // real finger instead of asserting them from a table. Goes quiet
        // after TOUCH_LOG_BUDGET samples.
        if crate::board::TOUCH_FT6336_ACTIVE_QUIRK && self.log_budget > 0 {
            self.log_budget -= 1;
            esp_println::println!(
                "[TOUCH] raw=({},{}) mapped=({},{}) fingers={} budget={}",
                raw_x, raw_y, x, y, fingers, self.log_budget
            );
        }

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
