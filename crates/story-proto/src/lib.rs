//! Endless LitRPG watch-client protocol core.
//!
//! Design spec: `~/Projects/endlesslitrpg/docs/superpowers/specs/2026-07-29-endless-litrpg-design.md`
//! (§8 artifacts + manifest, §9.1 routes, §9.4 the watch app).
//!
//! # The one number this crate is built around
//!
//! Every TTS plugin normalises to **16 kHz mono s16le, raw and headerless** —
//! exactly **32 bytes per millisecond**. So
//!
//! ```text
//! byte_offset = ms x 32
//! ```
//!
//! in closed form, with no frame table and no seek index. And it is an
//! *identity*, not an approximation: the render path zero-pads every segment to
//! a whole millisecond and asserts `duration_ms x 32 == pcm.len()` before
//! publishing (design §8.1). Verified against the live daemon: chapter 1 is
//! 452,729 ms and 14,487,328 bytes; 452,729 x 32 = 14,487,328.
//!
//! That is what lets the watch resume mid-chapter with a plain HTTP `Range`
//! request and no index — and it is why the firmware needs no decoder, no
//! resampler and no seek table. `audio_out::play_pcm` already eats this format
//! byte for byte.
//!
//! # Layout
//!
//! * [`json`] — a bounded, resumable JSON scanner that lets prose stream past
//!   without ever being held.
//! * [`model`] — the four payloads as bounded accumulators.
//! * this module — PCM offset math, HTTP `Range` framing, and the request and
//!   body encoders.
//!
//! # Discipline
//!
//! `no_std`, no allocation, no `core::fmt`. Integers are formatted by
//! [`push_u32`] because a `write!` would link `core::fmt` and this firmware
//! counts kilobytes. All arithmetic saturates; there is no input that panics.

#![no_std]
#![forbid(unsafe_code)]

pub mod json;
pub mod model;

use heapless::String;

// ---------------------------------------------------------------------------
// The audio contract
// ---------------------------------------------------------------------------

/// Playback sample rate, in Hz. Matches `audio_out::PLAY_SAMPLE_RATE`; the
/// firmware asserts they are equal at compile time.
pub const SAMPLE_RATE: u32 = 16_000;

/// Bytes per millisecond of audio: 16 kHz x 1 channel x 2 bytes / 1000.
pub const BYTES_PER_MS: usize = 32;

/// Bytes per second of audio.
pub const BYTES_PER_SEC: usize = BYTES_PER_MS * 1000;

/// The playback queue's chunk size. Matches `audio_out::PLAY_CHUNK`; the
/// firmware asserts they are equal at compile time, because a silent drift here
/// would mis-time every buffer calculation in the streaming loop.
pub const PLAY_CHUNK: usize = 512;

/// Milliseconds of audio in one [`PLAY_CHUNK`] (16 ms).
pub const CHUNK_MS: u32 = (PLAY_CHUNK / BYTES_PER_MS) as u32;

/// Byte offset of `ms`. Exact by construction (see module docs).
///
/// Saturates rather than wrapping: `u32` covers 37 hours of audio, so only a
/// nonsense input can reach the ceiling, and saturating keeps this panic-free
/// in a `no_std` release build where overflow checks are off anyway.
pub const fn ms_to_bytes(ms: u32) -> u32 {
    ms.saturating_mul(BYTES_PER_MS as u32)
}

/// Milliseconds represented by `bytes`, floored.
///
/// Flooring matches the daemon's `duration_ms()` (§8.1): 32 bytes is 1 ms, so a
/// legal 34-byte buffer is 1.0625 ms, and the server floors too. Both ends
/// flooring is what keeps the two in agreement.
pub const fn bytes_to_ms(bytes: u32) -> u32 {
    bytes / BYTES_PER_MS as u32
}

