fn main() {
    linker_be_nice();
    widen_rom_region();
    // make sure linkall.x is the last linker script (otherwise might cause problems with flip-link)
    println!("cargo:rustc-link-arg=-Tlinkall.x");

    // Slint UI for the `slint-demo` binary: compile the .slint file with
    // resources (fonts/images) pre-rendered for the no_std software renderer.
    let slint_config = slint_build::CompilerConfiguration::new()
        .embed_resources(slint_build::EmbedResourcesKind::EmbedForSoftwareRenderer);
    slint_build::compile_with_config("ui/slint/shell.slint", slint_config)
        .expect("failed to compile ui/slint/shell.slint");
}

/// #67: widen the ROM (flash-mapped code+rodata) region from esp-hal's hardcoded
/// **4 MiB** to the **6 MiB** `partitions.csv` already reserves per OTA slot.
///
/// ```text
/// esp-hal-1.1.1/ld/esp32c6/memory.x
///     ROM : ORIGIN = 0x42000000 + 0x20, LENGTH = 0x400000 - 0x20   <- 4 MiB
/// partitions.csv:  ota_0 / ota_1 = 0x600000                        <- 6 MiB each
/// C6 flash-cache MMU window: [0x42000000, 0x42800000)              <- 8 MiB
/// ```
///
/// Without this the firmware sits at **0.17 % free ROM (6,952 B)** and nothing of
/// meaningful size can LINK; the release profile is already `opt-level='s'` + fat
/// LTO, so no trimming lever remains. Flash-side twin of the #65 stack ceiling.
///
/// **ORIGIN is unchanged**, so sections land at identical addresses — verified:
/// baseline and widened builds have byte-identical `.text`/`.rodata` addresses,
/// sizes and high-water. This only relaxes the end-of-region check and does NOT
/// move `_bss_end`, so it is not the #65 crash class.
///
/// ## Why patch esp-hal's generated file
///
/// esp-hal's `build.rs` copies `ld/esp32c6/*` (incl. `memory.x`) into its own
/// `OUT_DIR` unconditionally and `linkall.x` does `INCLUDE memory.x` by name.
/// Shipping our own copy does NOT work: build scripts run in dependency order, so
/// esp-hal's `-L` always precedes ours and its file wins (tested, not assumed).
/// Ours runs after esp-hal's and before the link, so rewriting its generated file
/// is the one hook that reliably takes effect.
///
/// Rewritten unconditionally (idempotent) because **cargo does not treat
/// `memory.x` as a build input** — a stale file otherwise persists across builds
/// and you measure, or flash, the wrong artifact.
fn widen_rom_region() {
    const STOCK: &str = "LENGTH = 0x400000 - 0x20";
    const WIDE: &str = "LENGTH = 0x600000 - 0x20";

    // OUT_DIR = target/<triple>/<profile>/build/<our-pkg>-<hash>/out
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let Some(build_dir) = out.parent().and_then(|p| p.parent()) else { return };

    let mut patched = 0usize;
    let Ok(entries) = std::fs::read_dir(build_dir) else { return };
    for e in entries.flatten() {
        let name = e.file_name();
        if !name.to_string_lossy().starts_with("esp-hal-") {
            continue;
        }
        let mx = e.path().join("out").join("memory.x");
        let Ok(text) = std::fs::read_to_string(&mx) else { continue };
        if text.contains(WIDE) {
            patched += 1; // already wide; keep it that way
            continue;
        }
        if !text.contains(STOCK) {
            println!(
                "cargo:warning=#67: {} has neither the stock nor widened ROM LENGTH \
                 - esp-hal changed memory.x; re-check the region.",
                mx.display()
            );
            continue;
        }
        if std::fs::write(&mx, text.replace(STOCK, WIDE)).is_ok() {
            patched += 1;
        }
    }
    if patched == 0 {
        println!(
            "cargo:warning=#67: could not widen esp-hal's ROM region under {} - \
             build still capped at 4 MiB.",
            build_dir.display()
        );
    }
}

fn linker_be_nice() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() > 1 {
        let kind = &args[1];
        let what = &args[2];

        match kind.as_str() {
            "undefined-symbol" => match what.as_str() {
                what if what.starts_with("_defmt_") => {
                    eprintln!();
                    eprintln!(
                        "💡 `defmt` not found - make sure `defmt.x` is added as a linker script and you have included `use defmt_rtt as _;`"
                    );
                    eprintln!();
                }
                "_stack_start" => {
                    eprintln!();
                    eprintln!("💡 Is the linker script `linkall.x` missing?");
                    eprintln!();
                }
                what if what.starts_with("esp_rtos_") => {
                    eprintln!();
                    eprintln!(
                        "💡 `esp-radio` has no scheduler enabled. Make sure you have initialized `esp-rtos` or provided an external scheduler."
                    );
                    eprintln!();
                }
                "embedded_test_linker_file_not_added_to_rustflags" => {
                    eprintln!();
                    eprintln!(
                        "💡 `embedded-test` not found - make sure `embedded-test.x` is added as a linker script for tests"
                    );
                    eprintln!();
                }
                "free"
                | "malloc"
                | "calloc"
                | "get_free_internal_heap_size"
                | "malloc_internal"
                | "realloc_internal"
                | "calloc_internal"
                | "free_internal" => {
                    eprintln!();
                    eprintln!(
                        "💡 Did you forget the `esp-alloc` dependency or didn't enable the `compat` feature on it?"
                    );
                    eprintln!();
                }
                _ => (),
            },
            // we don't have anything helpful for "missing-lib" yet
            _ => {
                std::process::exit(1);
            }
        }

        std::process::exit(0);
    }

    println!(
        "cargo:rustc-link-arg=--error-handling-script={}",
        std::env::current_exe().unwrap().display()
    );
}
