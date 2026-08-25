//! #21/#56 keyed-CFG relay SCHEDULING — the pure decisions behind "does a config the
//! dashboard set actually reach the leaf it names".
//!
//! Extracted from `mode::RadioManager::broadcast_cached_configs` + `wifi::CfgCache` because
//! both had a failure mode that only a whole-fleet observation could see, and that no
//! observation on the board could: a config that is silently never relayed looks exactly
//! like a config that was relayed and applied, because nothing anywhere counts either one.
//!
//! ## The two bugs this module exists to make impossible
//!
//! **1. Tail starvation.** The relay walked the cache from index 0 every tick and emitted
//! every entry back-to-back. `CfgCache` is APPEND-ordered and never reorders, so a config
//! published *today* is always the LAST frame of the burst — and the head of the burst was
//! occupied by entries for node ids that are not boards any more. Whatever truncates a
//! 19-frame unspaced ESP-NOW burst therefore truncated the SAME entries every 10 s, forever.
//! A rotating cursor makes starvation structurally impossible: every slot is visited within
//! [`ticks_to_cover`] ticks no matter how few frames a given tick gets onto the air.
//!
//! **2. Cap overflow with no eviction.** `CfgCache::set` dropped a NEW `(id, key)` when full
//! and never evicted, so membership froze at whatever the first 16 arrivals were. Worse, a
//! retained config that is DELETED arrives as an empty payload and was cached as a permanent
//! empty-valued ghost — so merely *testing* a config burned a slot for good. Measured on the
//! live fleet 2026-07-28: 19 distinct `(id, key)` pairs wanted 16 slots. [`evict_slot`] lets a
//! real value reclaim a slot from a redundant clear, which is the correct priority — an empty
//! value means "keep current / board default", which is what a leaf that never hears it does
//! anyway, so a clear is the one entry that is safe to forget.
//!
//! Raising `CFG_CACHE_CAP` is deliberately NOT the fix: `vals` is
//! `[[u8; CFG_VALUE_MAX]; CAP]`, so +16 slots is ~1.3 KB of `.bss`, and esp-hal shrinks
//! `.stack` silently as `.bss` grows — the stack floor has ~2.2 KB of headroom, and spending
//! most of it here to paper over an eviction bug would trade a config bug for a stack bug.
//!
//! PURE: no esp-hal, no alloc, no HAL. Host-tested verbatim by
//! `experiments/cfg_relay_verify` (`#[path]`-include, like `wire`/`coexist`/`flood`).

/// Keyed-CFG frames emitted per relay tick (the ~10 s cadence in `main`).
///
/// The point is NOT airtime — 19 × ~80 B per 10 s is nothing. The point is that a bounded,
/// spaced emission cannot have a permanently-unreached tail: with a rotating cursor, coverage
/// is guaranteed even if the radio only ever gets the first frame or two of a tick out. 4 per
/// tick covers a full 16-slot cache in 4 ticks (~40 s), well inside the tolerance of an
/// edge-triggered, idempotent apply on the leaf side.
pub const CFG_RELAY_PER_TICK: usize = 4;

/// Upper bound on frames in ONE relay tick — i.e. the size of the caller's index buffer, and the
/// budget a PRIMING tick is allowed (see [`RelayCursor::take`]).
///
/// MUST be >= the `cfg_cache` capacity so a priming sweep can name every slot. A compile-time
/// assertion in `wifi.rs` ties the two together, because a silent mismatch here would reintroduce
/// exactly the starvation this module exists to prevent — the tail of the cache would become
/// unreachable on a priming tick and only the cursor would ever get to it.
pub const CFG_RELAY_MAX_BURST: usize = 16;

/// Ticks needed to visit every one of `count` slots at least once **in steady state** — the bound
/// `experiments/cfg_relay_verify` asserts against, and the anti-starvation contract.
///
/// Deliberately ignores priming (which covers everything in one tick): this is the WORST case, and
/// a bound that assumed the optimisation would be wrong exactly when the optimisation fails, which
/// is the only time anybody reads it.
///
/// `#[allow(dead_code)]` because this is the HOST-TEST half of the API: `cfg_relay_verify`
/// `#[path]`-includes this module and asserts the anti-starvation contract against this bound, but
/// the firmware binary only ever *cites* it (see the `broadcast_cached_configs` comment in
/// `mode.rs`), so a binary-crate build sees it as unused. Same precedent as `net::{ledger, treehead,
/// sth}`. The allow is on the ITEM, not the module, so the rest of `cfgsched` stays under `-D
/// warnings` — deleting it instead would delete the test's bound, which is the property being proven.
#[allow(dead_code)]
pub const fn ticks_to_cover(count: usize) -> usize {
    if count == 0 {
        0
    } else {
        count.div_ceil(CFG_RELAY_PER_TICK)
    }
}

