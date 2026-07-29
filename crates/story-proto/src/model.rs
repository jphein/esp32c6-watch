//! The four payloads the watch reads, as bounded accumulators over
//! [`crate::json`] events.
//!
//! Each model is a small state machine keyed on `(key, depth)`. None of them
//! ever holds a field it does not display: `text_md` (8.3 KB) and each segment's
//! `text` (up to 3.6 KB) stream past and are dropped. What remains is a fixed
//! upper bound, computed at compile time, that the firmware's `.bss` budget can
//! be checked against.
//!
//! Every cap is *reported*, never silent: each model counts what it had to drop
//! so a screen can say "latest 32 of 190" instead of quietly lying about
//! completeness.

use heapless::{String, Vec};

use crate::json::{Event, Text};

// ---------------------------------------------------------------------------
// Event plumbing
// ---------------------------------------------------------------------------

/// A consumer of scanner events.
pub trait EventSink {
    fn on_event(&mut self, ev: &Event, depth: u8);
}

/// Lets a `&mut dyn EventSink` be used wherever an `EventSink` is wanted.
///
/// This exists so the firmware's HTTP client can be **non-generic**. A client
/// generic over the sink is monomorphised once per model, and each copy carries
/// its own socket buffers in the main task's future — which lands in `.bss` and
/// steals from the stack. Three models cost three sets. One `dyn` call costs one.
/// Measured: that mistake put the `story` build 2,376 B under the stack floor.
impl<T: EventSink + ?Sized> EventSink for &mut T {
    fn on_event(&mut self, ev: &Event, depth: u8) {
        (**self).on_event(ev, depth)
    }
}

/// Drives a [`Scanner`](crate::json::Scanner) into an [`EventSink`].
///
/// Exists so the scanner and the sink are separate fields: the sink needs
/// `&mut self` while the scanner is also borrowed mutably, and splitting them
/// here is what makes that borrow legal without interior mutability.
pub struct Reader<S: EventSink> {
    scanner: crate::json::Scanner,
    pub sink: S,
}

impl<S: EventSink> Reader<S> {
    pub fn new(sink: S) -> Self {
        Self { scanner: crate::json::Scanner::new(), sink }
    }

    /// Feed the next piece of the response body. Piece boundaries may fall
    /// anywhere.
    pub fn feed(&mut self, bytes: &[u8]) {
        let Self { scanner, sink } = self;
        scanner.feed(bytes, &mut |ev, d| sink.on_event(ev, d));
    }

    /// True once malformed JSON was seen (sticky).
    pub fn error(&self) -> bool {
        self.scanner.error()
    }

    /// True when a complete, well-formed document was consumed. A socket that
    /// died mid-payload leaves this false — otherwise a half-read chapter is
    /// indistinguishable from a whole one.
    pub fn complete(&self) -> bool {
        self.scanner.complete()
    }

    pub fn into_sink(self) -> S {
        self.sink
    }
}

/// Copy `s` into a bounded string, clipping on a UTF-8 boundary.
///
/// `String::push_str` fails all-or-nothing on overflow, which would silently
/// yield an empty title for any name one byte too long. Clipping per character
/// degrades gracefully instead.
fn copy_clipped<const N: usize>(dst: &mut String<N>, s: &str) {
    dst.clear();
    for c in s.chars() {
        if dst.push(c).is_err() {
            return;
        }
    }
}

/// Non-negative `i64` narrowed to `u32`, saturating.
fn as_u32(v: i64) -> u32 {
    if v < 0 {
        0
    } else if v > u32::MAX as i64 {
        u32::MAX
    } else {
        v as u32
    }
}

fn as_u16(v: i64) -> u16 {
    if v < 0 {
        0
    } else if v > u16::MAX as i64 {
        u16::MAX
    } else {
        v as u16
    }
}

// ---------------------------------------------------------------------------
// GET /api/chapters
// ---------------------------------------------------------------------------

/// Chapter rows retained.
///
/// The list is a scrollable window backed by `?since=N`, so this bounds one
/// page rather than the story: paging back is a new request, not more RAM.
/// Trimmed 24 -> 16 to buy stack margin: the list draws ~6 rows at a time, so 16
/// still covers more than two screens of scrolling before a refetch, and the
/// `.bss` it frees is margin against a boot assert that has bitten this project
/// before. [`ChapterList::dropped`] reports anything the cap ate.
pub const MAX_CHAPTERS: usize = 16;

/// Chapter rows the list screen can actually **draw**.
///
/// Distinct from [`MAX_CHAPTERS`], which is a *parse* cap, and the distinction is
/// load-bearing: at 50 px a row plus the paging controls, only five fit on a
/// 502 px panel. Pushing all sixteen retained rows to the scene drew eleven of
/// them off-glass and overdrew the pager — and #75's lesson is that the
/// apps-menu OOM was a DRAWN-ITEM-COUNT problem, so instantiating rows nobody
/// can see costs scene memory as well as looking broken.
///
/// Paging steps by this, not by the parse cap.
pub const VISIBLE_CHAPTERS: usize = 5;
/// Retained chapter title length. Live titles are 22–27 chars.
pub const MAX_TITLE: usize = 40;

