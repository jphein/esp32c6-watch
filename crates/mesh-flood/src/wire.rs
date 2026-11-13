//! #13 — the PURE SMOLv1 relay-family wire codec + its ASCII field helpers.
//!
//! **ONE FILE, TWO PATHS, BYTE-IDENTICAL (smol#529).** The canonical copy is smol's
//! `rust/clock/src/net/wire.rs`; the same bytes live in the watch's `crates/mesh-flood/src/wire.rs`
//! (and so in smol's `targets/c6-watch` subtree). smol's `tools/gate.sh` fails on any difference
//! between the two smol paths. Edit the canonical copy, copy the file verbatim to the watch repo,
//! then refresh the subtree: never edit one copy alone. Everything below that names a verifier
//! describes the CANONICAL path.
//!
//! Extracted verbatim from `net/mode.rs` (byte-identical — a pure move, no logic change) so the
//! frame formats are HOST-unit-testable off-target, mirroring the `net/flood.rs` pure split. The
//! bidirectional byte-compat guard in `experiments/relay_compat` `#[path]`-includes the canonical
//! copy and asserts (a) a NEW-code frame parses under a vendored pre-#13 parser and (b) an
//! old-format RELAYACK parses under this matcher — the permanent mixed-fleet / #124-migration
//! insurance.
//!
//! In smol, `mode.rs` re-exports everything here via `use crate::net::wire::*;`, so its call sites
//! are unchanged. No `esp-hal`/`esp-wifi` deps — everything is `&[u8]` in / out.

// ---- codec-shared consts ---------------------------------------------------

/// Relay-bridge tags. RELAY carries a fragment of a leaf's telemetry uplink; RELAYACK is the
/// gateway's per-message received-fragment bitmap. The trailing space on RELAY disambiguates it
/// from RELAYACK at byte 12 (`' '` vs `'A'`), so match order is moot.
pub const RELAY_PREFIX: &[u8] = b"SMOLv1 RELAY "; // + "NNN MMMMM F C " + chunk
pub const RELAYACK_PREFIX: &[u8] = b"SMOLv1 RELAYACK "; // + "MMMMM BBB"
/// #13 multi-hop ACK tag (the R1 flooded RELAYACK2 — kept; the RELAY2 uplink tag was replaced by
/// UP2 in #124). NEW tag — an old firmware `classify()`s it to `None` (harmless). Diverges from
/// `RELAYACK` at byte 15, so `strip_prefix` never confuses them.
pub const RELAYACK2_PREFIX: &[u8] = b"SMOLv1 RELAYACK2 "; // + "TTT MMMMM BBB H"
/// #13 Stage B downlink freshness tags (10-digit `dl_seq` + verbatim `BATT|`/`GRID|` payload).
pub const BATT2_PREFIX: &[u8] = b"SMOLv1 BATT2 ";
pub const GRID2_PREFIX: &[u8] = b"SMOLv1 GRID2 ";
/// #124 generic uplink envelope tag (see the UP2 section below). Diverges from `RELAY` at byte 7
/// (`'U'` vs `'R'`), so `strip_prefix` never confuses it.
pub const UP2_PREFIX: &[u8] = b"SMOLv1 UP2 "; // + "OOO MMMMM H " + <inner frame>

/// Max telemetry payload per RELAY fragment (bytes) — encoders truncate the chunk to this.
pub const RELAY_CHUNK: usize = 64;
/// Max BATT/GRID downlink payload (bytes) — `encode_dl` truncates to this (matches the caches).
pub const BATT_PAYLOAD_MAX: usize = 96;

// ---- fixed-width ASCII field helpers --------------------------------------

/// Write a 5-digit zero-padded decimal (value mod 100000) into `out[..5]`.
pub fn write_u5(v: u32, out: &mut [u8]) {
    let v = v % 100_000;
    out[0] = b'0' + ((v / 10_000) % 10) as u8;
    out[1] = b'0' + ((v / 1_000) % 10) as u8;
    out[2] = b'0' + ((v / 100) % 10) as u8;
    out[3] = b'0' + ((v / 10) % 10) as u8;
    out[4] = b'0' + (v % 10) as u8;
}