/// A rotating cursor over the crown's `cfg_cache` slots.
///
/// Holds only "where the last tick stopped". Deliberately stores an INDEX and not an
/// `(id, key)`: the cache can shrink or be refilled in a different order under a crown
/// handover, and an index that is merely stale wraps harmlessly on the next tick, whereas a
/// remembered key that has vanished would need a search that could fail.
pub struct RelayCursor {
    next: usize,
    /// The `count` seen on the previous tick. GROWTH is the trigger for a priming sweep — see
    /// [`RelayCursor::take`]. Held rather than a bare `bool` so the trigger covers both "fresh
    /// crown" (0 → N) and "somebody just added a control" (N → N+1) with one rule.
    seen_count: usize,
}

impl Default for RelayCursor {
    fn default() -> Self {
        Self::new()
    }
}

impl RelayCursor {
    pub const fn new() -> Self {
        Self { next: 0, seen_count: 0 }
    }

    /// Fill `out` with the slot indices to relay this tick and return how many were written,
    /// advancing the cursor past them.
    ///
    /// Emits [`CFG_RELAY_PER_TICK`] in steady state, or a full sweep of up to
    /// [`CFG_RELAY_MAX_BURST`] on a **priming** tick.
    ///
    /// ## Priming: the cache GREW since the last tick
    ///
    /// Two situations want the same thing — everything on the air now, not in N ticks' time:
    ///
    /// * **A fresh crown after a handover (0 → N).** A new crown's `cfg_cache` starts EMPTY and
    ///   repopulates from the retained drain, so until it has been swept once, *no* leaf config is
    ///   reachable at all. This is not hypothetical: measured live 2026-07-28, a board took the
    ///   crown at `up=16 s` reporting `cfgq=0/16`. With tenure flapping on a minutes timescale the
    ///   handover is the COMMON case, not the exception, and it plausibly dominated the observed
    ///   3–5.5 min config latency far more than any missed 10 s tick did.
    /// * **Somebody just added a control (N → N+1).** `CfgCache` appends, so the new entry is LAST
    ///   — under a pure cursor the one config a person is actually waiting on waits a full rotation.
    ///
    /// Growth is the trigger for both, which is why `seen_count` is a count and not a flag.
    ///
    /// A priming tick reintroduces one long back-to-back burst, which is what the rotating cursor
    /// was added to stop relying on — deliberately. The combination is strictly better than either
    /// alone: if the priming burst truncates, the cursor still reaches everything it missed on
    /// subsequent ticks, so the burst is an OPTIMISATION and never the delivery guarantee. The
    /// anti-starvation contract still rests entirely on the cursor.
    ///
    /// Total and panic-free: `count == 0` writes nothing and re-arms priming (so a crown that loses
    /// and regains the role primes again); a cursor left beyond a shrunken `count` wraps to 0; a
    /// `count` below the budget yields each slot exactly once (never the same slot twice in one
    /// tick, which would waste budget on a duplicate while another slot went unvisited).
    pub fn take(&mut self, count: usize, out: &mut [usize; CFG_RELAY_MAX_BURST]) -> usize {
        if count == 0 {
            self.next = 0;
            self.seen_count = 0; // re-arm priming: an empty cache means a fresh (or demoted) crown
            return 0;
        }
        let prime = count > self.seen_count;
        self.seen_count = count;
        if self.next >= count {
            self.next = 0; // cache shrank (or first call) — resume from the top
        }
        let budget = if prime { CFG_RELAY_MAX_BURST } else { CFG_RELAY_PER_TICK };
        let n = count.min(budget);
        for (k, slot) in out.iter_mut().enumerate().take(n) {
            *slot = (self.next + k) % count;
        }
        self.next = (self.next + n) % count;
        n
    }

    /// The next slot index this cursor will emit (test/observability only).
    ///
    /// `#[allow(dead_code)]` for the same reason as [`ticks_to_cover`]: `cfg_relay_verify` reads it
    /// to prove the cursor ADVANCES between ticks and resets on an empty cache — a property with no
    /// firmware caller by design (the firmware just calls `take`). Item-scoped, not module-scoped.
    #[allow(dead_code)]
    pub fn peek(&self) -> usize {
        self.next
    }
}

/// Which slot a FULL cache may reuse for a new `(id, key)`: the first slot holding an EMPTY
/// value, or `None` if every slot carries a real config (the caller then drops and COUNTS it
/// — a dropped config must never be signalled by a log line alone, because release images are
/// serial-silent and the log is therefore not observability).
///
/// `lens` is the cache's per-slot value length, in slot order.
pub fn evict_slot(lens: &[u8]) -> Option<usize> {
    lens.iter().position(|&l| l == 0)
}
