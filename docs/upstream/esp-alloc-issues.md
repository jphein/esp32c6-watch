# Upstream defects in `esp-alloc` 0.10.0

Two defects found by source reading while investigating our own OOM issue (#75), then
verified independently against the crates.io registry source. Neither is caused by our
project; both are upstream. This file exists so they can be filed at
<https://github.com/esp-rs/esp-hal> (esp-alloc lives in that repo) without re-deriving
the analysis.

**Versions this was verified against — these findings WILL rot, re-check before filing:**

| Crate | Version | Notes |
|---|---|---|
| `esp-alloc` | **0.10.0** | crates.io `newest_version` at 2026-07-29 — i.e. these are present in the current release, not already fixed |
| `linked_list_allocator` | **0.10.6** | the backing allocator for the default `LLFF` algorithm |

Registry paths below are relative to
`~/.cargo/registry/src/index.crates.io-*/esp-alloc-0.10.0/`.

Background: `esp-alloc` selects its algorithm via the `heap_algorithm` esp-config option
(`esp_config.yml`), values `"LLFF"` (default) or `"TLSF"`. Defect 1 is algorithm-independent;
defect 2 and its adjacent nit are in the shared region layer and the `LLFF` backend.

---

## Defect 1 — `realloc` reads past the end of the source allocation

**File:** `src/malloc.rs`, `realloc_with_caps`, lines 96–117 (the defect is at 108–111).
**Severity:** Undefined behaviour with a plausible information-disclosure consequence. No
out-of-bounds *write*. Reachable in practice — these are `#[unsafe(no_mangle)] extern "C"`
symbols that exist to serve the ESP-IDF radio blobs, so `realloc` is called by code we do
not control.

### The allocator's own layout convention

`malloc_with_caps` (same file, lines 11–30) prepends a 4-byte header and returns the
**payload** pointer:

```rust
let total_size = size + 4;                       // :15
let ptr = crate::HEAP.alloc_caps(caps, Layout::from_size_align_unchecked(total_size, 4));
*(ptr as *mut usize) = total_size;               // :27  header = TOTAL, not payload
ptr.offset(4)                                    // :28  caller receives the PAYLOAD
```

So for a pointer `p` handed to a caller:
- the block spans `[p-4, p-4+total_size)`
- the **payload** spans `[p, p-4+total_size)` and is exactly `total_size - 4` bytes

`free` (lines 33–47) undoes this correctly: it steps back 4, reads `total_size`, and
deallocates with that size. The header convention itself is sound.

### The defective code

```rust
let len = usize::min(
    (ptr as *const u32).sub(1).read_volatile() as usize,   // :109  = TOTAL size
    new_size,
);
memcpy(p, ptr, len);                                       // :112  reads from the PAYLOAD
```

`.sub(1)` on a `*const u32` correctly lands on the header, so the value read is right. The
bug is that this value is `total_size` (payload **+ 4**), while the pointer it is paired
with, `ptr`, is the payload base. The copy length is therefore drawn from one unit of
measure and applied to another.

### Consequence

Let `S` be the old payload size, so the header holds `S + 4`.

    len = min(S + 4, new_size)

The source `ptr` has only `S` valid bytes, so the read overruns whenever `len > S`, i.e.
whenever **`new_size > S`** — any *growing* realloc. The overrun is
`min(4, new_size - S)` bytes, so it reaches its maximum of **4 bytes** once
`new_size >= S + 4`. A shrinking or equal realloc (`new_size <= S`) is unaffected.

The destination is safe: `len <= new_size` by construction and the destination payload is
`new_size` bytes, so there is no OOB write.

What gets copied is whatever follows the old block. With the default `LLFF` backend that
memory is managed by `linked_list_allocator`, which stores its free-list nodes **inside the
free blocks themselves** (`hole.rs`: "A sorted list of holes"). A `Hole { size, next }`
occupies the first 8 bytes of a free block, so if the neighbouring block is free, the 4
bytes copied into the caller's buffer can be a **live heap size or `next` pointer**. That
is an address-disclosure primitive, not merely garbage.

### Minimal reproduction sketch

```c
void *p = malloc(8);        /* total_size = 12; payload = [p, p+8) */
memset(p, 0xAA, 8);
void *q = realloc(p, 16);   /* len = min(12, 16) = 12
                               memcpy(q, p, 12) reads [p, p+12)
                               -> 4 bytes past the 8-byte payload */
```

Bytes `q[8..12]` are adjacent heap memory, not caller data. Under Miri or ASan this is a
`heap-buffer-overflow` read; on target it is silent.

### Proposed fix

Convert the header to a payload length before using it as a copy length:

```rust
let old_total = (ptr as *const u32).sub(1).read_volatile() as usize;
let len = usize::min(old_total - 4, new_size);
memcpy(p, ptr, len);
```

Deriving the `4` from `size_of::<u32>()` (or a shared `HEADER_SIZE` const used by
`malloc`/`free`/`realloc` alike) would be better still, since the literal is currently
repeated at four sites.

### Adjacent inconsistency, worth fixing in the same patch

The header is **written** as `usize` (`:27`) and **read** as `u32` (`:109`). These coincide
on the 32-bit Xtensa/RISC-V targets esp-alloc supports, so it is not a live bug, but the
two sites disagree about the header's type while `malloc` reserves a hard-coded 4 bytes for
it. One `HEADER_SIZE`/type used consistently would remove the class.

---

## Defect 2 — `dealloc` stops scanning at the first empty region slot

**File:** `src/lib.rs`. Defect at lines 681–686; contrast with `alloc_caps` at 538–541.
**Severity:** **Latent, not currently reachable** — see the honest severity note below. It
is a robustness/consistency defect: two functions iterate the same array by different rules.

### The asymmetry

The region table is a fixed array with empty slots (`src/lib.rs:409`):

```rust
heap: [Option<HeapRegion>; 3],
```

`alloc_caps` **skips** empty slots:

```rust
let mut iter = self
    .heap
    .iter_mut()
    .filter_map(|region| region.as_mut())        // :541  None slots skipped
    .filter(|region| region.capabilities.is_superset(capabilities));
```

`dealloc` **stops** at the first one:

```rust
let mut iter = this.heap.iter_mut();
while let Some(Some(region)) = iter.next() {     // :682  None terminates the loop
    if unsafe { region.try_deallocate(ptr, layout) } {
        break;
    }
}
```

`while let Some(Some(_))` binds only when *both* the iterator yields an item **and** that
item is `Some`. A `None` slot ends the loop rather than being skipped. If a populated region
ever sat behind an empty slot, allocations could be served from it while frees would never
be offered to it — `try_deallocate` would simply never be called for that region, the loop
would exit, and the memory would be silently lost. No panic, no error return: `dealloc`
returns `()`.

### Honest severity note — please read before filing

I could not construct a reachable path to this today, and the report is stronger for saying
so. `add_region` (`:428–444`) fills the **first** `None` slot it finds:

```rust
let free = self.heap.iter().enumerate().find(|v| v.1.is_none()).map(|v| v.0);
```

and there is no region-*removal* API. Slots therefore fill densely from index 0, so a
populated slot can never sit behind an empty one, and the two loops agree in every state
currently constructible. In our own project the two pools occupy slots 0 and 1 with slot 2
empty, and `dealloc` behaves identically to `alloc_caps`.

So this is **defence-in-depth, not a live bug**. It becomes a real silent-leak the moment
anyone adds region removal, allows sparse registration, or reorders the table. The fix is
one line and removes the future hazard along with the inconsistency:

```rust
let mut iter = this.heap.iter_mut().filter_map(|region| region.as_mut());
while let Some(region) = iter.next() {
    if unsafe { region.try_deallocate(ptr, layout) } { break; }
}
```

### Adjacent nit in the same code path — the two backends disagree

`LLFF`'s region range check is inclusive at the top (`src/heap/llff.rs:33`):

```rust
if self.heap.bottom() <= ptr.as_ptr() && self.heap.top() >= ptr.as_ptr() {
```

`Heap::top()` is **one past the end**, not the last valid byte. That is verifiable from
`linked_list_allocator` 0.10.6 itself, where size is the difference of the two
(`src/lib.rs:220–222`):

```rust
pub fn size(&self) -> usize {
    unsafe { self.holes.top.offset_from(self.holes.bottom) as usize }
}
```

so `top() == bottom() + size`. Using `>=` therefore claims one address beyond the region.

The `TLSF` backend in the same crate gets this right (`src/heap/tlsf.rs:62`):

```rust
if self.pool_start <= addr && self.pool_end > addr {      // exclusive — correct
```

The two backends are thus inconsistent with each other, and `LLFF` is the incorrect one.
Consequence today: none, because a pointer exactly at `top()` is never handed out. But if
two pools were ever placed adjacently — `top(A) == bottom(B)` — a free of B's first block
would be misrouted to A, which owns no such block. In our project the pools are far apart
with the stack between them, so we cannot hit it. Suggested fix: `self.heap.top() > ptr.as_ptr()`,
matching TLSF.

---

## Filing checklist

- [ ] Re-check against the newest `esp-alloc` (was 0.10.0 on 2026-07-29) — quote the version
      you verified, not this one.
- [ ] File at <https://github.com/esp-rs/esp-hal>; defects 1 and 2 are independent and
      deserve separate issues.
- [ ] Defect 1 is the one that matters. Lead with the UB and the free-list-metadata
      disclosure; the fix is one line.
- [ ] Present defect 2 as a hardening/consistency fix, explicitly noting it is not currently
      reachable. Overstating it invites dismissal of both.
- [ ] Neither defect is implicated in our issue #75 — that was drawn-item count and
      fragmentation, not allocator misbehaviour. Do not conflate them in the report.
