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