/// Write a 10-digit zero-padded decimal into `out[..10]`. 10 digits holds every u32, so no clamp
/// is needed — the full value round-trips through [`parse_u10`]. Filled least-significant-digit first.
pub fn write_u10(mut v: u32, out: &mut [u8]) {
    for i in (0..10).rev() {
        out[i] = b'0' + (v % 10) as u8;
        v /= 10;
    }
}

/// Parse a 3-digit ASCII id (`b"007"` -> 7). Rejects non-digits / short input.
pub fn parse_id(rest: &[u8]) -> Option<u8> {
    if rest.len() < 3 {
        return None;
    }
    let mut val: u16 = 0;
    for &b in &rest[..3] {
        if !b.is_ascii_digit() {
            return None;
        }
        val = val * 10 + (b - b'0') as u16;
    }
    (val <= 255).then_some(val as u8)
}

/// Parse exactly 5 ASCII digits into a u32. Rejects short/non-digit input.
pub fn parse_u5(rest: &[u8]) -> Option<u32> {
    if rest.len() < 5 {
        return None;
    }
    let mut val: u32 = 0;
    for &b in &rest[..5] {
        if !b.is_ascii_digit() {
            return None;
        }
        val = val * 10 + (b - b'0') as u32;
    }
    Some(val)
}

/// Parse exactly 10 ASCII digits into a u32. Accumulates in u64 + range-checks on the way out, so a
/// garbled 10-digit field that exceeds `u32::MAX` is rejected rather than silently wrapping.
pub fn parse_u10(rest: &[u8]) -> Option<u32> {
    if rest.len() < 10 {
        return None;
    }
    let mut val: u64 = 0;
    for &b in &rest[..10] {
        if !b.is_ascii_digit() {
            return None;
        }
        val = val * 10 + (b - b'0') as u64;
    }
    u32::try_from(val).ok()
}

// ---- RELAY / RELAYACK (single-hop) ----------------------------------------

/// Encode a RELAY fragment `"SMOLv1 RELAY NNN MMMMM F C " + <chunk>` into `out`; returns total
/// length (27-byte header + chunk). `chunk` is truncated to [`RELAY_CHUNK`].
pub fn encode_relay(src_id: u8, msgid: u16, frag: u8, count: u8, chunk: &[u8], out: &mut [u8]) -> usize {
    let mut n = 0;
    out[..RELAY_PREFIX.len()].copy_from_slice(RELAY_PREFIX);
    n += RELAY_PREFIX.len();
    out[n] = b'0' + (src_id / 100) % 10;
    out[n + 1] = b'0' + (src_id / 10) % 10;
    out[n + 2] = b'0' + src_id % 10;
    n += 3;
    out[n] = b' ';
    n += 1;
    write_u5(msgid as u32, &mut out[n..]);
    n += 5;
    out[n] = b' ';
    n += 1;
    out[n] = b'0' + frag;
    n += 1;
    out[n] = b' ';
    n += 1;
    out[n] = b'0' + count;
    n += 1;
    out[n] = b' ';
    n += 1;
    let len = chunk.len().min(RELAY_CHUNK);
    out[n..n + len].copy_from_slice(&chunk[..len]);
    n + len
}

/// Parse a RELAY frame into `(src_id, msgid, frag, count, chunk)`, or `None`. `chunk` borrows `data`.
pub fn parse_relay(data: &[u8]) -> Option<(u8, u16, u8, u8, &[u8])> {
    let rest = data.strip_prefix(RELAY_PREFIX)?;
    if rest.len() < 14 {
        return None;
    }
    let src_id = parse_id(&rest[0..3])?;
    let msgid = u16::try_from(parse_u5(&rest[4..9])?).ok()?;
    if !rest[10].is_ascii_digit() || !rest[12].is_ascii_digit() {
        return None;
    }
    let frag = rest[10] - b'0';
    let count = rest[12] - b'0';
    Some((src_id, msgid, frag, count, &rest[14..]))
}

/// Encode a `"SMOLv1 RELAYACK MMMMM BBB"` frame into `out`; returns length (25). `BBB` = the
/// 3-digit received-fragment bitmap (0..255).
pub fn encode_relayack(msgid: u16, bitmap: u8, out: &mut [u8]) -> usize {
    let mut n = 0;
    out[..RELAYACK_PREFIX.len()].copy_from_slice(RELAYACK_PREFIX);
    n += RELAYACK_PREFIX.len();
    write_u5(msgid as u32, &mut out[n..]);
    n += 5;
    out[n] = b' ';
    n += 1;
    out[n] = b'0' + (bitmap / 100) % 10;
    out[n + 1] = b'0' + (bitmap / 10) % 10;
    out[n + 2] = b'0' + bitmap % 10;
    n += 3;
    n
}

