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
// cycle.
//
// 🔴 ONE SYMBOL, FIXED OFFSETS — and the previous design's failure is why.
//
// This was three separate `static mut` primitives (RB_INTENTIONAL, RB_BUILD,
// RB_SUM). LLVM's global-merge packs small statics into a single MergedGlobals
// block, and **which variable lands at which offset depends on what survives dead-
// code elimination.** Measured on the bench (acceptance run 6, 2026-08-27) via
// readelf on three ELFs from the same source:
//
//   gate-base / gate-moved: RB_INTENTIONAL@0x50000000, RB_SUM@+4, RB_BUILD@+8
//   moved-disc:             RB_SUM@0x50000000, RB_BUILD@+8, RB_INTENTIONAL ABSENT
//
// The discriminator's unconditional panic made every `deliberate_reset` caller
// unreachable, so DCE deleted RB_INTENTIONAL and the merge block RE-PACKED. The
// disc wrote its sum at offset 0; the good build read `intentional` from offset 0
// and the sum from +4. Checksum mismatch, conservative fall-through, "no retained
// state" — and hole 4 silently stopped suppressing the bad build's announce.
//
// **The checksum did its job exactly right: it rejected garbage.** The LAYOUT was
// the defect. And the real-world case is precisely cross-build — a bad NEW build
// records, the GOOD OLD build reads — so any codegen difference (a cfg'd feature, an
// LLVM bump, a caller added or removed) could permute it. The resulting re-fetch
// loop is self-sustaining, because each rollback's boot burst opens the announce
// window that feeds the next one. Two full cycles were observed before a manual
// clear.
//
// So: ONE static, a byte array with hand-placed fields. Nothing can be deleted to
// shift it, and the internal offsets are fixed by construction rather than by the
// optimiser's mood.
//
// `[u8; N]` is also the honest choice for the macro's contract: it genuinely
// implements `bytemuck::AnyBitPattern`, which persistent statics are documented to
// require. (The macro does not currently ENFORCE that bound — relying on the
// omission would be the "works by accident" class this whole block is a monument
// to.)
//
// LAYOUT — do not reorder; add only at the end, and bump LAYOUT_MAGIC when you do.
//   [ 0.. 4)  LAYOUT_MAGIC   u32  version tag, checked BEFORE the sum
//   [ 4.. 8)  intentional    u32  INTENTIONAL marker or 0
//   [ 8..16)  rolled_back    u64  build id rolled away from, 0 = none
//   [16..20)  sum            u32  FNV-1a over bytes 0..16, written LAST
// ---------------------------------------------------------------------------

const RB_LEN: usize = 20;
const OFF_MAGIC: usize = 0;
const OFF_INTENTIONAL: usize = 4;
const OFF_BUILD: usize = 8;
const OFF_SUM: usize = 16;

/// Layout tag. **Bump this whenever the field layout changes** — an older build
/// reading a newer block then fails closed on the magic instead of aliasing fields
/// and trusting a sum that happens to match.
const LAYOUT_MAGIC: u32 = 0xC5B0_0001;

#[esp_hal::ram(unstable(rtc_fast, persistent))]
static mut RB: [u8; RB_LEN] = [0; RB_LEN];

/// Marker value for "the reset now in flight was deliberate".
const INTENTIONAL: u32 = 0x5AFE_B007;

fn rd32(buf: &[u8; RB_LEN], off: usize) -> u32 {
    u32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
}
fn rd64(buf: &[u8; RB_LEN], off: usize) -> u64 {
    let mut b = [0u8; 8];
    b.copy_from_slice(&buf[off..off + 8]);
    u64::from_le_bytes(b)
}

