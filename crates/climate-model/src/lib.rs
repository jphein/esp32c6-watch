//! HA climate-control correctness core (design spec §B′, 2026-07-20).
//!
//! Pure, `no_std`, host-testable — depends only on `core` + `heapless`, never on
//! esp-hal/Slint. The async I/O module (`src/net/mqtt_climate.rs`) feeds this
//! crate raw MQTT payload bytes and gets back typed state; the integrator maps
//! [`ClimateEntity`] → the Slint `ClimateCard`. Keeping the logic here is what
//! makes it `cargo test`-able on the host (esp-hal's `build.rs` panics off-target).
//!
//! # Untrusted input
//!
//! [`parse_state`] parses **retained MQTT payloads off the LAN broker** — attacker-
//! influenceable, not guaranteed well-formed or even UTF-8. Every parse path is
//! **bounded and panic-free**: no direct slice indexing on untrusted offsets, all
//! walks are `while i < len`, the total payload is size-capped, and oversized names
//! **truncate on a UTF-8 char boundary** (never a mid-codepoint slice → no
//! remote-crash vector). Same discipline as `crates/rssi::clip` and the
//! `crates/scan-model` adversarial MHR parser. Malformed → `None` (skip the entity).
//!
//! # Interface contract (aligned to the Climate UI, 2026-07-20)
//!
//! The Slint segmented control is the canonical consumer, so the enum
//! discriminants below are the **wire/UI ABI** and must not be reordered:
//! `HvacMode { Off=0, Heat=1, Cool=2, Auto=3, FanOnly=4, Dry=5 }` and
//! `HvacAction { Idle=0, Heating=1, Cooling=2 }`. HA's `heat_cool` folds into
//! `Auto` (the UI has no separate heat/cool mode); `drying`/`defrosting` fold into
//! `Cooling` (compressor running → cool/blue accent). The UI filters supported
//! modes via [`ClimateEntity::modes_mask`] (bit `m` set ⇒ mode `m` supported).

#![no_std]

use core::fmt::Write;
use heapless::{String, Vec};

/// Hard cap on a single climate state payload. The bridge emits ~200 B; anything
/// larger is rejected outright so the scan cost is bounded regardless of input.
const MAX_INPUT: usize = 2048;

/// Number of distinct HVAC modes (Off..=Dry) — the `modes` list capacity.
pub const MODE_COUNT: usize = 6;

/// Max climate entities the watch tracks at once (spec: Nests + up to 4 minisplits,
/// headroom to 12).
pub const MAX_ENTITIES: usize = 12;

/// HA `object_id` capacity. Real ids run long (e.g. `living_room_minisplit_thermostat`),
/// so 48 bytes; longer ids clip on a char boundary in [`ClimateState::upsert`].
pub const OBJ_ID_CAP: usize = 48;

/// HA `object_id` key type — a fixed-capacity string ([`OBJ_ID_CAP`] bytes).
pub type ObjId = String<OBJ_ID_CAP>;

// ---------------------------------------------------------------------------
// Enums — discriminants are the UI/wire ABI (see module docs). #[repr(i32)] so
// `mode as i32` / `action as i32` map 1:1 onto the Slint `ClimateCard` ints.
// ---------------------------------------------------------------------------

/// HVAC operating mode the user selects. Discriminants match the Slint segmented
/// control 1:1. HA's `heat_cool` folds into [`HvacMode::Auto`].
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
#[repr(i32)]
pub enum HvacMode {
    #[default]
    Off = 0,
    Heat = 1,
    Cool = 2,
    Auto = 3,
    FanOnly = 4,
    Dry = 5,
}

impl HvacMode {
    /// The `i32` the Slint `ClimateCard.mode` field expects.
    pub fn as_ui(self) -> i32 {
        self as i32
    }

