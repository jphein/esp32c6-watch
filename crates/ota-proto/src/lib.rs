//! Mesh-OTA protocol core — pure, `no_std`, host-testable, ADVERSARIAL.
//!
//! Parses **attacker-controllable** OTA frames off the unauthenticated ESP-NOW
//! mesh, so every parser here is bounds-checked and panic-free (like scan-model):
//! a hostile length/frame returns `None`, never over-reads or panics. Ported from
//! smol `clock/src/{ota,ota_mesh}.rs` (the pure protocol/crypto half; the
//! leaf-receive state machine + flash writer + smol_mesh demux are the on-watch
//! `net/`/main.rs part).
//!
//! Frames (SMOLv1 family, 12-byte tags, ≤250 B ESP-NOW MTU, all LE):
//!  - **OTAM** gw→leaf: signed manifest — M `"build|size|sha256hex"` (≤96 B) + 64-B ed25519 sig.
//!  - **OTAD** gw→leaf: one image chunk — 231-B payload at offset `seq*231`.
//!  - **OTAN** leaf→gw (unicast): windowed NAK — 64-chunk window, 8-B missing bitmap
//!    (all-zero = "window complete, advance" — the only positive ack).
//!
//! Security (brick-safety): verify the ed25519 sig over M **before** trusting any
//! field parsed from M or flashing a byte; then the anti-rollback [`gate`]
//! (build > running ∧ build > floor ∧ 0 < size ≤ slot); then a full SHA-256
//! integrity check on finalize before the otadata flip.

#![no_std]

pub mod leaf;

use sha2::{Digest, Sha256};

// ---- wire constants -------------------------------------------------------

/// Image bytes per OTAD chunk (250 − 12 tag − 3 target − 2 session − 2 seq).
pub const CHUNK_PAYLOAD: usize = 231;
/// Chunks per windowed-NAK window (→ 8-byte / u64 bitmap).
pub const WINDOW_CHUNKS: usize = 64;
/// Bytes in one full window reassembly buffer (64 * 231 = 14 784 ≈ 14.4 KB).
pub const WINDOW_BYTES: usize = WINDOW_CHUNKS * CHUNK_PAYLOAD;
/// Bytes of the OTAN missing-bitmap (one bit per chunk in the window).
pub const OTAN_BITMAP_BYTES: usize = WINDOW_CHUNKS / 8;
/// Max signed-manifest length (M = `"build|size|sha256hex"`).
pub const SIGNED_MSG_MAX: usize = 96;

pub const OTAM_PREFIX: &[u8] = b"SMOLv1 OTAM ";
pub const OTAD_PREFIX: &[u8] = b"SMOLv1 OTAD ";
pub const OTAN_PREFIX: &[u8] = b"SMOLv1 OTAN ";

// ---- node-id (3 ASCII digits) --------------------------------------------

/// Parse a 3-byte ASCII id field ("042" → 42). `None` on non-digit or > 255.
fn parse_id3(b: &[u8]) -> Option<u8> {
    if b.len() != 3 || !b.iter().all(|c| c.is_ascii_digit()) {
        return None;
    }
    core::str::from_utf8(b).ok()?.parse::<u8>().ok()
}

/// Write a u8 id as 3 ASCII digits into `out[..3]`.
fn write_id3(id: u8, out: &mut [u8]) {
    out[0] = b'0' + id / 100;
    out[1] = b'0' + (id / 10) % 10;
    out[2] = b'0' + id % 10;
}

// ---- decoded frames (borrow the RX buffer) --------------------------------

