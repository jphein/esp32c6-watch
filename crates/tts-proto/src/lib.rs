//! Notification read-aloud protocol core (design spec 2026-07-27).
//!
//! Pure, `no_std`, host-testable — depends only on `core` + `heapless`, never on
//! esp-hal/Slint. The async I/O module (`src/net/voice_tts.rs`) uses this crate
//! to build the request it POSTs to the LAN TTS bridge and to frame the reply;
//! the raw PCM that comes back goes straight to `audio_out` untouched.
//!
//! # Why this crate exists at all
//!
//! esp-hal's `build.rs` panics off-target, so anything living under `src/` can
//! never be `cargo test`-ed. The fiddly, get-it-wrong-quietly parts of this
//! feature — JSON escaping of attacker-influenced text, HTTP head framing,
//! byte↔millisecond math — are exactly what deserves real tests, so they live
//! here. Same pattern as `crates/mic-dsp` and `crates/climate-model`.
//!
//! # Untrusted input
//!
//! [`compose_utterance`] consumes notification titles/bodies that originate from
//! **retained MQTT payloads off the LAN broker** (`notify::handle_mqtt`), and
//! [`parse_response_head`] parses a network reply. Both are **bounded and
//! panic-free**: no slice indexing on untrusted offsets, every walk is a
//! `while i < len`, and overflow truncates rather than wrapping or panicking.
//!
//! # The audio contract (whole project, one format)
//!
//! Mono **16 kHz s16le**, end to end: the mic path, the STT upload, the playback
//! ring, and — confirmed live against Azure — the TTS bridge's response body.
//! That is why there is no decode step anywhere in this crate: the bytes off the
//! socket are already what `audio_out::push_chunk` wants.

#![no_std]
#![forbid(unsafe_code)]

use heapless::String;

// ---------------------------------------------------------------------------
// Audio math — one place for the 32 B/ms constant everything else derives from
// ---------------------------------------------------------------------------

/// Playback sample rate (Hz). Matches `audio_out::PLAY_SAMPLE_RATE`.
pub const SAMPLE_RATE: usize = 16_000;
/// Bytes per second of mono 16 kHz s16le: `16000 × 2`.
pub const BYTES_PER_SEC: usize = SAMPLE_RATE * 2;
/// One playback chunk in mono bytes. Mirrors `audio_out::PLAY_CHUNK`.
pub const PLAY_CHUNK: usize = 512;

/// Duration of `bytes` of mono 16 kHz s16le audio, in milliseconds.
///
/// Saturating: a nonsense length can never panic or wrap.
pub const fn bytes_to_ms(bytes: usize) -> u32 {
    // bytes / 32 — exact for the 16 kHz s16le contract.
    (bytes / (BYTES_PER_SEC / 1000)) as u32
}

/// Bytes of mono 16 kHz s16le audio for `ms` milliseconds (rounded down to a
/// whole stereo-safe sample: always even).
pub const fn ms_to_bytes(ms: u32) -> usize {
    let b = (ms as usize) * (BYTES_PER_SEC / 1000);
    b & !1 // whole 16-bit samples only
}

/// Number of [`PLAY_CHUNK`]-sized sends `bytes` will take (last one partial).
pub const fn chunk_count(bytes: usize) -> usize {
    bytes.div_ceil(PLAY_CHUNK)
}

// ---------------------------------------------------------------------------
// Speech composition
// ---------------------------------------------------------------------------

/// Cap on the text we will ask the bridge to speak.
///
/// Sized from the notification ring: `TITLE_CAP` (32) + `BODY_CAP` (96) + a
/// source prefix + separators, with headroom. At the measured HD-voice rate
/// (~17 chars/s) this is ~13 s of audio worst case — inside the bridge's 30 s
/// cap, and interruptible (see spec §6.6).
pub const MAX_SPEECH: usize = 224;

/// A composed utterance, ready to hand to [`encode_json_request`].
pub type Utterance = String<MAX_SPEECH>;

