//! Peer-sourced mesh OTA — the watch as a LEAF (#86, the #36 epic's killer
//! service): a smol gateway serves firmware over ESP-NOW; the watch receives,
//! verifies, and stages it with **no server and no WiFi**.
//!
//! Division of labor:
//!  - `ota-proto` (pure, host-tested): frame codec, ed25519 verify-before-
//!    trust, the anti-rollback gate, and [`ota_proto::leaf::LeafSession`] —
//!    the windowed-NAK state machine (17 tests).
//!  - THIS module: the flash [`ImageSink`] over the watch's GuardedFlash +
//!    partition machinery (ota_http's slot discipline, sector-per-lock), the
//!    heap window buffer with decline-on-pressure, and the demux driver
//!    `smol_mesh::handle_rx` + the main loop call into.
//!
//! Watch-specific decisions, each with its reason:
//!  - **Window buffer is heap, per-session** (`try_reserve` 14,784 B): a
//!    `.bss` static would eat the story combos' +2.4 KB stack margin (#65).
//!    Under heap pressure the watch DECLINES the session with a log — the
//!    gateway retries its OTAM periodically, so a declined session self-heals
//!    once pressure clears.
//!  - **Flash writes are sector-per-try_lock**: same-task callers never hold
//!    the FlashMutex (the main loop is sequential), so contention means a
//!    concurrent WiFi-OTA download — aborting one of two simultaneous OTAs is
//!    correct, and a 4 KB program per lock keeps any co-running audio inside
//!    its DMA budget (#75's erase-freeze lesson).
//!  - **Chip refusal before the first write**: the esp-idf header's chip_id
//!    (bytes 12..14) against `board::ESP_IMAGE_CHIP_ID`, returning through
//!    the same terminal path as everything else — a wrong-arm image costs a
//!    download, never a slot.
//!  - **Never reboots by itself** (ota_http's rule): `Complete` stages the
//!    slot and latches [`staged_build`]; the reboot policy stays with main.
//!
//! Anti-rollback floor: v1 passes the RUNNING build as the floor (an image
//! must be strictly newer than what runs). Persisting a separate floor across
//! downgrades is a follow-up (a config byte), documented not silently absent.

use embedded_storage::{ReadStorage, Storage};
use esp_bootloader_esp_idf::ota::{Ota, OtaImageState};
use esp_bootloader_esp_idf::partitions::{
    self, AppPartitionSubType, DataPartitionSubType, PartitionType,
};
use esp_println::println;
use sha2::{Digest, Sha256};

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};

use ota_proto::leaf::{ImageSink, LeafAction, LeafSession, MetaVerdict};
use ota_proto::{OtaFrame, WINDOW_BYTES};

/// The esp-idf app-image magic (first byte of every valid image).
const ESP_IMAGE_MAGIC: u8 = 0xE9;

/// Set when a mesh-received image has been verified + staged: the build
/// number main surfaces ("reboot to apply", same UX as the WiFi path).
/// 0 = nothing staged.
pub static MESH_STAGED_BUILD: AtomicU32 = AtomicU32::new(0);

/// The running build id (unix-seconds), shared with the WiFi OTA path.
fn running_build() -> u32 {
    crate::net::ota_http::BUILD_EPOCH as u32
}

/// Flash sink for one mesh-OTA session. Holds RAW geometry (offsets) rather
/// than partition-table borrows — the table is re-read at finalize for the
/// otadata flip, so a table borrow never lives across the session.
pub struct MeshImageSink {
    flash: &'static crate::FlashMutex,
    target_off: u32,
    target_len: u32,
    target_sub: AppPartitionSubType,
    written: u32,
    chip_checked: bool,
}

impl MeshImageSink {
    /// Discover the inactive slot (ota_http's exact selection) and open a
    /// sink onto it. `None` (with a log) when the layout refuses — factory
    /// tables without a second slot land here, not in a panic.
    pub fn open(flash: &'static crate::FlashMutex) -> Option<Self> {
        let mut pt_mem = [0u8; partitions::PARTITION_TABLE_MAX_LEN];
        let mut f = flash.try_lock().ok()?;
        let pt = partitions::read_partition_table(&mut *f, &mut pt_mem).ok()?;
        let booted = pt.booted_partition().ok()??;
        let current = match booted.partition_type() {
            PartitionType::App(sub) => sub,
            _ => return None,
        };
        let target = match current {
            AppPartitionSubType::Ota0 | AppPartitionSubType::Factory => {
                AppPartitionSubType::Ota1
            }
            AppPartitionSubType::Ota1 => AppPartitionSubType::Ota0,
            _ => return None,
        };
        let entry = pt.find_partition(PartitionType::App(target)).ok()??;
        if entry.offset() == booted.offset() {
            return None; // single-slot layout — nothing safe to write
        }
        Some(Self {
            flash,
            target_off: entry.offset(),
            target_len: entry.len(),
            target_sub: target,
            written: 0,
            chip_checked: false,
        })
    }

