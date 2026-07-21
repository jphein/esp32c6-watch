#![no_std]
//! Pure diagnostics model for the on-watch **Diag** screen.
//!
//! No hardware here — the firmware reads the live values (heap via
//! `esp_alloc::HEAP.stats()`, stack gap via the `_stack_start`/`_stack_end`
//! linker symbols, die temp via `esp_hal::tsens`, uptime, RSSI, battery) and the
//! git SHA / build date via `env!(GIT_SHA)` / `env!(BUILD_DATE)` (baked by
//! build.rs), then fills [`DiagMetrics`] + builds a [`Sigil`] each tick. This
//! crate is the host-testable core: the metrics struct, the capability bitmask,
//! the FPS smoother, and the inline realm-words sigil.

// ============================================================================
// Capabilities — on/off dots, in display order.
// ============================================================================

/// Capability flags for the Diag "capabilities" dot row. Bit order == the order
/// the dots render. The firmware sets each from its live subsystem state
/// (wifi_connected, ble_on, mesh_up, mic RECORDING-path ready, imu ok, …).
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Caps(pub u16);

impl Caps {
    pub const WIFI: u16 = 1 << 0;
    pub const BLE: u16 = 1 << 1;
    pub const MESH: u16 = 1 << 2;
    pub const MIC: u16 = 1 << 3;
    pub const IMU: u16 = 1 << 4;
    pub const RTC: u16 = 1 << 5;
    pub const TOUCH: u16 = 1 << 6;
    pub const LP_CORE: u16 = 1 << 7;
    pub const DISPLAY: u16 = 1 << 8;
    pub const RADIO: u16 = 1 << 9;

    /// Number of defined capabilities (dots to render).
    pub const COUNT: usize = 10;

    pub const fn new() -> Self {
        Self(0)
    }

    /// Set/clear one capability bit.
    pub fn set(&mut self, bit: u16, on: bool) {
        if on {
            self.0 |= bit;
        } else {
            self.0 &= !bit;
        }
    }

    /// Is a capability on?
    pub const fn has(&self, bit: u16) -> bool {
        self.0 & bit != 0
    }
}

// ============================================================================
// FPS smoother — computed in the render loop, displayed on Diag.
// ============================================================================

/// Exponential-moving-average FPS. The render loop counts frames over an
/// interval and feeds `(frames, dt_ms)`; the smoothed value avoids the jitter a
/// raw per-interval count shows. Seeds to the first sample so it reads true
/// immediately instead of ramping from zero.
#[derive(Clone, Copy, Default, Debug)]
pub struct Fps {
    ewma: f32,
    seeded: bool,
}

impl Fps {
    /// EMA weight of each new sample (0..1). 0.3 = responsive but steady.
    const ALPHA: f32 = 0.3;

    pub const fn new() -> Self {
        Self { ewma: 0.0, seeded: false }
    }

    /// Fold in `frames` rendered over the last `dt_ms` and return the smoothed
    /// fps. A zero interval is ignored (returns the current value) so a
    /// double-call in the same millisecond can't divide by zero or spike.
    pub fn update(&mut self, frames: u32, dt_ms: u32) -> f32 {
        if dt_ms == 0 {
            return self.ewma;
        }
        let inst = frames as f32 * 1000.0 / dt_ms as f32;
        if self.seeded {
            self.ewma = self.ewma * (1.0 - Self::ALPHA) + inst * Self::ALPHA;
        } else {
            self.ewma = inst;
            self.seeded = true;
        }
        self.ewma
    }

    pub const fn value(&self) -> f32 {
        self.ewma
    }
}

// ============================================================================
// Metrics — the struct the shell fills each tick and pushes to diag.slint.
// ============================================================================

/// Live diagnostics snapshot. All numeric so the slint side formats it (the
/// crate stays alloc-free). `die_temp_dc` is deci-degrees C (tenths), matching
/// the shell's existing `set_die_temp` contract. `stack_gap` is the #59 metric:
/// `_stack_start - _stack_end` — the leftover stack under RAM-top that silent
/// `.bss` growth steals.
#[derive(Clone, Copy, Default, Debug)]
pub struct DiagMetrics {
    pub heap_main_free: u32, // bytes, esp_alloc main region
    pub heap_recl_free: u32, // bytes, reclaimed region
    pub stack_gap: u32,      // bytes (the #59 stack-floor metric)
    pub cpu_mhz: u16,
    pub die_temp_dc: i16, // deci-°C (e.g. 425 = 42.5 °C)
    pub uptime_s: u32,
    pub rssi_dbm: i16, // 0 = n/a (not associated)
    pub batt_pct: u8,
    pub batt_mv: u16,
    pub fps: f32,
    pub caps: Caps,
}

// ============================================================================
// Sigil — version header (git SHA + build date + inline realm words).
// ============================================================================

/// realm-sigil "inline realm words" for embedded (no runtime service): a
/// deterministic, memorable (adjective, noun) pair derived from the build id.
/// 16×16 = 256 distinct sigils — plenty to tell two builds apart on-glass.
const REALM_ADJ: [&str; 16] = [
    "astral", "lucid", "umbral", "gilded", "hollow", "verdant", "crimson", "cobalt", "silent",
    "ember", "frost", "runic", "zephyr", "obsidian", "aurora", "liminal",
];
const REALM_NOUN: [&str; 16] = [
    "sigil", "warden", "lantern", "cipher", "beacon", "oracle", "phantom", "haven", "spire",
    "relay", "comet", "glyph", "tide", "forge", "echo", "vale",
];

/// Deterministic (adjective, noun) for a build id. Total (never panics): masks
/// into the 16-entry tables.
pub fn realm_words(build_id: u32) -> (&'static str, &'static str) {
    (
        REALM_ADJ[(build_id & 0xF) as usize],
        REALM_NOUN[((build_id >> 4) & 0xF) as usize],
    )
}

/// Fold a short git SHA (or any string) into a stable u32 build id (FNV-ish).
/// Same SHA → same id → same realm words across boots.
pub fn build_id(sha: &str) -> u32 {
    let mut id: u32 = 2166136261; // FNV offset basis
    for b in sha.as_bytes() {
        id ^= *b as u32;
        id = id.wrapping_mul(16777619); // FNV prime
    }
    id
}

/// The Diag version header. Borrows the compile-time strings the firmware passes
/// from `env!` (no alloc). `words` is computed once from the SHA.
#[derive(Clone, Copy, Debug)]
pub struct Sigil<'a> {
    pub version: &'a str, // CARGO_PKG_VERSION, e.g. "0.5.1"
    pub sha: &'a str,     // short git SHA, e.g. "2a375e9"
    pub date: &'a str,    // build/commit date, e.g. "2026-07-21"
    pub adj: &'static str,
    pub noun: &'static str,
}

impl<'a> Sigil<'a> {
    /// Build from the firmware's compile-time version/SHA/date. Realm words are
    /// derived from the SHA so they're stable per build.
    pub fn new(version: &'a str, sha: &'a str, date: &'a str) -> Self {
        let (adj, noun) = realm_words(build_id(sha));
        Self { version, sha, date, adj, noun }
    }
}