/// A decoded OTA frame. `Meta`/`Data` are gw→leaf; `Nak` is leaf→gw.
#[derive(Debug)]
pub enum OtaFrame<'a> {
    /// Signed session announce: manifest `m` bytes + the 64-byte ed25519 `sig`.
    /// Verify `sig` over `m` BEFORE trusting any field parsed from `m`.
    Meta { target: u8, session: u16, m: &'a [u8], sig: &'a [u8; 64] },
    /// One image chunk: `payload` are the image bytes at offset `seq * CHUNK_PAYLOAD`.
    Data { target: u8, session: u16, seq: u16, payload: &'a [u8] },
    /// Windowed NAK: bit `i` set ⇒ chunk `window_base + i` still missing; all-zero = complete.
    Nak { origin: u8, session: u16, window_base: u16, bitmap: &'a [u8] },
}

/// Parse one OTA frame off the mesh. Panic-free + bounds-checked against hostile
/// lengths (a bad `M_len`, over-long payload/bitmap, or short frame → `None`).
pub fn parse_ota_frame(data: &[u8]) -> Option<OtaFrame<'_>> {
    if let Some(rest) = data.strip_prefix(OTAM_PREFIX) {
        // target[3] session[2] M_len[1] M[M_len] sig[64]
        if rest.len() < 3 + 2 + 1 {
            return None;
        }
        let target = parse_id3(&rest[0..3])?;
        let session = u16::from_le_bytes([rest[3], rest[4]]);
        let m_len = rest[5] as usize;
        if m_len == 0 || m_len > SIGNED_MSG_MAX {
            return None; // hostile M_len can't over-read or blow buffers
        }
        let m_start = 6;
        let sig_start = m_start + m_len;
        let end = sig_start + 64;
        if rest.len() < end {
            return None;
        }
        let m = &rest[m_start..sig_start];
        let sig: &[u8; 64] = rest[sig_start..end].try_into().ok()?;
        return Some(OtaFrame::Meta { target, session, m, sig });
    }
    if let Some(rest) = data.strip_prefix(OTAD_PREFIX) {
        // target[3] session[2] seq[2] payload[..]
        if rest.len() < 3 + 2 + 2 {
            return None;
        }
        let target = parse_id3(&rest[0..3])?;
        let session = u16::from_le_bytes([rest[3], rest[4]]);
        let seq = u16::from_le_bytes([rest[5], rest[6]]);
        let payload = &rest[7..];
        if payload.len() > CHUNK_PAYLOAD {
            return None; // a chunk can never carry more than one payload's worth
        }
        return Some(OtaFrame::Data { target, session, seq, payload });
    }
    if let Some(rest) = data.strip_prefix(OTAN_PREFIX) {
        // origin[3] session[2] window_base[2] bitmap[..OTAN_BITMAP_BYTES]
        if rest.len() < 3 + 2 + 2 {
            return None;
        }
        let origin = parse_id3(&rest[0..3])?;
        let session = u16::from_le_bytes([rest[3], rest[4]]);
        let window_base = u16::from_le_bytes([rest[5], rest[6]]);
        let bitmap = &rest[7..];
        if bitmap.len() > OTAN_BITMAP_BYTES {
            return None;
        }
        return Some(OtaFrame::Nak { origin, session, window_base, bitmap });
    }
    None
}

// ---- encoders (fixed-width, bounded; return bytes written) ----------------

/// Encode an OTAM. `None` if `m` is empty/over-cap or `out` is too small.
pub fn encode_otam(target_id: u8, session: u16, m: &[u8], sig: &[u8; 64], out: &mut [u8]) -> Option<usize> {
    if m.is_empty() || m.len() > SIGNED_MSG_MAX {
        return None;
    }
    let total = OTAM_PREFIX.len() + 3 + 2 + 1 + m.len() + 64;
    if out.len() < total {
        return None;
    }
    let mut n = 0;
    out[..OTAM_PREFIX.len()].copy_from_slice(OTAM_PREFIX);
    n += OTAM_PREFIX.len();
    write_id3(target_id, &mut out[n..n + 3]);
    n += 3;
    out[n..n + 2].copy_from_slice(&session.to_le_bytes());
    n += 2;
    out[n] = m.len() as u8;
    n += 1;
    out[n..n + m.len()].copy_from_slice(m);
    n += m.len();
    out[n..n + 64].copy_from_slice(sig);
    n += 64;
    Some(n)
}

/// Encode an OTAD. `payload` is truncated to [`CHUNK_PAYLOAD`]. `None` if `out` too small.
pub fn encode_otad(target_id: u8, session: u16, seq: u16, payload: &[u8], out: &mut [u8]) -> Option<usize> {
    let plen = payload.len().min(CHUNK_PAYLOAD);
    let total = OTAD_PREFIX.len() + 3 + 2 + 2 + plen;
    if out.len() < total {
        return None;
    }
    let mut n = 0;
    out[..OTAD_PREFIX.len()].copy_from_slice(OTAD_PREFIX);
    n += OTAD_PREFIX.len();
    write_id3(target_id, &mut out[n..n + 3]);
    n += 3;
    out[n..n + 2].copy_from_slice(&session.to_le_bytes());
    n += 2;
    out[n..n + 2].copy_from_slice(&seq.to_le_bytes());
    n += 2;
    out[n..n + plen].copy_from_slice(&payload[..plen]);
    n += plen;
    Some(n)
}

