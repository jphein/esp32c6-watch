//! App-level OTA rollback gate — **this board has no bootloader rollback.**
//!
//! Acceptance 5.9 (2026-08-27) proved it: the on-flash second stage
//! (espflash-bundled *ESP-IDF v5.5.1-838-gd66ebb86d2e*) is built **without**
//! `BOOTLOADER_APP_ROLLBACK_ENABLE`. It boots whatever slot otadata *requests* and
//! ignores the state field entirely — `otadata requests Ok(Ota1), state Ok(New)` on
//! every cycle of a permanent panic loop, which is what the bench observed until
//! otadata was erased by hand over serial. So
//! [`crate::net::ota_http::mark_valid_if_pending`] writes a state nothing reads, and
//! rollback has to happen here.
//!
//! # The state machine
//!
//! | otadata state at boot | retained marker | action |
//! |---|---|---|
//! | `New` | — | write `PendingVerify` — the one try is now consumed |
//! | `PendingVerify` | **intentional** | clear marker, re-consume the try, continue |
//! | `PendingVerify` | absent | it crashed or hung → **roll back** |
//! | `Valid` / other | — | normal boot |
//!
//! `mark_valid_if_pending` at +10 s turns `PendingVerify` into `Valid`, so an image
//! that stays up keeps itself.
//!
//! # Why the intentional marker exists (hole 2)
//!
//! Without it, **any deliberate reboot inside the health window rolls back a
//! perfectly good image** — a CFG remote reboot or power-page reboot at t+3 s leaves
//! `PendingVerify`, and the next boot cannot tell that from a crash. `armed_reset`
//! is the single choke point for every *deliberate* reset, so it sets the marker;
//! `custom_halt` deliberately does **not**, because a panic *is* the failure this
//! gate exists to catch. That asymmetry is the design, not an oversight — do not
//! "fix" it by making both paths symmetric.
//!
//! # Why the rolled-back build id exists (hole 4)
//!
//! `ota_http::LAST_REFUSED_BUILD` cannot help here: it is **RAM** (cleared by the
//! rollback's own reboot) and it is only set by *pre-write* refusals — a
//! panicking-but-flashable image is never "refused" at all. So without this, the
//! retained announce for the bad build is still monotonic over the rolled-back-to
//! epoch, gets re-accepted, and the board fetches 3.4 MB, flashes it, boots, panics
//! and rolls back again, roughly every two minutes, forever. That is worse than the
//! local panic loop it replaces: it burns bandwidth **and 3.4 MB of flash writes per
//! cycle.**
//!
//! The bad image does not need to be told which build it is — **its own baked
//! [`crate::net::ota_http::BUILD_EPOCH`] IS that id** (an announce carries
//! `build_id == BUILD_EPOCH`, which `ota_push.sh` guarantees by stamping the config
//! before building). So the gate records its own epoch on the way out and
//! `handle_announce` refuses matches. A power cycle clears it ⇒ **one full try per
//! power cycle**, by which time a human is present.
//!
//! *Known limitation:* a hand-crafted announce whose id differs from the image's
//! baked epoch slips through. Such an announce is already anomalous; recorded rather
//! than assumed away.
//!
//! # 🔴 Why the retained block is CHECKSUMMED
//!
//! Not defensive habit — **esp-hal's own documentation requires it.**
//! `esp-hal-procmacros-0.22.0/src/lib.rs:88-95` warns that a persistent static can:
//!
//! 1. *"start the application with the static filled with **random bytes**"* if a
//!    reset lands before the RAM has been zeroed, and
//! 2. be **torn mid-update** — *"there is no way to keep some kinds of resets from
//!    happening while updating a persistent static — not even a critical section."*
//!
//! and recommends *"adding a checksum alongside the data."*
//!
//! Both failure directions are live, which is why a magic value alone will not do:
//! random bytes containing a plausible `rolled_back_build` would **suppress a
//! legitimate announce**, and random bytes with the marker set would **skip a needed
//! rollback**. Either is the mechanism doing the opposite of its job.
//!
//! A checksum failure is therefore read as **"no retained state"**, and that
//! fall-through is conservative in both directions: hole 2 rolls back (safe against
//! a bad image) and hole 4 permits one re-fetch (safe against suppressing a good
//! one). Both errors are the recoverable kind.

use esp_println::println;

// ---------------------------------------------------------------------------
// Retained across a software reset / watchdog / deep sleep; zeroed by a power
// cycle. Three primitives rather than one struct so no `bytemuck` derive (and
// therefore no direct dependency) is needed — `u32`/`u64` already satisfy the
// macro's `AnyBitPattern` requirement.
// ---------------------------------------------------------------------------

