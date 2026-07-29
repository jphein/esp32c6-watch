//! The `SMOLv1 ELECT` frame — one fixed-width ASCII record, byte-identical in
//! both repos.
//!
//! Layout (61 bytes, always):
//!
//! ```text
//! "SMOLv1 ELECT " <id:3> ' ' <epoch:10> ' ' <ch:2> ' ' <gw:3> ' ' <w:26>
//!  └── 13 ──────┘                                                  └ 13×2 ┘
//! ```
//!
//! Conventions are the existing SMOLv1 ones so this is unsurprising to read
//! alongside HELLO/TIME/RELAY: an ASCII `SMOLv1 <TAG> ` prefix, then
//! **fixed-width zero-padded decimal** fields, single-space separated. Tag byte
//! 7 is `'E'`, which no existing SMOLv1 tag uses (`H A B T G C S D R U F`), so
//! there is no collision, and firmware that predates this frame classifies it as
//! unknown and ignores it harmlessly.
//!
//! **Fixed width is the security property, not just a convenience.** The design
//! spec proposed a variable candidate list (`<n_cands> [<bssid> <ch> <rssi>]*`)
//! with a note to cap it. Because we elect a channel rather than a BSSID, the
//! candidate set is the 13 channels of the 2.4 GHz band — a constant. So the
//! frame has no length field, no repetition, and no bound to enforce: a
//! malformed or hostile frame is simply not 61 bytes, or fails a digit check.
//! There is nothing here for an attacker to grow.
//!
//! Well under the 250 B ESP-NOW payload cap, with room for a future field.

use crate::{ch_index, N_CHANNELS};

/// Frame tag. Trailing space matches every other SMOLv1 prefix.
pub const ELECT_PREFIX: &[u8] = b"SMOLv1 ELECT ";

/// Exact encoded length. A frame of any other length is not an ELECT frame.
pub const ELECT_LEN: usize = 13 + 3 + 1 + 10 + 1 + 2 + 1 + 3 + 1 + 2 * N_CHANNELS;

/// A decoded ELECT frame.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ElectFrame {
    pub node_id: u8,
    pub epoch: u32,
    pub channel: u8,
    pub gateway: u8,
    /// Per-channel weight, index 0 = ch1.
    pub w: [u8; N_CHANNELS],
}

/// Write `v` as `n` zero-padded ASCII digits. Values too large for the field are
/// clamped to all-nines rather than truncated to a wrong-but-plausible number.
fn write_num(v: u32, n: usize, out: &mut [u8]) {
    let mut v = v;
    let mut max = 1u64;
    for _ in 0..n {
        max *= 10;
    }
    if (v as u64) >= max {
        for b in out[..n].iter_mut() {
            *b = b'9';
        }
        return;
    }
    for i in (0..n).rev() {
        out[i] = b'0' + (v % 10) as u8;
        v /= 10;
    }
}

/// Parse exactly `n` ASCII digits. `None` on any non-digit — no lenient
/// whitespace, no partial parse.
fn parse_num(s: &[u8], n: usize) -> Option<u32> {
    if s.len() < n {
        return None;
    }
    let mut v: u32 = 0;
    for &b in &s[..n] {
        if !b.is_ascii_digit() {
            return None;
        }
        v = v.checked_mul(10)?.checked_add((b - b'0') as u32)?;
    }
    Some(v)
}

/// Encode into `out` (which must hold at least [`ELECT_LEN`]). Returns the
/// number of bytes written, or `None` if the buffer is too small or the channel
/// is out of range.
#[must_use]
pub fn encode(f: &ElectFrame, out: &mut [u8]) -> Option<usize> {
    if out.len() < ELECT_LEN || ch_index(f.channel).is_none() {
        return None;
    }
    let mut n = 0;
    out[..ELECT_PREFIX.len()].copy_from_slice(ELECT_PREFIX);
    n += ELECT_PREFIX.len();
    write_num(f.node_id as u32, 3, &mut out[n..]);
    n += 3;
    out[n] = b' ';
    n += 1;
    write_num(f.epoch, 10, &mut out[n..]);
    n += 10;
    out[n] = b' ';
    n += 1;
    write_num(f.channel as u32, 2, &mut out[n..]);
    n += 2;
    out[n] = b' ';
    n += 1;
    write_num(f.gateway as u32, 3, &mut out[n..]);
    n += 3;
    out[n] = b' ';
    n += 1;
    for i in 0..N_CHANNELS {
        // Weights are 0..=48 by construction, so 2 digits always suffice.
        write_num(f.w[i] as u32, 2, &mut out[n..]);
        n += 2;
    }
    debug_assert_eq!(n, ELECT_LEN);
    Some(n)
}

/// Parse a received payload. Returns `None` unless it is a well-formed ELECT
/// frame of exactly the right length with an in-range channel.
///
/// Strict by design: this is fed straight from unauthenticated broadcasts, so
/// every field is length- and digit-checked before it reaches election state.
#[must_use]
pub fn parse(data: &[u8]) -> Option<ElectFrame> {
    let rest = data.strip_prefix(ELECT_PREFIX)?;
    if rest.len() != ELECT_LEN - ELECT_PREFIX.len() {
        return None;
    }
    let node_id = u8::try_from(parse_num(&rest[0..3], 3)?).ok()?;
    if rest[3] != b' ' {
        return None;
    }
    let epoch = parse_num(&rest[4..14], 10)?;
    if rest[14] != b' ' {
        return None;
    }
    let channel = u8::try_from(parse_num(&rest[15..17], 2)?).ok()?;
    if rest[17] != b' ' {
        return None;
    }
    let gateway = u8::try_from(parse_num(&rest[18..21], 3)?).ok()?;
    if rest[21] != b' ' {
        return None;
    }
    ch_index(channel)?; // reject ch 0 / ch > 13 at the boundary
    let mut w = [0u8; N_CHANNELS];
    for i in 0..N_CHANNELS {
        let off = 22 + i * 2;
        w[i] = u8::try_from(parse_num(&rest[off..off + 2], 2)?).ok()?;
    }
    Some(ElectFrame {
        node_id,
        epoch,
        channel,
        gateway,
        w,
    })
}
