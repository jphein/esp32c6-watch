fn main() {
    // Default to the repo this harness lives in; override to measure another
    // checkout or a stashed "before" tree (see README.md).
    // measure.sh always sets this. It has no useful default: the harness is built
    // from a staging dir outside the repo, so there is nothing to derive it from.
    let root = std::env::var("WATCH_UI_ROOT")
        .expect("WATCH_UI_ROOT is unset — run tools/trackermeter/measure.sh, not cargo directly");
    let entry = format!("{root}/ui/slint/shell.slint");
    assert!(
        std::path::Path::new(&entry).is_file(),
        "WATCH_UI_ROOT={root} has no ui/slint/shell.slint"
    );
    // EmbedForSoftwareRenderer matches the firmware's build.rs, so glyphs come
    // from the same pre-rendered embedded set and the texture counts are real.
    let cfg = slint_build::CompilerConfiguration::new()
        .embed_resources(slint_build::EmbedResourcesKind::EmbedForSoftwareRenderer);
    slint_build::compile_with_config(&entry, cfg).expect("slint compile failed");
    println!("cargo:rerun-if-changed={root}/ui/slint");
    println!("cargo:rerun-if-env-changed=WATCH_UI_ROOT");
}