/// Round `bytes` down to a whole sample (2 bytes).
///
/// A `Range` response could, in principle, split a 16-bit sample across reads.
/// Feeding half a sample to the codec shifts every subsequent byte by one and
/// turns the rest of the chapter into noise, so the streaming loop aligns before
/// it queues.
pub const fn align_sample(bytes: u32) -> u32 {
    bytes & !1
}

/// Whole seconds in `ms`, for a `mm:ss` label.
pub const fn ms_to_secs(ms: u32) -> u32 {
    ms / 1000
}

// ---------------------------------------------------------------------------
// Range windows
// ---------------------------------------------------------------------------

/// How much audio one `Range` request covers: 60 s = 1,920,000 B.
///
/// **Why windowed rather than one open-ended request for the whole chapter.** A
/// 13-minute chapter is ~25 MB and would otherwise be a single socket held open
/// for the entire playback, paced by the speaker through the TCP receive window.
/// That works until it doesn't: any stall, AP roam or daemon restart loses the
/// whole chapter and there is nothing to retry *from*. Windowing bounds the
/// blast radius of a failure to one minute of audio, and because
/// [`ms_to_bytes`] is exact, resuming is just the next offset — no re-sync, no
/// index, no guesswork.
pub const WINDOW_BYTES: u32 = 60 * BYTES_PER_SEC as u32;

/// The `Range` window starting at `pos`, clamped to `total`.
///
/// Returns `None` once `pos` is at or past the end, which is the natural
/// "chapter finished" signal.
pub fn window_at(pos: u32, total: u32) -> Option<(u32, u32)> {
    if total == 0 || pos >= total {
        return None;
    }
    let end = pos.saturating_add(WINDOW_BYTES).min(total);
    // HTTP ranges are inclusive on both ends.
    Some((pos, end.saturating_sub(1)))
}

// ---------------------------------------------------------------------------
// HTTP response framing
// ---------------------------------------------------------------------------

/// Outcome of [`parse_head`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HeadParse {
    /// `\r\n\r\n` not seen yet — read more and call again.
    Incomplete,
    /// Not a status line we can make sense of. Abort.
    Malformed,
    Ok(ResponseHead),
}

/// A parsed HTTP response head.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct ResponseHead {
    pub status: u16,
    pub content_length: Option<u32>,
    /// From `Content-Range: bytes <first>-<last>/<total>`.
    pub content_range: Option<ContentRange>,
    /// Offset of the first body byte within the buffer handed to [`parse_head`].
    pub body_offset: usize,
}

/// A parsed `Content-Range` value.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct ContentRange {
    pub first: u32,
    pub last: u32,
    pub total: u32,
}

impl ResponseHead {
    /// Where in the *file* the body we are about to read begins.
    ///
    /// This is the load-bearing question when resuming, and the answer is not
    /// always "where we asked". A `206` reports its own `Content-Range`, and a
    /// server that ignored the `Range` answers `200` with the body starting at
    /// **byte 0** — in which case the caller must skip forward rather than
    /// treat the first byte as the resume point. Getting this wrong plays the
    /// chapter from the start while the progress bar claims otherwise.
    pub fn body_starts_at(&self, requested: u32) -> u32 {
        match (self.status, self.content_range) {
            (206, Some(cr)) => cr.first,
            (206, None) => requested, // 206 without the header: trust the ask
            _ => 0,                   // 200 (or anything else): full body
        }
    }

    /// True when the server honoured the `Range` request.
    pub fn is_partial(&self) -> bool {
        self.status == 206
    }

    /// Total file size, when the response reveals it.
    pub fn total_bytes(&self) -> Option<u32> {
        self.content_range.map(|cr| cr.total).or(self.content_length)
    }

    /// True for the 2xx codes these routes use.
    pub fn ok(&self) -> bool {
        matches!(self.status, 200 | 201 | 204 | 206)
    }
}