/// Build the spoken form of a notification: `"<source>. <title>. <body>"`.
///
/// The source prefix gives a listener the context a sighted user gets from the
/// card glyph ("Home Assistant." / "Battery."). Empty segments are skipped
/// cleanly — no stray ". ." runs.
///
/// Text handling (all of it exercised by the host tests):
/// - **Printable ASCII only.** Anything else is *dropped*, not replaced.
/// - **Runs of 2+ `'?'` are dropped; a lone `'?'` is kept.** `notify::sanitize`
///   maps every non-ASCII char to `'?'` for the display glyph sets, so an
///   emoji-laden HA notification reaches us as `"????"` — speaking that is worse
///   than silence. A single `'?'` is far more likely to be real punctuation
///   ("Door still open?"), and Azure uses it for interrogative prosody, so it
///   survives.
/// - **Whitespace collapses** to single spaces (a body full of newlines from
///   MQTT should not become a pile of pauses).
/// - **Sentence punctuation is inserted** between segments only when the
///   preceding segment does not already end in `.`/`!`/`?`/`:` — Azure uses it
///   for prosody, and doubling it reads as a stutter.
/// - **Overflow truncates on a word boundary** and appends `.`, so a clipped
///   utterance ends as a sentence instead of mid-syllable.
pub fn compose_utterance(source: &str, title: &str, body: &str) -> Utterance {
    let mut out = Utterance::new();
    for seg in [source, title, body] {
        push_segment(&mut out, seg);
    }
    finish_sentence(&mut out);
    out
}

/// Append one segment, inserting a sentence break if needed. Bounded: stops at
/// capacity rather than erroring, and never leaves a dangling separator.
fn push_segment(out: &mut Utterance, seg: &str) {
    // Where this segment starts — the floor for word-boundary truncation, so a
    // clipped segment can never eat back into the previous one.
    let start = out.len();
    let mut ap = Appender { pending_sep: !out.is_empty(), pending_space: false, start, out };
    // Length of the run of '?' currently being held back (see doc comment).
    let mut qrun = 0usize;

    for c in seg.chars() {
        if c == '?' {
            qrun += 1;
            continue;
        }
        // A run just ended: a lone '?' was real punctuation, keep it.
        if qrun == 1 && !ap.ch('?') {
            return;
        }
        qrun = 0;

        // Printable ASCII only; drop everything else (see doc comment).
        if !(' '..='~').contains(&c) {
            // Treat a dropped control char (newline/tab) as whitespace so words
            // do not run together: "line1\nline2" -> "line1 line2".
            if c.is_whitespace() {
                ap.pending_space = true;
            }
            continue;
        }
        if c == ' ' {
            ap.pending_space = true;
            continue;
        }
        if !ap.ch(c) {
            return;
        }
    }
    // Trailing lone '?' ("Door still open?").
    if qrun == 1 {
        let _ = ap.ch('?');
    }
}

/// Appends characters to an [`Utterance`], deferring the inter-segment
/// separator and collapsed whitespace until a real character actually arrives.
///
/// The deferral is what stops an all-dropped segment (a body that was pure
/// emoji, say) from leaving a dangling `". "` behind.
struct Appender<'a> {
    out: &'a mut Utterance,
    /// A `". "` owed before the next character (segment boundary).
    pending_sep: bool,
    /// A single `' '` owed before the next character (collapsed whitespace).
    pending_space: bool,
    /// Byte offset this segment began at — floor for word-boundary truncation.
    start: usize,
}

impl Appender<'_> {
    /// Append one character, paying any deferred separator/space first.
    /// Returns `false` once the buffer is full (caller stops).
    fn ch(&mut self, c: char) -> bool {
        if self.pending_sep {
            self.pending_sep = false;
            self.pending_space = false;
            if !ends_with_sentence_punct(self.out) && self.out.push('.').is_err() {
                return false;
            }
            if self.out.push(' ').is_err() {
                return false;
            }
        } else if self.pending_space {
            self.pending_space = false;
            // Never a leading space.
            if !self.out.is_empty() && self.out.push(' ').is_err() {
                return false;
            }
        }
        if self.out.push(c).is_err() {
            // Out of room: back off to the last word boundary so we end on a
            // whole word rather than a half one.
            truncate_to_word(self.out, self.start);
            return false;
        }
        true
    }
}