    pub fn slot_size(&self) -> u32 {
        self.target_len
    }
}

impl ImageSink for MeshImageSink {
    fn feed_window(&mut self, bytes: &[u8]) -> bool {
        if !self.chip_checked {
            // Refuse a wrong-arm image before the FIRST write (morpheus's
            // check, mesh edition): magic, then the chip id at bytes 12..14.
            if bytes.first() != Some(&ESP_IMAGE_MAGIC) {
                println!("[MESH-OTA] refused: not an esp app image (bad magic)");
                return false;
            }
            if bytes.len() >= 14 {
                let img_chip = u16::from_le_bytes([bytes[12], bytes[13]]);
                if img_chip != crate::board::ESP_IMAGE_CHIP_ID {
                    println!(
                        "[MESH-OTA] refused: chip mismatch (image {:#06x}, board {:#06x})",
                        img_chip,
                        crate::board::ESP_IMAGE_CHIP_ID
                    );
                    return false;
                }
            }
            self.chip_checked = true;
        }
        if self.written + bytes.len() as u32 > self.target_len {
            println!("[MESH-OTA] refused: image overruns the slot");
            return false;
        }
        // Sector-per-lock (#75 discipline): 4 KB program per hold, so a
        // concurrent config save or audio tick never waits out a whole window.
        let mut off = 0usize;
        while off < bytes.len() {
            let end = (off + 4096).min(bytes.len());
            let Ok(mut f) = self.flash.try_lock() else {
                // A concurrent WiFi OTA holds the flash — two simultaneous
                // OTAs is one too many; abort the mesh one.
                println!("[MESH-OTA] flash busy (WiFi OTA in flight?) - aborting");
                return false;
            };
            if f
                .write(self.target_off + self.written, &bytes[off..end])
                .is_err()
            {
                println!("[MESH-OTA] flash write failed at {} B", self.written);
                return false;
            }
            self.written += (end - off) as u32;
            off = end;
        }
        true
    }

    fn finalize(&mut self, size: u32, sha256: &[u8; 32]) -> bool {
        if self.written < size {
            return false;
        }
        // Readback SHA-256 over exactly `size` bytes, 4 KB per lock.
        let mut hasher = Sha256::new();
        let mut buf = [0u8; 4096];
        let mut done = 0u32;
        while done < size {
            let n = ((size - done) as usize).min(buf.len());
            let Ok(mut f) = self.flash.try_lock() else {
                println!("[MESH-OTA] flash busy during verify - aborting");
                return false;
            };
            if f.read(self.target_off + done, &mut buf[..n]).is_err() {
                return false;
            }
            drop(f);
            hasher.update(&buf[..n]);
            done += n as u32;
        }
        let digest = hasher.finalize();
        if digest[..] != sha256[..] {
            println!("[MESH-OTA] readback SHA mismatch - discarded (good slot intact)");
            return false;
        }
        // Stage: otadata flip to the target slot, state New (the bootloader's
        // rollback machinery takes it from there — ota_http's exact block).
        let mut pt_mem = [0u8; partitions::PARTITION_TABLE_MAX_LEN];
        let Ok(mut f) = self.flash.try_lock() else {
            return false;
        };
        let Ok(pt) = partitions::read_partition_table(&mut *f, &mut pt_mem) else {
            return false;
        };
        let Ok(Some(otadata)) =
            pt.find_partition(PartitionType::Data(DataPartitionSubType::Ota))
        else {
            return false;
        };
        let region = otadata.as_embedded_storage(&mut *f);
        let Ok(mut ota) = Ota::new(region, 2) else {
            return false;
        };
        if ota.set_current_app_partition(self.target_sub).is_err() {
            return false;
        }
        if ota.set_current_ota_state(OtaImageState::New).is_err() {
            return false;
        }
        println!("[MESH-OTA] image verified + staged - reboot to apply");
        true
    }
}