    /// HA `hvac_mode` string → mode. `heat_cool` folds into `Auto`. Unknown → `None`
    /// (so new HA modes are skipped, not mis-rendered).
    pub fn from_ha(s: &str) -> Option<HvacMode> {
        Some(match s {
            "off" => HvacMode::Off,
            "heat" => HvacMode::Heat,
            "cool" => HvacMode::Cool,
            "auto" => HvacMode::Auto,
            "heat_cool" => HvacMode::Auto, // UI has no separate heat_cool
            "fan_only" => HvacMode::FanOnly,
            "dry" => HvacMode::Dry,
            _ => return None,
        })
    }

    /// Mode → HA `hvac_mode` string, for command encode. `Auto` sends `"auto"`
    /// (see module docs: `heat_cool` was folded into `Auto` on ingest).
    pub fn to_ha(self) -> &'static str {
        match self {
            HvacMode::Off => "off",
            HvacMode::Heat => "heat",
            HvacMode::Cool => "cool",
            HvacMode::Auto => "auto",
            HvacMode::FanOnly => "fan_only",
            HvacMode::Dry => "dry",
        }
    }
}

/// What the equipment is currently doing — drives the card's accent color. The UI
/// has only three states; HA's richer `hvac_action` set folds down (see module docs).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
#[repr(i32)]
pub enum HvacAction {
    #[default]
    Idle = 0,
    Heating = 1,
    Cooling = 2,
}

impl HvacAction {
    /// The `i32` the Slint `ClimateCard.action` field expects.
    pub fn as_ui(self) -> i32 {
        self as i32
    }

    /// HA `hvac_action` string → action. `off`/`idle`/`fan` → `Idle`;
    /// `heating`/`preheating` → `Heating`; `cooling`/`drying`/`defrosting` →
    /// `Cooling` (compressor runs while drying → cool/blue accent). Unknown → `None`.
    pub fn from_ha(s: &str) -> Option<HvacAction> {
        Some(match s {
            "off" | "idle" | "fan" => HvacAction::Idle,
            "heating" | "preheating" => HvacAction::Heating,
            "cooling" | "drying" | "defrosting" => HvacAction::Cooling,
            _ => return None,
        })
    }
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// One HA `climate.*` entity, as the watch renders it. All numbers are in the
/// entity's native unit (Nest = °F) — the watch is a unit-agnostic passthrough.
#[derive(Clone, Debug, PartialEq)]
pub struct ClimateEntity {
    /// Friendly name (e.g. "Living Room"). Truncated to 32 bytes on a char boundary.
    pub name: String<32>,
    /// Current measured temperature, if the entity reports one (`null`/absent → `None`).
    pub cur: Option<f32>,
    /// Target setpoint, if set (`null`/absent → `None`).
    pub set: Option<f32>,
    /// Selected mode.
    pub mode: HvacMode,
    /// Current equipment action (accent color).
    pub action: HvacAction,
    /// Minimum settable setpoint.
    pub min: f32,
    /// Maximum settable setpoint.
    pub max: f32,
    /// Setpoint increment.
    pub step: f32,
    /// Supported modes (deduped). The UI filters on [`Self::modes_mask`].
    pub modes: Vec<HvacMode, MODE_COUNT>,
}

impl Default for ClimateEntity {
    fn default() -> Self {
        ClimateEntity {
            name: String::new(),
            cur: None,
            set: None,
            mode: HvacMode::Off,
            action: HvacAction::Idle,
            // Sane fallbacks if the bridge omits the bounds (it never should).
            min: 45.0,
            max: 95.0,
            step: 1.0,
            modes: Vec::new(),
        }
    }
}

impl ClimateEntity {
    /// Supported-modes bitmask: bit `m` set ⇒ mode with discriminant `m` is supported.
    /// `0b001111` (15) = off/heat/cool/auto; `0b111111` (63) = all six. The Slint
    /// segmented control filters its buttons with an arithmetic bit test on this.
    pub fn modes_mask(&self) -> u16 {
        let mut mask: u16 = 0;
        for &m in self.modes.iter() {
            mask |= 1u16 << (m as i32 as u16);
        }
        mask
    }
}

/// Accumulated climate state — one entry per HA `object_id`, upserted as retained
/// state messages arrive. Fixed capacity, heap-free.
#[derive(Clone, Debug, Default)]
pub struct ClimateState {
    /// `(object_id, entity)` pairs, keyed by the HA object id (e.g. "living_room").
    pub entities: Vec<(ObjId, ClimateEntity), MAX_ENTITIES>,
}

impl ClimateState {
    pub const fn new() -> Self {
        ClimateState {
            entities: Vec::new(),
        }
    }

