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