/// True when `s` already ends in sentence-final punctuation.
fn ends_with_sentence_punct(s: &str) -> bool {
    matches!(s.chars().next_back(), Some('.') | Some('!') | Some('?') | Some(':') | Some(';'))
}

/// Drop back to the last space at or after `floor`, so a truncated utterance
/// ends on a word boundary. Never cuts below `floor` (the segment start).
fn truncate_to_word(out: &mut Utterance, floor: usize) {
    let bytes = out.as_bytes();
    let mut cut = out.len();
    while cut > floor && bytes[cut - 1] != b' ' {
        cut -= 1;
    }
    // All one long word since `floor` — keep what we have rather than nuking it.
    if cut <= floor {
        return;
    }
    out.truncate(cut - 1); // also drop the trailing space
}

/// Ensure the utterance ends as a sentence (Azure's prosody wants it).
fn finish_sentence(out: &mut Utterance) {
    // Strip any trailing whitespace first.
    while out.as_bytes().last() == Some(&b' ') {
        let n = out.len() - 1;
        out.truncate(n);
    }
    if out.is_empty() || ends_with_sentence_punct(out) {
        return;
    }
    if out.push('.').is_err() {
        // Full to the brim: make room for the period rather than dropping it.
        let n = out.len() - 1;
        out.truncate(n);
        let _ = out.push('.');
    }
}

/// Is there anything worth a network round-trip? Guards against burning a
/// ~1.2 s Azure call (and an amp cycle) on an empty or punctuation-only card.
pub fn is_speakable(text: &str) -> bool {
    text.chars().any(|c| c.is_ascii_alphanumeric())
}

// ---------------------------------------------------------------------------
// Request encoding
// ---------------------------------------------------------------------------

/// Room for the JSON envelope plus worst-case escaping of [`MAX_SPEECH`].
///
/// Worst case per char is 6 bytes (a backslash-u escape), but the composer already
/// guarantees printable ASCII, where the worst case is 2 (`\"`, `\\`). Sized at
/// 2× + envelope, and the encoder is bounded regardless of what it is handed.
pub const MAX_REQUEST_BODY: usize = MAX_SPEECH * 2 + 32;

/// The JSON request body sent to the bridge's `POST /tts`.
pub type RequestBody = String<MAX_REQUEST_BODY>;

/// Encode `{"text":"<escaped>"}` for the bridge.
///
/// Returns `None` if the text does not fit — the caller must not send a
/// truncated JSON document (it would be a parse error at the bridge, surfaced
/// as a confusing 400 rather than a clean local skip).
///
/// Escaping covers `"`, `\`, and every control char below 0x20 as `\u00XX`.
/// The composer already strips non-ASCII, but this is the boundary that faces
/// the wire, so it re-escapes defensively rather than trusting its caller.
pub fn encode_json_request(text: &str) -> Option<RequestBody> {
    let mut out = RequestBody::new();
    out.push_str("{\"text\":\"").ok()?;
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\"").ok()?,
            '\\' => out.push_str("\\\\").ok()?,
            '\n' => out.push_str("\\n").ok()?,
            '\r' => out.push_str("\\r").ok()?,
            '\t' => out.push_str("\\t").ok()?,
            // Hand-rolled rather than `write!("\\u{:04x}")`: pulling
            // `core::fmt` into the firmware costs kilobytes of ROM, and this
            // binary is within a few KB of filling its 4 MiB ROM region.
            c if (c as u32) < 0x20 => {
                const HEX: &[u8; 16] = b"0123456789abcdef";
                let b = c as u8;
                out.push_str("\\u00").ok()?;
                out.push(HEX[(b >> 4) as usize] as char).ok()?;
                out.push(HEX[(b & 0xf) as usize] as char).ok()?;
            }
            c => out.push(c).ok()?,
        }
    }
    out.push_str("\"}").ok()?;
    Some(out)
}

// ---------------------------------------------------------------------------
// Response framing
// ---------------------------------------------------------------------------