/// Parse a RELAYACK frame into `(msgid, bitmap)`, or `None`.
pub fn parse_relayack(data: &[u8]) -> Option<(u16, u8)> {
    let rest = data.strip_prefix(RELAYACK_PREFIX)?;
    if rest.len() < 9 {
        return None;
    }
    let msgid = u16::try_from(parse_u5(&rest[0..5])?).ok()?;
    let bitmap = parse_id(&rest[6..9])?;
    Some((msgid, bitmap))
}

// ---- RELAYACK2 (#124/#13 flooded ACK — kept; RELAY2 removed, UP2 replaces it) ----------------

/// Encode a `"SMOLv1 RELAYACK2 TTT MMMMM BBB H"` frame into `out`; returns length (32).
pub fn encode_relayack2(target: u8, msgid: u16, bitmap: u8, hop: u8, out: &mut [u8]) -> usize {
    let mut n = 0;
    out[..RELAYACK2_PREFIX.len()].copy_from_slice(RELAYACK2_PREFIX);
    n += RELAYACK2_PREFIX.len();
    out[n] = b'0' + (target / 100) % 10;
    out[n + 1] = b'0' + (target / 10) % 10;
    out[n + 2] = b'0' + target % 10;
    n += 3;
    out[n] = b' ';
    n += 1;
    write_u5(msgid as u32, &mut out[n..]);
    n += 5;
    out[n] = b' ';
    n += 1;
    out[n] = b'0' + (bitmap / 100) % 10;
    out[n + 1] = b'0' + (bitmap / 10) % 10;
    out[n + 2] = b'0' + bitmap % 10;
    n += 3;
    out[n] = b' ';
    n += 1;
    out[n] = b'0' + hop;
    n += 1;
    n
}

/// Parse a RELAYACK2 frame into `(target, msgid, bitmap, hop)`, or `None`.
pub fn parse_relayack2(data: &[u8]) -> Option<(u8, u16, u8, u8)> {
    let rest = data.strip_prefix(RELAYACK2_PREFIX)?;
    if rest.len() < 15 {
        return None;
    }
    let target = parse_id(&rest[0..3])?;
    let msgid = u16::try_from(parse_u5(&rest[4..9])?).ok()?;
    let bitmap = parse_id(&rest[10..13])?;
    if !rest[14].is_ascii_digit() {
        return None;
    }
    let hop = rest[14] - b'0';
    Some((target, msgid, bitmap, hop))
}

/// The synthetic MAC a gateway keys a RELAY2 reassembly by — `00:00:00:00:00:<origin>`. A real
/// Espressif STA MAC is never all-zero, so this can't alias a single-hop leaf's real MAC.
/// (#124: reused verbatim for the UP2 gateway-reassembly of an inner RELAY, keyed by origin.)
pub fn synth_origin_mac(origin: u8) -> [u8; 6] {
    [0, 0, 0, 0, 0, origin]
}

// ---- UP2 generic uplink envelope (#124 — REPLACED RELAY2) -----------------
// `SMOLv1 UP2 ` + `"OOO MMMMM H "` + <verbatim inner SMOLv1 frame (RELAY/STAT/DIAG/SCAN)>. A latched
// leaf wraps ANY uplink frame so a stranded leaf's observability (/stat /diag /scan) reaches the
// gateway via the relay — RELAY2 could only carry telemetry. `OOO` = origin id; `MMMMM` = the
// ENVELOPE msgid (per-origin rolling, the FLOOD-DEDUP key — DISTINCT from any inner RELAY msgid,
// which stays the reassembly/ACK key); `H` = hop-limit. Old firmware classify()s UP2 → None
// (harmless) + can't relay anyway → no flag-day (a leaf needing H≥2 was already stranded pre-#13).
// Host-tested in experiments/relay_compat (golden bytes + wrap→unwrap→re-parse-inner + clamp).

