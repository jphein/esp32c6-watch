//! Heap allocation *attribution* (#75), opt-in via the `heap-hooks` feature.
//!
//! # ⚠️ It counts allocation EVENTS, not LIVE BLOCKS
//!
//! Read `net` for a persistent-cost question and the bucket counts for a churn
//! question. Confusing the two invalidated a pre-committed experiment on
//! 2026-07-29: the prediction was "~166 blocks in bucket 0 if the cost is one 16 B
//! tracker per rendered item", and bucket 0 came back at **1,493-1,715** against an
//! idle baseline of 13 per 2 s — not because 1,700 trackers were live, but because
//! Slint's `properties.rs:461` allocates one 16 B block per property a live binding
//! READS and throws the whole list away on every re-evaluation. The signal was
//! there and drowned. `net` bytes survived that churn (10,800 B, matching an
//! independent region-delta measurement to 0.6 %); the counts did not.
//!
//! Corollary: a bucket count is only interpretable against a measured baseline for
//! the same screen and the same duration, and the render loop's baseline is not
//! small — watchface idle churns 603 allocations / 42,564 B in ~2 s at `net=0`.
//! Allocation-heavy and perfectly leak-free, which is itself worth knowing: a
//! persistent cost shows up as a large positive `net` against a zero-net background.
//!
//! # What this answers that nothing else can
//!
//! `HEAP.free()` tells you how much is left. `harvest_free` (the sibling
//! `heap-forensics` feature in main.rs) tells you whether that number is
//! *honest*. Neither tells you **who took the bytes** — and two questions on
//! #75 could not be closed from source alone:
//!
//! 1. **The ~44 KB blob split.** `esp_radio::wifi::new()` consumes far more
//!    than the ~16 KB of static RX buffers that esp-radio documents
//!    (`static_rx_buf_num` = 10 x ~1.6 KB). The rest is inside the closed WiFi
//!    blob — supplicant, net80211/pp control blocks, PHY — and is not
//!    derivable by reading Rust.
//! 2. **The dependency-node count.** Slint's `properties.rs:461` performs one
//!    separate 16-byte `alloc()` per property that a live binding *reads*, and
//!    throws the whole list away and rebuilds it on every re-evaluation
//!    (`properties.rs:745`). The census could bound this only as ">= 238,
//!    plausibly 500-1000", which is the leading candidate for the ~15.7 KB of
//!    boot-time heap it could not attribute.
//!
//! Both are answerable because **every** allocation on this target funnels
//! through the two hooked functions, including the blob's: `malloc_with_caps`
//! -> `HEAP.alloc_caps` and `free` -> `HEAP.dealloc`
//! (`esp-alloc-0.10.0/src/malloc.rs:11,18,33,42`), which are exactly where
//! esp-alloc fires `_esp_alloc_alloc` / `_esp_alloc_dealloc`
//! (`lib.rs:649,658,667,670`). C-side and Rust-side allocations are both
//! visible.
//!
//! # The hard constraint: this code runs inside the allocator
//!
//! The hooks fire on EVERY alloc and dealloc, from every context — task code,
//! interrupt handlers, critical sections, and the WiFi blob. Therefore:
//!
//! * **It must never allocate.** An allocating hook re-enters the allocator and
//!   recurses until the stack dies. No `format!`, no `println!`, no `Vec`.
//! * **It must never lock.** A mutex here can deadlock against an allocation
//!   made while that same lock is held.
//! * **It must be cheap.** Relaxed atomics only — no ordering guarantees are
//!   needed because every counter is independent and is only read at a
//!   quiescent probe point.
//!
//! Reporting therefore happens *outside* the hook, via [`report`], which is
//! called from ordinary task context.
//!
//! # Cost, and why it is gated
//!
//! `.bss` on this target is not free: the stack is the leftover gap under RAM
//! top (`stack = _stack_start - _bss_end`), so every static byte steals stack
//! against the 71,680 B floor asserted at boot. This module's entire footprint
//! is [`Counters`] — 2 x `usize` + 2 x `u32` + 12 x `u32` = 16 four-byte words
//! = **64 bytes** of `.bss` —
//! and with the feature off it compiles to nothing at all. A default build pays
//! literally zero bytes and zero cycles.
//!
//! Unlike `heap-forensics`, this feature does **not** perturb what it measures:
//! it only counts. It would be safe to ship enabled; it is off by default
//! because 64 B is not worth spending on a question already answered. (The
//! `Snapshot` marks are a separate ~192 B of STACK, which is a different budget
//! from `.bss` and the one preflight's margin column actually measures.)

use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering::Relaxed};

/// Size-class buckets. Index `i` counts allocations of `16 << i` bytes or less,
/// with the final bucket catching everything larger.
///
/// Bucket 0 (`<= 16 B`) is the one that matters for the Slint question: a
/// dependency node is exactly 16 bytes, so the bucket-0 delta across scene
/// construction is a direct read on the node count.
const NBUCKETS: usize = 12;

/// Human labels for [`NBUCKETS`], used only by [`report`].
const BUCKET_LABELS: [&str; NBUCKETS] = [
    "<=16", "<=32", "<=64", "<=128", "<=256", "<=512", "<=1K", "<=2K", "<=4K", "<=8K", "<=16K",
    ">16K",
];

/// Map an allocation size to its bucket index. Branch-free-ish and tiny; this
/// runs on the allocator hot path.
#[inline(always)]
fn bucket_of(size: usize) -> usize {
    let mut i = 0;
    let mut limit = 16usize;
    while i < NBUCKETS - 1 {
        if size <= limit {
            return i;
        }
        limit <<= 1;
        i += 1;
    }
    NBUCKETS - 1
}