/// Outcome of [`parse_response_head`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HeadParse {
    /// `\r\n\r\n` not seen yet — read more and call again.
    Incomplete,
    /// Not a status line we can make sense of. Caller should abort.
    Malformed,
    /// Parsed.
    Ok(ResponseHead),
}

/// A parsed bridge response head.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ResponseHead {
    /// HTTP status code.
    pub status: u16,
    /// `Content-Length`, if present. The bridge always sends it for `/tts`
    /// (it fully synthesizes before replying — spec §2.2), so its absence
    /// means read-until-close.
    pub content_length: Option<usize>,
    /// Byte offset of the first body byte within the buffer.
    pub body_offset: usize,
}

impl ResponseHead {
    /// Duration of the announced body, in ms (0 when length is unknown).
    pub fn duration_ms(&self) -> u32 {
        self.content_length.map_or(0, bytes_to_ms)
    }
}

/// Parse a bridge response head out of `buf`.
///
/// Bounded and allocation-free. Handles the head arriving across several
/// `socket.read()` calls (the normal case): returns [`HeadParse::Incomplete`]
/// until the terminator lands.
pub fn parse_response_head(buf: &[u8]) -> HeadParse {
    let Some(term) = find(buf, b"\r\n\r\n") else {
        return HeadParse::Incomplete;
    };
    let head = &buf[..term];
    let body_offset = term + 4;

    let mut lines = split_crlf(head);
    let Some(status_line) = lines.next() else {
        return HeadParse::Malformed;
    };
    let Some(status) = parse_status(status_line) else {
        return HeadParse::Malformed;
    };

    let mut content_length = None;
    for line in lines {
        if let Some(v) = header_value(line, b"content-length") {
            content_length = parse_usize(v);
        }
    }

    HeadParse::Ok(ResponseHead { status, content_length, body_offset })
}

/// `HTTP/1.x <code> ...` → code.
fn parse_status(line: &[u8]) -> Option<u16> {
    if !line.starts_with(b"HTTP/") {
        return None;
    }
    let mut parts = line.split(|&b| b == b' ').filter(|p| !p.is_empty());
    let _version = parts.next()?;
    let code = parts.next()?;
    let n = parse_usize(code)?;
    if n > u16::MAX as usize {
        return None;
    }
    Some(n as u16)
}

/// If `line` is `Name: value` with `name` matching case-insensitively, return
/// the trimmed value.
fn header_value<'a>(line: &'a [u8], name: &[u8]) -> Option<&'a [u8]> {
    let colon = line.iter().position(|&b| b == b':')?;
    let (k, v) = line.split_at(colon);
    if k.len() != name.len() {
        return None;
    }
    if !k.iter().zip(name).all(|(a, b)| a.eq_ignore_ascii_case(b)) {
        return None;
    }
    Some(trim(&v[1..]))
}

fn parse_usize(s: &[u8]) -> Option<usize> {
    let s = trim(s);
    if s.is_empty() {
        return None;
    }
    let mut n: usize = 0;
    for &b in s {
        if !b.is_ascii_digit() {
            return None;
        }
        n = n.checked_mul(10)?.checked_add((b - b'0') as usize)?;
    }
    Some(n)
}

fn trim(mut s: &[u8]) -> &[u8] {
    while let [first, rest @ ..] = s {
        if first.is_ascii_whitespace() {
            s = rest;
        } else {
            break;
        }
    }
    while let [rest @ .., last] = s {
        if last.is_ascii_whitespace() {
            s = rest;
        } else {
            break;
        }
    }
    s
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Iterator over CRLF-separated lines (empty lines skipped).
fn split_crlf<'a>(mut s: &'a [u8]) -> impl Iterator<Item = &'a [u8]> + 'a {
    core::iter::from_fn(move || {
        while !s.is_empty() {
            let (line, rest) = match find(s, b"\r\n") {
                Some(p) => (&s[..p], &s[p + 2..]),
                None => (s, &s[s.len()..]),
            };
            s = rest;
            if !line.is_empty() {
                return Some(line);
            }
        }
        None
    })
}