/// Parse an HTTP response head from `buf`.
///
/// Bounded and allocation-free; returns [`HeadParse::Incomplete`] until the
/// `\r\n\r\n` terminator lands, so it can be called after every socket read.
pub fn parse_head(buf: &[u8]) -> HeadParse {
    let Some(term) = find(buf, b"\r\n\r\n") else {
        return HeadParse::Incomplete;
    };
    let Some(head) = buf.get(..term) else {
        return HeadParse::Malformed;
    };
    let body_offset = term.saturating_add(4);

    let mut lines = split_crlf(head);
    let Some(status_line) = lines.next() else {
        return HeadParse::Malformed;
    };
    let Some(status) = parse_status(status_line) else {
        return HeadParse::Malformed;
    };

    let mut out =
        ResponseHead { status, content_length: None, content_range: None, body_offset };
    for line in lines {
        if let Some(v) = header_value(line, b"content-length") {
            out.content_length = parse_u32(v);
        } else if let Some(v) = header_value(line, b"content-range") {
            out.content_range = parse_content_range(v);
        }
    }
    HeadParse::Ok(out)
}

/// `HTTP/1.x <code> ...` -> code.
fn parse_status(line: &[u8]) -> Option<u16> {
    if !line.starts_with(b"HTTP/") {
        return None;
    }
    let sp = line.iter().position(|&b| b == b' ')?;
    let rest = line.get(sp.saturating_add(1)..)?;
    let end = rest.iter().position(|&b| b == b' ').unwrap_or(rest.len());
    let digits = rest.get(..end)?;
    let v = parse_u32(digits)?;
    (100..=599).contains(&v).then_some(v as u16)
}

/// `bytes 1000000-1000511/14487328` -> (first, last, total).
fn parse_content_range(v: &[u8]) -> Option<ContentRange> {
    let v = trim(v);
    let v = strip_prefix_ci(v, b"bytes")?;
    let v = trim(v);
    let dash = v.iter().position(|&b| b == b'-')?;
    let slash = v.iter().position(|&b| b == b'/')?;
    if dash >= slash {
        return None;
    }
    let first = parse_u32(v.get(..dash)?)?;
    let last = parse_u32(v.get(dash.saturating_add(1)..slash)?)?;
    // `*` for an unknown total is legal; treat it as absent rather than fatal.
    let total = parse_u32(v.get(slash.saturating_add(1)..)?).unwrap_or(0);
    (first <= last).then_some(ContentRange { first, last, total })
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

fn split_crlf(buf: &[u8]) -> impl Iterator<Item = &[u8]> {
    buf.split(|&b| b == b'\n').map(|l| {
        if l.last() == Some(&b'\r') {
            l.get(..l.len().saturating_sub(1)).unwrap_or(l)
        } else {
            l
        }
    })
}

/// Case-insensitive `Name: value` match.
fn header_value<'a>(line: &'a [u8], name: &[u8]) -> Option<&'a [u8]> {
    let rest = strip_prefix_ci(line, name)?;
    let rest = trim(rest);
    let rest = rest.strip_prefix(b":")?;
    Some(trim(rest))
}

fn strip_prefix_ci<'a>(hay: &'a [u8], pre: &[u8]) -> Option<&'a [u8]> {
    if hay.len() < pre.len() {
        return None;
    }
    let (h, rest) = hay.split_at(pre.len());
    h.iter()
        .zip(pre)
        .all(|(a, b)| a.eq_ignore_ascii_case(b))
        .then_some(rest)
}

fn trim(mut b: &[u8]) -> &[u8] {
    while let Some((f, rest)) = b.split_first() {
        if f.is_ascii_whitespace() {
            b = rest;
        } else {
            break;
        }
    }
    while let Some((l, rest)) = b.split_last() {
        if l.is_ascii_whitespace() {
            b = rest;
        } else {
            break;
        }
    }
    b
}