/// Max ESP-NOW payload (bytes) — a frame MUST NOT exceed this on the wire.
pub const ESP_NOW_MTU: usize = 250;
/// UP2 envelope overhead: prefix `"SMOLv1 UP2 "` (11) + header `"OOO MMMMM H "` (12) = 23 B.
pub const UP2_OVERHEAD: usize = UP2_PREFIX.len() + 12;
/// Max inner frame a latched leaf may wrap so UP2 fits [`ESP_NOW_MTU`] AFTER the #190 group-MAC
/// auth trailer is appended at the send choke: 250 − 23 − 9 = 218. `encode_up2` CLAMPS the inner
/// to this + never emits > MTU; `send_to` then appends [`MAC_TRAILER_LEN`] so the on-wire frame is
/// ≤ [`ESP_NOW_MTU`]. For a prefix-tolerant inner (DIAG/SCAN are `key=val`), clamping truncates the
/// TAIL fields — the record still parses; the tail that falls off (cfg=/io=/dlseq=/dfwd=, appended
/// last) matters less than the record arriving. (Pre-#190 this was `MTU − UP2_OVERHEAD` = 227.)
pub const UP2_INNER_MAX: usize = ESP_NOW_MTU - UP2_OVERHEAD - MAC_TRAILER_LEN;

/// Encode a UP2 envelope: `"SMOLv1 UP2 " + "OOO MMMMM H " + <inner>`; returns total length (≤ MTU).
/// `env_msgid` is the envelope's own per-origin rolling counter (flood dedup). The inner is CLAMPED
/// to [`UP2_INNER_MAX`] so the frame never exceeds [`ESP_NOW_MTU`] (constraint: never emit > MTU).
pub fn encode_up2(origin: u8, env_msgid: u16, hop: u8, inner: &[u8], out: &mut [u8]) -> usize {
    let mut n = 0;
    out[..UP2_PREFIX.len()].copy_from_slice(UP2_PREFIX);
    n += UP2_PREFIX.len();
    out[n] = b'0' + (origin / 100) % 10;
    out[n + 1] = b'0' + (origin / 10) % 10;
    out[n + 2] = b'0' + origin % 10;
    n += 3;
    out[n] = b' ';
    n += 1;
    write_u5(env_msgid as u32, &mut out[n..]);
    n += 5;
    out[n] = b' ';
    n += 1;
    out[n] = b'0' + hop;
    n += 1;
    out[n] = b' ';
    n += 1;
    let len = inner.len().min(UP2_INNER_MAX);
    out[n..n + len].copy_from_slice(&inner[..len]);
    n + len
}

/// Parse a UP2 envelope into `(origin, env_msgid, hop, inner)`, or `None`. `inner` borrows `data` —
/// the verbatim inner SMOLv1 frame, which the caller re-runs through `parse_frame` + dispatches.
pub fn parse_up2(data: &[u8]) -> Option<(u8, u16, u8, &[u8])> {
    let rest = data.strip_prefix(UP2_PREFIX)?;
    // "OOO MMMMM H " = 12 header bytes (origin 3, sp, env_msgid 5, sp, hop 1, sp), then the inner frame.
    if rest.len() < 12 {
        return None;
    }
    let origin = parse_id(&rest[0..3])?;
    let env_msgid = u16::try_from(parse_u5(&rest[4..9])?).ok()?;
    if !rest[10].is_ascii_digit() {
        return None;
    }
    let hop = rest[10] - b'0';
    Some((origin, env_msgid, hop, &rest[12..]))
}

/// #124 Stage 2 — return a truncation length for `inner` that fits within `max` bytes AND ends on a
/// FIELD BOUNDARY (the last `|` separator at or before `max`), so a wrapped DIAG/SCAN record that
/// overflows [`UP2_INNER_MAX`] never truncates mid `key=val`. The dropped tail (the straddling field
/// plus any fields past it) is what prefix-tolerant parsers would ignore anyway.
///
/// When `inner.len() <= max` it returns `inner.len()` (fits, no clamp). Otherwise it returns the
/// index of the last `|` at position `<= max`, so bytes `[0..i)` are all whole fields (that `|` and
/// the partial field after it are excluded). With no `|` inside `max` (a single oversized field —
/// pathological; real DIAG/SCAN records are `|`-joined) it falls back to `max` (a raw cut). The frame
/// prefix ("SMOLv1 DIAG " + "OOO") holds no `|`, so scanning the whole buffer is safe.
///
/// The result is always `<= max` and `<= inner.len()`; pass `&inner[..clamp_inner_field_boundary(..)]`
/// to `encode_up2` (whose own raw clamp then never fires). Pure + deterministic — host-tested.
pub fn clamp_inner_field_boundary(inner: &[u8], max: usize) -> usize {
    if inner.len() <= max {
        return inner.len();
    }
    // Largest `|` index at or before `max` (max < len here, so `max` indexes a real byte).
    let mut cut = None;
    for (i, &b) in inner.iter().enumerate().take(max + 1) {
        if b == b'|' {
            cut = Some(i);
        }
    }
    cut.unwrap_or(max)
}

