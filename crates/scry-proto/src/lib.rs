//! smol #540 scry-station protocol core — the PURE, host-tested half of
//! `src/net/scry_client.rs`. The device half (sockets, the panel bus) stays
//! in the no_std bin; everything here is logic that takes bytes and returns
//! a decision, so it lives where a host test can hammer it — the tree's
//! `ota-proto`/`story-proto` pattern.
//!
//! Two surfaces, both adversarial (they parse an HTTP server's response,
//! which a bug or a partial read can malform):
//!   1. [`parse_tap_host`] — peel the `/tap` JSON into bound / unbound /
//!      rejected, without a JSON crate (the body is the server's own compact
//!      `{"host":"game"}` / `{"host":null}` shape).
//!   2. [`be_row_to_pixels`] — the rgb565 big-endian wire → `u16` panel-pixel
//!      conversion for one row, the hot loop the kiosk blits with.

#![cfg_attr(not(test), no_std)]

/// Longest host name accepted from `/tap` (server names are short).
pub const HOST_CAP: usize = 24;

/// The `/tap` verdict, decoded from the response body.
#[derive(Debug, PartialEq, Eq)]
pub enum TapHost {
    /// `{"host":"<name>"}` — paint `/screen/<name>`.
    Bound(heapless::String<HOST_CAP>),
    /// `{"host":null,...}` — an unbound sigil; paint `/screen-unbound/<uid>`.
    Unbound,
    /// The body did not carry a usable `host` field (malformed / truncated /
    /// name too long). The caller keeps the glass as-is rather than acting on
    /// a response it could not read.
    Rejected(&'static str),
}

/// Peel the `host` field out of a `/tap` response body. No JSON crate: the
/// server's shape is fixed and small, so this is a bounded scan — but it is
/// still ADVERSARIAL (a truncated or malformed body must yield `Rejected`,
/// never a panic or a bogus host). `body` is the response text AFTER the
/// HTTP head (or the whole response — the `"host"` search tolerates leading
/// headers).
pub fn parse_tap_host(body: &str) -> TapHost {
    let Some(at) = body.find("\"host\"") else {
        return TapHost::Rejected("no host field");
    };
    let rest = body[at + 6..].trim_start_matches([':', ' ']);
    if rest.starts_with("null") {
        return TapHost::Unbound;
    }
    // The value must be a quoted string with BOTH quotes present. Splitting on
    // the closing quote is wrong for a truncated body (`"gam` with no closing
    // quote yields the whole fragment) — require the terminator explicitly, or
    // a mid-value truncation is silently accepted as a complete host. (This
    // was the bug the host test caught; the inline version shipped with it.)
    let Some(r) = rest.strip_prefix('"') else {
        return TapHost::Rejected("host not a string");
    };
    let Some(end) = r.find('"') else {
        return TapHost::Rejected("unterminated host (truncated body?)");
    };
    let name = &r[..end];
    if name.is_empty() {
        return TapHost::Rejected("empty host");
    }
    let mut host: heapless::String<HOST_CAP> = heapless::String::new();
    if host.push_str(name).is_err() {
        return TapHost::Rejected("host name too long");
    }
    TapHost::Bound(host)
}

/// Convert one row of rgb565 **big-endian** wire bytes into panel `u16`
/// pixels. `wire.len()` must be `2 * out.len()`; excess `out` is left
/// untouched, a short `wire` stops early — the caller sizes both to the panel
/// width, so a mismatch is a truncated read, handled by not fabricating
/// pixels past the bytes received.
pub fn be_row_to_pixels(wire: &[u8], out: &mut [u16]) {
    for (i, px) in out.iter_mut().enumerate() {
        let b = i * 2;
        if b + 1 >= wire.len() {
            break;
        }
        *px = u16::from_be_bytes([wire[b], wire[b + 1]]);
    }
}

// ---------------------------------------------------------------------------
// The inscribe rite (scry.realm.watch, 2026-09-11): when an imbue binds a
// blank card, the server hands the station the card's URL and the station
// WRITES it onto the NTAG as an NDEF URI record — so a phone tap opens the
// same page the QR does. Everything byte-shaped lives here, host-tested; the
// RC522 transactions stay in `src/peripherals/rc522.rs`.
// ---------------------------------------------------------------------------

/// Longest inscribe URL accepted from `/tap`. The label kit's are ~55 chars.
pub const URL_CAP: usize = 96;
/// Bytes we are willing to write to a tag: NTAG213's whole user area (pages
/// 4–39). Every label-kit URL fits in ~48 B; a bigger tag just has spare room.
pub const NDEF_CAP: usize = 144;

/// Peel the optional `"inscribe":"https://…"` field out of a `/tap` body.
/// Absent, `null`, truncated, over-long, or not an http(s) URL → `None`:
/// the station never writes bytes it could not read back as a URL.
pub fn parse_tap_inscribe(body: &str) -> Option<heapless::String<URL_CAP>> {
    let at = body.find("\"inscribe\"")?;
    let rest = body[at + 10..].trim_start_matches([':', ' ']);
    let r = rest.strip_prefix('"')?;
    let end = r.find('"')?;
    let url = &r[..end];
    if !(url.starts_with("https://") || url.starts_with("http://")) || url.contains('\\') {
        return None;
    }
    let mut out: heapless::String<URL_CAP> = heapless::String::new();
    out.push_str(url).ok()?;
    Some(out)
}

/// ISO/IEC 14443-3 CRC_A over `data` (poly 0x8408 reflected, preset 0x6363),
/// returned in wire order (LSB first) — the two bytes appended to a Type A
/// frame. Vector: `50 00` (HLTA) → `57 CD`.
pub fn crc_a(data: &[u8]) -> [u8; 2] {
    let mut crc: u16 = 0x6363;
    for &b in data {
        let mut ch = b ^ (crc as u8);
        ch ^= ch << 4;
        let ch = ch as u16;
        crc = (crc >> 8) ^ (ch << 8) ^ (ch << 3) ^ (ch >> 4);
    }
    [crc as u8, (crc >> 8) as u8]
}

/// NFC Forum URI record prefix codes (RTD-URI), longest match first.
const URI_PREFIXES: [(u8, &str); 4] = [
    (0x02, "https://www."),
    (0x01, "http://www."),
    (0x04, "https://"),
    (0x03, "http://"),
];

/// Build the Type 2 Tag NDEF message for `url`, ready to write from page 4:
/// `03 <len> | D1 01 <plen> 55 <prefix> <rest…> | FE`, zero-padded to a whole
/// 4-byte page. Returns the byte count, or `None` when the URL does not fit
/// a one-byte TLV length / the `out` buffer (then nothing is written to the
/// tag — the caller reports it, never truncates).
pub fn ndef_uri_tlv(url: &str, out: &mut [u8; NDEF_CAP]) -> Option<usize> {
    let (code, rest) = URI_PREFIXES
        .iter()
        .find_map(|(c, p)| url.strip_prefix(*p).map(|r| (*c, r)))
        .unwrap_or((0x00, url));
    let payload_len = 1 + rest.len();
    let record_len = 4 + payload_len;
    if payload_len > 255 || record_len > 254 {
        return None;
    }
    let total = 2 + record_len + 1;
    let padded = (total + 3) & !3;
    if padded > out.len() {
        return None;
    }
    let mut n = 0usize;
    out[n] = 0x03; // NDEF message TLV
    out[n + 1] = record_len as u8;
    n += 2;
    out[n] = 0xD1; // MB|ME|SR, TNF=well-known
    out[n + 1] = 0x01; // type length
    out[n + 2] = payload_len as u8;
    out[n + 3] = 0x55; // 'U'
    n += 4;
    out[n] = code;
    n += 1;
    out[n..n + rest.len()].copy_from_slice(rest.as_bytes());
    n += rest.len();
    out[n] = 0xFE; // terminator TLV
    n += 1;
    while n < padded {
        out[n] = 0;
        n += 1;
    }
    Some(n)
}

/// Read the capability container (page 3) of a Type 2 tag: `E1 10 <size/8> <access>`.
/// Returns the user-memory capacity in bytes, or `None` when the tag carries
/// no NDEF CC (a factory-blank or non-NDEF tag — the station REFUSES rather
/// than formatting, because page 3 is one-time-programmable).
pub fn t2t_capacity(cc: &[u8; 4]) -> Option<usize> {
    if cc[0] != 0xE1 || cc[1] & 0xF0 != 0x10 {
        return None;
    }
    Some(cc[2] as usize * 8)
}