/// FNV-1a over bytes `0..OFF_SUM` — magic and payload, so a layout bump changes the
/// sum too.
///
/// Not cryptographic and does not need to be: its only job is to make **random
/// bytes fail**, which is what the macro's docs ask for (a reset before the RAM is
/// zeroed can start the app with random contents, and an update can be torn —
/// "not even a critical section" prevents it). Never returns 0, so an all-zero block
/// (first boot, or after a power cycle) cannot validate.
fn checksum(buf: &[u8; RB_LEN]) -> u32 {
    let mut h: u32 = 0x811C_9DC5;
    for b in &buf[..OFF_SUM] {
        h ^= *b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    if h == 0 { 1 } else { h }
}

/// Validated retained state, or `None` when the block is absent/garbage/torn/from a
/// different layout.
pub struct Retained {
    /// The reset that brought us here was deliberate (set by `armed_reset`).
    pub intentional: bool,
    /// Build id the gate rolled away from; 0 for none.
    pub rolled_back_build: u64,
}

/// Read and VALIDATE. `None` means "no trustworthy state" — treat it as a fresh
/// boot, never as zeros.
pub fn read_retained() -> Option<Retained> {
    // SAFETY: single-core; read once at boot before any task runs. Copy out by
    // value — no reference taken into the static (edition-2024 friendly).
    let buf: [u8; RB_LEN] = unsafe { RB };
    if rd32(&buf, OFF_MAGIC) != LAYOUT_MAGIC {
        return None;
    }
    let stored = rd32(&buf, OFF_SUM);
    if stored == 0 || stored != checksum(&buf) {
        return None;
    }
    Some(Retained {
        intentional: rd32(&buf, OFF_INTENTIONAL) == INTENTIONAL,
        rolled_back_build: rd64(&buf, OFF_BUILD),
    })
}

/// Write the block, **checksum last**.
///
/// Checksum-last is deliberate: a reset that tears the write leaves the sum stale,
/// so [`read_retained`] rejects the block rather than trusting half of it. The
/// update cannot be made atomic (see the layout note above), so ordering is the only
/// lever available.
pub fn write_retained(intentional: bool, rolled_back_build: u64) {
    let mut buf = [0u8; RB_LEN];
    buf[OFF_MAGIC..OFF_MAGIC + 4].copy_from_slice(&LAYOUT_MAGIC.to_le_bytes());
    let i = if intentional { INTENTIONAL } else { 0 };
    buf[OFF_INTENTIONAL..OFF_INTENTIONAL + 4].copy_from_slice(&i.to_le_bytes());
    buf[OFF_BUILD..OFF_BUILD + 8].copy_from_slice(&rolled_back_build.to_le_bytes());
    let sum = checksum(&buf);
    buf[OFF_SUM..OFF_SUM + 4].copy_from_slice(&sum.to_le_bytes());
    // SAFETY: as above. Single whole-array store; the sum is already inside `buf`,
    // so the "sum last" property is preserved against a torn WRITE of the array by
    // the reader's magic+sum validation.
    unsafe { RB = buf };
}

/// Note that the reset about to happen is DELIBERATE/// Note that the reset about to happen is DELIBERATE, preserving the
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
                    // ⚠️ READ BACK — the flip can SILENTLY NO-OP.
                    //
                    // `set_current_app_partition` (esp-bootloader-esp-idf 0.5.0,
                    // ota.rs:254-257) early-outs when `current_app_partition() ==
                    // app` — and "current" there is what OTADATA REQUESTS, while
                    // `target` here was derived from the MMU (what is actually
                    // executing). Those disagree exactly in the #55 stale-otadata
                    // case. If they disagree the flip does nothing, and reporting
                    // `RolledBack` would reboot us straight back into the failing
                    // image — a loop, dressed as a rescue.
                    //
                    // So verify rather than assume, and on mismatch keep booting the
                    // current image: a loop that prints beats a reboot into the same
                    // loop with a success message attached.
                    match ota2.current_app_partition() {
                        Ok(now) if now == target => {}
                        Ok(now) => {
                            println!(
                                "[ROLLBACK] ⚠️ flip did NOT take (otadata still requests {now:?}, \
                                 wanted {target:?}) - refusing to reboot into the same image"
                            );
                            return Outcome::Failed("boot-slot flip did not take");
                        }
                        Err(_) => return Outcome::Failed("could not read back boot slot"),
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