/// Encode an OTAN. `bitmap` is truncated to [`OTAN_BITMAP_BYTES`]. `None` if `out` too small.
pub fn encode_otan(origin_id: u8, session: u16, window_base: u16, bitmap: &[u8], out: &mut [u8]) -> Option<usize> {
    let blen = bitmap.len().min(OTAN_BITMAP_BYTES);
    let total = OTAN_PREFIX.len() + 3 + 2 + 2 + blen;
    if out.len() < total {
        return None;
    }
    let mut n = 0;
    out[..OTAN_PREFIX.len()].copy_from_slice(OTAN_PREFIX);
    n += OTAN_PREFIX.len();
    write_id3(origin_id, &mut out[n..n + 3]);
    n += 3;
    out[n..n + 2].copy_from_slice(&session.to_le_bytes());
    n += 2;
    out[n..n + 2].copy_from_slice(&window_base.to_le_bytes());
    n += 2;
    out[n..n + blen].copy_from_slice(&bitmap[..blen]);
    n += blen;
    Some(n)
}

/// "All chunks present" mask for a window of `len` chunks (`len` ≤ 64).
pub fn window_full_mask(len: u32) -> u64 {
    if len >= 64 {
        u64::MAX
    } else {
        (1u64 << len) - 1
    }
}

/// Decode a ≤8-byte LE OTAN missing-bitmap into a `u64` (bit `i` ⇒ chunk `base+i` missing).
pub fn bitmap_to_u64(b: &[u8]) -> u64 {
    let mut a = [0u8; 8];
    let n = b.len().min(8);
    a[..n].copy_from_slice(&b[..n]);
    u64::from_le_bytes(a)
}

// ---- signed manifest ------------------------------------------------------

/// A parsed OTA manifest. `build` binds the sig (anti rollback/mislabel replay).
#[derive(Clone, Copy, Debug)]
pub struct Announce {
    pub build: u32,
    pub size: u32,
    pub sha256: [u8; 32],
    sig: [u8; 64],
    signed_msg: [u8; SIGNED_MSG_MAX],
    signed_len: usize,
}

impl Announce {
    /// The 64-byte ed25519 signature.
    pub fn sig(&self) -> &[u8; 64] {
        &self.sig
    }
    /// The exact signed manifest bytes M = `"build|size|sha256hex"` (verify sig over THIS).
    pub fn signed_msg(&self) -> &[u8] {
        &self.signed_msg[..self.signed_len]
    }

    /// Reconstruct from a signed manifest `m` = `"build|size|sha256hex"` + its 64-byte
    /// `sig` (the mesh OTAM/ODEL form — no MQTT `OTA|…|url` wrapper). Panic-free — `None`
    /// on any malformed field or `m` over-cap. `splitn(3)` fail-closes a 4th field
    /// (trailing bytes land in the sha slot and fail the 64-hex parse).
    pub fn from_signed(m: &[u8], sig: &[u8; 64]) -> Option<Announce> {
        if m.is_empty() || m.len() > SIGNED_MSG_MAX {
            return None;
        }
        let s = core::str::from_utf8(m).ok()?;
        let mut it = s.splitn(3, '|');
        let build: u32 = it.next()?.parse().ok()?;
        let size: u32 = it.next()?.parse().ok()?;
        let sha256 = parse_hex_n::<32>(it.next()?)?;
        let mut signed_msg = [0u8; SIGNED_MSG_MAX];
        signed_msg[..m.len()].copy_from_slice(m);
        Some(Announce { build, size, sha256, sig: *sig, signed_msg, signed_len: m.len() })
    }
}

/// `N*2` hex chars → `N` bytes. `None` on wrong length or a non-hex char. Panic-free.
fn parse_hex_n<const N: usize>(hex: &str) -> Option<[u8; N]> {
    let b = hex.as_bytes();
    if b.len() != N * 2 {
        return None;
    }
    let mut out = [0u8; N];
    let mut i = 0;
    while i < N {
        out[i] = (hexval(b[i * 2])? << 4) | hexval(b[i * 2 + 1])?;
        i += 1;
    }
    Some(out)
}

