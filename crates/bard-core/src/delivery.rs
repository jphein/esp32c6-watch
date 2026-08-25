//! bard (#302): HOW the narration is delivered — reveal speed and the inf/page mode — plus the
//! strict parser for the CFG `V` value that carries both.
//!
//! Pure, `no_std`, alloc-free and host-tested: this is a parser for bytes that arrive from a broker
//! over the air, so "strict and panic-free" is the whole specification. A refusal must keep the
//! previous setting (the caller's job) and say which field was wrong (this module's job).
//!
//! Wire format: `<ms_per_char>:<mode>[:<font>]`, e.g. `160:inf`, `80:page`, `120:inf:9x15`.
//!   * the FONT field is OPTIONAL, so every value already retained on a broker (`120:inf`) keeps
//!     parsing and keeps its font — a new field must never invalidate a fleet's stored config.
//!   * EMPTY value ⇒ every default (the house "empty = board default" convention, as `S` and `T`).
//!   * an EMPTY FIELD ⇒ that field's default: `:page` sets the mode and leaves the speed alone,
//!     `80:` the reverse. Cheap to support and it saves the dashboard from having to know both.
//!   * the colon is REQUIRED otherwise — a bare `160` is refused rather than guessed at, because
//!     guessing is how `page` would one day be read as a speed.
//!   * speed out of range is CLAMPED, not refused (`Accepted::clamped` says so, for the log): the
//!     range exists to stop a 0 pegging the reveal loop, and refusing a well-meant 1000 would be
//!     less useful than honouring it as 500.

/// How the narration reaches the panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Write forever, scrolling continuously. Generation self-paces to the reveal (see the
    /// screen's backpressure), so the reading speed is also the compute duty cycle.
    Inf,
    /// Write one screenful of new text, then WAIT for a button press — turning a page.
    Page,
}

/// Panel font, i.e. how much story fits on the glass at once (#302, JP: "we should make font size a
/// slider in ha as well"). embedded-graphics ships these four ASCII monospace faces; the token is the
/// face's own name so it stays self-describing if the set ever changes.
///
/// The geometry each one yields on the 72×40 panel is computed in `bard::mod` — a bigger font means
/// FEWER characters, not less story: the KV window is sized in TOKENS, so generation is completely
/// unaffected (the model never learns what font it is being read in).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Font {
    /// 5×8 — 14 cols × 5 rows. The pre-#302 default and the most story per screen.
    F5x8,
    /// 6×10 — 12 cols × 4 rows.
    F6x10,
    /// 9×15 — 8 cols × 2 rows.
    F9x15,
    /// 10×20 — 7 cols × 2 rows. Readable across a room, about a phrase at a time.
    F10x20,
}

/// The 0.42" SSD1306 panel, in pixels. The character grid every font yields is this divided by the
/// face's glyph box — kept HERE, beside the font choice, so the geometry is host-testable and so the
/// operator-facing "how much fits" answer lives with the thing that decides it.
pub const PANEL_W: usize = 72;
/// Panel height in pixels.
pub const PANEL_H: usize = 40;

impl Font {
    /// The face's glyph box `(width, height)` in pixels. Mirrors embedded-graphics' own metrics for
    /// each face; `bard::panel_font` debug-asserts the two agree, so this cannot drift silently.
    pub const fn glyph(&self) -> (usize, usize) {
        match self {
            Font::F5x8 => (5, 8),
            Font::F6x10 => (6, 10),
            Font::F9x15 => (9, 15),
            Font::F10x20 => (10, 20),
        }
    }

    /// Characters that fit the panel at this face: `(cols, rows)`. At least 1×1 whatever the face, so
    /// no layout arithmetic downstream can divide into nothing.
    pub const fn grid(&self) -> (usize, usize) {
        let (w, h) = self.glyph();
        let cols = PANEL_W / w;
        let rows = PANEL_H / h;
        (if cols == 0 { 1 } else { cols }, if rows == 0 { 1 } else { rows })
    }
}

/// A delivery setting: reveal pace plus mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Delivery {
    /// Milliseconds per revealed character, always within [`Delivery::MS_MIN`]`..=`[`Delivery::MS_MAX`].
    pub ms_per_char: u16,
    /// Continuous or page-at-a-time.
    pub mode: Mode,
    /// Panel font — how much of the story is on the glass at once.
    pub font: Font,
}