#[esp_hal::ram(unstable(rtc_fast, persistent))]
static mut RB_INTENTIONAL: u32 = 0;
#[esp_hal::ram(unstable(rtc_fast, persistent))]
static mut RB_BUILD: u64 = 0;
#[esp_hal::ram(unstable(rtc_fast, persistent))]
static mut RB_SUM: u32 = 0;

/// Marker value for "the reset now in flight was deliberate".
const INTENTIONAL: u32 = 0x5AFE_B007;

/// FNV-1a over the retained payload.
///
/// Not cryptographic and does not need to be — its only job is to make **random
/// bytes fail**, which is exactly what the macro's docs ask for. Never returns 0, so
/// an all-zero block (first boot, or after a power cycle) cannot accidentally
/// validate: a zero stored sum can then never match a computed one.
fn checksum(intentional: u32, build: u64) -> u32 {
    let mut h: u32 = 0x811C_9DC5;
    for b in intentional
        .to_le_bytes()
        .iter()
        .chain(build.to_le_bytes().iter())
    {
        h ^= *b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    if h == 0 { 1 } else { h }
}

/// Validated retained state, or `None` when the block is absent/garbage/torn.
pub struct Retained {
    /// The reset that brought us here was deliberate (set by `armed_reset`).
    pub intentional: bool,
    /// Build id the gate rolled away from; 0 for none.
    pub rolled_back_build: u64,
}

/// Read and VALIDATE the retained block. `None` means "no trustworthy state" —
/// treat it as a fresh boot, never as zeros.
pub fn read_retained() -> Option<Retained> {
    // SAFETY: single-core, and these are read once at boot before any task can
    // touch them. Primitive reads, no references taken (edition-2024 friendly).
    let (i, b, s) = unsafe { (RB_INTENTIONAL, RB_BUILD, RB_SUM) };
    if s == 0 || s != checksum(i, b) {
        return None;
    }
    Some(Retained {
        intentional: i == INTENTIONAL,
        rolled_back_build: b,
    })
}

/// Write the retained block, checksum last.
///
/// Checksum-last is deliberate: a reset that tears the write leaves the sum stale,
/// so [`read_retained`] rejects the block instead of trusting half of it. There is
/// no way to make the update atomic (see the module docs), so the ordering is the
/// only lever available.
pub fn write_retained(intentional: bool, rolled_back_build: u64) {
    let i = if intentional { INTENTIONAL } else { 0 };
    // SAFETY: as above — single-core, boot-time or reset-path only.
    unsafe {
        RB_INTENTIONAL = i;
        RB_BUILD = rolled_back_build;
        RB_SUM = checksum(i, rolled_back_build);
    }
}

/// Note that the reset about to happen is DELIBERATE, preserving the
/// rolled-back-build id. Called from `armed_reset`; never from `custom_halt`.
pub fn mark_intentional_reset() {
    let prev = read_retained().map(|r| r.rolled_back_build).unwrap_or(0);
    write_retained(true, prev);
}

/// What the gate decided. The caller performs any reboot, so the gate never hides
/// a reset inside itself and stays straightforward to reason about.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Normal boot — state was `Valid` (or a layout with no otadata).
    Continue,
    /// `New` -> `PendingVerify`: the one try is now consumed.
    TryConsumed,
    /// `PendingVerify` + intentional marker: a deliberate reboot, not a crash.
    Intentional,
    /// otadata flipped to the other slot. **The caller MUST reboot.**
    RolledBack,
    /// Would have rolled back, but the target is not a bootable image — kept
    /// booting the current one instead. See [`Outcome::RefusedNoTarget`] rationale
    /// in `ota_http::rollback_target_is_bootable`.
    RefusedNoTarget,
    /// Something went wrong reading or writing otadata.
    Failed(&'static str),
}

/// Run the gate. Call as EARLY as flash allows — every line executed before this
/// is a window in which a panic loops the board forever, because the one try is
/// never consumed. See the design note for the prologue-window discussion.
///
/// On [`Outcome::RolledBack`] the caller must reboot **through `armed_reset`**, or
/// the reset can hang in ROM like every other reset on this chip.
pub fn gate(flash: &mut impl embedded_storage::Storage) -> Outcome {
    use esp_bootloader_esp_idf::ota::{Ota, OtaImageState};
    use esp_bootloader_esp_idf::partitions::{
        self, DataPartitionSubType, PartitionType,
    };

    let retained = read_retained();
    // Consume the marker unconditionally: it describes the reset that just
    // happened, so leaving it set would excuse the NEXT failure too.
    let carried_build = retained.as_ref().map(|r| r.rolled_back_build).unwrap_or(0);
    let was_intentional = retained.as_ref().map(|r| r.intentional).unwrap_or(false);
    write_retained(false, carried_build);

    let mut pt_mem = alloc::vec![0u8; partitions::PARTITION_TABLE_MAX_LEN];
    let Ok(pt) = partitions::read_partition_table(flash, &mut pt_mem) else {
        return Outcome::Failed("partition table read failed");
    };
    let Ok(Some(otadata)) = pt.find_partition(PartitionType::Data(DataPartitionSubType::Ota))
    else {
        // Factory layout: nothing to roll back to, nothing to track.
        return Outcome::Continue;
    };

    let region = otadata.as_embedded_storage(flash);
    let Ok(mut ota) = Ota::new(region, 2) else {
        return Outcome::Failed("otadata invalid");
    };
    let Ok(state) = ota.current_ota_state() else {
        return Outcome::Failed("otadata state read failed");
    };

    match state {
        OtaImageState::New => {
            // First boot of a freshly staged image. Consume the single try.
            if ota.set_current_ota_state(OtaImageState::PendingVerify).is_err() {
                return Outcome::Failed("could not write PendingVerify");
            }
            println!("[ROLLBACK] New -> PendingVerify (one try consumed, must reach mark-valid)");
            Outcome::TryConsumed
        }
        OtaImageState::PendingVerify if was_intentional => {
            // We rebooted on purpose inside the health window. Not a crash — give
            // the image its try back rather than rolling back a good build.
            println!("[ROLLBACK] PendingVerify + intentional marker - deliberate reboot, try re-consumed");
            Outcome::Intentional
        }
        OtaImageState::PendingVerify => {
            // Second boot without reaching mark-valid, and the reset was not
            // deliberate ⇒ the image crashed or hung. Roll back — but only if
            // there is something bootable to roll back TO (hole 3).
            println!("[ROLLBACK] PendingVerify with no intentional marker - image failed its try");
            // Drop the otadata borrow before re-reading the table for the target.
            drop(ota);
            match crate::net::ota_http::rollback_target_is_bootable(flash) {
                Ok((target, true)) => {
                    let mut pt_mem2 = alloc::vec![0u8; partitions::PARTITION_TABLE_MAX_LEN];
                    let Ok(pt2) = partitions::read_partition_table(flash, &mut pt_mem2) else {
                        return Outcome::Failed("partition table re-read failed");
                    };
                    let Ok(Some(od2)) =
                        pt2.find_partition(PartitionType::Data(DataPartitionSubType::Ota))
                    else {
                        return Outcome::Failed("otadata vanished");
                    };
                    let r2 = od2.as_embedded_storage(flash);
                    let Ok(mut ota2) = Ota::new(r2, 2) else {
                        return Outcome::Failed("otadata invalid on flip");
                    };
                    if ota2.set_current_app_partition(target).is_err() {
                        return Outcome::Failed("could not flip boot slot");
                    }
                    // Valid, not New: the slot we are returning to has already
                    // proven itself, and staging it as New would hand it a
                    // one-try gate it does not need — and could roll it back too.
                    if ota2.set_current_ota_state(OtaImageState::Valid).is_err() {
                        return Outcome::Failed("could not mark rolled-back slot Valid");
                    }
                    // Record OUR OWN build id so the retained announce for this bad
                    // build cannot immediately re-fetch it (hole 4). BUILD_EPOCH is
                    // this image's own epoch, which is what the announce carries.
                    write_retained(false, crate::net::ota_http::BUILD_EPOCH);
                    println!(
                        "[ROLLBACK] flipped to {target:?} (Valid); build {} recorded as rolled-back - REBOOTING",
                        crate::net::ota_http::BUILD_EPOCH
                    );
                    Outcome::RolledBack
                }
                Ok((target, false)) => {
                    // Refusing is the safe branch: a looping image still reboots
                    // and still prints; a dead slot does neither.
                    println!(
                        "[ROLLBACK] ⚠️ target {target:?} is NOT bootable - REFUSING to roll back, \
                         continuing on the current image (a loop that prints beats a dead slot)"
                    );
                    Outcome::RefusedNoTarget
                }
                Err(e) => Outcome::Failed(e),
            }
        }
        other => {
            // Valid / Invalid / Aborted: nothing for the gate to do.
            let _ = other;
            Outcome::Continue
        }
    }
}