    /// Replace-or-insert by `object_id`. A repeated id updates in place (retained
    /// state refresh); a new id appends. If the table is full an unknown id is
    /// dropped (bounded — never panics, never grows). The id is clipped to 24 bytes
    /// on a char boundary.
    pub fn upsert(&mut self, object_id: &str, entity: ClimateEntity) {
        for e in self.entities.iter_mut() {
            if e.0.as_str() == object_id {
                e.1 = entity;
                return;
            }
        }
        let mut id: ObjId = String::new();
        // clip_str never returns a fragment that overflows N, so push_str fits.
        let _ = id.push_str(clip_str(object_id, OBJ_ID_CAP));
        let _ = self.entities.push((id, entity));
    }

    /// The entity for `object_id`, if present.
    pub fn get(&self, object_id: &str) -> Option<&ClimateEntity> {
        self.entities
            .iter()
            .find(|e| e.0.as_str() == object_id)
            .map(|e| &e.1)
    }

    /// Number of tracked entities.
    pub fn len(&self) -> usize {
        self.entities.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entities.is_empty()
    }

    /// A 64-bit fingerprint of everything the UI renders into a `ClimateCard`.
    ///
    /// The integrator (watch main loop) pushes the Slint climate model only when
    /// this changes, instead of rebuilding a heap `Vec<ClimateCard>` + its
    /// `SharedString`s every tick. That per-tick churn fragmented the allocator
    /// until a routine ~7 KB allocation failed and the watch OOM-panicked while
    /// the Climate screen was open (Energy/Lights push scalars, so they never
    /// did). FNV-1a over each entity's rendered fields — name, cur, set, mode,
    /// action, min/max/step, modes_mask — plus the count, in order.
    pub fn render_fingerprint(&self) -> u64 {
        const OFFSET: u64 = 0xcbf29ce484222325;
        const PRIME: u64 = 0x100000001b3;
        let mut h = OFFSET;
        let mix = |bytes: &[u8], h: &mut u64| {
            for &b in bytes {
                *h ^= b as u64;
                *h = h.wrapping_mul(PRIME);
            }
        };
        mix(&(self.entities.len() as u64).to_le_bytes(), &mut h);
        // `Option<f32>` → a tag byte then the bit pattern (NaN-stable via to_bits).
        let opt_f32 = |v: Option<f32>, h: &mut u64| match v {
            Some(f) => {
                mix(&[1], h);
                mix(&f.to_bits().to_le_bytes(), h);
            }
            None => mix(&[0], h),
        };
        for (_id, e) in self.entities.iter() {
            mix(e.name.as_bytes(), &mut h);
            mix(&[0xff], &mut h); // name terminator (avoids "ab"+"c" == "a"+"bc")
            opt_f32(e.cur, &mut h);
            opt_f32(e.set, &mut h);
            mix(&[e.mode as i32 as u8, e.action as i32 as u8], &mut h);
            mix(&e.min.to_bits().to_le_bytes(), &mut h);
            mix(&e.max.to_bits().to_le_bytes(), &mut h);
            mix(&e.step.to_bits().to_le_bytes(), &mut h);
            mix(&e.modes_mask().to_le_bytes(), &mut h);
        }
        h
    }