/// A parsed value, plus whether the speed had to be clamped into range (worth a log line: the
/// operator asked for something we did not do).
///
/// Gated on the tiers that HAVE a config channel, like `persona::validate_prompt` (#303): in a
/// radio-free `bard` build nothing can offer a `V` value, so the parser is genuinely dead code
/// rather than merely unused — gated on the channel that feeds it instead of silenced with an allow.
#[cfg(any(feature = "espnow", feature = "hostsim", feature = "full"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Accepted {
    /// The setting to apply.
    pub delivery: Delivery,
    /// `true` if `ms_per_char` was outside the allowed range and was clamped.
    pub clamped: bool,
}

/// Why a `V` value was refused. Every variant leaves the previous setting in place. Gated as
/// [`Accepted`].
#[cfg(any(feature = "espnow", feature = "hostsim", feature = "full"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryErr {
    /// Longer than any legal value — refused before it is even looked at.
    TooLong {
        /// Bytes offered.
        got: usize,
    },
    /// No `:` separator: the value is not a `<ms>:<mode>` pair.
    Malformed,
    /// The speed field is not a decimal number.
    BadSpeed,
    /// The mode field is neither `inf` nor `page`.
    BadMode,
    /// The optional font field is not one of the four faces.
    BadFont,
}

impl Delivery {
    /// Spec §7's reading pace (~6 chars/s), the pre-#302 fixed value.
    pub const MS_DEFAULT: u16 = 160;
    /// What a node uses with no `V` set, and what an empty `V` restores. `Inf` is the headline
    /// behaviour: a bard that never stops talking.
    pub const DEFAULT: Delivery = Delivery {
        ms_per_char: Self::MS_DEFAULT,
        mode: Mode::Inf,
        font: Font::F5x8,
    };
    /// Reveal interval as the millisecond clock the screen compares against.
    pub const fn reveal_ms(&self) -> u64 {
        self.ms_per_char as u64
    }
}

#[cfg(any(feature = "espnow", feature = "hostsim", feature = "full"))]
impl Delivery {
    /// Fastest reveal: 50 chars/s. Below this the typewriter is not a typewriter, and 0 would spin
    /// the reveal loop against the buffer length every tick.
    pub const MS_MIN: u16 = 20;
    /// Slowest reveal: one character every half second. Slower than this and a screenful takes
    /// minutes, which reads as a hung board rather than as a slow bard.
    pub const MS_MAX: u16 = 500;
    /// Longest legal value (`500:page:10x20` is 14) with a little room to spare.
    pub const MAX_LEN: usize = 20;

