//! A bounded, resumable, allocation-free JSON scanner.
//!
//! # Why this exists rather than `serde_json_core`
//!
//! The daemon's chapter payload is ~18 KB: an 8,312-byte `text_md` plus segment
//! `text` fields up to **3,665 bytes each** (measured on live chapter 1). A
//! deserialize-into-a-struct parser needs the whole document, or at least whole
//! field values, resident. On a 512 KB device with no PSRAM and ~9 KB of `.bss`
//! headroom, that is the wrong shape — and the watch wants none of the prose
//! anyway, only `start_ms`/`end_ms`/`speaker`/`kind`.
//!
//! So this scanner is a **byte-at-a-time state machine** fed whatever arrives
//! from the socket, in pieces of any size, including a piece that splits a
//! string, an escape, a `\uXXXX` sequence or a number down the middle. It emits
//! events as they complete and holds nothing else.
//!
//! # The load-bearing property: discard without losing sync
//!
//! String values are captured up to [`MAX_STR`] bytes and the remainder is
//! **counted but not stored**. The state machine still tracks quoting and
//! escaping to the closing quote, so a 3,665-byte value costs 64 bytes of RAM
//! and cannot desynchronise the parse. That is the whole trick: prose falls on
//! the floor while the structure stays intact.
//!
//! # Hardening
//!
//! Every input is untrusted (§9.1: the daemon's routes are unauthenticated on
//! the LAN, so anything on the `/24` can shape these bytes). There is no
//! indexing that can be out of range, no `unwrap`, no recursion, and all
//! arithmetic is saturating. Nesting deeper than [`MAX_DEPTH`] latches an error
//! instead of overflowing a stack. A malformed document stops producing events;
//! it never panics.

use heapless::Vec;

/// Longest string value kept, in bytes. Titles, speaker names, `kind`s, item
/// names and equipment slot values all fit comfortably; prose does not, and is
/// meant not to.
pub const MAX_STR: usize = 64;

/// Longest object key kept, in bytes. The longest key in any payload the watch
/// reads is `cast_judged_against_backends` (28) on `/api/state`, which the watch
/// does not use; every key it *matches* is ≤ 16.
pub const MAX_KEY: usize = 32;

/// Maximum container nesting. The deepest live payload is `/api/chapters/{n}`
/// at 4 (`root → manifest → segments → element`); 8 leaves slack without
/// letting a hostile document push the frame stack.
pub const MAX_DEPTH: usize = 8;

/// Longest number token accepted, in bytes. `u64::MAX` is 20 digits.
const MAX_NUM: usize = 24;

/// A captured string, truncated to [`MAX_STR`] and always valid UTF-8.
///
/// `truncated` is not cosmetic: code matching a key or an enum-like value must
/// reject a truncated capture, because a truncated string can alias a different
/// legitimate one.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Text {
    buf: Vec<u8, MAX_STR>,
    /// Valid-UTF-8 prefix length of `buf`. Set once when the string completes,
    /// so [`as_str`](Self::as_str) is infallible and free.
    valid: usize,
    truncated: bool,
}

impl Text {
    /// The captured text. Never panics; never returns invalid UTF-8.
    pub fn as_str(&self) -> &str {
        match self.buf.get(..self.valid) {
            Some(b) => core::str::from_utf8(b).unwrap_or(""),
            None => "",
        }
    }

    /// True when the source string was longer than [`MAX_STR`] and was clipped.
    pub fn truncated(&self) -> bool {
        self.truncated
    }

    /// True when this capture is a complete, untruncated match for `s`.
    ///
    /// Truncation-aware by design: `matches("text")` must not be satisfied by
    /// the first four bytes of a 3,665-byte value. Deliberately NOT named `eq`:
    /// `Text` also derives `PartialEq`, and two methods with one name where only
    /// one of them is truncation-aware is a trap.
    pub fn matches(&self, s: &str) -> bool {
        !self.truncated && self.as_str() == s
    }

    /// Recompute `valid` as the longest valid-UTF-8 prefix.
    ///
    /// Truncating at [`MAX_STR`] can land mid-sequence (the daemon escapes most
    /// non-ASCII as `\uXXXX`, but raw UTF-8 is legal JSON and a hostile sender
    /// will use it). Trimming back to a boundary keeps `as_str` infallible
    /// rather than pushing the problem to every caller.
    fn finish(&mut self) {
        self.valid = match core::str::from_utf8(&self.buf) {
            Ok(_) => self.buf.len(),
            Err(e) => e.valid_up_to(),
        };
    }

