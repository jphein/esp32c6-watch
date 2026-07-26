//! Write-range guard for the watch's ONE runtime flash writer (#55).
//!
//! Pure interval math, no_std, no deps — the firmware wraps `esp_storage::
//! FlashStorage` in a `GuardedFlash` that consults a [`WriteGuard`] before
//! every `Storage::write`; this crate holds the decision logic so it is
//! host-unit-testable (the firmware itself can't build on the host).
//!
//! Why it exists (#55, the eldritch-lantern brick): the OTA path derived the
//! "currently running slot" from otadata instead of from the MMU. Stale
//! otadata (left over from a pre-#50 layout) claimed `ota_1` while the
//! bootloader had actually fallen back to `ota_0`, so "the other slot" —
//! the download target — resolved to the very partition the CPU was executing
//! from. Every 4 KB chunk then erase+rewrote the running image in place until
//! the erase of the sector holding live WiFi rodata (flash `0x152000`) killed
//! the app mid-read-modify-write, leaving a checksum-broken `ota_0` and an
//! empty `ota_1`: a boot-loop soft brick.
//!
//! The guard is the systemic half of the fix: whatever a caller *computes*,
//! a write that intersects a protected range (the booted app slot, the
//! bootloader + partition table region) is refused before it touches flash.
//!
//! Semantics:
//! - Ranges are absolute flash byte spans, half-open `[start, end)`.
//! - [`WriteGuard::check`] tests the exact span of a write.
//! - [`WriteGuard::check_rmw`] first rounds the span OUT to erase-unit
//!   boundaries — `esp-storage`'s `Storage::write` is a read-modify-write
//!   that unconditionally ERASES every touched 4 KB sector, so the write's
//!   true blast radius is the sector-aligned superset.
//! - Zero-length writes touch nothing and always pass; zero-length deny
//!   ranges protect nothing and are ignored.
//! - All arithmetic is u64-widened: `offset + len` can never wrap into a
//!   false pass.

#![cfg_attr(not(test), no_std)]

/// Maximum number of protected ranges a [`WriteGuard`] can hold.
///
/// The firmware needs at most three: the bootloader + partition-table region
/// and (fail-safe, when the booted slot can't be determined) both app slots.
pub const MAX_RANGES: usize = 4;

/// A refused write: the attempted span and the protected range it intersects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Violation {
    /// Byte offset of the attempted write (as given, NOT sector-rounded).
    pub offset: u32,
    /// Length of the attempted write in bytes.
    pub len: u32,
    /// Start of the protected range that was hit.
    pub range_start: u32,
    /// End (exclusive) of the protected range that was hit.
    pub range_end: u32,
}

/// A deny-list of absolute flash ranges `[start, end)` that writes must
/// never intersect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriteGuard {
    ranges: [(u32, u32); MAX_RANGES],
    len: usize,
}

impl Default for WriteGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl WriteGuard {
    /// An empty guard: every write passes.
    pub const fn new() -> Self {
        Self {
            ranges: [(0, 0); MAX_RANGES],
            len: 0,
        }
    }

    /// Protect `[start, start + len)`. Zero-length ranges are ignored.
    ///
    /// Errors when the table is full ([`MAX_RANGES`]) — the caller must treat
    /// that as fatal misconfiguration, never as "unprotected is fine".
    pub fn deny(&mut self, start: u32, len: u32) -> Result<(), ()> {
        if len == 0 {
            return Ok(());
        }
        if self.len == MAX_RANGES {
            return Err(());
        }
        let end = (start as u64 + len as u64).min(u32::MAX as u64 + 1);
        // Saturate the exclusive end at u32::MAX: a range reaching the 4 GiB
        // boundary protects everything from `start` up.
        let end = if end > u32::MAX as u64 { u32::MAX } else { end as u32 };
        self.ranges[self.len] = (start, end);
        self.len += 1;
        Ok(())
    }

    /// Number of protected ranges currently held.
    pub fn ranges(&self) -> usize {
        self.len
    }

    /// Check the exact span `[offset, offset + len)` against the deny-list.
    pub fn check(&self, offset: u32, len: u32) -> Result<(), Violation> {
        if len == 0 {
            return Ok(());
        }
        let start = offset as u64;
        let end = offset as u64 + len as u64;
        for &(rs, re) in &self.ranges[..self.len] {
            if start < re as u64 && (rs as u64) < end {
                return Err(Violation {
                    offset,
                    len,
                    range_start: rs,
                    range_end: re,
                });
            }
        }
        Ok(())
    }

