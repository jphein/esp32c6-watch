//! #164 — the PURE per-peer link-quality (ETX-style) metric.
//!
//! ## What this is
//! smol has no quantitative link-quality signal today: multi-hop escalation
//! ([`super::flood::HopLatch`]) is binary (stranded / not), peer eviction (#28/#86) ranks on
//! raw RSSI, and channel parking (#126) keys on "got a signal." RSSI is *signal strength*;
//! it does not measure *delivery*. This module distills a peer's HELLO-reception history into
//! an **ETX** (Expected-Transmission-Count-style) **cost** — the complementary *quality* axis
//! that #155 (link-quality-aware channel selection) and #165 (best-relay) actually need.
//!
//! Ported from babeld's `neighbour.c` (MIT), the reference implementation studied for #163
//! (`docs/superpowers/research/althea-babel-study.md`): a per-neighbour 16-bit **reach**
//! shift-register of recent Hello reception, smoothed into an inverse-reachability cost with
//! **recency weighting** (the two most-recent slots count for more). This is the one piece the
//! study ranked ADOPT — small, `no_std`-trivial (a `u16` + integer shifts/divide, no FPU), and
//! host-testable like [`super::flood`].
//!
//! ## The pure/driver split (the #123 lesson)
//! This module is the **pure brain** — no `esp-hal`/`esp-wifi`, no time, no I/O. The caller
//! (the `Roster` in `net/mode.rs`) owns the *wiring*: it sets a per-peer "heard a HELLO this
//! interval" flag in the HELLO service arm and calls [`LinkQuality::tick`] once per HELLO
//! cadence. Keeping the wiring to "set a flag + tick" makes the host tests
//! (`experiments/etx_verify`) the trigger-wiring coverage too — the #123 lesson: last campaign
//! only the [`HopLatch`](super::flood::HopLatch) *math* was host-tested, not the wiring that
//! drove it, so an on-air trigger bug slipped past green builds.
//!
//! ## v1 is one-way (rxcost)
//! [`LinkQuality`] measures *our* reception of a peer's HELLOs — babeld's `rxcost`. babeld's
//! full link cost also folds in `txcost` (how well the peer hears *us*, echoed back in an IHU
//! TLV) to defeat asymmetric links. That two-way refinement needs a wire change and is a
//! deliberate follow-up (noted on #164); v1 delivers the foundational one-way signal with
//! **no new frame** — it reads the HELLOs smol already broadcasts.

/// Cost of a link we have not heard from within the register's window — the "unreachable"
/// sentinel. Higher cost = worse link; a real (heard) link is `0..=253`, so `254` is unused and
/// `255` is unambiguously "no recent HELLOs" (mirrors babeld's `INFINITY` retraction marker).
pub const INFINITY: u8 = 255;

/// Weight mask for the reach register's low 14 bits when smoothing (babeld `neighbour.c`).
const SREACH_LOW_MASK: u16 = 0x3FFF;
/// Maximum smoothed-reachability value (`reach == 0xFFFF`): `(0x8000>>2) + (0x4000>>1) + 0x3FFF`.
const SREACH_MAX: u32 = 0x7FFF;

/// One peer's link quality: a 16-bit **reach** shift-register of recent HELLO reception, MSB =
/// most-recent interval. `Copy` so it lives inline in a `.bss` roster `Node` (no heap).
///
/// Drive it once per HELLO cadence with [`tick`](LinkQuality::tick): `heard = true` if at least
/// one HELLO arrived from this peer since the last tick, else `false`. Read the derived
/// [`cost`](LinkQuality::cost) (0 = perfect … 253 = very lossy … [`INFINITY`] = unheard).
///
/// v1 exposes only `cost()` — babeld's `two_three()` up/down heuristic and a raw-register
/// accessor are deliberately omitted (YAGNI + the crate's no-dead-code `-D warnings` bar); add
/// them alongside the first consumer (#126 parking / #165 best-relay).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LinkQuality {
    /// Hello-reception history: bit 15 (MSB) = the most recent interval, bit 0 = oldest. A heard
    /// interval shifts in a `1` at the top; a missed interval shifts in a `0` (decay). `0` = no
    /// HELLO anywhere in the last 16 intervals.
    reach: u16,
}

impl LinkQuality {
    /// A never-heard peer (empty register → [`INFINITY`] cost). `const` so a roster of these
    /// initializes in `.bss` with no runtime work.
    pub const fn new() -> Self {
        Self { reach: 0 }
    }

    /// Advance one HELLO interval. `heard` = at least one HELLO arrived from this peer since the
    /// last tick. Shifts the window right (aging every slot) and sets the most-recent bit iff
    /// heard — exactly babeld's `reach >>= 1; if heard { reach |= 0x8000 }`.
    pub fn tick(&mut self, heard: bool) {
        self.reach >>= 1;
        if heard {
            self.reach |= 0x8000;
        }
    }

    /// The ETX-style link cost, `0..=253` for a heard link (0 = every recent HELLO arrived) or
    /// [`INFINITY`] (255) if nothing was heard in the window. **Recency-weighted**: the two most
    /// recent intervals count for more, so a link that just went quiet becomes expensive fast
    /// (and a link that just came back cheap fast) — this is what makes it useful for channel /
    /// relay decisions rather than a lagging average.
    ///
    /// Faithful to babeld's `neighbour_rxcost`: it smooths the register into
    /// `sreach ∈ [0, 0x7FFF]` (weighting the top two bits), where higher = more reachable, then
    /// maps to an inverse cost. babeld computes `cost = 0x8000*base/(sreach+1)` on an open-ended
    /// scale; smol instead maps `sreach` linearly onto `0..=253` so the cost fits a single byte
    /// for the DIAG record + roster and leaves `255` free as the unambiguous unreachable marker.
    pub fn cost(&self) -> u8 {
        if self.reach == 0 {
            return INFINITY;
        }
        // babeld's recency-weighted smoothed reachability: top bit ×2, next bit ×1, rest ×1.
        // Range 1..=0x7FFF (reach != 0 here ⇒ sreach >= 1).
        let sreach = ((self.reach & 0x8000) >> 2) as u32
            + ((self.reach & 0x4000) >> 1) as u32
            + (self.reach & SREACH_LOW_MASK) as u32;
        // Map reachability → cost: sreach = MAX ⇒ 0 (perfect); sreach = 1 ⇒ 253 (very lossy).
        let scaled = (sreach * 253) / SREACH_MAX; // 0..=253
        (253 - scaled) as u8
    }
}

impl Default for LinkQuality {
    fn default() -> Self {
        Self::new()
    }
}