    /// Supported-modes mask for `object_id` (`0` if unknown). Convenience for the
    /// integrator mapping the collection → `ClimateCard`s.
    pub fn modes_mask(&self, object_id: &str) -> u16 {
        self.get(object_id).map(ClimateEntity::modes_mask).unwrap_or(0)
    }
}

// ---------------------------------------------------------------------------
// State parse — untrusted, bounded, panic-free.
// ---------------------------------------------------------------------------

/// Parse a bridge climate-state payload into a [`ClimateEntity`].
///
/// Expected shape (fields may be reordered, missing, or accompanied by unknown
/// extras — all tolerated):
/// ```json
/// {"name":"Living Room","cur":71.5,"set":72,"mode":"heat","action":"heating",
///  "min":50,"max":90,"step":1.0,"modes":["off","heat","cool","auto"]}
/// ```
///
/// Returns `None` only on **structural** garbage (not an object, unterminated
/// string, unbalanced brackets, oversized payload). Missing fields fall back to
/// [`ClimateEntity::default`] values; `cur`/`set` are genuinely optional. Never panics.
pub fn parse_state(input: &[u8]) -> Option<ClimateEntity> {
    if input.len() > MAX_INPUT {
        return None;
    }
    let mut p = Parser::new(input);
    p.skip_ws();
    if p.peek()? != b'{' {
        return None;
    }
    p.i += 1;

    let mut r_name = None;
    let mut r_cur = None;
    let mut r_set = None;
    let mut r_mode = None;
    let mut r_action = None;
    let mut r_min = None;
    let mut r_max = None;
    let mut r_step = None;
    let mut r_modes = None;

    loop {
        p.skip_ws();
        match p.peek()? {
            b'}' => break, // end of object; `p` is dropped after the loop
            b',' => {
                p.i += 1;
                continue;
            }
            b'"' => {}
            _ => return None, // unexpected token where a key was due
        }
        let (ks, ke) = p.parse_string()?;
        p.skip_ws();
        if p.peek()? != b':' {
            return None;
        }
        p.i += 1;
        let (vs, ve) = p.skip_value()?;
        match &input[ks..ke] {
            b"name" => r_name = Some((vs, ve)),
            b"cur" => r_cur = Some((vs, ve)),
            b"set" => r_set = Some((vs, ve)),
            b"mode" => r_mode = Some((vs, ve)),
            b"action" => r_action = Some((vs, ve)),
            b"min" => r_min = Some((vs, ve)),
            b"max" => r_max = Some((vs, ve)),
            b"step" => r_step = Some((vs, ve)),
            b"modes" => r_modes = Some((vs, ve)),
            _ => {} // unknown key — ignore (forward-compatible with bridge additions)
        }
    }

    let mut ent = ClimateEntity::default();

    if let Some((s, e)) = r_name {
        if let Some(content) = string_content(&input[s..e]) {
            decode_str_into(&mut ent.name, content);
        }
    }
    ent.cur = r_cur.and_then(|(s, e)| parse_num(&input[s..e]));
    ent.set = r_set.and_then(|(s, e)| parse_num(&input[s..e]));
    if let Some((s, e)) = r_mode {
        if let Some(m) = string_content(&input[s..e]).and_then(|c| HvacMode::from_ha(str_prefix(c)))
        {
            ent.mode = m;
        }
    }
    if let Some((s, e)) = r_action {
        if let Some(a) =
            string_content(&input[s..e]).and_then(|c| HvacAction::from_ha(str_prefix(c)))
        {
            ent.action = a;
        }
    }
    if let Some(v) = r_min.and_then(|(s, e)| parse_num(&input[s..e])) {
        ent.min = v;
    }
    if let Some(v) = r_max.and_then(|(s, e)| parse_num(&input[s..e])) {
        ent.max = v;
    }
    if let Some(v) = r_step.and_then(|(s, e)| parse_num(&input[s..e])) {
        ent.step = v;
    }
    if let Some((s, e)) = r_modes {
        ent.modes = decode_modes(&input[s..e]);
    }

    Some(ent)
}

// ---------------------------------------------------------------------------
// Command encode
// ---------------------------------------------------------------------------

/// Encode a setpoint command payload: `encode_set_temp(72.0)` → `{"set":72.0}`.
/// Non-finite input is coerced to `0.0` so the output is always valid JSON.
pub fn encode_set_temp(temp: f32) -> String<32> {
    let t = if temp.is_finite() { temp } else { 0.0 };
    let mut s: String<32> = String::new();
    // One decimal covers whole and half-degree steps; fits well within 32 B.
    let _ = write!(s, "{{\"set\":{:.1}}}", t);
    s
}

/// Encode a mode command payload: `encode_set_mode(HvacMode::Heat)` → `{"mode":"heat"}`.
pub fn encode_set_mode(mode: HvacMode) -> String<32> {
    let mut s: String<32> = String::new();
    let _ = write!(s, "{{\"mode\":\"{}\"}}", mode.to_ha());
    s
}

// ---------------------------------------------------------------------------
// Setpoint step/clamp
// ---------------------------------------------------------------------------

/// Adjust `current_set` by `delta`, snap to the nearest `step` (measured from
/// `min`), and clamp to `[min, max]`. Used by the detail-view −/+ buttons.
///
/// Robust to junk: non-finite / non-positive `step` skips snapping; inverted or
/// non-finite bounds skip that side of the clamp. Never panics.
pub fn clamp_step(current_set: f32, delta: f32, min: f32, max: f32, step: f32) -> f32 {
    let mut target = current_set + delta;

    if step.is_finite() && step > 0.0 && min.is_finite() {
        let q = (target - min) / step;
        target = min + round_half_away(q) * step;
    }

    if min.is_finite() && target < min {
        target = min;
    }
    if max.is_finite() && max >= min && target > max {
        target = max;
    }
    target
}

/// Round half away from zero, no libm (`core` has no `f32::round`). Saturating
/// float→int cast keeps it panic-free for any magnitude.
fn round_half_away(x: f32) -> f32 {
    if !x.is_finite() {
        return 0.0;
    }
    if x >= 0.0 {
        (x + 0.5) as i64 as f32
    } else {
        (x - 0.5) as i64 as f32
    }
}

// ---------------------------------------------------------------------------
// Bounded JSON helpers (hand-rolled — no serde_json; only what the flat bridge
// payload needs, all walks bounded by slice length).
// ---------------------------------------------------------------------------

/// Cursor over a byte slice with panic-free primitives (only `peek`/`get`, never
/// direct `self.b[i]`).
struct Parser<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Parser<'a> {
    fn new(b: &'a [u8]) -> Self {
        Parser { b, i: 0 }
    }

    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }

    fn skip_ws(&mut self) {
        while let Some(c) = self.peek() {
            if is_ws(c) {
                self.i += 1;
            } else {
                break;
            }
        }
    }

    /// At an opening `"`: return the content byte-range `[start, end)` (escapes
    /// intact) and leave `i` just past the closing `"`. `None` if unterminated.
    fn parse_string(&mut self) -> Option<(usize, usize)> {
        if self.peek()? != b'"' {
            return None;
        }
        self.i += 1;
        let start = self.i;
        while let Some(c) = self.peek() {
            match c {
                b'\\' => self.i += 2, // skip escaped pair; overshoot past end ends the loop
                b'"' => {
                    let end = self.i;
                    self.i += 1;
                    return Some((start, end));
                }
                _ => self.i += 1,
            }
        }
        None
    }

    /// Skip one JSON value (string/object/array/bare token) and return its full
    /// byte-range. `None` if the value is malformed or absent.
    fn skip_value(&mut self) -> Option<(usize, usize)> {
        self.skip_ws();
        let start = self.i;
        match self.peek()? {
            b'"' => {
                self.parse_string()?;
                Some((start, self.i))
            }
            b'{' => {
                self.skip_balanced(b'{', b'}')?;
                Some((start, self.i))
            }
            b'[' => {
                self.skip_balanced(b'[', b']')?;
                Some((start, self.i))
            }
            b',' | b'}' | b']' | b':' => None, // a delimiter is not a value
            _ => {
                // bare token: number / true / false / null → run to a delimiter
                while let Some(c) = self.peek() {
                    if c == b',' || c == b'}' || c == b']' || is_ws(c) {
                        break;
                    }
                    self.i += 1;
                }
                if self.i == start {
                    None
                } else {
                    Some((start, self.i))
                }
            }
        }
    }

    /// At `open`: consume through the matching `close`, honoring nested pairs and
    /// skipping over string contents (so brackets inside strings don't count).
    /// `None` if unbalanced.
    fn skip_balanced(&mut self, open: u8, close: u8) -> Option<()> {
        if self.peek()? != open {
            return None;
        }
        self.i += 1;
        let mut depth = 1u32;
        while let Some(c) = self.peek() {
            if c == b'"' {
                self.parse_string()?;
            } else if c == open {
                depth += 1;
                self.i += 1;
            } else if c == close {
                depth -= 1;
                self.i += 1;
                if depth == 0 {
                    return Some(());
                }
            } else {
                self.i += 1;
            }
        }
        None
    }
}

