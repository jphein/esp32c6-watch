fn main() {
    linker_be_nice();
    widen_rom_region();
    stamp_build_sigil();
    // make sure linkall.x is the last linker script (otherwise might cause problems with flip-link)
    println!("cargo:rustc-link-arg=-Tlinkall.x");

    // Slint UI for the `slint-demo` binary: compile the .slint file with
    // resources (fonts/images) pre-rendered for the no_std software renderer.
    // PER-BOARD SCENE ROOT (#cyd-c5). The Slint layouts are absolute-positioned
    // for a specific panel, so each board compiles its own root:
    //
    //   board-waveshare-c6  -> ui/slint/shell.slint   (410x502 portrait)
    //   board-cyd-c5        -> ui/cyd/shell.slint     (320x240 landscape)
    //
    // Both roots must export the same component surface (WatchShell + its
    // properties/callbacks) — slint_shell.rs compiles against whichever is
    // selected. The CYD set may import shared pieces from ui/slint/ (theme,
    // controls) where their fixed sizes fit the smaller panel; that judgement
    // is the layout work's, not this build script's.
    //
    // FALLBACK while the CYD set is being built: if the CYD root does not exist
    // yet, compile the C6 scene and say so loudly — a 410x502 scene on a 320x240
    // panel renders cropped garbage, but it LINKS, which is what the C5 arm
    // needs for stack/image measurement before the layouts land. The warning is
    // the discriminator between "wrong layout by fallback" and "wrong layout by
    // bug".
    // The S3 CYD is the same 320x240 landscape class, so it shares the ui/cyd
    // scene set until a layout pass says otherwise (board_es3c28p.rs's panel
    // facts differ driver-side — MADCTL/inversion — not scene-side).
    let cyd = std::env::var("CARGO_FEATURE_BOARD_CYD_C5").is_ok()
        || std::env::var("CARGO_FEATURE_BOARD_ESP32S3_CYD").is_ok();
    let cyd_root = "ui/cyd/shell.slint";
    let ui_root = if cyd && std::path::Path::new(cyd_root).exists() {
        cyd_root
    } else {
        if cyd {
            println!("cargo:warning=cyd-class board: {cyd_root} not present yet — compiling the C6 scene as a LINK-ONLY fallback (renders cropped on this panel)");
        }
        "ui/slint/shell.slint"
    };
    let slint_config = slint_build::CompilerConfiguration::new()
        .embed_resources(slint_build::EmbedResourcesKind::EmbedForSoftwareRenderer);
    slint_build::compile_with_config(ui_root, slint_config)
        .unwrap_or_else(|e| panic!("failed to compile {ui_root}: {e}"));
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
    // Bake the push-OTA build epoch into a GREPPABLE marker (appended to WSIGIL
    // below) so any publisher can read an image's baked OTA_BUILD before it
    // announces. The running firmware's BUILD_EPOCH is a `const u64` (invisible
    // to `strings`), so before this a hand-rolled push could announce an epoch
    // GREATER than the image actually bakes — and since the accept-gate is
    // `announce > BUILD_EPOCH` with zero-touch reinstall, that mismatch is an
    // infinite self-reinstall loop (S3 probe-v2, 2026-08-27). Emitted here,
    // ahead of both return paths, so every build carries it; "0" when unset
    // (dev builds), matching ota_http::BUILD_EPOCH's own fallback.
    println!("cargo:rerun-if-env-changed=OTA_BUILD");
    println!(
        "cargo:rustc-env=OTA_BUILD_MARK={}",
        std::env::var("OTA_BUILD")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| "0".to_string())
    );
    // Declared inputs. `crates` is NOT optional here and was the hole in the
    // first version of this: every path dependency (including the vendored Slint
    // renderer, where most of this project's hot work happens) lives there, so
    // omitting it produced a build whose bytes were new and whose label was the
    // previous one. Worse — if the ONLY dirt is under `crates/`, the stamp is
    // computed on a tree git calls clean, so the watch reports a clean HEAD hash
    // with no `*`: a dirty build wearing a clean label, which is the exact bug
    // this whole mechanism exists to prevent, one level down.
    //
    // A path that does NOT exist makes cargo treat the crate as permanently
    // dirty ("the file `X` is missing", every build), which here would mean a
    // full slint recompile forever, visible only under `cargo build -v`. So
    // optional paths are declared only when present.
    for path in [
        "src",
        "ui",
        "crates",              // ALL path deps, incl. the vendored renderer
        "Cargo.toml",
        "Cargo.lock",          // a dependency bump changes the bytes
        "build.rs",
        "partitions.csv",      // fed to espflash; changes the image layout
        ".cargo/config.toml",  // holds ESP_LOG, which esp-println bakes in
    ] {
        if std::path::Path::new(path).exists() {
            println!("cargo:rerun-if-changed={path}");
        }
    }
    // git plumbing. `.git/index` matters because `git add` flips a file from
    // `??` to `A ` in `--porcelain`, which changes the dirty hash with no source
    // edit. `.git/packed-refs` matters because `git gc` (auto-gc is on by
    // default) DELETES `.git/refs/heads/<branch>` and folds it in there — after
    // which a declaration of the loose path would be a missing file, i.e. the
    // permanent-rebuild trap above.
    let mut git_inputs = vec![
        ".git/HEAD".to_string(),
        ".git/index".to_string(),
        ".git/packed-refs".to_string(),
    ];
    if let Some(head_ref) = git(&["symbolic-ref", "-q", "HEAD"]) {
        git_inputs.push(format!(".git/{head_ref}"));
    }
    for path in git_inputs {
        if std::path::Path::new(&path).exists() {
            println!("cargo:rerun-if-changed={path}");
        }
    }

    // fambuild (JP's standard build path since 2026-07-29) rsyncs the worktree to
    // familiar EXCLUDING `/.git`, so git is unavailable at the far end and every
    // remote build would stamp `no-git` — silently defeating the whole mechanism
    // on the path that produces most images. So an externally computed hash wins:
    // fambuild runs `tools/build_hash.sh` on katana, where git exists, and exports
    // the result.
    println!("cargo:rerun-if-env-changed=WATCH_BUILD_HASH");
    if let Ok(ext) = std::env::var("WATCH_BUILD_HASH") {
        let ext = ext.trim().to_string();
        if !ext.is_empty() {
            let dirty = ext.ends_with('*');
            let bare = ext.trim_end_matches('*');
            let sigil = sigil_id::build_name_for_hash(bare)
                .map(|(adj, noun)| format!("{adj} {noun}"))
                .unwrap_or_else(|| "no-git".to_string());
            println!("cargo:rustc-env=BUILD_SIGIL={sigil}");
            println!("cargo:rustc-env=BUILD_HASH={ext}");
            println!("cargo:warning=build sigil: {sigil} \u{00b7} {ext} (supplied)");
            let _ = dirty;
            return;
        }
    }

    let (hash, dirty) = match git(&["rev-parse", "HEAD"]) {
        Some(head) => {
            // `--porcelain` covers untracked + staged; `diff HEAD` covers content.
            //
            // The flags are not decoration. `git diff` renders a tracked binary
            // as "Binary files … differ" with NO content, so two different
            // binaries would hash identically — `--binary` emits the real delta.
            // There are no tracked binaries today, but Slint embeds resources
            // from `ui/`, so the day a font or PNG is committed there, edits to
            // it would otherwise be invisible to this hash. And the diff TEXT is
            // sensitive to the invoking user's git config (`diff.external`,
            // textconv, algorithm), which would make "same sources -> same name"
            // a per-host property — this builds on both katana and familiar.
            // `--untracked-files=all` lists files inside untracked directories
            // rather than just the directory name.
            let status = git(&["status", "--porcelain=v1", "--untracked-files=all"])
                .unwrap_or_default();
            let diff = git(&[
                "-c", "diff.external=",
                "diff", "--no-ext-diff", "--no-textconv", "--binary", "HEAD",
            ])
            .unwrap_or_default();
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
    // C6-ONLY until measured. This function rewrites esp-hal's GENERATED
    // esp32c6 memory.x (4 MiB -> the 6 MiB partitions.csv reserves); the C5 has
    // a different memory map, different generated file, and possibly no need —
    // its first linking image may sit under 4 MiB. Budgets are measured, never
    // inherited: the C5 gets its own arm here IF its measured image size
    // demands one, seeded from the first link on real hardware (cyd session
    // reports it). Silently applying C6 arithmetic to a C5 layout is the exact
    // class of inherited-number bug this repo spent 2026-07-29 rooting out.
    #[cfg(not(feature = "board-waveshare-c6"))]
    {
        println!("cargo:warning=widen_rom_region: skipped (not the C6 board) — if the image fails to LINK on ROM size, this is where the per-chip arm goes");
        return;
    }

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

    // LLD-ONLY. The RISC-V boards link with rust-lld, which understands
    // --error-handling-script (the friendly undefined-symbol hints above). The
    // S3's xtensa target links through xtensa-esp32s3-elf-GCC, which rejects
    // the flag outright ("unrecognized command-line option") and kills the
    // link — so the S3 arm trades the nice hints for a link that happens.
    #[cfg(not(feature = "board-esp32s3-cyd"))]
    println!(
        "cargo:rustc-link-arg=--error-handling-script={}",
        std::env::current_exe().unwrap().display()
    );
}