/// One row of the chapter index.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct ChapterRow {
    pub number: u16,
    pub title: String<MAX_TITLE>,
    pub duration_ms: u32,
    pub has_audio: bool,
    /// `null` for chapters with no rendered audio yet.
    pub total_bytes: Option<u32>,
}

impl ChapterRow {
    /// Whole seconds, for a `mm:ss` label.
    pub fn duration_s(&self) -> u32 {
        self.duration_ms / 1000
    }

    /// True when this chapter can actually be played.
    pub fn playable(&self) -> bool {
        self.has_audio && self.total_bytes.is_some_and(|b| b > 0)
    }
}

/// Which field the current key selects. Cheaper and clearer than comparing
/// strings at assignment time.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum ChKey {
    #[default]
    None,
    Number,
    Title,
    DurationMs,
    HasAudio,
    TotalBytes,
}

/// Streaming accumulator for `GET /api/chapters`.
#[derive(Default)]
pub struct ChapterList {
    pub rows: Vec<ChapterRow, MAX_CHAPTERS>,
    /// Rows discarded because the cap was reached (oldest first).
    pub dropped: u16,
    cur: ChapterRow,
    key: ChKeyState,
}

#[derive(Default)]
struct ChKeyState {
    key: Option<ChKey>,
    open: bool,
}

impl ChapterList {
    pub fn new() -> Self {
        Self::default()
    }

    /// Row for `number`, if retained.
    pub fn find(&self, number: u16) -> Option<&ChapterRow> {
        self.rows.iter().find(|r| r.number == number)
    }

    /// Highest-numbered retained chapter that can be played.
    pub fn latest_playable(&self) -> Option<&ChapterRow> {
        self.rows.iter().rfind(|r| r.playable())
    }

    fn commit(&mut self) {
        if self.cur.number == 0 {
            return; // no `number` seen — not a chapter object
        }
        if self.rows.is_full() && !self.rows.is_empty() {
            self.rows.remove(0);
            self.dropped = self.dropped.saturating_add(1);
        }
        let _ = self.rows.push(core::mem::take(&mut self.cur));
    }
}