    /// Append one byte, flagging truncation once full. Never fails.
    fn push(&mut self, b: u8) {
        if self.buf.push(b).is_err() {
            self.truncated = true;
        }
    }

    /// Append a `char`'s UTF-8 encoding (used by `\uXXXX` decoding).
    fn push_char(&mut self, c: char) {
        let mut enc = [0u8; 4];
        for b in c.encode_utf8(&mut enc).as_bytes() {
            self.push(*b);
        }
    }
}

/// One lexical event. Owned rather than borrowed so the scanner can hand it out
/// while holding `&mut self` — the buffer is moved out with `mem::take`, which
/// also performs the reset the next token needs, so this costs no copy.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Event {
    ObjOpen,
    ObjClose,
    ArrOpen,
    ArrClose,
    /// An object member name.
    Key(Text),
    /// A string value.
    Str(Text),
    /// An integer value. A fractional part, if any, is discarded (no payload
    /// the watch reads carries one; accepting it beats rejecting the document).
    Int(i64),
    Bool(bool),
    Null,
}

/// Which container we are inside, so a completed string can be classified as a
/// key or a value without lookahead.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Frame {
    Obj,
    Arr,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum St {
    /// Between tokens.
    Idle,
    /// Inside a string; `key` says whether it becomes [`Event::Key`].
    Str { key: bool },
    /// Just consumed a backslash inside a string.
    Esc { key: bool },
    /// Collecting the 4 hex digits of `\uXXXX`; `n` already collected.
    Hex { key: bool, n: u8 },
    /// Accumulating a number token.
    Num,
    /// Accumulating a bare literal (`true` / `false` / `null`).
    Lit,
}

/// A resumable JSON scanner. Feed it bytes; it calls your sink per event.
///
/// One instance parses one document. [`error`](Self::error) latches on
/// malformed input and no further events are emitted, so a caller can check it
/// once at the end rather than after every feed.
pub struct Scanner {
    st: St,
    stack: Vec<Frame, MAX_DEPTH>,
    /// In an object: true when the next string is a member name.
    want_key: bool,
    /// In an object: a key has been read and its `:` has not. Makes a missing
    /// colon a structural error instead of silently accepting `{"a" 1}` — which
    /// matters because [`complete`](Scanner::complete) is what the firmware
    /// trusts to mean "a whole, well-formed chapter arrived".
    need_colon: bool,
    text: Text,
    num: Vec<u8, MAX_NUM>,
    hex: u16,
    /// High surrogate awaiting its pair.
    pending_hi: Option<u16>,
    error: bool,
}

impl Default for Scanner {
    fn default() -> Self {
        Self::new()
    }
}

impl Scanner {
    pub const fn new() -> Self {
        Self {
            st: St::Idle,
            stack: Vec::new(),
            want_key: false,
            need_colon: false,
            text: Text { buf: Vec::new(), valid: 0, truncated: false },
            num: Vec::new(),
            hex: 0,
            pending_hi: None,
            error: false,
        }
    }

    /// True once malformed input was seen. Sticky.
    pub fn error(&self) -> bool {
        self.error
    }

    /// True when every opened container has been closed — i.e. a complete
    /// document was consumed. Distinguishes "done" from "the socket died
    /// mid-payload", which otherwise look identical to the caller.
    pub fn complete(&self) -> bool {
        !self.error && self.stack.is_empty() && matches!(self.st, St::Idle)
    }

    /// Current nesting depth.
    pub fn depth(&self) -> u8 {
        self.stack.len() as u8
    }

    /// Feed a slice. `sink` receives `(event, depth)` where `depth` is the
    /// nesting level the event sits at: for `ObjOpen`/`ObjClose` it is the
    /// depth of that object itself, and for keys and scalars it is the depth of
    /// their containing object or array.
    ///
    /// Call repeatedly with successive pieces; boundaries may fall anywhere,
    /// including inside a string, an escape or a number.
    pub fn feed<F>(&mut self, bytes: &[u8], sink: &mut F)
    where
        F: FnMut(&Event, u8),
    {
        for &b in bytes {
            if self.error {
                return;
            }
            self.byte(b, sink);
        }
    }

    fn fail(&mut self) {
        self.error = true;
    }