fn hexval(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

// ---- ed25519 verify -------------------------------------------------------

/// Default fleet root-of-trust — the PUBLIC ed25519 verify key (smol's fleet key;
/// PUBLIC by design). The watch can swap in its own via [`verify_signature_with`].
/// Private key lives only in Vaultwarden; rotating = a firmware rebuild.
pub const OTA_SIGNING_PUBKEY: [u8; 32] = [
    0x77, 0x4f, 0x8a, 0xd7, 0x1d, 0x37, 0x52, 0xff, 0xe8, 0xf9, 0x0a, 0x7b, 0xde, 0x1c, 0x1e, 0x7d,
    0x33, 0x4b, 0x55, 0xcd, 0x9a, 0xce, 0x40, 0xe4, 0xdf, 0x2b, 0x5f, 0x5b, 0xd5, 0xf7, 0x67, 0x09,
];

/// Ed25519-verify `signed_msg` against a caller-supplied 32-byte public key.
/// Returns FALSE on ANY error (bad key/sig encoding, or a failed check) — fail-closed.
/// Per-arch backends (see Cargo.toml's note): the SEMANTICS are identical —
/// RFC 8032 verify, fail-closed on any malformed input. dalek's verify_strict
/// additionally rejects small-order/mixed-order keys, a strictly SMALLER
/// accept set; the fleet's real signing key is a proper prime-order point, so
/// every legitimately-signed manifest passes both backends.
#[cfg(not(target_arch = "xtensa"))]
pub fn verify_signature_with(pubkey: &[u8; 32], signed_msg: &[u8], sig: &[u8; 64]) -> bool {
    let Ok(pk) = ed25519_compact::PublicKey::from_slice(pubkey) else {
        return false;
    };
    let Ok(s) = ed25519_compact::Signature::from_slice(sig) else {
        return false;
    };
    pk.verify(signed_msg, &s).is_ok()
}
#[cfg(target_arch = "xtensa")]
pub fn verify_signature_with(pubkey: &[u8; 32], signed_msg: &[u8], sig: &[u8; 64]) -> bool {
    let Ok(vk) = ed25519_dalek::VerifyingKey::from_bytes(pubkey) else {
        return false;
    };
    let s = ed25519_dalek::Signature::from_bytes(sig);
    vk.verify_strict(signed_msg, &s).is_ok()
}

/// Ed25519-verify against [`OTA_SIGNING_PUBKEY`]. Fail-closed. Call at the integrity
/// gate BEFORE trusting the manifest or flashing.
pub fn verify_signature(signed_msg: &[u8], sig: &[u8; 64]) -> bool {
    verify_signature_with(&OTA_SIGNING_PUBKEY, signed_msg, sig)
}

// ---- anti-rollback freshness gate -----------------------------------------

/// Why a manifest was refused (logged; the sad path never panics or touches flash).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reject {
    /// `build <= running` — downgrade or stale-retained replay.
    NotNewer,
    /// `build <= fresh_floor` — signed-intermediate replay below the anti-rollback floor.
    BelowFloor,
    /// `size` is 0 or larger than the target slot.
    BadSize,
}

/// Anti-rollback gate (mesh form — no URL/host; the mesh has no fetch URL, unlike the
/// WiFi path). Accept iff `build > running` ∧ `build > fresh_floor` ∧ `0 < size ≤ slot_size`.
/// Signature validity is the CALLER's precondition (verify_signature over M first).
/// `Ok(())` means "safe to receive + eventually flash".
pub fn gate(build: u32, size: u32, running: u32, fresh_floor: u32, slot_size: u32) -> Result<(), Reject> {
    if build <= running {
        return Err(Reject::NotNewer);
    }
    if build <= fresh_floor {
        return Err(Reject::BelowFloor);
    }
    if size == 0 || size > slot_size {
        return Err(Reject::BadSize);
    }
    Ok(())
}

// ---- sha-256 integrity ----------------------------------------------------

/// Streaming SHA-256 accumulator for the finalize integrity gate. Feed each flashed
/// chunk with [`Sha256Ctx::update`]; [`Sha256Ctx::finish`] → the 32-byte digest to
/// compare against [`Announce::sha256`] BEFORE the otadata flip.
pub struct Sha256Ctx(Sha256);

impl Default for Sha256Ctx {
    fn default() -> Self {
        Self::new()
    }
}

impl Sha256Ctx {
    pub fn new() -> Self {
        Self(Sha256::new())
    }
    pub fn update(&mut self, data: &[u8]) {
        self.0.update(data);
    }
    pub fn finish(self) -> [u8; 32] {
        self.0.finalize().into()
    }
}

/// One-shot SHA-256 (tests / small inputs).
pub fn sha256(data: &[u8]) -> [u8; 32] {
    let mut c = Sha256Ctx::new();
    c.update(data);
    c.finish()
}