fn is_ws(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r')
}

/// The longest valid-UTF-8 prefix of `b` as `&str` (untrusted bytes may not be
/// valid UTF-8; never panics).
fn str_prefix(b: &[u8]) -> &str {
    match core::str::from_utf8(b) {
        Ok(s) => s,
        Err(e) => core::str::from_utf8(&b[..e.valid_up_to()]).unwrap_or(""),
    }
}

/// Strip a string value's surrounding quotes, returning the (still-escaped) content
/// bytes. `raw` is a value range from [`Parser::skip_value`]. `None` if not a string.
fn string_content(raw: &[u8]) -> Option<&[u8]> {
    let mut s = 0;
    let mut e = raw.len();
    while s < e && is_ws(raw[s]) {
        s += 1;
    }
    while e > s && is_ws(raw[e - 1]) {
        e -= 1;
    }
    if e - s < 2 || raw[s] != b'"' || raw[e - 1] != b'"' {
        return None;
    }
    Some(&raw[s + 1..e - 1])
}

/// Decode a JSON string body (`content`, without quotes) into a fixed-capacity
/// `String`, translating standard escapes. Overflowing chars are **dropped**
/// (heapless `push` writes a whole char or nothing), so the result never exceeds
/// `N` bytes and is always valid UTF-8 — truncation lands on a char boundary.
///
/// Iterates by **char**, never by byte index: an untrusted `name` may put a
/// multibyte codepoint right after a backslash (e.g. `\é`), and any fixed `+2`
/// byte step would land mid-codepoint and panic on the next slice. Char-aware
/// iteration is structurally immune to that (regression: oracle-t9-spec, the
/// `rssi::clip` char-boundary class).
fn decode_str_into<const N: usize>(dst: &mut String<N>, content: &[u8]) {
    let mut chars = str_prefix(content).chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            push_char(dst, c);
            continue;
        }
        match chars.next() {
            None => break, // dangling backslash
            Some('"') => push_char(dst, '"'),
            Some('\\') => push_char(dst, '\\'),
            Some('/') => push_char(dst, '/'),
            Some('n') => push_char(dst, '\n'),
            Some('t') => push_char(dst, '\t'),
            Some('r') => push_char(dst, '\r'),
            Some('b') => push_char(dst, '\u{08}'),
            Some('f') => push_char(dst, '\u{0c}'),
            Some('u') => {
                // Consume up to 4 hex digits; a complete \uXXXX becomes its char
                // (lone surrogates / incomplete escapes are dropped, never panic).
                let mut cp: u32 = 0;
                let mut n = 0;
                while n < 4 {
                    match chars.peek() {
                        Some(&h) if h.is_ascii_hexdigit() => {
                            cp = (cp << 4) | h.to_digit(16).unwrap_or(0);
                            chars.next();
                            n += 1;
                        }
                        _ => break,
                    }
                }
                if n == 4 {
                    if let Some(ch) = char::from_u32(cp) {
                        push_char(dst, ch);
                    }
                }
            }
            // Unknown escape (incl. `\` followed by a multibyte char): emit the
            // following char literally. Lenient but panic-free.
            Some(other) => push_char(dst, other),
        }
    }
}