    fn byte<F>(&mut self, b: u8, sink: &mut F)
    where
        F: FnMut(&Event, u8),
    {
        match self.st {
            St::Str { key } => self.byte_in_str(b, key, sink),
            St::Esc { key } => self.byte_in_esc(b, key),
            St::Hex { key, n } => self.byte_in_hex(b, key, n),
            St::Num => {
                if matches!(b, b'0'..=b'9' | b'-' | b'+' | b'.' | b'e' | b'E') {
                    // Overlong tokens are clipped, not rejected: the value is
                    // already nonsense for our purposes and bailing would
                    // discard an otherwise-good document.
                    let _ = self.num.push(b);
                } else {
                    self.finish_num(sink);
                    self.st = St::Idle;
                    self.byte(b, sink); // re-dispatch the delimiter
                }
            }
            St::Lit => {
                if b.is_ascii_alphabetic() {
                    let _ = self.num.push(b);
                } else {
                    self.finish_lit(sink);
                    if self.error {
                        return;
                    }
                    self.st = St::Idle;
                    self.byte(b, sink);
                }
            }
            St::Idle => self.byte_idle(b, sink),
        }
    }

    fn byte_idle<F>(&mut self, b: u8, sink: &mut F)
    where
        F: FnMut(&Event, u8),
    {
        match b {
            b' ' | b'\t' | b'\r' | b'\n' => {}
            b'{' => {
                if !self.value_ok() {
                    return self.fail();
                }
                if self.stack.push(Frame::Obj).is_err() {
                    return self.fail();
                }
                self.want_key = true;
                self.need_colon = false;
                sink(&Event::ObjOpen, self.depth());
            }
            b'[' => {
                if !self.value_ok() {
                    return self.fail();
                }
                if self.stack.push(Frame::Arr).is_err() {
                    return self.fail();
                }
                self.want_key = false;
                self.need_colon = false;
                sink(&Event::ArrOpen, self.depth());
            }
            b'}' => {
                // A key whose colon or value never arrived (`{"a"}`, `{"a":}`).
                if self.need_colon || !self.want_key {
                    return self.fail();
                }
                let d = self.depth();
                match self.stack.pop() {
                    Some(Frame::Obj) => sink(&Event::ObjClose, d),
                    _ => return self.fail(),
                }
                self.after_value();
            }
            b']' => {
                let d = self.depth();
                match self.stack.pop() {
                    Some(Frame::Arr) => sink(&Event::ArrClose, d),
                    _ => return self.fail(),
                }
                self.after_value();
            }
            b'"' => {
                let is_key = self.in_obj() && self.want_key;
                if !is_key && !self.value_ok() {
                    return self.fail();
                }
                self.text = Text::default();
                self.pending_hi = None;
                self.st = St::Str { key: is_key };
            }
            b':' => {
                if !self.need_colon {
                    return self.fail();
                }
                self.need_colon = false;
            }
            b',' => {
                if self.need_colon {
                    return self.fail();
                }
                self.want_key = self.in_obj();
            }
            b'-' | b'0'..=b'9' => {
                if !self.value_ok() {
                    return self.fail();
                }
                self.num.clear();
                let _ = self.num.push(b);
                self.st = St::Num;
            }
            b't' | b'f' | b'n' => {
                if !self.value_ok() {
                    return self.fail();
                }
                self.num.clear();
                let _ = self.num.push(b);
                self.st = St::Lit;
            }
            _ => self.fail(),
        }
    }

    /// True when a value may legally start here: outside an object always, and
    /// inside one only after a key and its colon.
    fn value_ok(&self) -> bool {
        !self.in_obj() || (!self.want_key && !self.need_colon)
    }

    fn byte_in_str<F>(&mut self, b: u8, key: bool, sink: &mut F)
    where
        F: FnMut(&Event, u8),
    {
        // Anything other than a backslash settles the question of whether a
        // held high surrogate will get its pair: it will not.
        if b != b'\\' {
            self.flush_pending_hi();
        }
        match b {
            b'\\' => self.st = St::Esc { key },
            b'"' => {
                self.text.finish();
                let t = core::mem::take(&mut self.text);
                let d = self.depth();
                if key {
                    sink(&Event::Key(t), d);
                    // A colon must follow before any value is legal.
                    self.want_key = false;
                    self.need_colon = true;
                    self.st = St::Idle;
                } else {
                    sink(&Event::Str(t), d);
                    self.st = St::Idle;
                    self.after_value();
                }
            }
            // Unescaped control characters are illegal JSON. Accepted rather
            // than fatal: rejecting the whole chapter over one stray byte in
            // prose we are about to discard would be the worse failure.
            _ => self.text.push(b),
        }
    }