// ---- BATT2 / GRID2 (#13 Stage B downlink freshness) -----------------------

/// Encode a downlink freshness frame `<prefix> + "SSSSSSSSSS " + <payload>` into `out`; returns
/// length. `prefix` is [`BATT2_PREFIX`]/[`GRID2_PREFIX`]; `seq` is the 10-digit freshness.
pub fn encode_dl(prefix: &[u8], seq: u32, payload: &[u8], out: &mut [u8]) -> usize {
    let mut n = 0;
    out[..prefix.len()].copy_from_slice(prefix);
    n += prefix.len();
    write_u10(seq, &mut out[n..]);
    n += 10;
    out[n] = b' ';
    n += 1;
    let len = payload.len().min(BATT_PAYLOAD_MAX);
    out[n..n + len].copy_from_slice(&payload[..len]);
    n + len
}

/// Parse a downlink freshness frame with `prefix` into `(seq, payload)`, or `None`. `payload` borrows `data`.
pub fn parse_dl<'a>(prefix: &[u8], data: &'a [u8]) -> Option<(u32, &'a [u8])> {
    let rest = data.strip_prefix(prefix)?;
    if rest.len() < 11 {
        return None;
    }
    let seq = parse_u10(&rest[0..10])?;
    Some((seq, &rest[11..]))
}

// ---- #190 group HMAC-SHA256 transport auth (Fork B / B1) -------------------
//
// An app-layer authentication trailer appended to EVERY SMOLv1 broadcast/unicast frame at the send
// choke and verified-then-stripped before the parser runs on RX. Keyed by a fleet-shared 32-byte
// `GROUP_KEY` (`secrets.rs`, git-ignored). This is AUTHENTICITY, not confidentiality — the payload
// stays plaintext (design §4.3). The ESP-NOW hardware CANNOT encrypt broadcast (#36), so this
// software MAC is the reshaped #190 transport rung. It reuses the same `sha2` already in the OTA
// path — no `esp-hal`/`esp-wifi` deps, so this stays the pure/host-testable codec module
// (`experiments/mac_verify` `#[path]`-includes the canonical copy, mirroring `flood`/`etx`).
//
// Wire layout (trailer, appended after the frame): `… frame … | key-epoch(1 B) | tag(MAC_TAG_LEN)`
//   tag = truncate(HMAC-SHA256(GROUP_KEY[epoch], frame_bytes ‖ epoch), MAC_TAG_LEN)
// The epoch byte is COVERED by the MAC (so a flipped epoch fails), and it also selects the key on
// verify, enabling OTA-able rotation via a two-epoch overlap window (design §4.1/§6).

/// Truncated group-MAC tag length (bytes). 64-bit = online-forgery-only (no offline attack; HMAC
/// key strength is unaffected by output truncation), 2⁻⁶⁴ per attempt ≈ 10¹¹ yr at ESP-NOW frame
/// rates (design §4.1). Uniform fleet-wide for B1; the low-rate-frame 12-B variant is a documented
/// future refinement, not needed for the outsider threat model.
pub const MAC_TAG_LEN: usize = 8;

/// Total group-MAC trailer overhead reserved out of [`ESP_NOW_MTU`]: `key-epoch(1) + tag`.
pub const MAC_TRAILER_LEN: usize = 1 + MAC_TAG_LEN;

/// SHA-256 HMAC block size (bytes).
const HMAC_BLOCK: usize = 64;