impl EventSink for ChapterList {
    fn on_event(&mut self, ev: &Event, depth: u8) {
        // Rows are the depth-2 objects of the depth-1 array.
        match ev {
            Event::ObjOpen if depth == 2 => {
                self.cur = ChapterRow::default();
                self.key = ChKeyState { key: None, open: true };
            }
            Event::ObjClose if depth == 2 && self.key.open => {
                self.key.open = false;
                self.commit();
            }
            Event::Key(k) if depth == 2 && self.key.open => {
                self.key.key = Some(if k.matches("number") {
                    ChKey::Number
                } else if k.matches("title") {
                    ChKey::Title
                } else if k.matches("duration_ms") {
                    ChKey::DurationMs
                } else if k.matches("has_audio") {
                    ChKey::HasAudio
                } else if k.matches("total_bytes") {
                    ChKey::TotalBytes
                } else {
                    ChKey::None
                });
            }
            _ if depth == 2 && self.key.open => {
                let Some(key) = self.key.key else { return };
                match (key, ev) {
                    (ChKey::Number, Event::Int(v)) => self.cur.number = as_u16(*v),
                    (ChKey::Title, Event::Str(t)) => {
                        copy_clipped(&mut self.cur.title, t.as_str())
                    }
                    (ChKey::DurationMs, Event::Int(v)) => self.cur.duration_ms = as_u32(*v),
                    (ChKey::HasAudio, Event::Bool(v)) => self.cur.has_audio = *v,
                    (ChKey::TotalBytes, Event::Int(v)) => {
                        self.cur.total_bytes = Some(as_u32(*v))
                    }
                    (ChKey::TotalBytes, Event::Null) => self.cur.total_bytes = None,
                    _ => {}
                }
                self.key.key = None;
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// GET /api/chapters/{n} — the manifest segment index
// ---------------------------------------------------------------------------

/// Segments retained per chapter.
///
/// **Sized against measured reality in both manifest shapes**, because they
/// disagree violently and because sizing this on one sample has already been
/// wrong twice:
///
/// | chapter | turn-level | **sentence-level** | duration | longest entry text |
/// |---|---|---|---|---|
/// | 1 | 7 | **50** | 7m33s | 3,665 B → **200 B** |
/// | 2 | **58** | **92** | 9m55s | 1,263 B → 199 B |
/// | 3 | 7 | **83** | 18m05s | 7,506 B → **200 B** |
///
/// Two different scaling laws are at work, which is why one sample never
/// sufficed. **Turn** count tracks *dialogue density* — chapter 2 has 37
/// `character` turns while chapter 3 is 18 minutes of narration in 7 blocks.
/// **Sentence** count tracks *prose length* — so chapter 3, with the fewest
/// turns, has the second-most entries.
///
/// 128 is ~1.4x the worst observed (92) and, at chapter 3's rate of ~32 entries
/// per 1,000 words, covers roughly a 4,000-word chapter against a 2,000-word
/// target. Affordable only because speaker names are interned
/// ([`SegmentIndex::speakers`]), which makes a segment ~12 bytes rather than ~34.
///
/// # Overflow REFUSES; it does not truncate
///
/// A truncated manifest is not a smaller manifest — it is a highlight that
/// silently desynchronises partway through a chapter, which is the audible-only
/// failure class this project keeps producing. So on overflow
/// [`SegmentIndex::usable`] goes false and the caller suppresses highlighting for
/// that chapter entirely. **Playback is unaffected**, because the player needs
/// only `total_bytes` from the chapter row, never the manifest — so the
/// degradation is "no speaker chip, story still plays", and it is counted in
/// [`SegmentIndex::dropped`] rather than being silent.
///
/// If refusals ever start appearing, the fix is a server-side slice
/// (`?from_ms=&limit=`) feeding a sliding window around the playback position —
/// **not** a bigger array here. Entries scale with prose, so no fixed cap is
/// safe forever, and this one is a measured trade rather than a knob to turn.
pub const MAX_SEGMENTS: usize = 128;

/// Distinct speaker names retained per chapter.
///
/// The observed maximum is **5** (chapter 2: narrator, Kaelen, Sera, Shadow,
/// SYSTEM). 12 is generous headroom for an ensemble scene, and cheap because
/// this is the only place a name is stored.
pub const MAX_SPEAKERS: usize = 12;

/// Retained speaker-name length. Live names are ≤ 8 chars.
pub const MAX_SPEAKER: usize = 16;

/// A [`Segment`] whose speaker could not be interned (name table full).
const NO_SPEAKER: u8 = u8::MAX;

/// Segment voice class. Drives the playback screen's speaker chip.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum SegKind {
    #[default]
    Narrator,
    /// RPG stat block, spoken by the robotic SYSTEM voice.
    System,
    /// A character speaking.
    Dialogue,
    /// A `kind` the firmware does not know. Rendered neutrally rather than
    /// dropped, so a daemon-side addition degrades instead of vanishing.
    Other,
}

impl SegKind {
    fn parse(s: &str) -> Self {
        match s {
            "narrator" => SegKind::Narrator,
            "system" => SegKind::System,
            "dialogue" | "character" | "speech" => SegKind::Dialogue,
            _ => SegKind::Other,
        }
    }

    /// Display label.
    pub fn label(self) -> &'static str {
        match self {
            SegKind::Narrator => "Narrator",
            SegKind::System => "SYSTEM",
            SegKind::Dialogue => "Dialogue",
            SegKind::Other => "",
        }
    }
}

/// One manifest segment, prose excluded.
///
/// The speaker is a **table index, not a string**: chapter 2 has 58 segments but
/// only five distinct speakers, so interning turns ~34 bytes per segment into
/// ~12 and is what makes a 96-segment cap affordable. Resolve it with
/// [`SegmentIndex::speaker_of`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Segment {
    pub idx: u16,
    pub start_ms: u32,
    pub end_ms: u32,
    pub kind: SegKind,
    speaker_id: u8,
}

impl Default for Segment {
    fn default() -> Self {
        Self { idx: 0, start_ms: 0, end_ms: 0, kind: SegKind::Narrator, speaker_id: NO_SPEAKER }
    }
}

impl Segment {
    /// Byte offset of this segment's first sample. Exact by construction —
    /// every segment is zero-padded to a whole millisecond server-side, so
    /// `ms x 32` is an identity, not an approximation (design §8.1).
    pub fn start_byte(&self) -> u32 {
        crate::ms_to_bytes(self.start_ms)
    }

    pub fn end_byte(&self) -> u32 {
        crate::ms_to_bytes(self.end_ms)
    }