    fn byte_in_esc(&mut self, b: u8, key: bool) {
        self.st = St::Str { key };
        // Only `\u` can supply a low surrogate; any other escape orphans one
        // that is being held.
        if b != b'u' {
            self.flush_pending_hi();
        }
        match b {
            b'"' => self.text.push(b'"'),
            b'\\' => self.text.push(b'\\'),
            b'/' => self.text.push(b'/'),
            b'b' => self.text.push(0x08),
            b'f' => self.text.push(0x0c),
            b'n' => self.text.push(b'\n'),
            b'r' => self.text.push(b'\r'),
            b't' => self.text.push(b'\t'),
            b'u' => {
                self.hex = 0;
                self.st = St::Hex { key, n: 0 };
            }
            // Unknown escape: keep the character itself. Lenient for the same
            // reason as above.
            other => self.text.push(other),
        }
    }

    fn byte_in_hex(&mut self, b: u8, key: bool, n: u8) {
        let Some(v) = hex_val(b) else {
            // Not a hex digit — abandon the escape and treat the byte as text.
            self.st = St::Str { key };
            self.flush_pending_hi();
            self.text.push(b);
            return;
        };
        self.hex = (self.hex << 4) | v as u16;
        let n = n.saturating_add(1);
        if n < 4 {
            self.st = St::Hex { key, n };
            return;
        }

        let cp = self.hex;
        self.st = St::Str { key };
        match self.pending_hi.take() {
            // Low surrogate completing a pair.
            Some(hi) if (0xDC00..=0xDFFF).contains(&cp) => {
                let scalar = 0x1_0000
                    + (((hi as u32) - 0xD800) << 10)
                    + ((cp as u32) - 0xDC00);
                self.text.push_char(char::from_u32(scalar).unwrap_or('\u{FFFD}'));
            }
            // A high surrogate not followed by a low one: emit a replacement
            // for the orphan, then reconsider the current code point.
            Some(_) => {
                self.text.push_char('\u{FFFD}');
                self.emit_cp(cp);
            }
            None => self.emit_cp(cp),
        }
    }

    /// Emit a replacement char for a high surrogate that never got its pair.
    ///
    /// Without this, `"\ud83dx"` silently drops the orphan and the string comes
    /// back as `"x"` — data loss that looks like correct output.
    fn flush_pending_hi(&mut self) {
        if self.pending_hi.take().is_some() {
            self.text.push_char('\u{FFFD}');
        }
    }

    /// Store one BMP code point, holding a high surrogate for its pair.
    fn emit_cp(&mut self, cp: u16) {
        if (0xD800..=0xDBFF).contains(&cp) {
            self.pending_hi = Some(cp);
        } else {
            self.text.push_char(char::from_u32(cp as u32).unwrap_or('\u{FFFD}'));
        }
    }

    fn finish_num<F>(&mut self, sink: &mut F)
    where
        F: FnMut(&Event, u8),
    {
        let v = parse_int(&self.num).unwrap_or(0);
        self.num.clear();
        sink(&Event::Int(v), self.depth());
        self.after_value();
    }

    fn finish_lit<F>(&mut self, sink: &mut F)
    where
        F: FnMut(&Event, u8),
    {
        let d = self.depth();
        let ev = match self.num.as_slice() {
            b"true" => Event::Bool(true),
            b"false" => Event::Bool(false),
            b"null" => Event::Null,
            _ => return self.fail(),
        };
        self.num.clear();
        sink(&ev, d);
        self.after_value();
    }

    /// After any complete value, an object expects a key next.
    fn after_value(&mut self) {
        self.want_key = self.in_obj();
        self.need_colon = false;
    }

    fn in_obj(&self) -> bool {
        matches!(self.stack.last(), Some(Frame::Obj))
    }
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Integer part of a JSON number token. Saturating, so no input can panic and
/// none can wrap into a plausible-looking wrong value.
fn parse_int(tok: &[u8]) -> Option<i64> {
    let (neg, rest) = match tok.split_first() {
        Some((b'-', r)) => (true, r),
        Some(_) => (false, tok),
        None => return None,
    };
    let mut acc: i64 = 0;
    let mut any = false;
    for &b in rest {
        match b {
            b'0'..=b'9' => {
                any = true;
                acc = acc.saturating_mul(10).saturating_add((b - b'0') as i64);
            }
            // Stop at the fractional/exponent part: we want the integer value.
            b'.' | b'e' | b'E' => break,
            _ => return None,
        }
    }
    if !any {
        return None;
    }
    Some(if neg { -acc } else { acc })
}