/// The whole instrument: 64 bytes of `.bss`.
struct Counters {
    alloc_bytes: AtomicUsize,
    alloc_count: AtomicU32,
    free_bytes: AtomicUsize,
    free_count: AtomicU32,
    /// Allocations by size class. Deallocations are deliberately NOT bucketed —
    /// the interesting quantity is what was *taken*, and halving the hot-path
    /// work on `free` matters because frees outnumber allocs during teardown.
    buckets: [AtomicU32; NBUCKETS],
}

static C: Counters = Counters {
    alloc_bytes: AtomicUsize::new(0),
    alloc_count: AtomicU32::new(0),
    free_bytes: AtomicUsize::new(0),
    free_count: AtomicU32::new(0),
    #[allow(clippy::declare_interior_mutable_const)]
    buckets: [const { AtomicU32::new(0) }; NBUCKETS],
};

/// A point-in-time copy of the counters. Plain integers — safe to subtract.
///
/// `Copy` and 60-odd bytes, so callers keep these as ordinary locals on the
/// stack rather than adding more `.bss`.
#[derive(Clone, Copy)]
pub struct Snapshot {
    pub alloc_bytes: usize,
    pub alloc_count: u32,
    pub free_bytes: usize,
    pub free_count: u32,
    pub buckets: [u32; NBUCKETS],
}

impl Snapshot {
    /// Net bytes still held from the allocations counted between two snapshots.
    ///
    /// Signed because a bracketed region can legitimately free more than it
    /// allocates (a teardown), and silently saturating that to zero would hide
    /// exactly the case worth seeing.
    pub fn net_bytes(&self) -> i64 {
        self.alloc_bytes as i64 - self.free_bytes as i64
    }
}

/// Read the counters. Non-allocating; safe to call from task context.
pub fn snapshot() -> Snapshot {
    let mut buckets = [0u32; NBUCKETS];
    for (i, b) in buckets.iter_mut().enumerate() {
        *b = C.buckets[i].load(Relaxed);
    }
    Snapshot {
        alloc_bytes: C.alloc_bytes.load(Relaxed),
        alloc_count: C.alloc_count.load(Relaxed),
        free_bytes: C.free_bytes.load(Relaxed),
        free_count: C.free_count.load(Relaxed),
        buckets,
    }
}

/// Print what happened between `since` and now, tagged.
///
/// This is the readout that answers both open questions:
///
/// * Bracket `esp_radio::wifi::new()` and `BleConnector::new()` -> `net` is the
///   real per-stack permanent cost, and the bucket histogram shows the shape of
///   the blob's allocations (a handful of ~1.6 KB RX buffers vs. a long tail of
///   small control blocks).
/// * Bracket `ShellUi::new()` -> the `<=16` bucket delta is the Slint
///   dependency-node count, directly.
///
/// Called from ordinary task context, never from inside a hook.
pub fn report(tag: &str, since: &Snapshot) {
    let now = snapshot();
    esp_println::println!(
        "[HOOKS] {}: alloc={}B/{}  free={}B/{}  net={}B",
        tag,
        now.alloc_bytes.wrapping_sub(since.alloc_bytes),
        now.alloc_count.wrapping_sub(since.alloc_count),
        now.free_bytes.wrapping_sub(since.free_bytes),
        now.free_count.wrapping_sub(since.free_count),
        now.net_bytes() - since.net_bytes(),
    );
    // Histogram on its own line: a bucket is printed only when it moved, so the
    // common case is short and the interesting case is obvious.
    esp_println::print!("[HOOKS] {}: sizes", tag);
    for i in 0..NBUCKETS {
        let d = now.buckets[i].wrapping_sub(since.buckets[i]);
        if d != 0 {
            esp_println::print!(" {}={}", BUCKET_LABELS[i], d);
        }
    }
    esp_println::println!();
}

// === the hooks themselves =================================================
//
// esp-alloc declares these as `unsafe extern "Rust"` when its `alloc-hooks`
// feature is on (`esp-alloc-0.10.0/src/lib.rs:172-176`); enabling the feature
// without defining both symbols is a link error, which is why the two live
// here and not behind any further conditional.
//
// Signatures must match esp-alloc's declaration exactly. `EnumSet` comes from
// esp-alloc's own re-export (`esp_alloc::export::enumset`) so this costs no
// new dependency.

/// Allocation hook. Runs inside the allocator — see the module docs for why
/// this may not allocate, lock, or print.
///
/// `_ptr` is unused: attribution here is by size class, not address. Tracking
/// live blocks by address would need a map, and a map would allocate.
#[unsafe(no_mangle)]
pub fn _esp_alloc_alloc(
    _heap: &esp_alloc::EspHeap,
    _caps: esp_alloc::export::enumset::EnumSet<esp_alloc::MemoryCapability>,
    _ptr: usize,
    size: usize,
) {
    C.alloc_bytes.fetch_add(size, Relaxed);
    C.alloc_count.fetch_add(1, Relaxed);
    C.buckets[bucket_of(size)].fetch_add(1, Relaxed);
}

/// Deallocation hook. Same constraints as [`_esp_alloc_alloc`].
#[unsafe(no_mangle)]
pub fn _esp_alloc_dealloc(_heap: &esp_alloc::EspHeap, _ptr: usize, size: usize) {
    C.free_bytes.fetch_add(size, Relaxed);
    C.free_count.fetch_add(1, Relaxed);
}