/// Push a char, silently dropping it if the target is full (bounded truncation).
fn push_char<const N: usize>(dst: &mut String<N>, ch: char) {
    let _ = dst.push(ch);
}

/// Parse a JSON number token → `f32`. Handles sign, fraction, and exponent (the
/// exponent is clamped so a giant literal can't spin). Returns `None` for
/// `null`/`true`/`false`/empty/trailing-junk. Never panics.
fn parse_num(raw: &[u8]) -> Option<f32> {
    let n = raw.len();
    let mut i = 0;
    let mut end = n;
    while i < end && is_ws(raw[i]) {
        i += 1;
    }
    while end > i && is_ws(raw[end - 1]) {
        end -= 1;
    }
    if i >= end {
        return None;
    }

    let mut neg = false;
    if raw[i] == b'-' || raw[i] == b'+' {
        neg = raw[i] == b'-';
        i += 1;
    }

    let mut int_part: f64 = 0.0;
    let mut any = false;
    while i < end && raw[i].is_ascii_digit() {
        int_part = int_part * 10.0 + (raw[i] - b'0') as f64;
        i += 1;
        any = true;
    }

    let mut frac: f64 = 0.0;
    let mut scale: f64 = 1.0;
    if i < end && raw[i] == b'.' {
        i += 1;
        while i < end && raw[i].is_ascii_digit() {
            frac = frac * 10.0 + (raw[i] - b'0') as f64;
            scale *= 10.0;
            i += 1;
            any = true;
        }
    }
    if !any {
        return None;
    }

    let mut val = int_part + frac / scale;

    if i < end && (raw[i] == b'e' || raw[i] == b'E') {
        i += 1;
        let mut eneg = false;
        if i < end && (raw[i] == b'-' || raw[i] == b'+') {
            eneg = raw[i] == b'-';
            i += 1;
        }
        let mut exp: i32 = 0;
        let mut eany = false;
        while i < end && raw[i].is_ascii_digit() {
            exp = exp.saturating_mul(10).saturating_add((raw[i] - b'0') as i32);
            i += 1;
            eany = true;
        }
        if !eany {
            return None;
        }
        let mut e = if eneg { -exp } else { exp }.clamp(-400, 400);
        while e > 0 {
            val *= 10.0;
            e -= 1;
            if val.is_infinite() {
                break;
            }
        }
        while e < 0 {
            val /= 10.0;
            e += 1;
        }
    }

    if i != end {
        return None; // trailing junk → not a clean number
    }
    if neg {
        val = -val;
    }
    Some(val as f32)
}

