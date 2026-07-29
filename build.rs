fn main() {
    linker_be_nice();
    widen_rom_region();
    stamp_build_sigil();
    // make sure linkall.x is the last linker script (otherwise might cause problems with flip-link)
    println!("cargo:rustc-link-arg=-Tlinkall.x");

    // Slint UI for the `slint-demo` binary: compile the .slint file with
    // resources (fonts/images) pre-rendered for the no_std software renderer.
    let slint_config = slint_build::CompilerConfiguration::new()
        .embed_resources(slint_build::EmbedResourcesKind::EmbedForSoftwareRenderer);
    slint_build::compile_with_config("ui/slint/shell.slint", slint_config)
        .expect("failed to compile ui/slint/shell.slint");
}

/// Stamp this build with a **realm-sigil forge name + short hash**, so the
/// About page identifies the *image that is running* rather than a constant.
///
/// ## The bug this fixes
///
/// The About page showed `v{CARGO_PKG_VERSION}` — `v0.12.1`, a string that is
/// identical in every build ever made from this crate version. It therefore
/// could not answer the only question anyone ever asks it ("did my OTA land?"),
/// and on 2026-07-29 it was read as evidence that an OTA had NOT landed. It was
/// evidence of nothing at all. A version label that cannot change is worse than
/// no label, because it is trusted.
///
/// ## Why a name and not just the hash
///
/// Seven hex characters are unreadable at a glance on a 410 px panel, and two
/// builds an hour apart look alike. `Bellowed Kiln` does not. The hash stays
/// beside it as the actual identifier; the words are the human index. Same
/// `(hash, realm)` gives the same name in Go, Python, JS and Rust, so
/// `sigil generate --realm forge <hash>` on any host verifies what the watch
/// shows — the label is checkable, not merely printed.
///
/// ## Dirty builds get their OWN hash, deliberately
///
/// Most of this project's flashes are of uncommitted trees. If a dirty build
/// reported HEAD's hash, every debug flash in a session would carry the SAME
/// label — reintroducing the exact failure above, one level down. So a dirty
/// build is named from a content hash over `HEAD + status + diff`, marked with a
/// trailing `*`. Two dirty builds differ iff their sources differ, which is the
/// property that makes "still says the old sigil" a real diagnosis.
///
/// Untracked files reach the hash through `--porcelain` (their *names*, not
/// their contents) — enough to notice a new module appearing, not enough to
/// notice an edit inside one that was never `git add`ed. Adding it makes it
/// fully tracked.
///
/// ## Freshness
///
/// `slint_build` already emits `rerun-if-changed` for the `.slint` files, which
/// NARROWS cargo's default "rerun on any package change" to just those — so
/// without the paths declared below, the stamp would go stale exactly when it
/// matters (edit a `.rs`, reflash, read last build's name). Declaring `src`,
/// `ui` and `.git/HEAD` costs a `slint` recompile on each source edit; a version
/// label that silently lags the binary is not worth saving those seconds.
fn stamp_build_sigil() {
    // Re-run whenever the committed hash, the working tree or the UI changes.
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=ui");
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=.git/HEAD");
    if let Some(head_ref) = git(&["symbolic-ref", "-q", "HEAD"]) {
        println!("cargo:rerun-if-changed=.git/{head_ref}");
    }

    let (hash, dirty) = match git(&["rev-parse", "HEAD"]) {
        Some(head) => {
            // `--porcelain` covers untracked + staged; `diff HEAD` covers content.
            let status = git(&["status", "--porcelain"]).unwrap_or_default();
            let diff = git(&["diff", "HEAD"]).unwrap_or_default();
            if status.is_empty() && diff.is_empty() {
                (head[..7].to_string(), false)
            } else {
                // `hash-object` WITHOUT `-w`: computes the id, writes nothing to
                // the object database. A build must not litter the user's repo.
                let blob = format!("{head}
{status}
{diff}");
                match git_stdin(&["hash-object", "--stdin"], &blob) {
                    Some(h) if h.len() >= 7 => (h[..7].to_string(), true),
                    // Hash failed but we know it is dirty — say so rather than
                    // presenting HEAD as if it were what got built.
                    _ => (format!("{}", &head[..7]), true),
                }
            }
        }
        None => (String::new(), false),
    };

    let (sigil, hash_label) = if hash.is_empty() {
        // No git (source tarball / detached build host). Refuse to invent a name.
        ("no-git".to_string(), "unknown".to_string())
    } else {
        let name = sigil_id::build_name_for_hash(&hash)
            .map(|(adj, noun)| format!("{adj} {noun}"))
            .unwrap_or_else(|| "no-git".to_string());
        (name, if dirty { format!("{hash}*") } else { hash })
    };

    println!("cargo:rustc-env=BUILD_SIGIL={sigil}");
    println!("cargo:rustc-env=BUILD_HASH={hash_label}");
    // Echoed so a flash/OTA log records exactly what went on the glass — the
    // tooling greps this line instead of recomputing and possibly disagreeing.
    println!("cargo:warning=build sigil: {sigil} \u{00b7} {hash_label}");
}

/// `git <args>` -> trimmed stdout, or `None` if git is missing or the command
/// fails. Never panics: a missing git must degrade the label, not break the build.
fn git(args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?;
    Some(s.trim_end().to_string())
}

/// `git <args>` with `input` on stdin -> trimmed stdout.
fn git_stdin(args: &[&str], input: &str) -> Option<String> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let mut child = Command::new("git")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .ok()?;
    child.stdin.as_mut()?.write_all(input.as_bytes()).ok()?;
    let out = child.wait_with_output().ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8(out.stdout).ok().map(|s| s.trim_end().to_string())
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
