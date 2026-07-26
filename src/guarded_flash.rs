//! `GuardedFlash` (#55): the one runtime flash writer, range-checked.
//!
//! Wraps the shared `esp_storage::FlashStorage` so that EVERY
//! `embedded_storage::Storage::write` — config saves, otadata flips, OTA
//! chunk streams, whatever a future caller computes — is validated against a
//! deny-list of protected flash ranges before it touches hardware. A write
//! that intersects a protected range is refused with
//! [`FlashStorageError::OutOfBounds`] and a loud log line; it can never
//! silently corrupt the running image again.
//!
//! Protected at boot (see `main.rs`): the bootloader + partition-table region
//! (`0x0..0x9000`) and the app slot the CPU is actually executing from (MMU
//! probe via `PartitionTable::booted_partition`). If the booted slot cannot
//! be determined, BOTH app slots are protected — OTA then refuses on its own
//! booted-slot check anyway, and config/otadata writes still work.
//!
//! The span check is sector-rounded ([`WriteGuard::check_rmw`]):
//! esp-storage's `Storage::write` is a read-modify-write that unconditionally
//! ERASES every 4 KB sector it touches, so the true blast radius of a write
//! is its sector-aligned superset. The range math itself lives in the
//! host-tested `crates/flash-guard` (pure, no_std) — see its tests for the
//! exact #55 incident vectors.
//!
//! Deliberately NOT implemented: the `NorFlash` erase/word-write traits. The
//! only way through this wrapper is the checked `Storage` interface — code
//! that wants a raw erase does not compile against the shared mutex.

use embedded_storage::{ReadStorage, Storage};
use esp_println::println;
use esp_storage::{FlashStorage, FlashStorageError};
use flash_guard::WriteGuard;

/// NOR flash erase unit (one sector) on the ESP32-C6.
const SECTOR_SIZE: u32 = 4096;

/// The shared flash handle with a write deny-list. See module docs.
pub struct GuardedFlash {
    inner: FlashStorage<'static>,
    guard: WriteGuard,
}

impl GuardedFlash {
    /// Wrap `inner`, refusing writes that intersect `guard`'s ranges.
    pub fn new(inner: FlashStorage<'static>, guard: WriteGuard) -> Self {
        Self { inner, guard }
    }
}

impl ReadStorage for GuardedFlash {
    type Error = FlashStorageError;

    fn read(&mut self, offset: u32, bytes: &mut [u8]) -> Result<(), Self::Error> {
        self.inner.read(offset, bytes)
    }

    fn capacity(&self) -> usize {
        self.inner.capacity()
    }
}

impl Storage for GuardedFlash {
    fn write(&mut self, offset: u32, bytes: &[u8]) -> Result<(), Self::Error> {
        if let Err(v) = self.guard.check_rmw(offset, bytes.len() as u32, SECTOR_SIZE) {
            // Refuse + log, never panic: a panicking guard would turn a
            // refused write into a reboot loop of its own. The caller's
            // error path names the operation that was refused.
            println!(
                "[FLASH-GUARD] REFUSED write {:#x}+{:#x} - erase footprint hits protected {:#x}..{:#x} (#55)",
                v.offset, v.len, v.range_start, v.range_end
            );
            return Err(FlashStorageError::OutOfBounds);
        }
        self.inner.write(offset, bytes)
    }
}
