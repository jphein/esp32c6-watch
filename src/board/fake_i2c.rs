//! A bus for six devices that are not on this board (#cyd-c5).
//!
//! The CYD has no AXP2101, no FT3168, no PCF85063A, no QMI8658, no ES8311 and no
//! ES7210. `main.rs` constructs all six unconditionally, and each is generic over
//! `embedded_hal::i2c::I2c` behind a shared `RefCellDevice` — so substituting the
//! bus at ONE line satisfies every constructor and all ~48 of their call sites
//! with zero call-site edits. That is the whole leverage of this file.
//!
//! # ⚠️ It FAILS CLOSED, and that is the entire design — do not "fix" it
//!
//! Almost every read in `main.rs` is wrapped `if let Ok(x) = …`, so returning
//! `Err` from every transaction means the block simply does not run. The danger
//! is the opposite: a shim that returns `Ok(0)`, because **zero is a meaningful
//! value** downstream. Verified line by line:
//!
//! ```text
//! main.rs   let mut batt_pct: u8 = 0;      <- initialised to ZERO, not a sentinel
//!           if let Ok(pct) = power.get_battery_percent() {
//!               batt_pct = pct;
//!               if batt_pct < 15 && !charging && !low_batt_notified {   <- 0 < 15 == TRUE
//!                   notify::push(Source::Battery, "Battery low", …)     <- EVERY BOOT
//! ```
//!
//! The low-battery check sits *inside* the `Ok` arm, so `Err` defuses it and
//! `Ok(0)` fires it. **If a later change makes this succeed, re-read this note
//! first.**
//!
//! # Why a shim rather than a real bus talking to nothing
//!
//! Both fail closed, and the first plan of record was the real bus: leave the C5
//! I2C peripheral configured, let every read NACK into `Err`. That is sound —
//! but it needs two pins, and on this board it does not have them. `GPIO7` is
//! the **panel's MOSI**, and the only GPIOs the vendor documents as free
//! (`connections.md:15`: 0 and 28) are the CC1101/NRF24 expansion header. So the
//! real-bus version spends the board's last free pins on a bus wired to nothing.
//!
//! The shim costs zero pins and is strictly more deterministic — `Err` by
//! construction rather than `Err` by expected-NACK. As the watch session put it
//! when reversing their own call: the no-shim argument "was right about the
//! property and wrong about the mechanism."
//!
//! Scaffold: delete when the real capability gates (`has-pmu`, `has-imu`,
//! `has-audio`) reach the publishers and these six constructors stop being
//! compiled on this board at all.

/// A bus that is not connected to anything, and says so.
pub struct FakeI2c;

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub struct FakeI2cError;

impl embedded_hal::i2c::Error for FakeI2cError {
    fn kind(&self) -> embedded_hal::i2c::ErrorKind {
        embedded_hal::i2c::ErrorKind::Other
    }
}

impl embedded_hal::i2c::ErrorType for FakeI2c {
    type Error = FakeI2cError;
}

impl embedded_hal::i2c::I2c for FakeI2c {
    /// `read`/`write`/`write_read` all have default impls over `transaction`, so
    /// this single method is the whole shim.
    fn transaction(
        &mut self,
        _addr: u8,
        _ops: &mut [embedded_hal::i2c::Operation<'_>],
    ) -> Result<(), Self::Error> {
        Err(FakeI2cError) // <- the one line that matters. See the module header.
    }
}
