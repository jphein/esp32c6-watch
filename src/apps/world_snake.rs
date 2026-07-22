// World Snake — smol's shared-world MMO mesh snake (SMOLv1 SNK), ported from
// jphein/smol rust/clock/src/mesh_snake/{snake_core.rs,mod.rs}.
//
// One 256x256 toroidal world shared by every node on the ESP-NOW mesh. Each
// node is the sole authority over its own snake and broadcasts an absolute,
// stateless 18-byte head snapshot at 5 Hz (per-id phase-jittered); peers are
// reconstructed by dead-reckoning the observed head (the body is never on the
// wire). Food and treasure are pure functions of (GAME_SEED, mesh-time
// bucket), so every board computes the same spawns with zero messaging.
//
// Wire format (BYTE-COMPATIBLE with the C3 fleet, build 36+):
//   [0..11)  "SMOLv1 SNK "  ASCII prefix
//   [11]     ver    u8      1 (v2 = 19 B with trailing score byte; parse degrades)
//   [12]     id     u8      sender snake id
//   [13]     tick   u8      wrapping step counter (ordering + dead-reckon base)
//   [14]     flags  u8      bit0 alive | bits1-2 heading (0=U 1=R 2=D 3=L)
//                           | bits3-7 active power (0=none, 1..=6 defined)
//   [15]     head_x u8      world cell X (toroidal)
//   [16]     head_y u8      world cell Y (toroidal)
//   [17]     length u8      segment count (body dead-reckoned, not sent)
//
// The app can't own EspNow: the main loop drains `pending_tx()` and feeds
// `handle_rx()` with SNK-prefixed payloads while the app is active.

use core::fmt::Write as _;

use embedded_graphics::geometry::Point as EgPoint;
use embedded_graphics::mono_font::ascii::{FONT_6X10, FONT_10X20};
use embedded_graphics::mono_font::MonoTextStyle;
use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::{PrimitiveStyle, Rectangle, RoundedRectangle};
use embedded_graphics::text::{Alignment, Baseline, Text};

use crate::apps::{App, AppInput, AppResult};
use crate::drivers::framebuffer::Framebuffer;
use crate::peripherals::touch::SwipeDirection;

// ===========================================================================
// Game tunables (mirrored from snake_core.rs — the fleet's source of truth)
// ===========================================================================

/// World dimensions (cells), toroidal. 256x256 ratified (design §10.1).
const WORLD_W: u16 = 256;
const WORLD_H: u16 = 256;
/// Base movement step (ms), §10.7 FINAL.
const STEP_MS: u32 = 200;
/// Zephyr Rune (Haste) step (ms) ~= 1.75x faster.
const HASTE_STEP_MS: u32 = 114;
/// Fresh-spawn body length.
const START_LEN: usize = 3;
/// Hard max body length (ring-buffer capacity).
const SNAKE_CAP: usize = 64;
/// Own-state broadcast period (ms) — 5 Hz, one snapshot per step.
const BROADCAST_PERIOD_MS: u32 = 200;
/// Max simultaneously-tracked peers (also the phase-jitter slot count).
const PEER_CAP: usize = 16;
/// No frame within this (ms) => peer despawns.
const PEER_STALE_MS: u32 = 5_000;
/// Simultaneous food beacons per bucket.
const FOOD_COUNT: usize = 12;
/// Food re-roll period (s).
const FOOD_PERIOD_S: u32 = 20;
/// Treasure (power) re-roll period (s).
const TREASURE_PERIOD_S: u32 = 45;
/// Shared compile-time food/treasure seed (NOT per-node) — only the mesh-clock
/// bucket must converge across boards for spawns to agree.
const GAME_SEED: u32 = 0x5340_4B45; // "S@KE"
/// Salt distinguishing the treasure stream from the food stream.
const TREASURE_KSALT: u32 = 0x5EED_7EA5;
/// Design-target peer count; also the number of phase slots.
const PHASE_NMAX: u8 = 16;

// Power durations (s), design §11.1.
const DUR_PHANTOM_S: u32 = 6;
const DUR_HASTE_S: u32 = 5;
const DUR_SHIELD_S: u32 = 10;
const DUR_MIDAS_S: u32 = 8;
const DUR_REVEAL_S: u32 = 10;
const DUR_PHOENIX_S: u32 = 10;

// ===========================================================================
// SMOLv1 SNK wire frame — byte-identical to mesh_snake/snake_core.rs
// ===========================================================================

/// ASCII, sniffer-greppable frame prefix. Exactly 11 bytes.
pub const SNK_PREFIX: &[u8; 11] = b"SMOLv1 SNK ";
/// Version 1 frame: 18 B, no score byte (score == length).
const SNK_VER: u8 = 1;
/// Version 2 frame: 18 B core + 1 B explicit score.
const SNK_VER_SCORE: u8 = 2;
/// Total on-wire length of a v1 frame.
const SNK_FRAME_LEN: usize = 18;
/// Total on-wire length of a v2 frame.
const SNK_FRAME_LEN_V2: usize = 19;
/// TX scratch size the main loop should provide.
pub const SNK_TX_BUF: usize = SNK_FRAME_LEN_V2;

// flags byte bit layout (FINAL, cross-board wire contract):
const FLAG_ALIVE_MASK: u8 = 0b0000_0001;
const FLAG_HEADING_SHIFT: u8 = 1;
const FLAG_HEADING_MASK: u8 = 0b11;
const FLAG_POWER_SHIFT: u8 = 3;
const FLAG_POWER_MASK: u8 = 0b0001_1111;