/// HMAC-SHA256 over `msg` keyed by `key` → the full 32-byte tag. Hand-rolled on top of `sha2`
/// (no `hmac`/`digest` crate dependency) — RFC 2104 construction, verified against RFC 4231 KATs in
/// `experiments/mac_verify`. Pure + `no_std` + no alloc. Keys longer than the block are hashed
/// first (RFC 2104); smol's 32-byte `GROUP_KEY` takes the short-key path.
pub fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut k0 = [0u8; HMAC_BLOCK];
    if key.len() > HMAC_BLOCK {
        let d = Sha256::digest(key);
        k0[..32].copy_from_slice(&d);
    } else {
        k0[..key.len()].copy_from_slice(key);
    }
    let mut ipad = [0x36u8; HMAC_BLOCK];
    let mut opad = [0x5cu8; HMAC_BLOCK];
    for i in 0..HMAC_BLOCK {
        ipad[i] ^= k0[i];
        opad[i] ^= k0[i];
    }
    let mut inner = Sha256::new();
    inner.update(&ipad[..]);
    inner.update(msg);
    let inner_digest = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(&opad[..]);
    outer.update(&inner_digest[..]);
    let mut tag = [0u8; 32];
    tag.copy_from_slice(&outer.finalize());
    tag
}

/// Constant-time byte-slice equality (folds all bytes into one accumulator — no early return on the
/// first mismatch, so a network attacker learns nothing from tag-compare timing).
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for i in 0..a.len() {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

/// Append the group-MAC trailer to `buf[..len]` (the built frame) IN PLACE; returns the new total
/// length (`len + MAC_TRAILER_LEN`). Writes `epoch` at `buf[len]`, then `truncate(HMAC(key, buf[..=len]), MAC_TAG_LEN)`
/// after it. The caller MUST guarantee `buf.len() >= len + MAC_TRAILER_LEN` (asserted); the frame
/// builders reserve this out of the MTU (`UP2_INNER_MAX`, `OBS_VALUE_MAX`) so it always holds.
pub fn append_group_mac(buf: &mut [u8], len: usize, key: &[u8; 32], epoch: u8) -> usize {
    debug_assert!(buf.len() >= len + MAC_TRAILER_LEN, "buf too small for MAC trailer");
    buf[len] = epoch;
    let tag = hmac_sha256(&key[..], &buf[..len + 1]);
    buf[len + 1..len + 1 + MAC_TAG_LEN].copy_from_slice(&tag[..MAC_TAG_LEN]);
    len + 1 + MAC_TAG_LEN
}

/// Outcome of [`verify_group_mac`].
///
/// **#471: the failure is TWO verdicts, not one.** It used to be a single `Fail` covering five
/// causes — absent trailer, truncated frame, wrong key, unknown epoch, forgery — and #190's Wave-1
/// gate is stated in terms of the counter that fold produces. That made the gate **unevaluable**
/// (an operator reading `mf=145287` learns only that something was not verifiable 145,287 times)
/// and, on a fleet with legitimately un-MAC'd members, **unreachable**: four watch-derived boards
/// vendor `append_group_mac` and never call it, so a benign share inflates the number forever and
/// `mac_fail ≈ 0` can never be true whether or not anything is wrong.
///
/// The split is drawn where the *evidence* actually divides, which is not where the prose list of
/// causes divides — see [`verify_group_mac`]. A frame either **claimed an epoch we accept** or it
/// did not, and that is the only distinction the bytes support.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MacVerdict {
    /// A valid trailer verified against one of the accepted keys. `payload_len` is the frame length
    /// WITHOUT the trailer — parse `data[..payload_len]`.
    Ok { payload_len: usize },
    /// **Nothing claimed an accepted epoch.** Either the frame is shorter than a trailer, or the byte
    /// where an epoch would sit matches no accepted epoch.
    ///
    /// This is the **benign-dominated** class and it is *expected to be large and non-zero forever*:
    /// a legacy un-MAC'd frame has no trailer at all, so its final 9 bytes are ordinary payload and
    /// the "epoch" read out of them is just whatever byte was there. Counted as `mf=`.
    ///
    /// Two causes are deliberately folded here (no-trailer and runt) — both benign, both
    /// indistinguishable from the bytes, and neither actionable. That is a different thing from the
    /// fold #471 is about, which mixed benign with actionable.
    Unkeyed,
    /// **A frame claimed an epoch we accept, and its tag did not verify.** Wrong key, corruption, or
    /// a forgery — the security event this feature exists to detect. Counted as `mfk=`.
    ///
    /// Unlike [`MacVerdict::Unkeyed`], **this one is expected to be ZERO on a healthy fleet**, which
    /// is what makes it a usable gate. See [`verify_group_mac`] for why legacy frames do not leak
    /// into it, and the compile-time guard that keeps that true across an epoch rotation.
    BadTag,
}