/// Decode a `modes` JSON array (e.g. `["off","heat","cool","auto"]`) into a deduped
/// mode list. Unknown mode strings are skipped; `heat_cool` collapses onto an
/// existing `Auto` (deduped). Bounded by `raw.len()`; never panics.
fn decode_modes(raw: &[u8]) -> Vec<HvacMode, MODE_COUNT> {
    let mut v: Vec<HvacMode, MODE_COUNT> = Vec::new();
    let n = raw.len();
    let mut i = 0;
    while i < n && is_ws(raw[i]) {
        i += 1;
    }
    if i >= n || raw[i] != b'[' {
        return v;
    }
    i += 1;
    while i < n {
        while i < n && is_ws(raw[i]) {
            i += 1;
        }
        if i >= n {
            break;
        }
        match raw[i] {
            b']' => break,
            b',' => i += 1,
            b'"' => {
                i += 1;
                let start = i;
                while i < n {
                    match raw[i] {
                        b'\\' => i += 2,
                        b'"' => break,
                        _ => i += 1,
                    }
                }
                if i >= n {
                    break; // unterminated element
                }
                let content = &raw[start..i];
                i += 1; // past closing quote
                if let Some(m) = HvacMode::from_ha(str_prefix(content)) {
                    if !v.contains(&m) {
                        let _ = v.push(m); // ignore overflow beyond MODE_COUNT
                    }
                }
            }
            _ => i += 1, // unexpected token — skip
        }
    }
    v
}

/// Left-truncate `s` to at most `n` bytes on a UTF-8 char boundary — never panics.
/// Same hardening as `crates/rssi::clip`: untrusted names must not be byte-sliced
/// mid-codepoint (that panics = remote-crash vector). Walks back to the largest
/// boundary ≤ `n`.
fn clip_str(s: &str, n: usize) -> &str {
    if s.len() <= n {
        return s;
    }
    let mut end = n;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}