    /// Parse a CFG `V` value against `current` (whose fields survive where the value omits them).
    ///
    /// See the module doc for the format. Never panics, never allocates, and never partially
    /// applies: either the whole value is accepted or the caller keeps what it had.
    pub fn parse(value: &[u8], current: Delivery) -> Result<Accepted, DeliveryErr> {
        if value.len() > Self::MAX_LEN {
            return Err(DeliveryErr::TooLong { got: value.len() });
        }
        // Empty ⇒ back to the board defaults, not to `current`: this is the retain-clear path, and
        // clearing a config topic has to mean "forget what I set", same as `S` and `T`.
        if value.is_empty() {
            return Ok(Accepted {
                delivery: Self::DEFAULT,
                clamped: false,
            });
        }
        let Some(sep) = value.iter().position(|&b| b == b':') else {
            return Err(DeliveryErr::Malformed);
        };
        let (speed, rest) = (&value[..sep], &value[sep + 1..]);
        // The font is an OPTIONAL third field: absent ⇒ keep the current one, so a retained
        // two-field value neither breaks nor silently resets a font the operator chose.
        let (mode, font) = match rest.iter().position(|&b| b == b':') {
            Some(i) => (&rest[..i], Some(&rest[i + 1..])),
            None => (rest, None),
        };

        let mut clamped = false;
        let ms_per_char = if speed.is_empty() {
            current.ms_per_char
        } else {
            let mut n = 0u32;
            for &b in speed {
                if !b.is_ascii_digit() {
                    return Err(DeliveryErr::BadSpeed);
                }
                // Saturate rather than wrap: "99999999" is a clamp, not an overflow.
                n = n.saturating_mul(10).saturating_add((b - b'0') as u32);
            }
            let want = n.min(u16::MAX as u32) as u16;
            let got = want.clamp(Self::MS_MIN, Self::MS_MAX);
            clamped = got != want;
            got
        };

        let mode = if mode.is_empty() {
            current.mode
        // Forgiving on CASE (an operator may type the mode by hand), strict on everything else — a
        // machine channel should not be guessed at.
        } else if mode.eq_ignore_ascii_case(b"inf") {
            Mode::Inf
        } else if mode.eq_ignore_ascii_case(b"page") {
            Mode::Page
        } else {
            return Err(DeliveryErr::BadMode);
        };

        let font = match font {
            // ABSENT (or empty) means DEFAULT, not keep-current — and this was wrong the other way
            // round until the dashboard proved it. HA omits the third field exactly when the value IS
            // the default, for compatibility with pre-font firmware, so "keep current" made the
            // default UNREACHABLE: pick `9x15`, then select 5x8, HA publishes `160:inf`, the node
            // keeps 9x15, and the control silently does nothing.
            //
            // A control that cannot return to its own default reads as a broken board — the same class
            // of defect as everything else found today, where the system's way of saying nothing is
            // indistinguishable from it working.
            //
            // The compatibility requirement this seemed to violate is intact: a retained two-field
            // value must keep PARSING, which it does. Keeping its FONT was my addition, and it was
            // unjustifiable on inspection — a value written before the font field existed cannot
            // express a font preference, so the only honest reading of it is "the default". A caller
            // that wants a non-default font must say so, which is one rule instead of two.
            None | Some(b"") => Self::DEFAULT.font,
            Some(f) if f.eq_ignore_ascii_case(b"5x8") => Font::F5x8,
            Some(f) if f.eq_ignore_ascii_case(b"6x10") => Font::F6x10,
            Some(f) if f.eq_ignore_ascii_case(b"9x15") => Font::F9x15,
            Some(f) if f.eq_ignore_ascii_case(b"10x20") => Font::F10x20,
            Some(_) => return Err(DeliveryErr::BadFont),
        };

        Ok(Accepted {
            delivery: Delivery {
                ms_per_char,
                mode,
                font,
            },
            clamped,
        })
    }
}


/// The last `V` value a node acted on, so a RETAINED value — re-offered on every gateway burst — is
/// applied and logged ONCE rather than six times a minute (the bench found eight identical lines in
/// one run). The house pattern is to log on change (LED, units); this is that, for a config whose
/// "change" has to be judged on the OFFERED BYTES: a REFUSED value has no parsed setting to compare
/// against, and a refusal is precisely the case that must stay visible without drowning a soak.
///
/// Pure and host-tested because the two things that could go wrong are quiet ones: the empty offer
/// (length 0 — a real, meaningful value meaning "restore defaults") must not read as "nothing seen
/// yet", and an over-long value must not dedupe against a different value with the same prefix in a
/// way that hides a distinct refusal.
#[cfg(any(feature = "espnow", feature = "hostsim", feature = "full"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LastOffer {
    buf: [u8; Delivery::MAX_LEN],
    /// Bytes recorded, or [`LastOffer::NOTHING`] when no value has ever been offered.
    len: u8,
    /// True length of the last offer, before the `MAX_LEN` prefix cut — so two over-long values that
    /// share a prefix but differ in length are still told apart.
    full_len: u16,
}

#[cfg(any(feature = "espnow", feature = "hostsim", feature = "full"))]
impl LastOffer {
    /// `len` sentinel: no value offered yet. Not 0 — that is the empty (restore-defaults) offer.
    const NOTHING: u8 = u8::MAX;

    /// Nothing offered yet.
    pub const NONE: LastOffer = LastOffer {
        buf: [0; Delivery::MAX_LEN],
        len: Self::NOTHING,
        full_len: 0,
    };

    /// Whether `value` differs from the recorded offer — and RECORD it either way, so the caller's
    /// only job is to decide whether to act and speak.
    ///
    /// Records before the caller parses, deliberately: a refused value is deduplicated too, so a
    /// retained bad value warns once and a DIFFERENT bad value warns again.
    pub fn is_new(&mut self, value: &[u8]) -> bool {
        let n = value.len().min(Delivery::MAX_LEN);
        let full = value.len().min(u16::MAX as usize) as u16;
        let same = self.len != Self::NOTHING
            && self.len as usize == n
            && self.full_len == full
            && self.buf[..n] == value[..n];
        if same {
            return false;
        }
        self.buf[..n].copy_from_slice(&value[..n]);
        self.len = n as u8;
        self.full_len = full;
        true
    }
}