/// Verify a frame's group-MAC trailer against the accepted `(epoch, key)` set (the current key, plus
/// optionally the next epoch's key during a rotation overlap window — design §4.1). Constant-time
/// tag comparison. On success returns the payload length (trailer stripped). Verify-then-parse: the
/// caller runs `parse_frame` only on `data[..payload_len]`, so a bad/absent MAC never reaches the
/// parser under the enforce policy (design §7.3).
pub fn verify_group_mac(data: &[u8], keys: &[(u8, &[u8; 32])]) -> MacVerdict {
    if data.len() < MAC_TRAILER_LEN {
        // Too short to carry a trailer, so it cannot have claimed an epoch. Benign class.
        return MacVerdict::Unkeyed;
    }
    let payload_len = data.len() - MAC_TRAILER_LEN;
    let epoch = data[payload_len];
    let tag = &data[payload_len + 1..]; // exactly MAC_TAG_LEN bytes
    let mut claimed = false;
    for &(ep, key) in keys {
        if ep != epoch {
            continue;
        }
        // #471: the frame CLAIMED an epoch we accept. From here a mismatch is actionable, not
        // legacy — record that before the tag check so the verdict survives a loop that finds no
        // match. (Set inside the loop, not from `keys.iter().any(...)` above it, so the two can
        // never disagree about which epochs were actually tried.)
        claimed = true;
        // Recompute over the frame bytes PLUS the epoch byte — exactly what `append_group_mac` covered.
        let full = hmac_sha256(&key[..], &data[..payload_len + 1]);
        if ct_eq(&full[..MAC_TAG_LEN], tag) {
            return MacVerdict::Ok { payload_len };
        }
    }
    if claimed {
        MacVerdict::BadTag
    } else {
        MacVerdict::Unkeyed
    }
}

/// #471: is this epoch value one that a LEGACY un-MAC'd frame can never accidentally claim?
///
/// `BadTag` (`mfk=`) is only a usable security signal if benign traffic cannot land in it, and
/// whether it can is decided **entirely by the VALUE of the epoch byte** — a property nothing
/// recorded until now.
///
/// A legacy frame has no trailer, so [`verify_group_mac`] reads its "epoch" out of ordinary payload.
/// It reaches `BadTag` iff that payload byte happens to equal an accepted epoch (the tag then always
/// mismatches). Naively that is ~1 in 256 — which would put a permanent noise floor under the gate
/// and reintroduce, at 1/256 scale, the exact "unreachable pass condition" defect #471 exists to
/// remove. But every SMOLv1 frame reaching the verifier is `b"SMOLv1 …"`-prefixed **ASCII**, so the
/// byte at that offset is printable ASCII. Keep the epoch out of that range and the false-positive
/// rate is not small — it is **structurally zero**.
///
/// ⚠️ The hazard is the rotation instruction, which says to **bump** the epoch. Bumping reaches
/// `0x20` (space) at epoch 32, and `SMOLv1 ` frames are full of spaces — that rotation would flood
/// `mfk=` with benign traffic indistinguishable from the attack the counter reports. NUL is excluded
/// too: it is the natural "unset" value, and an epoch of 0 would make every zero-padded buffer a
/// claimant.
///
/// The ASSERTION lives at the call site in `net/mode.rs`, not here, because this module is
/// deliberately **secrets-free** so the host verifiers can `#[path]`-include it standalone — a
/// `crate::secrets` reference here breaks `relay_compat` and friends, which is how this function
/// came to exist rather than an inline `const _: () = assert!(…)`. The rule lives with the code it
/// describes; the secret stays where the secrets are.
pub const fn group_epoch_is_unambiguous(epoch: u8) -> bool {
    epoch != 0 && (epoch < 0x20 || epoch > 0x7E)
}

