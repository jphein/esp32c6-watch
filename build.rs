fn main() {
    linker_be_nice();
    stamp_sigil();
    // make sure linkall.x is the last linker script (otherwise might cause problems with flip-link)
    println!("cargo:rustc-link-arg=-Tlinkall.x");

    // Slint UI for the `slint-demo` binary: compile the .slint file with
    // resources (fonts/images) pre-rendered for the no_std software renderer.
    let slint_config = slint_build::CompilerConfiguration::new()
        .embed_resources(slint_build::EmbedResourcesKind::EmbedForSoftwareRenderer);
    slint_build::compile_with_config("ui/slint/shell.slint", slint_config)
        .expect("failed to compile ui/slint/shell.slint");
}

/// Bake the git short-SHA + build date into env vars for the Diag sigil
/// (`env!("GIT_SHA")` / `env!("BUILD_DATE")` in the firmware). Resolution order,
/// most-authoritative first, so it Just Works across environments:
///   1. `GIT_SHA` / `BUILD_DATE` env (explicit override / release pipeline)
///   2. `GITHUB_SHA` (GitHub Actions — the CI/OTA builds that ship to glass)
///   3. local `git` (developer checkout with a `.git`)
///   4. `"dev"` / `"unknown"` fallback
/// NB: `fambuild` rsyncs the worktree WITHOUT `.git`, so step 3 fails there and
/// dev builds read "dev" — that's fine; the shipped/CI sigil (step 2) is real.
fn stamp_sigil() {
    let sha = std::env::var("GIT_SHA")
        .ok()
        .or_else(|| {
            std::env::var("GITHUB_SHA")
                .ok()
                .map(|s| s.chars().take(7).collect())
        })
        .or_else(|| git(&["rev-parse", "--short", "HEAD"]))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "dev".to_string());

    let date = std::env::var("BUILD_DATE")
        .ok()
        .or_else(|| git(&["log", "-1", "--format=%cd", "--date=short"]))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string());

    println!("cargo:rustc-env=GIT_SHA={sha}");
    println!("cargo:rustc-env=BUILD_DATE={date}");
    // Re-stamp when HEAD moves or an override changes (best-effort — the path
    // may not exist under fambuild's git-less rsync, which cargo tolerates).
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-env-changed=GIT_SHA");
    println!("cargo:rerun-if-env-changed=GITHUB_SHA");
    println!("cargo:rerun-if-env-changed=BUILD_DATE");
}

/// Run a git command, returning trimmed stdout on success, else `None`.
fn git(args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git").args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
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