fn parse_u32(b: &[u8]) -> Option<u32> {
    let b = trim(b);
    if b.is_empty() {
        return None;
    }
    let mut acc: u32 = 0;
    for &c in b {
        if !c.is_ascii_digit() {
            return None;
        }
        acc = acc.saturating_mul(10).saturating_add((c - b'0') as u32);
    }
    Some(acc)
}

// ---------------------------------------------------------------------------
// Integer formatting without core::fmt
// ---------------------------------------------------------------------------

/// Append `v` in decimal. Returns `false` if the string is full.
///
/// Exists because `write!`/`format_args!` would link `core::fmt` (kilobytes of
/// ROM for integer formatting alone). Same trick as `voice_tts::push_u32`,
/// hoisted here so it is host-tested rather than duplicated per call site.
pub fn push_u32<const N: usize>(s: &mut String<N>, v: u32) -> bool {
    let mut buf = [0u8; 10];
    let mut n = 0usize;
    let mut v = v;
    loop {
        let d = (v % 10) as u8;
        match buf.get_mut(n) {
            Some(slot) => *slot = b'0' + d,
            None => return false,
        }
        n = n.saturating_add(1);
        v /= 10;
        if v == 0 {
            break;
        }
    }
    for i in (0..n).rev() {
        let Some(&c) = buf.get(i) else { return false };
        if s.push(c as char).is_err() {
            return false;
        }
    }
    true
}

/// Append `v` zero-padded to `width` digits (max 10).
///
/// The media route needs it: `pcm_url` is `/media/0001.pcm`, four digits with
/// leading zeros, while `/api/chapters/1` takes the bare integer. Two different
/// conventions on the same chapter number in the same API, so this is a real
/// source of 404s rather than a formatting nicety.
pub fn push_u32_pad<const N: usize>(s: &mut String<N>, v: u32, width: usize) -> bool {
    let mut digits = 1usize;
    let mut probe = v;
    while probe >= 10 {
        probe /= 10;
        digits = digits.saturating_add(1);
    }
    for _ in digits..width.min(10) {
        if s.push('0').is_err() {
            return false;
        }
    }
    push_u32(s, v)
}

// ---------------------------------------------------------------------------
// Request bodies
// ---------------------------------------------------------------------------

/// Cap on a director note, matching the daemon's 4096-byte bound with room to
/// spare. A dictated note is a sentence or two.
pub const MAX_NOTE: usize = 240;

/// Encoded JSON request body.
pub type Body = String<{ MAX_NOTE * 2 + 48 }>;

/// `{"body":"...","source":"watch"}` for `POST /api/notes`.
///
/// The text comes from the STT gateway's transcript, so it is machine-generated
/// from speech and can legitimately contain quotes and apostrophes. Escaping is
/// hand-rolled (no `serde`), which is exactly why it is host-tested against
/// adversarial input rather than trusted.
pub fn encode_note(text: &str) -> Option<Body> {
    let mut s = Body::new();
    s.push_str("{\"body\":\"").ok()?;
    escape_json_into(&mut s, text, MAX_NOTE)?;
    s.push_str("\",\"source\":\"watch\"}").ok()?;
    Some(s)
}

/// `{"consumed_through":N}` for `PUT /api/progress`.
pub fn encode_progress(consumed_through: u16) -> Option<Body> {
    let mut s = Body::new();
    s.push_str("{\"consumed_through\":").ok()?;
    if !push_u32(&mut s, consumed_through as u32) {
        return None;
    }
    s.push('}').ok()?;
    Some(s)
}

/// True when `text` is worth sending as a note — an empty or punctuation-only
/// transcript should not become a director note that steers the story.
pub fn is_notable(text: &str) -> bool {
    text.chars().any(|c| c.is_ascii_alphanumeric())
}

