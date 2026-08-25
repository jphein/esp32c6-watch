//! bard (#300) tok512 tokenizer: SentencePiece-BPE encode + decode over the packed table
//! inside the SBRD blob. Mirrors llama2.c's `tokenizer.c` semantics exactly — the model was
//! trained against those conventions, so "more sensible" here would mean "wrong".
//!
//! `no_std`, zero alloc, no panic paths: the table is walked once in [`Tokenizer::new`]
//! (which returns `None` on a malformed table rather than trusting it), all buffers are
//! fixed-size, and every id/index is bounds-checked. Linear scans over 512 entries are
//! deliberate — encode runs ONCE per story, and a sorted index would cost RAM we'd rather
//! spend on the KV cache.
//!
//! Two conventions worth knowing, both inherited from SentencePiece via llama2.c:
//!   * DUMMY PREFIX — encode prepends the " " token (a lookup of the one-space string, not a
//!     byte id) before the text, so "Once" tokenizes as " Once". [`Tokenizer::decode`] undoes it by
//!     stripping ONE leading space when the previous token was BOS.
//!   * BYTE FALLBACK — ids 3..259 are literal `<0xXX>` STRINGS, not raw bytes. A character
//!     with no token of its own encodes as `<0xXX>`, and [`Tokenizer::decode`] converts it back to the
//!     byte. (For the shipped tok512 all 88 single-char tokens cover plain-ASCII stories, so
//!     this path is cold — but the model can still SAMPLE such an id, and dropping it would
//!     silently eat characters.)

use super::nano_llm::{rf32, MAX_VOCAB};

/// Beginning-of-sequence id (`\n<s>\n` in the table).
pub const BOS: u16 = 1;
/// End-of-sequence id (`\n</s>\n`).
pub const EOS: u16 = 2;
/// Ids 3..259 are the `<0xXX>` byte-fallback tokens, so byte `b` lives at `3 + b`.
const BYTE_BASE: usize = 3;
/// Scratch for a merge candidate. Pairs longer than the table's longest token can't be in
/// vocab and are skipped before copying, so this only has to hold `max_token_len` (7 here).
const CONCAT: usize = 16;

/// `BYTE_PIECES[b] == b`, so [`Tokenizer::decode`] can return a `&[u8]` of length 1 for a
/// `<0xXX>` token without allocating or borrowing from a temporary.
const fn byte_pieces() -> [u8; 256] {
    let mut a = [0u8; 256];
    let mut i = 0;
    while i < 256 {
        a[i] = i as u8;
        i += 1;
    }
    a
}
static BYTE_PIECES: [u8; 256] = byte_pieces();

/// One hex nibble, or `None` if `c` isn't hex.
const fn hex(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'A'..=b'F' => Some(c - b'A' + 10),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    }
}

/// `b"<0x1F>"` → `Some(0x1F)`; anything else → `None`.
fn parse_byte_token(t: &[u8]) -> Option<u8> {
    if t.len() == 6 && t[0] == b'<' && t[1] == b'0' && t[2] == b'x' && t[5] == b'>' {
        Some((hex(t[3])? << 4) | hex(t[4])?)
    } else {
        None
    }
}

/// Borrowed view of the blob's tokenizer table plus the two indexes encode/decode need.
pub struct Tokenizer<'a> {
    /// The packed table: `u32 max_token_len`, then `vocab × { f32 score, u8 len, bytes }`.
    table: &'a [u8],
    /// Live vocabulary size (≤ [`MAX_VOCAB`]).
    vocab: usize,
    /// Byte offset of each entry within `table` (points at its score word).
    offsets: [u32; MAX_VOCAB],
    /// `byte_id[b]` = the id whose text is exactly the single byte `b`, else 0 (`<unk>`,
    /// a safe sentinel since no real single-byte token can be id 0).
    byte_id: [u16; 256],
    /// Longest token text actually present, MEASURED during the walk. The table's leading
    /// word is advisory only — trusting a too-small value there would silently skip merges.
    max_token_len: usize,
}

impl<'a> Tokenizer<'a> {
    /// Index `table` for a `vocab`-entry tokenizer, or `None` if the table is malformed
    /// (short, inconsistent, or holding a token longer than the 16-byte merge scratch).
    pub fn new(table: &'a [u8], vocab: usize) -> Option<Self> {
        if vocab == 0 || vocab > MAX_VOCAB || table.len() < 4 {
            return None;
        }
        let mut t = Self {
            table,
            vocab,
            offsets: [0; MAX_VOCAB],
            byte_id: [0; 256],
            max_token_len: 0,
        };
        // Walk the packed entries once: record each offset, the single-byte index, and the
        // real longest length. Any entry running past the end fails the whole construction.
        let mut p = 4usize;
        for id in 0..vocab {
            if p + 5 > table.len() {
                return None;
            }
            let ln = table[p + 4] as usize;
            let end = p + 5 + ln;
            if end > table.len() {
                return None;
            }
            t.offsets[id] = p as u32;
            if ln == 1 && t.byte_id[table[p + 5] as usize] == 0 {
                t.byte_id[table[p + 5] as usize] = id as u16;
            }
            if ln > t.max_token_len {
                t.max_token_len = ln;
            }
            p = end;
        }
        if t.max_token_len > CONCAT {
            return None;
        }
        // Parity with the exporter's `assert p == len(tok)`: the walk must land EXACTLY on the
        // end. Trailing bytes mean the table and `vocab` disagree about how many entries there
        // are — refuse rather than run on a table we only partly understand.
        if p != table.len() {
            return None;
        }
        Some(t)
    }