/// The per-watch mesh-OTA driver: session + window buffer + sink, owned by
/// the mesh (one concurrent session, like smol's leaves).
pub struct MeshOta {
    pub session: LeafSession,
    win_buf: Option<Vec<u8>>,
    sink: Option<MeshImageSink>,
}

impl MeshOta {
    pub const fn new() -> Self {
        Self {
            session: LeafSession::new(),
            win_buf: None,
            sink: None,
        }
    }

    /// Drive one parsed OTA frame. Returns an OTAN to UNICAST to the
    /// session's gateway when the machine asks for one.
    pub fn on_frame(
        &mut self,
        frame: OtaFrame<'_>,
        src: [u8; 6],
        my_id: u8,
        now_ms: u64,
        flash: &'static crate::FlashMutex,
        out: &mut [u8],
    ) -> Option<usize> {
        match frame {
            OtaFrame::Meta {
                target,
                session,
                m,
                sig,
            } => {
                let running = running_build();
                // Slot size needs a sink probe; open lazily only when the
                // pure verdict could accept (cheap rejections first).
                let probe = MeshImageSink::open(flash);
                let slot = probe.as_ref().map_or(0, |s| s.slot_size());
                match self.session.evaluate_meta(
                    target, session, m, sig, my_id, running, running, slot,
                ) {
                    MetaVerdict::Accept {
                        build,
                        size,
                        sha256,
                    } => {
                        // Resource commit: heap window buffer,
                        // decline-on-pressure (#65 — never a .bss static).
                        let mut buf: Vec<u8> = Vec::new();
                        if buf.try_reserve_exact(WINDOW_BYTES).is_err() {
                            println!(
                                "[MESH-OTA] declined session {session}: heap pressure \
                                 (need {} B window) - gateway will re-offer",
                                WINDOW_BYTES
                            );
                            return None;
                        }
                        buf.resize(WINDOW_BYTES, 0);
                        let Some(sink) = probe else {
                            println!("[MESH-OTA] declined: no inactive slot (factory layout?)");
                            return None;
                        };
                        self.win_buf = Some(buf);
                        self.sink = Some(sink);
                        self.session.arm(session, build, size, sha256, src, now_ms);
                        println!(
                            "[MESH-OTA] session {session} armed - build {build} ({size} B) \
                             from {src:02x?}"
                        );
                        None
                    }
                    MetaVerdict::BadSignature => {
                        println!("[MESH-OTA] OTAM sig FAILED - ignored (no state, no flash)");
                        None
                    }
                    MetaVerdict::Rejected(why) => {
                        println!("[MESH-OTA] OTAM rejected: {why:?}");
                        None
                    }
                    _ => None,
                }
            }
            OtaFrame::Data {
                target,
                session,
                seq,
                payload,
            } => {
                let (Some(buf), Some(sink)) = (self.win_buf.as_mut(), self.sink.as_mut())
                else {
                    return None;
                };
                let action = self.session.on_data(
                    target, session, seq, payload, src, my_id, now_ms, buf, sink, out,
                );
                self.apply(action, out)
            }
            OtaFrame::Nak { .. } => None, // leaf→gateway traffic; not for us
        }
    }

    /// Periodic pump (NAK cadence, deadlines, finalize acks). Call ~every
    /// main-loop tick; cheap no-op when idle.
    pub fn tick(&mut self, my_id: u8, now_ms: u64, out: &mut [u8]) -> Option<usize> {
        if !self.session.is_active() {
            return None;
        }
        let action = self.session.tick(my_id, now_ms, out);
        self.apply(action, out)
    }

    fn apply(&mut self, action: LeafAction, _out: &mut [u8]) -> Option<usize> {
        match action {
            LeafAction::Nak(n) => Some(n),
            LeafAction::Complete { build } => {
                MESH_STAGED_BUILD.store(build, Ordering::Relaxed);
                println!("[MESH-OTA] build {build} staged via mesh - reboot to apply");
                self.release();
                None
            }
            LeafAction::Abort => {
                self.release();
                None
            }
            LeafAction::None => None,
        }
    }

    /// Return the ~14.8 KB window buffer to the heap the moment the session
    /// ends, success or not.
    fn release(&mut self) {
        self.win_buf = None;
        self.sink = None;
    }
}
