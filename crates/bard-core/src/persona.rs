//! bard (#300): per-node protagonist — every board tells its own kind of stories (spec §8).
//!
//! Words must be TinyStories-FREQUENT. The model has a 512-token vocabulary, so the realm's own
//! names ("Draconic Dominion", "Eldritch Nexus") would shred into `<0xXX>` byte fallbacks and
//! poison the prompt (spec §8) — hence plain nursery nouns, each of which the tokenizer holds
//! as whole words. The node's identity survives in WHICH protagonist it gets, not in spelling
//! its realm name at the model.
//!
//! Pure: no `hw`, no radio, no alloc. Exported to the host lib beside the other cores so the
//! prompt lengths are testable off-device.

/// The 16 protagonists, indexed by node id (see [`protagonist`]).
pub const PROTAGONISTS: [&str; 16] = [
    "a little dragon",
    "a little owl",
    "a little bird",
    "a brave cat",
    "a tiny robot",
    "a little fish",
    "a small dog",
    "a little bunny",
    "a happy bear",
    "a little star",
    "a small mouse",
    "a little duck",
    "a small frog",
    "a kind girl",
    "a brave boy",
    "a little pony",
];

/// This node's protagonist.
///
/// id7 Draconic Dominion → dragon, id8 Eldritch Nexus → owl, id9 Jade Herald → bird: the three
/// live boards get a creature that matches their realm persona. Every other id falls back to
/// `id % 16`, so an unprovisioned board still tells a coherent story instead of nothing.
pub fn protagonist(node_id: u8) -> &'static str {
    PROTAGONISTS[match node_id {
        7 => 0,
        8 => 1,
        9 => 2,
        n => (n as usize) % 16,
    }]
}

/// Fill `buf` with this node's full prompt; returns the used length.
///
/// Takes a caller-owned buffer rather than returning a `&str` because the firmware builds this
/// into a static — no alloc anywhere in the bard. The longest possible prompt is 28 + 15 = 43
/// bytes, so the 64-byte buffer cannot overflow (asserted by the host test over all 16).
pub fn prompt(node_id: u8, buf: &mut [u8; 64]) -> usize {
    let mut n = 0;
    for part in ["Once upon a time, there was ", protagonist(node_id)] {
        // The 64-byte buffer and the table are coupled only by arithmetic that happens to work
        // (28 + 15 = 43). Say so out loud, so adding "a very sleepy little dragon" to
        // PROTAGONISTS trips here in a debug build instead of panicking on the slice below.
        debug_assert!(
            n + part.len() <= buf.len(),
            "prompt buffer too small for this protagonist"
        );
        buf[n..n + part.len()].copy_from_slice(part.as_bytes());
        n += part.len();
    }
    n
}

// ── #303 runtime prompt validation ──────────────────────────────────────────────────────
//
// A prompt settable from the HA dashboard is only safe if bad prompts are REFUSED, so the
// validation is the feature — not an afterthought. But be precise about the two DIFFERENT
// hazards, because they need different answers (measured against the shipped tok512 table):
//
//   1. Bytes with no token at all — emoji, most accented letters, exotic punctuation. These
//      encode to `<0xXX>` BYTE-FALLBACK tokens the model never saw in training. Unambiguously
//      bad, and cheap to detect ⇒ **hard reject**.
//   2. ASCII words the vocabulary simply lacks — "Eldritch", "Nexus", a person's name. The
//      table has single-char tokens for A-Z/a-z, so these are REPRESENTABLE: they fragment
//      into char-level tokens rather than shredding. The model handles them badly (they are
//      out-of-distribution for a 260K-param TinyStories net) but the output is still English
//      ⇒ **accept, and report the fragmentation** so the operator can see why prose got worse.
//
// The tell for (2) is bytes-per-token: ordinary TinyStories prose runs ~2.5 B/token (measured:
// 58 bytes of plain prose = 23 tokens), while a fragmented word approaches 1 B/token.
// `validate_prompt` returns the token count so the caller can log both numbers instead of
// silently accepting a prompt that will read poorly.
//
// Useful emergent property: because fragmentation inflates the token count, the token BUDGET
// below doubles as an automatic backstop on hazard (2) — mild fragmentation is accepted and
// reported, severe fragmentation refuses itself for spending the whole window. Both behaviours
// are pinned by tests, so this comment cannot quietly become false.