    /// `(score, text)` for `id`; `(0.0, b"")` for an out-of-range id (never panics — the
    /// sampler is a separate concern and must not be able to fault the tokenizer).
    pub fn entry(&self, id: u16) -> (f32, &'a [u8]) {
        if id as usize >= self.vocab {
            return (0.0, &[]);
        }
        let p = self.offsets[id as usize] as usize;
        let ln = self.table[p + 4] as usize;
        (rf32(self.table, p), &self.table[p + 5..p + 5 + ln])
    }

    /// Raw text of `id` (empty when out of range).
    pub fn text(&self, id: u16) -> &'a [u8] {
        self.entry(id).1
    }

    /// The id whose text is exactly `s`, if any. Linear — see the module note.
    pub fn lookup(&self, s: &[u8]) -> Option<u16> {
        (0..self.vocab as u16).find(|&id| self.text(id) == s)
    }

    /// Printable bytes for `id`, given the token before it.
    ///
    /// Strips the dummy-prefix space directly after BOS and expands `<0xXX>` back to its
    /// byte. May therefore return a single byte of a multi-byte UTF-8 sequence — callers
    /// that need `str` must reassemble (the display path writes bytes, so it doesn't).
    pub fn decode(&self, prev: u16, id: u16) -> &'a [u8] {
        let t = self.text(id);
        let t = if prev == BOS && t.first() == Some(&b' ') {
            &t[1..]
        } else {
            t
        };
        match parse_byte_token(t) {
            Some(b) => &BYTE_PIECES[b as usize..b as usize + 1],
            None => t,
        }
    }

    /// Encode `text` into `out` as BOS followed by BPE ids; returns the count written.
    ///
    /// Greedy highest-score pair merging, exactly like llama2.c: seed with per-character
    /// tokens (dummy space first), then repeatedly merge the adjacent pair whose
    /// concatenation is the best-scoring vocabulary entry. Output is truncated rather than
    /// overflowing `out`.
    pub fn encode(&self, text: &str, out: &mut [u16]) -> usize {
        if out.is_empty() {
            return 0;
        }
        out[0] = BOS;
        let mut n = 1usize;

        // Dummy prefix: the " " TOKEN (llama2.c looks up the one-space STRING here, which is
        // not necessarily `byte_id[b' ']` in a vocab with fancier whitespace tokens).
        if !text.is_empty() && n < out.len() {
            if let Some(id) = self.lookup(b" ") {
                out[n] = id;
                n += 1;
            }
        }

        // Seed per CODEPOINT, not per byte. The table holds 14 multi-byte UTF-8 tokens
        // (’ “ ” – — ‘ é … ñ â € ™ and two spaces), and seeding byte-wise would shred each into
        // three `<0xXX>` fallbacks that can NEVER merge back into the real token. llama2.c
        // accumulates continuation bytes, looks the whole codepoint up first, and only falls
        // back per byte on a miss; `chars()` is exactly that segmentation for the valid UTF-8
        // a `&str` guarantees.
        let mut cp = [0u8; 4];
        for ch in text.chars() {
            if n == out.len() {
                break;
            }
            let piece = ch.encode_utf8(&mut cp).as_bytes();
            let whole = if piece.len() == 1 {
                // Fast path: the single-byte index IS a one-byte `lookup`.
                match self.byte_id[piece[0] as usize] {
                    0 => None,
                    id => Some(id),
                }
            } else {
                self.lookup(piece)
            };
            match whole {
                Some(id) => {
                    out[n] = id;
                    n += 1;
                }
                None => {
                    for &b in piece {
                        if n == out.len() {
                            break;
                        }
                        out[n] = if BYTE_BASE + (b as usize) < self.vocab {
                            (BYTE_BASE + b as usize) as u16 // byte fallback: the `<0xXX>` token
                        } else {
                            0 // <unk>: no token and no fallback slot
                        };
                        n += 1;
                    }
                }
            }
        }

        // Merge until no adjacent pair concatenates to a known token.
        loop {
            let mut best_score = f32::NEG_INFINITY;
            let mut best_id = 0u16;
            let mut best_at = usize::MAX;
            for i in 0..n.saturating_sub(1) {
                let (a, b) = (self.text(out[i]), self.text(out[i + 1]));
                let ln = a.len() + b.len();
                // Longer than any token in the table ⇒ cannot be a merge; also keeps the
                // concat inside CONCAT without a runtime check.
                if ln > self.max_token_len {
                    continue;
                }
                let mut buf = [0u8; CONCAT];
                buf[..a.len()].copy_from_slice(a);
                buf[a.len()..ln].copy_from_slice(b);
                if let Some(id) = self.lookup(&buf[..ln]) {
                    let score = self.entry(id).0;
                    if score > best_score {
                        best_score = score;
                        best_id = id;
                        best_at = i;
                    }
                }
            }
            if best_at == usize::MAX {
                break;
            }
            out[best_at] = best_id;
            for k in best_at + 1..n - 1 {
                out[k] = out[k + 1];
            }
            n -= 1;
        }
        n
    }
}