// Authoritative active-power ids (0 = none, 1..=6 defined, 7..=31 reserved).
const POWER_PHANTOM: u8 = 1; // Wraith Veil — phase through all bodies
const POWER_HASTE: u8 = 2; // Zephyr Rune — ~1.75x speed
const POWER_SHIELD: u8 = 3; // Aegis Ward — absorb one lethal hit
const POWER_MIDAS: u8 = 4; // Midas Sigil — food yields +3 length
const POWER_REVEAL: u8 = 5; // Mothlight Lantern — reveal (advisory)
const POWER_PHOENIX: u8 = 6; // Phoenix Ember — respawn keeping length
const POWER_COUNT: u8 = 6;

fn power_name(power: u8) -> &'static str {
    match power {
        POWER_PHANTOM => "Wraith Veil",
        POWER_HASTE => "Zephyr Rune",
        POWER_SHIELD => "Aegis Ward",
        POWER_MIDAS => "Midas Sigil",
        POWER_REVEAL => "Mothlight",
        POWER_PHOENIX => "Phoenix",
        _ => "?",
    }
}

/// A decoded SMOLv1 SNK frame. Mirrors the on-wire fields 1:1.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SnkFrame {
    pub ver: u8,
    pub id: u8,
    pub tick: u8,
    pub alive: bool,
    pub heading: Dir,
    pub power: u8,
    pub head: Cell,
    pub length: u8,
    /// v1 wire: implied == length. v2 wire: explicit byte.
    pub score: u8,
}

impl SnkFrame {
    fn flags(&self) -> u8 {
        (self.alive as u8) & FLAG_ALIVE_MASK
            | ((self.heading.to_bits() & FLAG_HEADING_MASK) << FLAG_HEADING_SHIFT)
            | ((self.power & FLAG_POWER_MASK) << FLAG_POWER_SHIFT)
    }
}

/// Encode a v1 (18 B) frame. Returns bytes written, or None if `out` is small.
pub fn encode_snk(f: &SnkFrame, out: &mut [u8]) -> Option<usize> {
    if out.len() < SNK_FRAME_LEN {
        return None;
    }
    out[..SNK_PREFIX.len()].copy_from_slice(SNK_PREFIX);
    out[11] = f.ver;
    out[12] = f.id;
    out[13] = f.tick;
    out[14] = f.flags();
    out[15] = (f.head.x & 0xff) as u8;
    out[16] = (f.head.y & 0xff) as u8;
    out[17] = f.length;
    Some(SNK_FRAME_LEN)
}

/// Parse a SMOLv1 SNK frame. Version-degrading, not version-rejecting: the
/// stable 18 B core decodes for any ver >= 1; the score byte is read only when
/// ver >= 2 and 19 B are present, otherwise score = length. Total, rejects
/// garbage, never panics.
pub fn parse_snk(buf: &[u8]) -> Option<SnkFrame> {
    if buf.len() < SNK_FRAME_LEN {
        return None; // truncated
    }
    if &buf[..SNK_PREFIX.len()] != SNK_PREFIX.as_slice() {
        return None; // foreign tag / garbage
    }
    let ver = buf[11];
    if ver == 0 {
        return None; // 0 = uninitialized/garbage, never a real frame
    }
    let flags = buf[14];
    let alive = flags & FLAG_ALIVE_MASK != 0;
    let heading = Dir::from_bits((flags >> FLAG_HEADING_SHIFT) & FLAG_HEADING_MASK);
    let power = (flags >> FLAG_POWER_SHIFT) & FLAG_POWER_MASK;
    let length = buf[17];
    let score = if ver >= SNK_VER_SCORE && buf.len() >= SNK_FRAME_LEN_V2 {
        buf[18]
    } else {
        length
    };
    Some(SnkFrame {
        ver,
        id: buf[12],
        tick: buf[13],
        alive,
        heading,
        power,
        head: Cell::new(buf[15] as u16, buf[16] as u16),
        length,
        score,
    })
}

/// Wrap-aware (RFC 1982) "is `a` newer than `b`" for the tick:u8 counter.
fn tick_is_newer(a: u8, b: u8) -> bool {
    let d = a.wrapping_sub(b);
    d != 0 && d < 128
}

/// Per-id broadcast offset (ms) within the 200 ms window (netcode spec §2):
/// `(id % nmax) * (period / nmax)` — mandatory jitter above N~8.
fn phase_offset_ms(id: u8, nmax: u8, period_ms: u32) -> u32 {
    let nmax = nmax.max(1);
    (id % nmax) as u32 * (period_ms / nmax as u32)
}

// ===========================================================================
// World / Cell / Dir — toroidal math (fixed 256x256)
// ===========================================================================

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Cell {
    pub x: u16,
    pub y: u16,
}