/// How many of the shared context window's tokens a prompt may spend. `SEQ_CAP` is 80 and the
/// prompt and the story draw from the SAME window, so every prompt token is a story token
/// spent: 32 leaves ~48 for the tale. The built-in prompts cost ~15.
// GATING: the runtime prompt arrives over the keyed-CFG channel, which needs the radio — so in a
// radio-free `bard` build (a dedicated storyteller board) there is no way to set one and this code
// is genuinely dead, not merely unused. Gate it on the channel that feeds it rather than silencing
// the lint; `hostsim` compiles it for the tests.
#[cfg(any(feature = "espnow", feature = "hostsim", feature = "full"))]
pub const PROMPT_TOKEN_BUDGET: usize = 32;

/// The first byte-fallback token id (`<0x00>`); ids `3..=258` are the 256 raw-byte tokens.
#[cfg(any(feature = "espnow", feature = "hostsim", feature = "full"))]
const BYTE_FALLBACK_LO: u16 = 3;
#[cfg(any(feature = "espnow", feature = "hostsim", feature = "full"))]
const BYTE_FALLBACK_HI: u16 = 258;

/// Why a candidate prompt was refused. Carries the position so the operator can be told
/// *which* word is the problem rather than just "invalid".
#[cfg(any(feature = "espnow", feature = "hostsim", feature = "full"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptErr {
    /// Longer than the 64-byte prompt buffer (and the 64-byte CFG value that carries it).
    TooLong { got: usize },
    /// Not valid UTF-8 — a CFG payload is arbitrary bytes off the wire, never trusted.
    NotUtf8,
    /// Encodes past [`PROMPT_TOKEN_BUDGET`]; would leave too little window for a story.
    TooManyTokens { got: usize },
    /// Contains a byte the vocabulary has no token for (emoji, most non-ASCII), so it shredded
    /// into `<0xXX>` byte-fallback tokens. `at_byte` locates the first one, so the operator can
    /// be told WHICH character is the problem. Note this does NOT fire for merely-unknown ASCII
    /// words — see hazard (2) above; those are accepted and reported as fragmentation.
    UnrepresentableByte { at_byte: usize },
}

/// Validate a candidate prompt against the model's own vocabulary.
///
/// Returns the token count on success. Pure and host-tested: the firmware calls this before
/// storing an operator-supplied prompt, and rejects keep the previous value.
#[cfg(any(feature = "espnow", feature = "hostsim", feature = "full"))]
pub fn validate_prompt(tok: &super::tokenizer::Tokenizer, bytes: &[u8]) -> Result<usize, PromptErr> {
    if bytes.len() > 64 {
        return Err(PromptErr::TooLong { got: bytes.len() });
    }
    let s = core::str::from_utf8(bytes).map_err(|_| PromptErr::NotUtf8)?;
    let mut ids = [0u16; 64];
    let n = tok.encode(s, &mut ids);
    if n > PROMPT_TOKEN_BUDGET {
        return Err(PromptErr::TooManyTokens { got: n });
    }
    // Walk the ids, tracking how far into the text each one lands, so a refusal can point at
    // the offending word instead of just failing.
    let mut at = 0usize;
    for (i, &id) in ids[..n].iter().enumerate() {
        if (BYTE_FALLBACK_LO..=BYTE_FALLBACK_HI).contains(&id) {
            return Err(PromptErr::UnrepresentableByte { at_byte: at });
        }
        // id 0 is `<unk>` (5 bytes of literal text) and BOS carries no text; neither advances
        // the cursor meaningfully, and the leading BOS is index 0.
        if i > 0 {
            at += tok.text(id).len();
        }
    }
    Ok(n)
}