    pub fn duration_ms(&self) -> u32 {
        self.end_ms.saturating_sub(self.start_ms)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SgKey {
    None,
    Idx,
    StartMs,
    EndMs,
    Kind,
    Speaker,
    ChapDuration,
    ChapTitle,
    BytesPerMs,
    SampleRate,
}

/// Streaming accumulator for `GET /api/chapters/{n}`.
///
/// Keeps the segment index and the chapter's timing facts; **discards
/// `text_md` and every segment `text`**, which is the entire reason this is a
/// streaming parser (§ crate docs).
pub struct SegmentIndex {
    pub chapter: u16,
    pub title: String<MAX_TITLE>,
    pub duration_ms: u32,
    /// From the manifest, so nothing hardcodes 32 (design §8.1).
    pub bytes_per_ms: u16,
    pub sample_rate: u32,
    pub segments: Vec<Segment, MAX_SEGMENTS>,
    /// Distinct speaker names, in first-appearance order. [`Segment`]s index
    /// into this rather than each carrying a copy.
    pub speakers: Vec<String<MAX_SPEAKER>, MAX_SPEAKERS>,
    /// Segments beyond the cap.
    pub dropped: u16,
    /// Distinct speakers beyond [`MAX_SPEAKERS`], so an unnamed chip is a known
    /// state rather than a mystery.
    pub speakers_dropped: u16,

    key: Option<SgKey>,
    /// Depth at which a segment element object opens, once located.
    seg_obj_depth: Option<u8>,
    in_manifest: bool,
    cur: Segment,
    open: bool,
}

impl Default for SegmentIndex {
    fn default() -> Self {
        Self::new()
    }
}

impl SegmentIndex {
    pub fn new() -> Self {
        Self {
            chapter: 0,
            title: String::new(),
            duration_ms: 0,
            bytes_per_ms: crate::BYTES_PER_MS as u16,
            sample_rate: crate::SAMPLE_RATE,
            segments: Vec::new(),
            speakers: Vec::new(),
            dropped: 0,
            speakers_dropped: 0,
            key: None,
            seg_obj_depth: None,
            in_manifest: false,
            cur: Segment::default(),
            open: false,
        }
    }

    /// Position in [`segments`](Self::segments) of the segment covering `ms`.
    ///
    /// Binary search when the index is sorted, linear scan when it is not — a
    /// daemon-side ordering bug should degrade to "slower" rather than to
    /// "confidently wrong speaker". A position at or past the end clamps to the
    /// last segment, so the playback screen holds the final speaker through the
    /// closing milliseconds instead of blanking.
    pub fn segment_idx_at(&self, ms: u32) -> Option<usize> {
        let s = self.segments.as_slice();
        let last = s.len().checked_sub(1)?;

        if !s.windows(2).all(|w| w[0].start_ms <= w[1].start_ms) {
            return s.iter().position(|g| ms >= g.start_ms && ms < g.end_ms);
        }

        // Rightmost segment whose start_ms <= ms.
        let mut lo = 0usize;
        let mut hi = s.len();
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if s.get(mid).is_some_and(|g| g.start_ms <= ms) {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        let i = lo.checked_sub(1)?; // ms precedes the first segment
        let seg = s.get(i)?;
        (ms < seg.end_ms || i == last).then_some(i)
    }

    /// The segment covering `ms`.
    pub fn segment_at(&self, ms: u32) -> Option<&Segment> {
        self.segments.get(self.segment_idx_at(ms)?)
    }

    /// True when segments tile the chapter with no gap or overlap. The daemon
    /// asserts this before publishing; re-checking is cheap and turns a
    /// server-side regression into a visible state instead of drifting
    /// highlights.
    pub fn contiguous(&self) -> bool {
        let s = self.segments.as_slice();
        if s.is_empty() {
            return false;
        }
        s.windows(2).all(|w| w[0].end_ms == w[1].start_ms)
    }

    /// Total audio bytes implied by the manifest.
    pub fn total_bytes(&self) -> u32 {
        crate::ms_to_bytes(self.duration_ms)
    }

    /// True when this index can be trusted to drive highlighting.
    ///
    /// Four ways it cannot, each of which would otherwise show a **confidently
    /// wrong** speaker rather than none:
    ///
    /// 1. **Overflow** — the cap was hit, so the tail of the chapter is missing
    ///    and every lookup past that point is wrong. Refuse, don't truncate
    ///    (see [`MAX_SEGMENTS`]).
    /// 2. **Empty** — nothing parsed.
    /// 3. **Not contiguous** — a gap or overlap means the daemon's own
    ///    `is_contiguous` assertion did not hold; offsets have drifted.
    /// 4. **A different `bytes_per_ms`** — see [`Self::rate_matches`].
    ///
    /// The caller's response is to suppress the speaker chip, **not** to refuse
    /// the chapter: playback needs only `total_bytes` from the chapter index, so
    /// the story still plays.
    pub fn usable(&self) -> bool {
        self.dropped == 0 && !self.segments.is_empty() && self.contiguous() && self.rate_matches()
    }

    /// True when the manifest's own `bytes_per_ms` matches what this hardware can
    /// actually play.
    ///
    /// The design hands clients `bytes_per_ms` "so none of them hardcodes 32"
    /// (spec §8.1). This watch reads it — and then checks it, because it
    /// **cannot adapt**: `audio_out` clocks the ring at 16 kHz mono s16le,
    /// `PLAY_CHUNK` is 512 B = 16 ms, and there is no resampler and no room for
    /// one. So the honest behaviour is not to adapt but to *detect and decline*.
    ///
    /// Silently assuming 32 against a manifest that said otherwise would desync
    /// every Range offset and every highlight by a growing amount — exactly the
    /// silent cumulative drift §8.1 exists to prevent.
    pub fn rate_matches(&self) -> bool {
        // 0 means the field was absent (bare manifests omit it); trust the
        // default rather than reject a payload that never made a claim.
        self.bytes_per_ms == 0 || self.bytes_per_ms as usize == crate::BYTES_PER_MS
    }

    /// The speaker name for `seg`, or `""` when the name table overflowed.
    pub fn speaker_of(&self, seg: &Segment) -> &str {
        match self.speakers.get(seg.speaker_id as usize) {
            Some(s) => s.as_str(),
            None => "",
        }
    }

    /// The speaker name at a playback position, for the "who is speaking" chip.
    pub fn speaker_at(&self, ms: u32) -> &str {
        match self.segment_at(ms) {
            Some(s) => self.speaker_of(s),
            None => "",
        }
    }

    /// Intern `name`, returning its table index.
    ///
    /// Linear scan over ≤ [`MAX_SPEAKERS`] entries — with five real speakers
    /// that is cheaper than any hashing, and it preserves first-appearance
    /// order, which is the order a cast list would want.
    fn intern(&mut self, name: &str) -> u8 {
        if name.is_empty() {
            return NO_SPEAKER;
        }
        if let Some(i) = self.speakers.iter().position(|s| s.as_str() == name) {
            return i as u8;
        }
        let mut s: String<MAX_SPEAKER> = String::new();
        copy_clipped(&mut s, name);
        let next = self.speakers.len();
        // Never index NO_SPEAKER, and never exceed u8.
        if next >= MAX_SPEAKERS || next >= NO_SPEAKER as usize {
            self.speakers_dropped = self.speakers_dropped.saturating_add(1);
            return NO_SPEAKER;
        }
        match self.speakers.push(s) {
            Ok(()) => next as u8,
            Err(_) => {
                self.speakers_dropped = self.speakers_dropped.saturating_add(1);
                NO_SPEAKER
            }
        }
    }

    fn commit(&mut self) {
        if self.cur.end_ms == 0 && self.cur.start_ms == 0 && self.cur.speaker_id == NO_SPEAKER {
            return;
        }
        if self.segments.is_full() {
            self.dropped = self.dropped.saturating_add(1);
            self.cur = Segment::default();
            return;
        }
        let _ = self.segments.push(core::mem::take(&mut self.cur));
    }

    fn root_key(&mut self, k: &Text) {
        self.key = Some(if k.matches("duration_ms") {
            SgKey::ChapDuration
        } else if k.matches("title") {
            SgKey::ChapTitle
        } else if k.matches("number") {
            SgKey::Idx // reused: chapter number at depth 1
        } else {
            // `text_md` lands here and is deliberately unmapped, so its 8.3 KB
            // of prose is scanned and dropped.
            SgKey::None
        });
        if k.matches("manifest") {
            self.in_manifest = true;
        }
    }
}

impl EventSink for SegmentIndex {
    fn on_event(&mut self, ev: &Event, depth: u8) {
        // --- locate the segments array ------------------------------------
        if let Event::Key(k) = ev {
            if k.matches("segments") {
                // Key sits at depth d; the array opens at d+1 and each element
                // object at d+2.
                self.seg_obj_depth = Some(depth.saturating_add(2));
            }
        }

        let seg_depth = self.seg_obj_depth;

        // --- inside a segment element -------------------------------------
        if Some(depth) == seg_depth {
            match ev {
                Event::ObjOpen => {
                    self.cur = Segment::default();
                    self.open = true;
                    self.key = None;
                    return;
                }
                Event::ObjClose if self.open => {
                    self.open = false;
                    self.commit();
                    return;
                }
                Event::Key(k) if self.open => {
                    self.key = Some(if k.matches("idx") {
                        SgKey::Idx
                    } else if k.matches("start_ms") {
                        SgKey::StartMs
                    } else if k.matches("end_ms") {
                        SgKey::EndMs
                    } else if k.matches("kind") {
                        SgKey::Kind
                    } else if k.matches("speaker") {
                        SgKey::Speaker
                    } else {
                        // `text` (up to 3.6 KB) and `voice_ref` land here and
                        // are dropped as they stream.
                        SgKey::None
                    });
                    return;
                }
                _ if self.open => {
                    let Some(key) = self.key.take() else { return };
                    match (key, ev) {
                        (SgKey::Idx, Event::Int(v)) => self.cur.idx = as_u16(*v),
                        (SgKey::StartMs, Event::Int(v)) => self.cur.start_ms = as_u32(*v),
                        (SgKey::EndMs, Event::Int(v)) => self.cur.end_ms = as_u32(*v),
                        (SgKey::Kind, Event::Str(t)) => {
                            self.cur.kind = SegKind::parse(t.as_str())
                        }
                        (SgKey::Speaker, Event::Str(t)) => {
                            self.cur.speaker_id = self.intern(t.as_str());
                        }
                        _ => {}
                    }
                    return;
                }
                _ => return,
            }
        }

        // --- chapter root scalars (depth 1) -------------------------------
        if depth == 1 {
            match ev {
                Event::Key(k) => self.root_key(k),
                _ => {
                    let Some(key) = self.key.take() else { return };
                    match (key, ev) {
                        (SgKey::ChapDuration, Event::Int(v)) => self.duration_ms = as_u32(*v),
                        (SgKey::ChapTitle, Event::Str(t)) => {
                            copy_clipped(&mut self.title, t.as_str())
                        }
                        (SgKey::Idx, Event::Int(v)) => self.chapter = as_u16(*v),
                        _ => {}
                    }
                }
            }
            return;
        }

        // --- manifest scalars (depth 2) -----------------------------------
        if depth == 2 && self.in_manifest {
            match ev {
                Event::Key(k) => {
                    self.key = Some(if k.matches("bytes_per_ms") {
                        SgKey::BytesPerMs
                    } else if k.matches("sample_rate") {
                        SgKey::SampleRate
                    } else if k.matches("duration_ms") {
                        SgKey::ChapDuration
                    } else {
                        SgKey::None
                    });
                }
                _ => {
                    let Some(key) = self.key.take() else { return };
                    match (key, ev) {
                        (SgKey::BytesPerMs, Event::Int(v)) => self.bytes_per_ms = as_u16(*v),
                        (SgKey::SampleRate, Event::Int(v)) => self.sample_rate = as_u32(*v),
                        // Trust the manifest's own duration over the row's when
                        // both are present; they agree by construction (§8.1).
                        (SgKey::ChapDuration, Event::Int(v)) => self.duration_ms = as_u32(*v),
                        _ => {}
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// GET /api/character/{subject}
// ---------------------------------------------------------------------------

pub const MAX_SUBJECT: usize = 32;
pub const MAX_STATUS: usize = 56;
pub const MAX_LOCATION: usize = 24;
pub const MAX_ITEM: usize = 28;
pub const MAX_ITEMS: usize = 8;
pub const MAX_SLOT_VAL: usize = 24;

/// The eleven equipment slots, in display order.
///
/// A whitelist, and that is the point (design §9.4.1): the gate rejects an
/// invented `equip:third_arm` server-side, so this renderer never needs
/// defensive layout for a slot it has no row for.
pub const EQUIP_SLOTS: [&str; 11] = [
    "head", "amulet", "chest", "cloak", "hands", "legs", "feet", "main_hand", "off_hand",
    "ring1", "ring2",
];

/// Human labels for [`EQUIP_SLOTS`], same order.
pub const EQUIP_LABELS: [&str; 11] = [
    "Head", "Amulet", "Chest", "Cloak", "Hands", "Legs", "Feet", "Main hand", "Off hand",
    "Ring I", "Ring II",
];

/// The six whitelisted appearance traits, in display order.
pub const APPEAR_TRAITS: [&str; 6] = ["height", "build", "hair", "eyes", "skin", "notable"];

/// Human labels for [`APPEAR_TRAITS`], same order.
pub const APPEAR_LABELS: [&str; 6] = ["Height", "Build", "Hair", "Eyes", "Skin", "Notable"];

/// One inventory line.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Item {
    pub name: String<MAX_ITEM>,
    pub count: u16,
}

/// `GET /api/character` — feeds BOTH the stats and character screens from one
/// 501-byte response.
///
/// Every numeric is `Option`: on the live ledger `hp`, `gold` and `location` are
/// all `null` today, and all eleven equipment slots and six appearance traits
/// are too. Null is the common path here, not the edge case — so an absent
/// value is a first-class state rather than a zero that would render as a
/// full-width empty HP bar.
pub struct Character {
    pub subject: String<MAX_SUBJECT>,
    pub known: bool,
    pub level: Option<u16>,
    pub xp: Option<u32>,
    pub hp: Option<u32>,
    pub max_hp: Option<u32>,
    pub gold: Option<u32>,
    pub location: Option<String<MAX_LOCATION>>,
    pub status: Option<String<MAX_STATUS>>,
    pub inventory: Vec<Item, MAX_ITEMS>,
    /// Items beyond the cap.
    pub items_dropped: u16,
    /// Indexed by [`EQUIP_SLOTS`].
    pub equip: [Option<String<MAX_SLOT_VAL>>; 11],
    /// Indexed by [`APPEAR_TRAITS`].
    pub appear: [Option<String<MAX_SLOT_VAL>>; 6],

    section: Section,
    key: CharKey,
    /// Current key inside a nested section (slot / trait / item name).
    slot: Option<usize>,
    item: Item,
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum Section {
    #[default]
    Root,
    Inventory,
    Equipment,
    Appearance,
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum CharKey {
    #[default]
    None,
    Subject,
    Known,
    Level,
    Xp,
    Hp,
    MaxHp,
    Gold,
    Location,
    Status,
}

impl Default for Character {
    fn default() -> Self {
        Self::new()
    }
}

impl Character {
    pub fn new() -> Self {
        Self {
            subject: String::new(),
            known: false,
            level: None,
            xp: None,
            hp: None,
            max_hp: None,
            gold: None,
            location: None,
            status: None,
            inventory: Vec::new(),
            items_dropped: 0,
            equip: Default::default(),
            appear: Default::default(),
            section: Section::Root,
            key: CharKey::None,
            slot: None,
            item: Item::default(),
        }
    }

    /// HP as a 0..=1 fraction, or `None` when either end is unknown.
    ///
    /// Returning `None` rather than 0 is deliberate: the live ledger has
    /// `hp: null` against `max_hp: 110`, and a bar drawn at zero would assert
    /// the protagonist is dead.
    pub fn hp_fraction(&self) -> Option<f32> {
        let (hp, max) = (self.hp?, self.max_hp?);
        if max == 0 {
            return None;
        }
        Some((hp as f32 / max as f32).clamp(0.0, 1.0))
    }

    /// Equipment value for a slot index, or `None` when empty.
    pub fn equip_at(&self, i: usize) -> Option<&str> {
        self.equip.get(i)?.as_ref().map(|s| s.as_str())
    }

    /// Appearance value for a trait index, or `None` when unknown.
    pub fn appear_at(&self, i: usize) -> Option<&str> {
        self.appear.get(i)?.as_ref().map(|s| s.as_str())
    }

    /// How many of the eleven slots carry an item — lets the screen say
    /// "3 of 11 equipped" instead of showing eleven silent dashes.
    pub fn equipped_count(&self) -> usize {
        self.equip.iter().filter(|s| s.is_some()).count()
    }

    /// How many of the six traits are known.
    pub fn appearance_count(&self) -> usize {
        self.appear.iter().filter(|s| s.is_some()).count()
    }

    fn set_opt_str<const N: usize>(dst: &mut Option<String<N>>, t: &Text) {
        let mut s: String<N> = String::new();
        copy_clipped(&mut s, t.as_str());
        *dst = if s.is_empty() { None } else { Some(s) };
    }
}

impl EventSink for Character {
    fn on_event(&mut self, ev: &Event, depth: u8) {
        match (depth, ev) {
            // --- section entry / exit -------------------------------------
            (1, Event::Key(k)) => {
                self.section = if k.matches("inventory") {
                    Section::Inventory
                } else if k.matches("equipment") {
                    Section::Equipment
                } else if k.matches("appearance") {
                    Section::Appearance
                } else {
                    Section::Root
                };
                self.key = if k.matches("subject") {
                    CharKey::Subject
                } else if k.matches("known") {
                    CharKey::Known
                } else if k.matches("level") {
                    CharKey::Level
                } else if k.matches("xp") {
                    CharKey::Xp
                } else if k.matches("hp") {
                    CharKey::Hp
                } else if k.matches("max_hp") {
                    CharKey::MaxHp
                } else if k.matches("gold") {
                    CharKey::Gold
                } else if k.matches("location") {
                    CharKey::Location
                } else if k.matches("status") {
                    CharKey::Status
                } else {
                    CharKey::None
                };
            }

            // --- root scalars --------------------------------------------
            (1, _) => {
                let key = core::mem::take(&mut self.key);
                match (key, ev) {
                    (CharKey::Subject, Event::Str(t)) => {
                        copy_clipped(&mut self.subject, t.as_str())
                    }
                    (CharKey::Known, Event::Bool(v)) => self.known = *v,
                    (CharKey::Level, Event::Int(v)) => self.level = Some(as_u16(*v)),
                    (CharKey::Xp, Event::Int(v)) => self.xp = Some(as_u32(*v)),
                    (CharKey::Hp, Event::Int(v)) => self.hp = Some(as_u32(*v)),
                    (CharKey::MaxHp, Event::Int(v)) => self.max_hp = Some(as_u32(*v)),
                    (CharKey::Gold, Event::Int(v)) => self.gold = Some(as_u32(*v)),
                    (CharKey::Location, Event::Str(t)) => {
                        Self::set_opt_str(&mut self.location, t)
                    }
                    (CharKey::Status, Event::Str(t)) => Self::set_opt_str(&mut self.status, t),
                    // Explicit nulls stay None — see `hp_fraction`.
                    _ => {}
                }
            }

            // --- nested section members ----------------------------------
            (2, Event::Key(k)) => {
                let name = k.as_str();
                self.slot = match self.section {
                    Section::Equipment => EQUIP_SLOTS.iter().position(|s| *s == name),
                    Section::Appearance => APPEAR_TRAITS.iter().position(|s| *s == name),
                    Section::Inventory => {
                        self.item = Item::default();
                        copy_clipped(&mut self.item.name, name);
                        Some(0)
                    }
                    Section::Root => None,
                };
            }
            (2, _) => {
                let Some(i) = self.slot.take() else { return };
                match (self.section, ev) {
                    (Section::Equipment, Event::Str(t)) => {
                        if let Some(cell) = self.equip.get_mut(i) {
                            Self::set_opt_str(cell, t);
                        }
                    }
                    (Section::Appearance, Event::Str(t)) => {
                        if let Some(cell) = self.appear.get_mut(i) {
                            Self::set_opt_str(cell, t);
                        }
                    }
                    (Section::Inventory, Event::Int(v)) => {
                        if self.item.name.is_empty() {
                            return;
                        }
                        self.item.count = as_u16(*v);
                        if self.inventory.is_full() {
                            self.items_dropped = self.items_dropped.saturating_add(1);
                        } else {
                            let _ = self.inventory.push(core::mem::take(&mut self.item));
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// GET / PUT /api/progress
// ---------------------------------------------------------------------------

/// The playback cursor and buffer health.
///
/// `consumed_through` is what stops generation running away, so the watch
/// reporting it accurately is load-bearing for the whole system, not just for
/// resume convenience (design §9.4).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Progress {
    pub consumed_through: u16,
    pub latest_chapter: u16,
    pub chapters_ahead: u16,
    pub ready_ahead: u16,
    pub buffer_target: u16,
    pub buffer_healthy: bool,
    pub next_chapter: Option<u16>,
    pub next_playable: Option<u16>,
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum ProgKey {
    #[default]
    None,
    ConsumedThrough,
    LatestChapter,
    ChaptersAhead,
    ReadyAhead,
    BufferTarget,
    BufferHealthy,
    NextChapter,
    NextPlayable,
}

impl Progress {
    pub fn new() -> Self {
        Self::default()
    }

    /// Chapters generated but not yet listened to. The daemon uses this to
    /// decide whether to keep writing; the watch shows it as buffer health.
    pub fn unread(&self) -> u16 {
        self.latest_chapter.saturating_sub(self.consumed_through)
    }
}

/// Parse state for [`Progress`], kept out of the data struct so `Progress`
/// stays a comparable value — a `PartialEq` that also compared a half-finished
/// key would make two identical cursors test unequal.
#[derive(Default)]
pub struct ProgressSink {
    pub progress: Progress,
    key: ProgKey,
}

impl ProgressSink {
    pub fn new() -> Self {
        Self::default()
    }
}

impl EventSink for ProgressSink {
    fn on_event(&mut self, ev: &Event, depth: u8) {
        if depth != 1 {
            return;
        }
        match ev {
            Event::Key(k) => {
                self.key = if k.matches("consumed_through") {
                    ProgKey::ConsumedThrough
                } else if k.matches("latest_chapter") {
                    ProgKey::LatestChapter
                } else if k.matches("chapters_ahead") {
                    ProgKey::ChaptersAhead
                } else if k.matches("ready_ahead") {
                    ProgKey::ReadyAhead
                } else if k.matches("buffer_target") {
                    ProgKey::BufferTarget
                } else if k.matches("buffer_healthy") {
                    ProgKey::BufferHealthy
                } else if k.matches("next_chapter") {
                    ProgKey::NextChapter
                } else if k.matches("next_playable") {
                    ProgKey::NextPlayable
                } else {
                    ProgKey::None
                };
            }
            _ => {
                let key = core::mem::take(&mut self.key);
                let p = &mut self.progress;
                match (key, ev) {
                    (ProgKey::ConsumedThrough, Event::Int(v)) => {
                        p.consumed_through = as_u16(*v)
                    }
                    (ProgKey::LatestChapter, Event::Int(v)) => p.latest_chapter = as_u16(*v),
                    (ProgKey::ChaptersAhead, Event::Int(v)) => p.chapters_ahead = as_u16(*v),
                    (ProgKey::ReadyAhead, Event::Int(v)) => p.ready_ahead = as_u16(*v),
                    (ProgKey::BufferTarget, Event::Int(v)) => p.buffer_target = as_u16(*v),
                    (ProgKey::BufferHealthy, Event::Bool(v)) => p.buffer_healthy = *v,
                    (ProgKey::NextChapter, Event::Int(v)) => p.next_chapter = Some(as_u16(*v)),
                    (ProgKey::NextPlayable, Event::Int(v)) => {
                        p.next_playable = Some(as_u16(*v))
                    }
                    _ => {}
                }
            }
        }
    }
}