impl Cell {
    pub const fn new(x: u16, y: u16) -> Self {
        Self { x, y }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Dir {
    #[default]
    North,
    East,
    South,
    West,
}

impl Dir {
    const fn opposite(self) -> Self {
        match self {
            Dir::North => Dir::South,
            Dir::East => Dir::West,
            Dir::South => Dir::North,
            Dir::West => Dir::East,
        }
    }

    /// y grows downward (screen convention); North decreases y.
    const fn delta(self) -> (i32, i32) {
        match self {
            Dir::North => (0, -1),
            Dir::East => (1, 0),
            Dir::South => (0, 1),
            Dir::West => (-1, 0),
        }
    }

    /// Wire encoding: 0=U 1=R 2=D 3=L (explicit — the contract can't drift).
    const fn to_bits(self) -> u8 {
        match self {
            Dir::North => 0,
            Dir::East => 1,
            Dir::South => 2,
            Dir::West => 3,
        }
    }

    const fn from_bits(b: u8) -> Self {
        match b & 0b11 {
            0 => Dir::North,
            1 => Dir::East,
            2 => Dir::South,
            _ => Dir::West,
        }
    }
}

fn wrap_x(x: i32) -> u16 {
    x.rem_euclid(WORLD_W as i32) as u16
}

fn wrap_y(y: i32) -> u16 {
    y.rem_euclid(WORLD_H as i32) as u16
}

/// Move `c` by (dx, dy) with toroidal wrap — THE constructor for moved cells.
fn cell_add(c: Cell, dx: i32, dy: i32) -> Cell {
    Cell {
        x: wrap_x(c.x as i32 + dx),
        y: wrap_y(c.y as i32 + dy),
    }
}

/// Forward (non-negative) distance from `b` to `a` along +x, mod W — how a
/// viewport places a cell relative to the camera origin across the seam.
fn forward_x(a: u16, b: u16) -> u16 {
    (a as i32 - b as i32).rem_euclid(WORLD_W as i32) as u16
}

fn forward_y(a: u16, b: u16) -> u16 {
    (a as i32 - b as i32).rem_euclid(WORLD_H as i32) as u16
}

// ===========================================================================
// Snake — fixed-cap segment ring buffer (ported verbatim logic)
// ===========================================================================

#[derive(Clone, Copy, PartialEq, Eq)]
enum StepOutcome {
    Moved,
    Blocked,
}

struct Snake {
    seg: [Cell; SNAKE_CAP],
    head_idx: usize,
    len: usize,
    target_len: usize,
    heading: Dir,
}

impl Snake {
    /// Spawn a coherent snake: `head` plus a straight body laid out behind it.
    fn new(head: Cell, heading: Dir, len: usize) -> Self {
        let len = len.clamp(1, SNAKE_CAP);
        let mut seg = [head; SNAKE_CAP];
        let (bdx, bdy) = heading.opposite().delta();
        for i in 0..len {
            let idx = (SNAKE_CAP - i) % SNAKE_CAP;
            seg[idx] = cell_add(head, bdx * i as i32, bdy * i as i32);
        }
        Self {
            seg,
            head_idx: 0,
            len,
            target_len: len,
            heading,
        }
    }

    fn head(&self) -> Cell {
        self.seg[self.head_idx]
    }

    fn grow(&mut self, n: usize) {
        self.target_len = (self.target_len + n).min(SNAKE_CAP);
    }

    /// Iterate body cells head->tail (segment 0 = head).
    fn segments(&self) -> impl Iterator<Item = Cell> + '_ {
        (0..self.len).map(move |i| self.seg[(self.head_idx + SNAKE_CAP - i) % SNAKE_CAP])
    }

    fn first_segments(&self, n: usize) -> impl Iterator<Item = Cell> + '_ {
        (0..n.min(self.len)).map(move |i| self.seg[(self.head_idx + SNAKE_CAP - i) % SNAKE_CAP])
    }

    /// Advance the head one cell. `blocked(cell)` reports externally-occupied
    /// cells; self-collision is checked internally unless `phase_self`
    /// (Wraith Veil). Moving into the vacating tail cell is allowed unless
    /// growing this tick (classic snake rule).
    fn step(&mut self, blocked: impl Fn(Cell) -> bool, phase_self: bool) -> StepOutcome {
        let (dx, dy) = self.heading.delta();
        let new_head = cell_add(self.head(), dx, dy);

        let growing = self.len < self.target_len;
        let remaining = if growing { self.len } else { self.len - 1 };

        let hit_self = !phase_self && self.first_segments(remaining).any(|c| c == new_head);
        if hit_self || blocked(new_head) {
            return StepOutcome::Blocked;
        }

        self.head_idx = (self.head_idx + 1) % SNAKE_CAP;
        self.seg[self.head_idx] = new_head;
        if growing {
            self.len += 1;
        }
        StepOutcome::Moved
    }
}

// ===========================================================================
// Peers — dead-reckoned remote snakes, tick ordering + staleness despawn
// ===========================================================================

#[derive(Clone, Copy, Default)]
struct PeerSnake {
    id: u8,
    tick: u8,
    /// Local clock at the last accepted frame (staleness + reckon base).
    last_ms: u32,
    head0: Cell,
    heading: Dir,
    length: u16,
    alive: bool,
    power: u8,
    active: bool,
}

impl PeerSnake {
    /// Dead-reckon the head forward, clamped to 3 cells past the last frame —
    /// the spec's `min(elapsed/STEP_MS, 3)` rule (survives 10-30% loss without
    /// runaway ghosts).
    fn dead_reckon_head(&self, now_ms: u32) -> Cell {
        let steps = (now_ms.saturating_sub(self.last_ms) / STEP_MS).min(3) as i32;
        let (dx, dy) = self.heading.delta();
        cell_add(self.head0, dx * steps, dy * steps)
    }

