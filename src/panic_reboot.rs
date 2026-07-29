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
    esp_println::println!("[PANIC] rebooting (custom-halt) — backtrace above");
    for _ in 0..4_000_000u32 {
        core::hint::spin_loop();
    }
    esp_hal::system::software_reset()
}
