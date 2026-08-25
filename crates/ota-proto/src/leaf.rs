//! The mesh-OTA LEAF session state machine — smol's `ota_mesh.rs::OtaLeafSession`
//! (leaf half, @ the vendoring baseline in lib.rs) mirrored with two deliberate
//! watch adaptations, both about WHERE resources live rather than protocol:
//!
//! 1. **The flash writer is a trait** ([`ImageSink`]) instead of smol's
//!    concrete `LeafImageWriter` — the watch's sink wraps its GuardedFlash +
//!    partition machinery (src/net/), and the state machine stays pure and
//!    host-testable (a `Vec` sink in the tests).
//! 2. **The 14,784-byte window buffer is CALLER-OWNED** instead of smol's
//!    `static` — on the watch a `.bss` static that size would eat the story
//!    combos' +2.4 KB stack margin outright (the #65 geometry), so the buffer
//!    is heap-allocated per session and the watch may DECLINE a session under
//!    heap pressure: [`LeafSession::evaluate_meta`] (pure verdict) is split
//!    from [`LeafSession::arm`] (state commit) precisely so the caller can
//!    try-allocate between them.
//!
//! Protocol semantics (verify-before-trust, the anti-rollback gate, windowed
//! NAKs where the all-zero bitmap is the only positive ack, the finalize-ack
//! choreography, deadlines) are UNCHANGED — the no-fork rule. Timing consts
//! are smol's values verbatim.

use crate::{
    encode_otan, gate, verify_signature, window_full_mask, Announce, Reject, CHUNK_PAYLOAD,
    OTAN_BITMAP_BYTES, WINDOW_BYTES, WINDOW_CHUNKS,
};

/// Chunks needed for `size` bytes (last chunk short).
pub fn total_chunks(size: u32) -> u32 {
    size.div_ceil(CHUNK_PAYLOAD as u32)
}

// ---- timing (smol's values verbatim) ---------------------------------------
/// Min gap between OTANs (both gap-NAKs and finalize-acks).
pub const LEAF_IDLE_NAK_MS: u64 = 500;
/// Hard cap on a whole session.
pub const LEAF_SESSION_MAX_MS: u64 = 600_000;
/// Grace before the first chunk (the gateway may still be fetching the image).
pub const LEAF_FIRST_CHUNK_GRACE_MS: u64 = 330_000;
/// Mid-session stall: no NEW chunk for this long discards the session.
pub const LEAF_PROGRESS_STALL_MS: u64 = 30_000;
/// Finalize-ack repeats (the gateway reads any as delivered-confirmed).
pub const LEAF_FINALIZE_ACK_MAX: u8 = 4;
/// Window in which those repeats are sent before self-activation.
pub const LEAF_FINALIZE_ACK_WINDOW_MS: u64 = 1_200;

/// Where image bytes go. `feed_window` writes one completed window's bytes at
/// the current append position (the machine feeds windows strictly in order);
/// `finalize` re-reads the written image, verifies `sha256` over exactly
/// `size` bytes, and stages the slot (otadata flip) — true = staged.
pub trait ImageSink {
    fn feed_window(&mut self, bytes: &[u8]) -> bool;
    fn finalize(&mut self, size: u32, sha256: &[u8; 32]) -> bool;
}

/// What the caller must transmit / do. Mirrors smol's enum; `Complete` hands
/// back the build number — the caller owns the reboot policy.
#[derive(Debug, PartialEq, Eq)]
pub enum LeafAction {
    None,
    /// Unicast `out[..len]` (an OTAN) to the gateway's MAC.
    Nak(usize),
    /// Session discarded (bad write / stall / verify fail). Good slot intact.
    Abort,
    /// Image received, SHA-verified, slot staged. Reboot when policy allows.
    Complete { build: u32 },
}

/// The pure verdict on an inbound OTAM, decided BEFORE any resource commit.
#[derive(Debug, PartialEq, Eq)]
pub enum MetaVerdict {
    NotForUs,
    /// Re-send of the live session's OTAM — ignore.
    Dup,
    BadSignature,
    BadManifest,
    Rejected(Reject),
    /// Verify+gate passed: the caller may allocate the window buffer + open
    /// the sink, then [`LeafSession::arm`].
    Accept { build: u32, size: u32, sha256: [u8; 32] },
}