    /// Fill `out` with body cells (head first), straight-line approximation
    /// trailing opposite the heading (no turn history on the wire).
    fn body_cells(&self, now_ms: u32, out: &mut [Cell]) -> usize {
        let head = self.dead_reckon_head(now_ms);
        let (bdx, bdy) = self.heading.opposite().delta();
        let n = (self.length as usize).min(out.len());
        for (i, slot) in out.iter_mut().enumerate().take(n) {
            *slot = cell_add(head, bdx * i as i32, bdy * i as i32);
        }
        n
    }
}

struct PeerTable {
    peers: [PeerSnake; PEER_CAP],
}

impl PeerTable {
    fn new() -> Self {
        Self {
            peers: [PeerSnake::default(); PEER_CAP],
        }
    }

    /// Insert or update by id with tick-wrap ordering: a late straggler can't
    /// rubber-band a peer. Every accepted frame fully refreshes the peer.
    fn ingest(&mut self, f: &SnkFrame, recv_ms: u32) {
        for p in self.peers.iter_mut() {
            if p.active && p.id == f.id {
                if !tick_is_newer(f.tick, p.tick) {
                    return; // stale tick — dropped
                }
                Self::apply(p, f, recv_ms);
                return;
            }
        }
        for p in self.peers.iter_mut() {
            if !p.active {
                Self::apply(p, f, recv_ms);
                return;
            }
        }
        // Table full of other live peers — dropped (Overflow).
    }

    fn apply(p: &mut PeerSnake, f: &SnkFrame, recv_ms: u32) {
        p.id = f.id;
        p.tick = f.tick;
        p.last_ms = recv_ms;
        p.head0 = f.head;
        p.heading = f.heading;
        p.length = f.length as u16;
        p.alive = f.alive;
        p.power = f.power;
        p.active = true;
    }

    fn prune(&mut self, now_ms: u32, despawn_ms: u32) {
        for p in self.peers.iter_mut() {
            if p.active && now_ms.saturating_sub(p.last_ms) > despawn_ms {
                p.active = false;
            }
        }
    }

    fn active(&self) -> impl Iterator<Item = &PeerSnake> + '_ {
        self.peers.iter().filter(|p| p.active)
    }

    fn active_count(&self) -> usize {
        self.peers.iter().filter(|p| p.active).count()
    }
}

// ===========================================================================
// Food / treasure — deterministic spawns, pure fn of (seed, time bucket)
// ===========================================================================