    /// Check a read-modify-write: the span is rounded OUT to `erase_unit`
    /// boundaries before checking, because the underlying writer erases every
    /// touched sector whole (`esp-storage` `Storage::write` semantics).
    ///
    /// `erase_unit` must be a power of two (4096 for ESP32-C6 NOR flash);
    /// a zero/non-power-of-two unit falls back to the exact-span check.
    pub fn check_rmw(&self, offset: u32, len: u32, erase_unit: u32) -> Result<(), Violation> {
        if len == 0 {
            return Ok(());
        }
        if erase_unit == 0 || !erase_unit.is_power_of_two() {
            return self.check(offset, len);
        }
        let mask = erase_unit as u64 - 1;
        let start = offset as u64 & !mask;
        let end = (offset as u64 + len as u64 + mask) & !mask;
        for &(rs, re) in &self.ranges[..self.len] {
            if start < re as u64 && (rs as u64) < end {
                return Err(Violation {
                    offset,
                    len,
                    range_start: rs,
                    range_end: re,
                });
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The shipped 16 MB / 6 MB-slot layout (#50).
    const BOOT_REGION: (u32, u32) = (0x0, 0x9000); // bootloader + partition table
    const OTA_0: (u32, u32) = (0x10_000, 0x60_0000);
    const OTA_1: (u32, u32) = (0x61_0000, 0x60_0000);
    const OTADATA: u32 = 0xD000;
    const CONFIG: u32 = 0xC1_0000;
    const SECTOR: u32 = 4096;

    /// Guard as the firmware builds it when running from ota_0.
    fn guard_running_ota0() -> WriteGuard {
        let mut g = WriteGuard::new();
        g.deny(BOOT_REGION.0, BOOT_REGION.1).unwrap();
        g.deny(OTA_0.0, OTA_0.1).unwrap();
        g
    }

    // --- The #55 incident vectors, byte for byte -------------------------

    #[test]
    fn incident_first_chunk_into_running_slot_is_refused() {
        // Chunk 0 of the self-overwrite: write(0x10000, 4096) — the RMW that
        // replanted the app-descriptor SHA at flash 0x100B0.
        let g = guard_running_ota0();
        let v = g.check_rmw(0x10_000, 4096, SECTOR).unwrap_err();
        assert_eq!((v.range_start, v.range_end), (0x10_000, 0x61_0000));
    }

    #[test]
    fn incident_fatal_chunk_322_is_refused() {
        // The chunk whose sector erase (flash 0x152000) killed the watch.
        let g = guard_running_ota0();
        assert!(g.check_rmw(0x15_2000, 4096, SECTOR).is_err());
    }

    #[test]
    fn legit_targets_still_pass_while_ota0_is_protected() {
        let g = guard_running_ota0();
        // Config record, both slots (primary + backup mirror).
        assert!(g.check_rmw(CONFIG, 113, SECTOR).is_ok());
        assert!(g.check_rmw(CONFIG + 0x1000, 113, SECTOR).is_ok());
        // otadata select entries (both 4K copies).
        assert!(g.check_rmw(OTADATA, 32, SECTOR).is_ok());
        assert!(g.check_rmw(OTADATA + 0x1000, 32, SECTOR).is_ok());
        // A real OTA download into the inactive slot, first and deep chunk.
        assert!(g.check_rmw(OTA_1.0, 4096, SECTOR).is_ok());
        assert!(g.check_rmw(OTA_1.0 + 0x14_2000, 4096, SECTOR).is_ok());
    }

    #[test]
    fn bootloader_and_partition_table_are_refused() {
        let g = guard_running_ota0();
        assert!(g.check_rmw(0x0, 16, SECTOR).is_err()); // bootloader
        assert!(g.check_rmw(0x8000, 32, SECTOR).is_err()); // partition table
    }

    // --- Interval semantics ----------------------------------------------

    #[test]
    fn exact_boundaries_are_half_open() {
        let mut g = WriteGuard::new();
        g.deny(0x1000, 0x1000).unwrap(); // protect [0x1000, 0x2000)
        // Adjacent on both sides: allowed.
        assert!(g.check(0x0, 0x1000).is_ok()); // ends exactly at start
        assert!(g.check(0x2000, 0x1000).is_ok()); // begins exactly at end
        // One byte over each edge: refused.
        assert!(g.check(0x0, 0x1001).is_err());
        assert!(g.check(0x1FFF, 1).is_err());
        // Contained and containing: refused.
        assert!(g.check(0x1800, 4).is_err());
        assert!(g.check(0x0, 0x10_000).is_err());
    }

    #[test]
    fn rmw_rounding_widens_the_span_to_sectors() {
        let mut g = WriteGuard::new();
        g.deny(0x2000, 0x1000).unwrap(); // protect sector [0x2000, 0x3000)
        // Exact-span check would pass, but the RMW erases sector 0x2000.
        assert!(g.check(0x1F00, 0x80).is_ok());
        assert!(g.check_rmw(0x1F00, 0x80, SECTOR).is_ok()); // stays in 0x1000-sector
        assert!(g.check_rmw(0x1FFF, 2, SECTOR).is_err()); // straddles into 0x2000
        assert!(g.check_rmw(0x3000, 1, SECTOR).is_ok()); // next sector, clear
        assert!(g.check_rmw(0x2FFF, 1, SECTOR).is_err()); // last protected byte
    }

    #[test]
    fn zero_length_write_passes_and_zero_length_deny_is_ignored() {
        let mut g = WriteGuard::new();
        g.deny(0x1000, 0).unwrap(); // ignored
        assert_eq!(g.ranges(), 0);
        g.deny(0x1000, 0x1000).unwrap();
        assert!(g.check(0x1000, 0).is_ok());
        assert!(g.check_rmw(0x1000, 0, SECTOR).is_ok());
    }

    #[test]
    fn offset_plus_len_overflow_cannot_wrap_into_a_pass() {
        let mut g = WriteGuard::new();
        g.deny(0xFFFF_F000, 0x1000).unwrap(); // protect the top sector
        // u32 wrap would fold this back to a tiny span near 0.
        assert!(g.check(0xFFFF_FF00, 0x200).is_err());
        assert!(g.check_rmw(0xFFFF_FF00, 0x200, SECTOR).is_err());
        // And a deny whose end exceeds u32 saturates instead of wrapping.
        let mut g2 = WriteGuard::new();
        g2.deny(0xFFFF_0000, 0xFFFF_FFFF).unwrap();
        assert!(g2.check(0xFFFF_FFFE, 1).is_err());
        assert!(g2.check(0x0, 0x1000).is_ok());
    }

    #[test]
    fn table_full_is_an_error_not_a_silent_pass() {
        let mut g = WriteGuard::new();
        for i in 0..MAX_RANGES as u32 {
            g.deny(i * 0x1000, 0x100).unwrap();
        }
        assert!(g.deny(0x10_0000, 0x100).is_err());
        // The existing ranges still enforce.
        assert!(g.check(0x0, 1).is_err());
    }

    #[test]
    fn empty_guard_allows_everything() {
        let g = WriteGuard::new();
        assert!(g.check(0, u32::MAX).is_ok());
        assert!(g.check_rmw(0x15_2000, 4096, SECTOR).is_ok());
    }

    #[test]
    fn non_power_of_two_erase_unit_falls_back_to_exact() {
        let mut g = WriteGuard::new();
        g.deny(0x2000, 0x1000).unwrap();
        assert!(g.check_rmw(0x1F00, 0x80, 0).is_ok());
        assert!(g.check_rmw(0x1F00, 0x80, 3000).is_ok());
        assert!(g.check_rmw(0x2000, 1, 0).is_err());
    }

    #[test]
    fn fail_safe_mode_protects_both_slots() {
        // When the booted slot can't be determined the firmware denies BOTH
        // app slots — OTA to either is refused, config/otadata still work.
        let mut g = WriteGuard::new();
        g.deny(BOOT_REGION.0, BOOT_REGION.1).unwrap();
        g.deny(OTA_0.0, OTA_0.1).unwrap();
        g.deny(OTA_1.0, OTA_1.1).unwrap();
        assert!(g.check_rmw(OTA_0.0, 4096, SECTOR).is_err());
        assert!(g.check_rmw(OTA_1.0, 4096, SECTOR).is_err());
        assert!(g.check_rmw(CONFIG, 113, SECTOR).is_ok());
        assert!(g.check_rmw(OTADATA, 32, SECTOR).is_ok());
    }
}