/// Escape `text` as JSON string contents, clipping the *source* at `cap` chars.
///
/// Control characters become `\uXXXX`; non-ASCII is dropped rather than
/// re-encoded, mirroring `notify::sanitize`'s ASCII clamp — the daemon stores
/// what we send verbatim and the watch has no business inventing encodings.
fn escape_json_into<const N: usize>(s: &mut String<N>, text: &str, cap: usize) -> Option<()> {
    for (i, c) in text.chars().enumerate() {
        if i >= cap {
            break;
        }
        match c {
            '"' => s.push_str("\\\"").ok()?,
            '\\' => s.push_str("\\\\").ok()?,
            '\n' => s.push_str("\\n").ok()?,
            '\r' => s.push_str("\\r").ok()?,
            '\t' => s.push_str("\\t").ok()?,
            c if (c as u32) < 0x20 => {
                s.push_str("\\u00").ok()?;
                let v = c as u8;
                s.push(hex_digit(v >> 4)).ok()?;
                s.push(hex_digit(v & 0x0f)).ok()?;
            }
            c if c.is_ascii() => s.push(c).ok()?,
            // Non-ASCII dropped (see doc comment).
            _ => {}
        }
    }
    Some(())
}

fn hex_digit(v: u8) -> char {
    match v {
        0..=9 => (b'0' + v) as char,
        10..=15 => (b'a' + (v - 10)) as char,
        _ => '0',
    }
}

// ---------------------------------------------------------------------------
// Request composition
// ---------------------------------------------------------------------------

/// A composed HTTP request head.
pub type Head = String<256>;

/// HTTP verbs these routes need.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Method {
    Get,
    Post,
    Put,
}

impl Method {
    const fn text(self) -> &'static str {
        match self {
            Method::Get => "GET ",
            Method::Post => "POST ",
            Method::Put => "PUT ",
        }
    }
}

/// Which daemon route to address.
///
/// An enum rather than caller-formatted strings because the media route and the
/// API route zero-pad the same chapter number differently
/// (`/media/0001.pcm` vs `/api/chapters/1`) — a 404 waiting to happen if each
/// call site formats its own path. Centralised here, it is covered by one test.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Route {
    /// `GET /api/chapters?since=N`
    Chapters { since: u16 },
    /// `GET /api/chapters/{n}` — text + segments + manifest.
    Chapter { n: u16 },
    /// `GET /media/{n:04}.pcm` — Range-capable playback.
    Media { n: u16 },
    /// `GET /api/character` — protagonist (subject omitted, per §9.4.1).
    Character,
    /// `GET` or `PUT /api/progress`
    Progress,
    /// `POST /api/notes`
    Notes,
    /// `GET /api/story`
    Story,
}

impl Route {
    /// Append this route's path. Returns `false` if the buffer is full.
    pub fn push_path<const N: usize>(self, s: &mut String<N>) -> bool {
        match self {
            Route::Chapters { since } => {
                if s.push_str("/api/chapters?since=").is_err() {
                    return false;
                }
                push_u32(s, since as u32)
            }
            Route::Chapter { n } => {
                if s.push_str("/api/chapters/").is_err() {
                    return false;
                }
                push_u32(s, n as u32)
            }
            Route::Media { n } => {
                if s.push_str("/media/").is_err() {
                    return false;
                }
                // Four digits, zero-padded: the daemon's own `pcm_url`.
                if !push_u32_pad(s, n as u32, 4) {
                    return false;
                }
                s.push_str(".pcm").is_ok()
            }
            Route::Character => s.push_str("/api/character").is_ok(),
            Route::Progress => s.push_str("/api/progress").is_ok(),
            Route::Notes => s.push_str("/api/notes").is_ok(),
            Route::Story => s.push_str("/api/story").is_ok(),
        }
    }
}