fn splitmix64(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn food_at(seed: u32, time_bucket: u32) -> Cell {
    let h = splitmix64(((seed as u64) << 32) | time_bucket as u64);
    Cell {
        x: wrap_x((h & 0xFFFF) as i32),
        y: wrap_y(((h >> 16) & 0xFFFF) as i32),
    }
}

/// Fill `out` with the bucket's deterministic food beacons; every node
/// computes the identical set with zero messaging.
fn food_cells(seed: u32, time_bucket: u32, out: &mut [Cell; FOOD_COUNT]) -> usize {
    for (i, slot) in out.iter_mut().enumerate() {
        let s = seed ^ (i as u32).wrapping_mul(0x9E37_79B1);
        *slot = food_at(s, time_bucket);
    }
    FOOD_COUNT
}

/// Deterministic treasure spawn: the cell AND which power (1..=6).
fn treasure_at(seed: u32, treasure_bucket: u32) -> (Cell, u8) {
    let h = splitmix64(((seed ^ TREASURE_KSALT) as u64) << 32 | treasure_bucket as u64);
    let cell = Cell {
        x: wrap_x((h & 0xFFFF) as i32),
        y: wrap_y(((h >> 16) & 0xFFFF) as i32),
    };
    let power = 1 + (h % POWER_COUNT as u64) as u8;
    (cell, power)
}

/// Per-bucket "already eaten" set — the anti-farm guard: each beacon feeds
/// you exactly once per bucket. Auto-clears when the bucket rolls.
struct EatenSet {
    bucket: u32,
    cells: [Cell; FOOD_COUNT],
    len: usize,
}

impl EatenSet {
    const fn new() -> Self {
        Self {
            bucket: u32::MAX,
            cells: [Cell::new(0, 0); FOOD_COUNT],
            len: 0,
        }
    }

    fn eat(&mut self, bucket: u32, cell: Cell) -> bool {
        if bucket != self.bucket {
            self.bucket = bucket;
            self.len = 0;
        }
        if self.cells[..self.len].contains(&cell) {
            return false;
        }
        if self.len < FOOD_COUNT {
            self.cells[self.len] = cell;
            self.len += 1;
        }
        true
    }
}

// ===========================================================================
// Viewport — scrolling camera over the torus, sized for the 410x502 AMOLED
// ===========================================================================

const SCREEN_W: i32 = 410;
const SCREEN_H: i32 = 502;
/// Pixels per world cell (the C3 fleet renders 4 px on a 72x40 OLED; the
/// watch has room for 16 px).
const CELL_PX: i32 = 16;
/// Visible world window: 25x28 cells = 400x448 px.
const VIEW_COLS: u16 = 25;
const VIEW_ROWS: u16 = 28;
const VIEW_X: i32 = (SCREEN_W - VIEW_COLS as i32 * CELL_PX) / 2; // 5
const VIEW_Y: i32 = 46; // below the HUD strip

/// Camera window over the torus; `origin` is the world cell at the top-left.
#[derive(Clone, Copy)]
struct Camera {
    origin: Cell,
}

impl Camera {
    fn centered_on(head: Cell) -> Self {
        Self {
            origin: Cell {
                x: wrap_x(head.x as i32 - (VIEW_COLS / 2) as i32),
                y: wrap_y(head.y as i32 - (VIEW_ROWS / 2) as i32),
            },
        }
    }

    /// Map a world cell to the top-left screen pixel, or None if off-window.
    /// Seam-aware: offsets are computed modulo the world size.
    fn world_to_screen(&self, c: Cell) -> Option<(i32, i32)> {
        let col = forward_x(c.x, self.origin.x);
        let row = forward_y(c.y, self.origin.y);
        if col < VIEW_COLS && row < VIEW_ROWS {
            Some((
                VIEW_X + col as i32 * CELL_PX,
                VIEW_Y + row as i32 * CELL_PX,
            ))
        } else {
            None
        }
    }
}

/// Per-id peer color (the fleet's mono OLED used hollow-vs-solid; the watch
/// gets a palette).
const PEER_COLORS: [Rgb565; 8] = [
    Rgb565::new(31, 32, 0),  // orange
    Rgb565::new(0, 63, 31),  // cyan
    Rgb565::new(31, 0, 31),  // magenta
    Rgb565::new(31, 63, 0),  // yellow
    Rgb565::new(8, 30, 31),  // sky blue
    Rgb565::new(31, 20, 20), // pink
    Rgb565::new(16, 63, 8),  // spring green
    Rgb565::new(31, 25, 0),  // amber
];

fn peer_color(id: u8) -> Rgb565 {
    PEER_COLORS[(id % 8) as usize]
}

// ===========================================================================
// The app
// ===========================================================================

pub struct WorldSnakeApp {
    id: u8,
    snake: Snake,
    peers: PeerTable,
    /// Wrapping broadcast tick (wire ordering). NOT reset by setup() — peers
    /// remember our last tick and would drop a restarted-from-0 stream.
    tick: u8,
    /// Internal monotonic clock (ms), accumulated from AppInput.dt_ms. All
    /// peer recv stamps, dead-reckoning and TX scheduling use this one clock.
    now_ms: u32,
    /// Mesh Unix clock for food/treasure buckets; free-runs on dt_ms between
    /// `set_unix()` corrections from the SMOLv1 TIME authority.
    unix_now: u32,
    unix_frac_ms: u32,
    unix_synced: bool,
    last_step_ms: u32,
    /// Next phase-jittered broadcast deadline on the internal clock.
    next_tx_ms: u32,
    phase_ms: u32,
    dead: bool,
    /// Active own power (0 = none) + clock-based expiry (Unix s).
    power: u8,
    power_until_unix: u32,
    aegis_charged: bool,
    phoenix_ready: bool,
    eaten: EatenSet,
    took_treasure_bucket: Option<u32>,
    /// Render frame counter (phantom flicker phase).
    frame: u32,
    rx_frames: u32,
}

impl WorldSnakeApp {
    pub fn new(id: u8) -> Self {
        Self {
            id,
            snake: Snake::new(Cell::new(WORLD_W / 2, WORLD_H / 2), Dir::East, START_LEN),
            peers: PeerTable::new(),
            tick: 0,
            now_ms: 0,
            unix_now: 0,
            unix_frac_ms: 0,
            unix_synced: false,
            last_step_ms: 0,
            next_tx_ms: 0,
            phase_ms: phase_offset_ms(id, PHASE_NMAX, BROADCAST_PERIOD_MS),
            dead: false,
            power: 0,
            power_until_unix: 0,
            aegis_charged: false,
            phoenix_ready: false,
            eaten: EatenSet::new(),
            took_treasure_bucket: None,
            frame: 0,
            rx_frames: 0,
        }
    }

    // ---- mesh plumbing (called by the main loop, which owns EspNow) --------

    /// Feed the mesh Unix clock (from SMOLv1 TIME adoption / NTP). Buckets
    /// only need to converge across boards for food/treasure to agree.
    pub fn set_unix(&mut self, unix: u32) {
        self.unix_now = unix;
        self.unix_frac_ms = 0;
        self.unix_synced = true;
    }

    /// Accept one received ESP-NOW payload (SNK-prefixed). Non-SNK or own-id
    /// frames are ignored; garbage never panics.
    pub fn handle_rx(&mut self, data: &[u8]) {
        if let Some(f) = parse_snk(data) {
            if f.id == self.id {
                return; // our own broadcast echoed back
            }
            self.rx_frames = self.rx_frames.wrapping_add(1);
            self.peers.ingest(&f, self.now_ms);
        }
    }

    /// The TX path: when the per-id phase-jittered 5 Hz deadline has passed,
    /// encode our absolute head snapshot into `out` (>= 18 B) and return its
    /// length. The main loop drains this and broadcasts over ESP-NOW. Dead
    /// snakes still announce (alive=0) so peers clear them fast.
    pub fn pending_tx(&mut self, out: &mut [u8]) -> Option<usize> {
        if self.now_ms < self.next_tx_ms {
            return None;
        }
        // Re-arm on the period grid + per-id phase slot (netcode spec §2).
        let slot = self.now_ms.saturating_sub(self.phase_ms) / BROADCAST_PERIOD_MS;
        self.next_tx_ms = (slot + 1) * BROADCAST_PERIOD_MS + self.phase_ms;

        self.tick = self.tick.wrapping_add(1);
        let f = SnkFrame {
            ver: SNK_VER,
            id: self.id,
            tick: self.tick,
            alive: !self.dead,
            heading: self.snake.heading,
            power: self.active_power(),
            head: self.snake.head(),
            length: self.snake.len.min(255) as u8,
            score: self.snake.len.min(255) as u8,
        };
        encode_snk(&f, out)
    }

    // ---- game rules (ported from mesh_snake/mod.rs) -------------------------

    fn active_power(&self) -> u8 {
        if self.power != 0 && self.unix_now < self.power_until_unix {
            self.power
        } else {
            0
        }
    }

    /// Respawn at the world centre. Phoenix keeps length; a plain respawn
    /// resets to START_LEN.
    fn respawn(&mut self, keep_len: usize) {
        let head = Cell::new(WORLD_W / 2, WORLD_H / 2);
        let len = keep_len.clamp(START_LEN, SNAKE_CAP);
        self.snake = Snake::new(head, Dir::East, len);
        self.dead = false;
        self.last_step_ms = self.now_ms;
        self.power = 0;
        self.aegis_charged = false;
        self.phoenix_ready = false;
    }

    fn steer(&mut self, swipe: SwipeDirection) {
        let want = match swipe {
            SwipeDirection::Up => Dir::North,
            SwipeDirection::Down => Dir::South,
            SwipeDirection::Left => Dir::West,
            SwipeDirection::Right => Dir::East,
            _ => return,
        };
        // A >1-segment snake can't reverse into itself.
        if self.snake.len > 1 && want == self.snake.heading.opposite() {
            return;
        }
        self.snake.heading = want;
    }

    /// Food / treasure pickup at the new head (both-get-it eaten-race: honest
    /// default — the cell simply moves next bucket, no authority handshake).
    fn handle_pickups(&mut self) {
        let head = self.snake.head();

        // Treasure (rarer bucket). One alive at a time.
        let tbucket = self.unix_now / TREASURE_PERIOD_S;
        let (tcell, tpower) = treasure_at(GAME_SEED, tbucket);
        if head == tcell && self.took_treasure_bucket != Some(tbucket) {
            self.took_treasure_bucket = Some(tbucket);
            self.grant_power(tpower);
        }

        // Food (K beacons). +1 length, or +3 under Midas.
        let fbucket = self.unix_now / FOOD_PERIOD_S;
        let mut beacons = [Cell::default(); FOOD_COUNT];
        let n = food_cells(GAME_SEED, fbucket, &mut beacons);
        if beacons[..n].contains(&head) && self.eaten.eat(fbucket, head) {
            let grow = if self.active_power() == POWER_MIDAS { 3 } else { 1 };
            self.snake.grow(grow);
        }
    }

    fn grant_power(&mut self, power: u8) {
        self.power = power;
        let dur = match power {
            POWER_PHANTOM => DUR_PHANTOM_S,
            POWER_HASTE => DUR_HASTE_S,
            POWER_SHIELD => DUR_SHIELD_S,
            POWER_MIDAS => DUR_MIDAS_S,
            POWER_PHOENIX => DUR_PHOENIX_S,
            _ => DUR_REVEAL_S,
        };
        self.power_until_unix = self.unix_now + dur;
        if power == POWER_SHIELD {
            self.aegis_charged = true;
        }
        if power == POWER_PHOENIX {
            self.phoenix_ready = true;
        }
    }

    /// Our 1-based rank on the length leaderboard (desc length, ties id asc).
    fn rank(&self) -> u8 {
        let my_len = self.snake.len as u16;
        let mut rank: u8 = 1;
        for p in self.peers.active() {
            if !p.alive {
                continue;
            }
            if p.length > my_len || (p.length == my_len && p.id < self.id) {
                rank = rank.saturating_add(1);
            }
        }
        rank
    }

    /// Top-3 (id, length) over own + live peers — length desc, ties id asc.
    fn leaderboard_top3(&self) -> ([(u8, u16); 3], usize) {
        const CAP: usize = 1 + PEER_CAP;
        let mut cand = [(0u8, 0u16); CAP];
        let mut nc = 0;
        cand[nc] = (self.id, self.snake.len as u16);
        nc += 1;
        for p in self.peers.active() {
            if p.alive && nc < CAP {
                cand[nc] = (p.id, p.length);
                nc += 1;
            }
        }
        let mut out = [(0u8, 0u16); 3];
        let mut used = [false; CAP];
        let take = nc.min(3);
        for slot in out.iter_mut().take(take) {
            let mut best: Option<usize> = None;
            for (i, &(cid, clen)) in cand[..nc].iter().enumerate() {
                if used[i] {
                    continue;
                }
                match best {
                    None => best = Some(i),
                    Some(b) => {
                        let (bid, blen) = cand[b];
                        if clen > blen || (clen == blen && cid < bid) {
                            best = Some(i);
                        }
                    }
                }
            }
            let bi = best.unwrap_or(0);
            used[bi] = true;
            *slot = cand[bi];
        }
        (out, take)
    }

    // ---- rendering ----------------------------------------------------------

    fn draw_world<D: DrawTarget<Color = Rgb565>>(&self, d: &mut D) {
        let cam = Camera::centered_on(self.snake.head());
        let cell = Size::new((CELL_PX - 2) as u32, (CELL_PX - 2) as u32);

        // Food: small orange-red dots.
        let fbucket = self.unix_now / FOOD_PERIOD_S;
        let mut beacons = [Cell::default(); FOOD_COUNT];
        let n = food_cells(GAME_SEED, fbucket, &mut beacons);
        let food_style = PrimitiveStyle::with_fill(Rgb565::new(31, 20, 0));
        for &c in &beacons[..n] {
            if let Some((px, py)) = cam.world_to_screen(c) {
                let _ = RoundedRectangle::with_equal_corners(
                    Rectangle::new(EgPoint::new(px + 4, py + 4), Size::new(8, 8)),
                    Size::new(4, 4),
                )
                .into_styled(food_style)
                .draw(d);
            }
        }

        // Treasure: a gold plus glyph while unclaimed this bucket.
        let tbucket = self.unix_now / TREASURE_PERIOD_S;
        if self.took_treasure_bucket != Some(tbucket) {
            let (tcell, _) = treasure_at(GAME_SEED, tbucket);
            if let Some((px, py)) = cam.world_to_screen(tcell) {
                let gold = PrimitiveStyle::with_fill(Rgb565::new(31, 55, 4));
                let _ = Rectangle::new(EgPoint::new(px + 6, py), Size::new(4, 16))
                    .into_styled(gold)
                    .draw(d);
                let _ = Rectangle::new(EgPoint::new(px, py + 6), Size::new(16, 4))
                    .into_styled(gold)
                    .draw(d);
            }
        }

        // Peers: colored body (dim) + head (bright) + id label. Phantom peers
        // flicker ~3 Hz.
        let label_style = MonoTextStyle::new(&FONT_6X10, Rgb565::WHITE);
        let mut buf = [Cell::default(); SNAKE_CAP];
        for p in self.peers.active() {
            if !p.alive {
                continue;
            }
            if p.power == POWER_PHANTOM && (self.frame / 4) % 2 == 0 {
                continue;
            }
            let color = peer_color(p.id);
            let dim = Rgb565::new(color.r() / 2, color.g() / 2, color.b() / 2);
            let m = p.body_cells(self.now_ms, &mut buf);
            for (i, &c) in buf[..m].iter().enumerate() {
                if let Some((px, py)) = cam.world_to_screen(c) {
                    let style =
                        PrimitiveStyle::with_fill(if i == 0 { color } else { dim });
                    let _ = RoundedRectangle::with_equal_corners(
                        Rectangle::new(EgPoint::new(px + 1, py + 1), cell),
                        Size::new(3, 3),
                    )
                    .into_styled(style)
                    .draw(d);
                }
            }
            // Id tag floating above the (dead-reckoned) head.
            if let Some((px, py)) = cam.world_to_screen(p.dead_reckon_head(self.now_ms)) {
                let mut tag: heapless::String<8> = heapless::String::new();
                let _ = write!(tag, "{}", p.id);
                let ty = if py - 12 < VIEW_Y { py + CELL_PX + 10 } else { py - 4 };
                let _ = Text::with_baseline(
                    tag.as_str(),
                    EgPoint::new(px + CELL_PX / 2 - 3, ty - 8),
                    label_style,
                    Baseline::Top,
                )
                .draw(d);
            }
        }

        // Self: green body, bright head; phantom flickers, shield gets a halo.
        let phantom = self.active_power() == POWER_PHANTOM;
        if !(phantom && (self.frame / 4) % 2 == 0) {
            let body_style = PrimitiveStyle::with_fill(Rgb565::new(0, 28, 0));
            let head_style = PrimitiveStyle::with_fill(Rgb565::GREEN);
            for (i, c) in self.snake.segments().enumerate() {
                if let Some((px, py)) = cam.world_to_screen(c) {
                    let _ = RoundedRectangle::with_equal_corners(
                        Rectangle::new(EgPoint::new(px + 1, py + 1), cell),
                        Size::new(3, 3),
                    )
                    .into_styled(if i == 0 { head_style } else { body_style })
                    .draw(d);
                    if i == 0 && self.active_power() == POWER_SHIELD {
                        let _ = Rectangle::new(
                            EgPoint::new(px - 1, py - 1),
                            Size::new((CELL_PX + 2) as u32, (CELL_PX + 2) as u32),
                        )
                        .into_styled(PrimitiveStyle::with_stroke(Rgb565::CYAN, 1))
                        .draw(d);
                    }
                }
            }
        }
    }

    fn draw_hud<D: DrawTarget<Color = Rgb565>>(&self, d: &mut D) {
        let mut s: heapless::String<48> = heapless::String::new();
        let p = self.active_power();
        let _ = write!(
            s,
            "#{} L:{} peers:{}",
            self.rank(),
            self.snake.len,
            self.peers.active_count()
        );
        if p != 0 {
            let _ = write!(s, " {}", power_name(p));
        }
        let style = MonoTextStyle::new(&FONT_10X20, Rgb565::CYAN);
        let _ = Text::with_baseline(s.as_str(), EgPoint::new(8, 12), style, Baseline::Top)
            .draw(d);
        if !self.unix_synced {
            // Free-running clock: food buckets differ from the fleet's until
            // mesh time is adopted (transient, by design).
            let warn = MonoTextStyle::new(&FONT_6X10, Rgb565::CSS_GRAY);
            let _ = Text::with_baseline("no mesh time", EgPoint::new(320, 20), warn, Baseline::Top)
                .draw(d);
        }
    }

    fn draw_death<D: DrawTarget<Color = Rgb565>>(&self, d: &mut D) {
        let big = MonoTextStyle::new(&FONT_10X20, Rgb565::RED);
        let _ = Text::with_alignment(
            "DEAD - tap to respawn",
            EgPoint::new(205, 180),
            big,
            Alignment::Center,
        )
        .draw(d);
        let style = MonoTextStyle::new(&FONT_10X20, Rgb565::WHITE);
        let (top, n) = self.leaderboard_top3();
        for (i, &(id, len)) in top[..n].iter().enumerate() {
            let mut line: heapless::String<32> = heapless::String::new();
            let me = if id == self.id { "*" } else { " " };
            let _ = write!(line, "{}.{}id{:03}  L:{}", i + 1, me, id, len);
            let _ = Text::with_alignment(
                line.as_str(),
                EgPoint::new(205, 240 + i as i32 * 30),
                style,
                Alignment::Center,
            )
            .draw(d);
        }
    }
}

impl App for WorldSnakeApp {
    fn name(&self) -> &str {
        "World Snake"
    }

    fn setup(&mut self) {
        // Fresh spawn; clocks, tick counter and peer table survive so the
        // wire stream stays monotonic and known peers reappear instantly.
        self.respawn(START_LEN);
        self.eaten = EatenSet::new();
        self.took_treasure_bucket = None;
    }

    fn update(&mut self, input: &AppInput) -> AppResult {
        // Advance the internal clocks.
        self.now_ms = self.now_ms.wrapping_add(input.dt_ms);
        self.unix_frac_ms += input.dt_ms;
        self.unix_now += self.unix_frac_ms / 1000;
        self.unix_frac_ms %= 1000;
        self.frame = self.frame.wrapping_add(1);

        // Input: swipes steer; a tap respawns when dead.
        if let Some(swipe) = input.swipe {
            if !self.dead {
                self.steer(swipe);
            }
        }
        if self.dead && input.tap {
            self.respawn(START_LEN);
        }

        // Despawn stale peers (single-tier).
        self.peers.prune(self.now_ms, PEER_STALE_MS);

        if self.dead {
            return AppResult::Continue;
        }

        // Movement step at STEP_MS (or Zephyr Haste).
        let step_ms = if self.active_power() == POWER_HASTE {
            HASTE_STEP_MS
        } else {
            STEP_MS
        };
        if self.now_ms.saturating_sub(self.last_step_ms) < step_ms {
            return AppResult::Continue;
        }
        self.last_step_ms = self.now_ms;

        // While WE are phantom we phase through everything (own body + peers);
        // otherwise non-phantom live peer bodies block us.
        let phantom = self.active_power() == POWER_PHANTOM;
        let outcome = if phantom {
            self.snake.step(|_| false, true)
        } else {
            let peers = &self.peers;
            let now_ms = self.now_ms;
            self.snake.step(|c| peer_body_hits(peers, c, now_ms), false)
        };

        match outcome {
            StepOutcome::Moved => {
                self.handle_pickups();
                if self.power != 0 && self.unix_now >= self.power_until_unix {
                    self.power = 0;
                }
            }
            StepOutcome::Blocked => {
                if !phantom {
                    if self.aegis_charged {
                        // Aegis Ward absorbs one lethal hit.
                        self.aegis_charged = false;
                        self.power = 0;
                    } else if self.phoenix_ready {
                        // Phoenix Ember: instant respawn keeping length.
                        let keep = self.snake.len;
                        self.respawn(keep);
                    } else {
                        self.dead = true;
                    }
                }
            }
        }

        AppResult::Continue
    }

    // Remote peers dead-reckon between our steps, so repaint on a steady 30fps
    // cadence rather than only on local steps (matches the old arm's next_flush).
    fn min_flush_ms(&self) -> u32 {
        33
    }

    fn render(&self, d: &mut Framebuffer) {
        let _ = Rectangle::new(EgPoint::zero(), Size::new(SCREEN_W as u32, SCREEN_H as u32))
            .into_styled(PrimitiveStyle::with_fill(Rgb565::BLACK))
            .draw(d);
        // Subtle frame around the world window.
        let _ = Rectangle::new(
            EgPoint::new(VIEW_X - 2, VIEW_Y - 2),
            Size::new(
                (VIEW_COLS as i32 * CELL_PX + 4) as u32,
                (VIEW_ROWS as i32 * CELL_PX + 4) as u32,
            ),
        )
        .into_styled(PrimitiveStyle::with_stroke(Rgb565::new(4, 8, 4), 1))
        .draw(d);

        if self.dead {
            self.draw_hud(d);
            self.draw_death(d);
            return;
        }
        self.draw_world(d);
        self.draw_hud(d);
    }
}

/// True if `c` lies on any NON-phantom live peer's dead-reckoned body
/// (dead or phantom peers are non-lethal).
fn peer_body_hits(peers: &PeerTable, c: Cell, now_ms: u32) -> bool {
    let mut buf = [Cell::default(); SNAKE_CAP];
    for p in peers.active() {
        if !p.alive || p.power == POWER_PHANTOM {
            continue;
        }
        let m = p.body_cells(now_ms, &mut buf);
        if buf[..m].contains(&c) {
            return true;
        }
    }
    false
}
