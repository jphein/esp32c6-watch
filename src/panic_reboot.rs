// Reboot on panic instead of hanging forever (#75, esp-backtrace `custom-halt`).
//
// esp-backtrace's default tail is `arch::interrupt_free(|| loop {})` — a permanent
// hang with interrupts disabled. The panel keeps its last drawn frame, the mesh
// goes quiet, and only pulling power brings the watch back. Every "frozen watch"
// that turned out to be an OOM panic presented identically to a true wedge for
// exactly this reason, which is part of why the three freezes took so long to
// separate from each other.
//
// A watch that reboots loses its uptime and whatever was on screen. A watch that
// hangs makes the wearer physically power-cycle it. So: reboot — but only AFTER
// esp-backtrace has printed the panic and backtrace (it invokes this last), with a
// spin first so the USB-serial FIFO drains. Resetting too early truncates the very
// backtrace that makes a panic diagnosable, which would put us back to freezes
// with no evidence — the whole problem this session started with.
//
// A cycle spin, not a `Timer`: the executor is not usable from a panic.
//
// This fixes no panic. It converts an unrecoverable state into a recoverable one,
// which is worth having whichever bug fires.
//
// `include!`d by BOTH binaries rather than living in the lib: `custom-halt` is a
// crate-wide feature, so every binary must resolve the symbol, and neither binary
// references the lib target so its rlib is never linked (`--gc-sections` drops it,
// and the link fails with `undefined symbol: custom_halt`).
#[unsafe(no_mangle)]
extern "Rust" fn custom_halt() -> ! {
    // ═════════════════════════════════════════════════════════════════════════
    // ARM THE RTC WATCHDOG FIRST — before any call below can block. (#11)
    // ═════════════════════════════════════════════════════════════════════════
    // Everything after this point is best-effort. This is the part that MUST
    // work, because without it this handler has a path that never reaches its
    // own `software_reset()` and hangs exactly like the esp-backtrace default it
    // was written to replace.
    //
    // THE DEADLOCK. `HEAP.stats()` and `HEAP.free()` both go through
    // `self.inner.with(..)` (esp-alloc-0.10.0 lib.rs:627-634) — they TAKE THE
    // HEAP LOCK. There is no non-blocking accessor: every public reader on
    // `EspHeap` (`used`, `free`, `free_caps`, `stats`) locks. So if a panic fires
    // while that lock is held, the heap read below blocks forever, the reset is
    // never reached, and the wearer has to physically power-cycle the watch.
    //
    // That is not hypothetical. The known 100 %-reproducible corruption panic —
    // `Freed node aliases existing hole! Bad free?` — is an assert INSIDE
    // `linked_list_allocator`'s `dealloc`, which runs inside `with(..)`, i.e.
    // with the lock held. The `[PANIC-HEAP]` instrumentation added to make OOM
    // panics legible would therefore wedge on the one panic class that
    // reproduces every time, and wedge BEFORE printing — strictly worse than not
    // instrumenting at all.
    //
    // Currently reachable? Barely: `SCENE_DROP_ON_SUSPEND` is `false`, so that
    // teardown is unarmed, and the two known OOM sites (properties.rs:63,
    // vrc.rs:155) assert AFTER `alloc()` has returned and released the lock. The
    // condition is written down because it is a condition, not a guarantee — any
    // future panic raised from inside an alloc/dealloc critical section lands in
    // the wedge, and nothing warns you.
    //
    // ⚠️ AND THE LOCK IS NOT THE ORIGINAL BUG. The wedge that opened #11 — img9dbg,
    // panic #2, `rst:0x3 boot:0x3c` then silence for 3+ minutes, recovered only by
    // reflashing — happened BEFORE any heap read existed in this handler. So that
    // stall had a different cause and **it was never identified.** The lock
    // deadlock above is a SECOND way in, introduced later by the very
    // instrumentation meant to make panics legible.
    //
    // Which is the argument for a watchdog rather than a targeted fix: arming the
    // one mechanism that fires when the core is stuck does not require knowing
    // what it is stuck on. It covers the deadlock, it covers the original unknown,
    // and it covers the next one. Do not let this note read as "the wedge was the
    // heap lock, solved" — the original mechanism is still unexplained, and if a
    // silent stall is ever seen again WITH this watchdog armed, that is a new and
    // much more interesting fact.
    //
    // WHY A WATCHDOG rather than a try-lock: esp-alloc exposes no try-lock, so
    // the choice is between reading the heap and being deadlock-proof. The RWDT
    // buys both — it is the only mechanism that still fires when the core is
    // stuck in a lock it can never acquire.
    //
    // 5 s: far above this handler's real cost (~28M spin cycles plus a handful of
    // prints, well under a second) so it never truncates a healthy panic report,
    // and far below the "user gives up and pulls power" threshold. `enable()`
    // needs no stage configuration — esp-hal documents its default as "stage 0
    // resets the system".
    {
        use esp_hal::rtc_cntl::{Rtc, RwdtStage};
        // SAFETY: a terminal path that ends in `software_reset()`. On this board
        // LPWR is never claimed at all — `Rtc::new(peripherals.LPWR)` in `main`
        // is gated on `has-light-sleep`, which the CYD does not declare — so
        // there is nothing to alias. On a board that does claim it, the alias
        // touches only the LP_WDT registers, on a path whose sole remaining job
        // is to reset the chip. `Rtc::new` is a pure constructor (no register
        // writes, no allocation), and neither `Rtc` nor `Rwdt` implements `Drop`,
        // so the armed timeout survives this scope ending.
        // `esp_hal::time::Duration`, NOT `core::time::Duration` — rtc_cntl's own
        // doc example shows `use core::time::Duration`, which does not compile
        // against this signature. Read from the signature, not the example.
        let mut rtc = Rtc::new(unsafe { esp_hal::peripherals::LPWR::steal() });
        rtc.rwdt.set_timeout(
            RwdtStage::Stage0,
            esp_hal::time::Duration::from_millis(5_000),
        );
        rtc.rwdt.enable();
    }

    for _ in 0..24_000_000u32 {
        core::hint::spin_loop();
    }
    // ★ HEAP STATE AT THE MOMENT OF THE PANIC — the number every allocation
    // failure needs and none of them print.
    //
    // Two OOM panics have now cost a symbolization round each because the message
    // says nothing about memory:
    //   i-slint-core properties.rs:63   assert!(!mem.is_null(), "allocation failed")
    //   vtable      vrc.rs:155          NonNull::new(mem).unwrap()   <- reads as a
    //                                   refcount bug; it is `alloc()` returning null
    // The second one is worse than useless: one line below it sits
    // `assert!(!mem.is_null())` with a GOOD message, permanently unreachable behind
    // the unwrap. So the crate that reports the failure decides how legible it is,
    // and we do not control those crates. This does: whatever panics, for whatever
    // reason, the heap state that caused it is on the wire.
    //
    // It also closes the sampling gap that made the first OOM unattributable — every
    // other heap figure we have is a ~2.7 s beat sample, so the trough that actually
    // failed was never observed. This reading is, by construction, the one that
    // matters.
    //
    // ⚠️ READS ONLY. `stats()` is a plain struct read with no allocation.
    // `largest_free_block()` is deliberately NOT called here: it probes by
    // allocating, and allocating inside an out-of-memory panic invites a nested
    // panic that would destroy the backtrace we just printed.
    //
    // Reported region-BY-region, and deliberately NOT by index. This file is
    // `include!`d into BOTH binaries and only `main` registers PSRAM, so a
    // positional `main_free=`/`recl_free=` labelling is wrong in one of the two
    // by construction — and wrong SILENTLY. Image 11 added a third region at the
    // FRONT of this list, which is exactly the edit that re-points positional
    // labels with no compile error to catch it. Enumerating cannot mislabel:
    // every line states its own index, size and capability, so it reads
    // correctly at 2 regions or 3 and needs no edit at 4.
    let hs = esp_alloc::HEAP.stats();
    esp_println::println!("[PANIC-HEAP] total_free={}", esp_alloc::HEAP.free());
    for (i, r) in hs.region_stats.iter().enumerate() {
        if let Some(r) = r.as_ref() {
            esp_println::println!(
                "[PANIC-HEAP] rgn{} size={} used={} free={} ext={}",
                i,
                r.size,
                r.used,
                r.free,
                r.capabilities
                    .contains(esp_alloc::MemoryCapability::External),
            );
        }
    }
    esp_println::println!("[PANIC] rebooting (custom-halt) — backtrace above");
    for _ in 0..4_000_000u32 {
        core::hint::spin_loop();
    }
    esp_hal::system::software_reset()
}