pub struct LeafSession {
    active: bool,
    session_id: u16,
    build: u32,
    size: u32,
    sha256: [u8; 32],
    total_chunks: u32,
    window_base: u32,
    window_recv: u64,
    gateway_mac: [u8; 6],
    session_deadline_ms: u64,
    last_new_chunk_ms: u64,
    last_nak_ms: u64,
    finalize_since_ms: u64,
    finalize_wb: u32,
    finalize_acks_sent: u8,
    finalize_build: u32,
}

impl LeafSession {
    pub const fn new() -> Self {
        Self {
            active: false,
            session_id: 0,
            build: 0,
            size: 0,
            sha256: [0u8; 32],
            total_chunks: 0,
            window_base: 0,
            window_recv: 0,
            gateway_mac: [0u8; 6],
            session_deadline_ms: 0,
            last_new_chunk_ms: 0,
            last_nak_ms: 0,
            finalize_since_ms: 0,
            finalize_wb: 0,
            finalize_acks_sent: 0,
            finalize_build: 0,
        }
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    pub fn gateway_mac(&self) -> [u8; 6] {
        self.gateway_mac
    }

    /// (received_chunks_floor, total_chunks, build) while a session runs.
    pub fn progress(&self) -> Option<(u32, u32, u32)> {
        if !self.active {
            return None;
        }
        Some((self.window_base, self.total_chunks, self.build))
    }

    fn discard(&mut self) {
        *self = Self::new();
    }

    /// Pure OTAM verdict — no state change, no resources.
    /// `running`/`fresh_floor`/`slot_size` feed the anti-rollback [`gate`].
    #[allow(clippy::too_many_arguments)]
    pub fn evaluate_meta(
        &self,
        target: u8,
        session: u16,
        m: &[u8],
        sig: &[u8; 64],
        my_id: u8,
        running: u32,
        fresh_floor: u32,
        slot_size: u32,
    ) -> MetaVerdict {
        if target != my_id {
            return MetaVerdict::NotForUs;
        }
        if self.active && self.session_id == session {
            return MetaVerdict::Dup;
        }
        if !verify_signature(m, sig) {
            return MetaVerdict::BadSignature;
        }
        let Some(ann) = Announce::from_signed(m, sig) else {
            return MetaVerdict::BadManifest;
        };
        if let Err(why) = gate(ann.build, ann.size, running, fresh_floor, slot_size) {
            return MetaVerdict::Rejected(why);
        }
        MetaVerdict::Accept {
            build: ann.build,
            size: ann.size,
            sha256: ann.sha256,
        }
    }

    /// Commit the session state (the caller has the buffer + sink in hand).
    pub fn arm(
        &mut self,
        session: u16,
        build: u32,
        size: u32,
        sha256: [u8; 32],
        src: [u8; 6],
        now: u64,
    ) {
        *self = Self::new();
        self.active = true;
        self.session_id = session;
        self.build = build;
        self.size = size;
        self.sha256 = sha256;
        self.total_chunks = total_chunks(size);
        self.gateway_mac = src;
        self.session_deadline_ms = now.saturating_add(LEAF_SESSION_MAX_MS);
        self.last_new_chunk_ms = now;
    }

    /// Length in chunks of the window based at `wb`.
    fn window_len(&self, wb: u32) -> u32 {
        (self.total_chunks - wb).min(WINDOW_CHUNKS as u32)
    }

    /// One inbound OTAD. `win_buf` is the caller-owned window buffer
    /// (≥ [`WINDOW_BYTES`]); `sink` receives completed windows in order.
    #[allow(clippy::too_many_arguments)]
    pub fn on_data(
        &mut self,
        target: u8,
        session: u16,
        seq: u16,
        payload: &[u8],
        src: [u8; 6],
        my_id: u8,
        now: u64,
        win_buf: &mut [u8],
        sink: &mut dyn ImageSink,
        out: &mut [u8],
    ) -> LeafAction {
        if target != my_id || !self.active || session != self.session_id {
            return LeafAction::None;
        }
        if src != self.gateway_mac {
            return LeafAction::None; // one gateway per session — spoof guard
        }
        let seq = seq as u32;
        if seq >= self.total_chunks {
            return LeafAction::None;
        }
        let off = seq * CHUNK_PAYLOAD as u32;
        let expected = if seq == self.total_chunks - 1 {
            self.size - off
        } else {
            CHUNK_PAYLOAD as u32
        };
        if payload.len() as u32 != expected || off + payload.len() as u32 > self.size {
            return LeafAction::None;
        }

        let wb = self.window_base;
        if seq < wb {
            // A chunk from an already-advanced window: the gateway missed our
            // advance-ack — re-ack that window (all-zero bitmap).
            let acked_base = (seq / WINDOW_CHUNKS as u32) * WINDOW_CHUNKS as u32;
            let zero = [0u8; OTAN_BITMAP_BYTES];
            let n = encode_otan(my_id, self.session_id, acked_base as u16, &zero, out)
                .unwrap_or(0);
            return LeafAction::Nak(n);
        }
        if seq >= wb + WINDOW_CHUNKS as u32 {
            return LeafAction::None; // future window — gateway advances in order
        }

        let i = (seq - wb) as usize;
        let base = i * CHUNK_PAYLOAD;
        win_buf[base..base + payload.len()].copy_from_slice(payload);
        let bit = 1u64 << i;
        if self.window_recv & bit == 0 {
            self.window_recv |= bit;
            self.last_new_chunk_ms = now;
        }

        let wlen = self.window_len(wb);
        let mask = window_full_mask(wlen);
        if self.window_recv & mask != mask {
            return LeafAction::None; // gaps remain — tick's idle NAK covers them
        }

        // Window complete: flash it, advance, ack.
        let window_bytes =
            (WINDOW_BYTES as u32).min(self.size - wb * CHUNK_PAYLOAD as u32) as usize;
        if !sink.feed_window(&win_buf[..window_bytes]) {
            self.discard();
            return LeafAction::Abort;
        }
        self.window_base = wb + WINDOW_CHUNKS as u32;
        self.window_recv = 0;
        self.last_new_chunk_ms = now;
        self.last_nak_ms = now;

        if self.window_base < self.total_chunks {
            let zero = [0u8; OTAN_BITMAP_BYTES];
            let n = encode_otan(my_id, self.session_id, wb as u16, &zero, out).unwrap_or(0);
            return LeafAction::Nak(n);
        }

        // Last window flashed: verify + stage, then the finalize-ack window.
        if sink.finalize(self.size, &self.sha256) {
            self.finalize_since_ms = now.max(1);
            self.finalize_wb = wb;
            self.finalize_acks_sent = 1;
            self.finalize_build = self.build;
            self.last_nak_ms = now;
            let zero = [0u8; OTAN_BITMAP_BYTES];
            let n = encode_otan(my_id, self.session_id, wb as u16, &zero, out).unwrap_or(0);
            LeafAction::Nak(n)
        } else {
            self.discard();
            LeafAction::Abort
        }
    }

    /// Periodic pump: finalize-ack repeats, stall/deadline discard, gap NAKs.
    pub fn tick(&mut self, my_id: u8, now: u64, out: &mut [u8]) -> LeafAction {
        if !self.active {
            return LeafAction::None;
        }
        if self.finalize_since_ms != 0 {
            let window_open =
                now.saturating_sub(self.finalize_since_ms) < LEAF_FINALIZE_ACK_WINDOW_MS;
            if window_open && self.finalize_acks_sent < LEAF_FINALIZE_ACK_MAX {
                if now.saturating_sub(self.last_nak_ms) < LEAF_IDLE_NAK_MS {
                    return LeafAction::None;
                }
                self.last_nak_ms = now;
                self.finalize_acks_sent = self.finalize_acks_sent.saturating_add(1);
                let zero = [0u8; OTAN_BITMAP_BYTES];
                let n = encode_otan(my_id, self.session_id, self.finalize_wb as u16, &zero, out)
                    .unwrap_or(0);
                return LeafAction::Nak(n);
            }
            let build = self.finalize_build;
            self.discard();
            return LeafAction::Complete { build };
        }
        let stall_ms = if self.window_base == 0 && self.window_recv == 0 {
            LEAF_FIRST_CHUNK_GRACE_MS
        } else {
            LEAF_PROGRESS_STALL_MS
        };
        if now >= self.session_deadline_ms
            || now.saturating_sub(self.last_new_chunk_ms) >= stall_ms
        {
            self.discard();
            return LeafAction::Abort;
        }
        if now.saturating_sub(self.last_nak_ms) < LEAF_IDLE_NAK_MS {
            return LeafAction::None;
        }
        let wb = self.window_base;
        let wlen = self.window_len(wb);
        let mask = window_full_mask(wlen);
        let missing = mask & !self.window_recv;
        if missing == 0 {
            return LeafAction::None;
        }
        self.last_nak_ms = now;
        let n = encode_otan(my_id, self.session_id, wb as u16, &missing.to_le_bytes(), out)
            .unwrap_or(0);
        LeafAction::Nak(n)
    }
}

impl Default for LeafSession {
    fn default() -> Self {
        Self::new()
    }
}