/// True iff `frame` is an **OTA-family** frame — one the receiver consumes at the `parse_ota_frame`
/// dispatch (OTAM/OTAD/OTAN) or the relay-drain `parse_ldbg` capture (LDBG), BOTH of which run
/// BEFORE the group-MAC [`verify_group_mac`]-then-strip in `service()`. Such a frame must therefore
/// NEVER carry the trailer: it would arrive at those parsers with 9 un-strippable trailing bytes
/// (a MAC'd OTAN's bitmap overflows its cap → dropped → the OTA window never advances; a MAC'd
/// partial-chunk OTAD corrupts the reassembled image → finalize-SHA mismatch). `SMOLv1 OTA*` covers
/// OTAM/OTAD/OTAN (and any future `OTAx`) structurally; `LDBG ` is the receive-diagnostic beacon.
/// (The #237 arbitration frames `ODEL`/`ODON` are exempt by construction — they send via
/// `send_arb_raw`, never `send_to` — so they need no entry here.)
pub fn is_ota_family(frame: &[u8]) -> bool {
    frame.starts_with(b"SMOLv1 OTA") || frame.starts_with(b"SMOLv1 LDBG ")
}

/// The send-choke TX decision: `true` ⇒ append the group-MAC trailer, `false` ⇒ send `frame`
/// verbatim. Verbatim when the frame is (a) not a SMOLv1 frame (e.g. the #25 WLED WiZmote emit —
/// a trailer would corrupt a third-party receiver), (b) an [`is_ota_family`] frame (bypasses the
/// verify-then-strip by design), or (c) too large to fit the trailer under [`ESP_NOW_MTU`] (never
/// emit > MTU). Pure so the exact `send_to` decision is host-tested (`experiments/mac_verify`) and
/// the OTA-exemption cannot silently regress.
pub fn should_group_mac(frame: &[u8]) -> bool {
    frame.starts_with(b"SMOLv1 ")
        && !is_ota_family(frame)
        && frame.len() + MAC_TRAILER_LEN <= ESP_NOW_MTU
}

// ── #382 `|cut=` tag helpers ───────────────────────────────────────────────────────────
// Pure byte functions, so they live HERE rather than in `net/wifi.rs` where they are used:
// this module is the dep-free, host-includable codec (`experiments/relay_compat` and
// `mac_verify` `#[path]`-include it), and a continuation format I cannot test on hardware
// is one I want a host verifier to exercise byte-for-byte. Putting them next to their
// caller would have made them untestable off-target — the same coupling #367 guards.
/// #382: byte offset of the trailing `|cut=` field, or `None`. Scans from the end because the tag
/// is appended last by construction — matching an earlier `|cut=` would cut a real record at a
/// coincidence, and a DIAG record is attacker-influenced only in the sense that any peer can
/// broadcast one, so "coincidence" is not a purely theoretical worry.
pub fn find_last_cut(rec: &[u8]) -> Option<usize> {
    const TAG: &[u8] = b"|cut=";
    if rec.len() < TAG.len() {
        return None;
    }
    (0..=rec.len() - TAG.len()).rev().find(|&i| &rec[i..i + TAG.len()] == TAG)
}

/// Parse the decimal after `|cut=`. Returns `None` on a malformed tag rather than a default, so the
/// caller can decline to touch a record it does not understand.
pub fn parse_cut(tag: &[u8]) -> Option<usize> {
    let digits = tag.get(5..)?;
    if digits.is_empty() || !digits.iter().all(|b| b.is_ascii_digit()) {
        return None;
    }
    // `try_fold`, not `fold` over an Option: clippy's `manual_try_fold` refuses the latter under
    // `-D warnings`, and it is right — the short-circuit is the point, and spelling it with `?`
    // inside a `fold` hides that the accumulator can abandon early.
    digits
        .iter()
        .try_fold(0usize, |acc, b| acc.checked_mul(10)?.checked_add((b - b'0') as usize))
}

/// Write `|cut=<n>` into `out` (max 8 bytes: the tag plus 3 digits, matching `CUT_TAG_MAX`),
/// returning the length. `n` is clamped to 999 exactly as `broadcast_diag` clamps it, so the two
/// ends of the protocol cannot disagree about the field's width.
pub fn write_cut(out: &mut [u8; 8], n: usize) -> usize {
    out[..5].copy_from_slice(b"|cut=");
    let n = n.min(999);
    if n >= 100 {
        out[5] = b'0' + (n / 100) as u8;
        out[6] = b'0' + ((n / 10) % 10) as u8;
        out[7] = b'0' + (n % 10) as u8;
        8
    } else if n >= 10 {
        out[5] = b'0' + (n / 10) as u8;
        out[6] = b'0' + (n % 10) as u8;
        7
    } else {
        out[5] = b'0' + n as u8;
        6
    }
}