/// Compose a request head for a dotted-quad host.
///
/// `range` adds `Range: bytes=<first>-<last>`; `body_len` adds a JSON
/// `Content-Type` and `Content-Length`. `Connection: close` throughout, matching
/// `voice_tts` — the daemon is not asked to keep anything alive, and each
/// windowed Range request is its own connection so a dead socket costs one
/// window rather than the chapter.
///
/// No DNS and no TLS: the address is always a literal IPv4, which is a hard
/// requirement inherited from the watch (design §9.1).
pub fn request(
    method: Method,
    route: Route,
    host: [u8; 4],
    port: u16,
    range: Option<(u32, u32)>,
    body_len: Option<usize>,
) -> Option<Head> {
    let mut s = Head::new();
    s.push_str(method.text()).ok()?;
    if !route.push_path(&mut s) {
        return None;
    }
    s.push_str(" HTTP/1.1\r\nHost: ").ok()?;
    for (i, o) in host.iter().enumerate() {
        if i > 0 {
            s.push('.').ok()?;
        }
        if !push_u32(&mut s, *o as u32) {
            return None;
        }
    }
    s.push(':').ok()?;
    if !push_u32(&mut s, port as u32) {
        return None;
    }
    if let Some((first, last)) = range {
        s.push_str("\r\nRange: bytes=").ok()?;
        if !push_u32(&mut s, first) {
            return None;
        }
        s.push('-').ok()?;
        if !push_u32(&mut s, last) {
            return None;
        }
    }
    if let Some(len) = body_len {
        s.push_str("\r\nContent-Type: application/json\r\nContent-Length: ").ok()?;
        if !push_u32(&mut s, len as u32) {
            return None;
        }
    }
    s.push_str("\r\nConnection: close\r\n\r\n").ok()?;
    Some(s)
}

// ---------------------------------------------------------------------------
// Retry budget
// ---------------------------------------------------------------------------

/// Audio in a failed attempt that still counts as forward progress: 1 s.
pub const MIN_PROGRESS_BYTES: u32 = BYTES_PER_SEC as u32;

/// Zero-progress failures tolerated before abandoning a chapter.
pub const MAX_WINDOW_RETRIES: u8 = 3;

/// Absolute retry cap per chapter, so progress-resets cannot spin forever.
pub const MAX_TOTAL_RETRIES: u16 = 64;

/// The "keep going or give up" decision for a failed `Range` window.
///
/// # Why this lives here and not in the firmware
///
/// It was in `src/net/story_play.rs`, where **it could not be tested at all** —
/// that crate depends on `esp-hal`, which does not build for the host. And it was
/// wrong: `attempt` was cleared only by a *fully delivered* window, so a link
/// delivering 4.6 s of audio per attempt burned three strikes in ~14 s and
/// abandoned a chapter that was still making progress. That flaw was caught by
/// reading a production access log, which is a bad way to find out.
///
/// So the decision moved to the one place with tests. The rule: **only a
/// zero-progress failure is evidence the far end is gone.**
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct RetryBudget {
    /// Consecutive failures that delivered nothing.
    stalled: u8,
    /// Every failure this chapter, progress or not.
    total: u16,
}

impl RetryBudget {
    pub const fn new() -> Self {
        Self { stalled: 0, total: 0 }
    }

    /// A window was delivered in full: the link is healthy, clear the strikes.
    pub fn delivered(&mut self) {
        self.stalled = 0;
    }

    /// A window failed after advancing `progress` bytes.
    ///
    /// Returns `true` when playback should give up. Progress at or above
    /// [`MIN_PROGRESS_BYTES`] clears the strike count — the link is slow, not
    /// dead — while [`MAX_TOTAL_RETRIES`] still guarantees termination.
    pub fn failed(&mut self, progress: u32) -> bool {
        self.total = self.total.saturating_add(1);
        if progress >= MIN_PROGRESS_BYTES {
            self.stalled = 0;
        } else {
            self.stalled = self.stalled.saturating_add(1);
        }
        self.stalled >= MAX_WINDOW_RETRIES || self.total >= MAX_TOTAL_RETRIES
    }

    /// Total failures this chapter — what `Session::retries` reports.
    pub fn total(&self) -> u16 {
        self.total
    }
}
