#!/usr/bin/env bash
# build-s3.sh — the S3 (board-esp32s3-cyd) arm's build path. The riscv arms use
# fambuild directly; the Xtensa arm needs three things fambuild doesn't do:
#   1. the espup toolchain (+esp) with ~/export-esp.sh sourced (GCC linker
#      xtensa-esp32s3-elf-gcc must be on PATH — an unsourced shell fails with
#      "linker not found", which reads like a broken toolchain);
#   2. CARGO_PROFILE_RELEASE_OPT_LEVEL=2 — the size-optimising levels (s/z)
#      crash the Xtensa LLVM scavenger under fat LTO (smol targets/s3-cyd/
#      PORT-SCOPING.md §6.1); 2 links. NEVER change the shared global profile:
#      every C3/C6/C5 measurement on record depends on it.
#   3. fetching the xtensa ELF back (fambuild's fetch is hardcoded to the
#      riscv triple; see the stale-artifact hazard note in fambuild itself).
#
# ⚠️ KNOWN BLOCKER (2026-08-25, espup 1.95.0.0): building with the ui/cyd
# scene set crashes rustc's Xtensa isel —
#   LLVM ERROR: Cannot select: XtensaISD::PCREL_WRAPPER TargetConstantPool
#     [2 x float] [1.0, 0.0]   (symbol: InnerCydBacklightToggle...inits0_0)
# at opt 1/2/3 fat LTO (thin LTO: scavenger crash; lto=off: spill crash).
# The C6 scene set does NOT trigger it, so this script builds with the
# LINK-ONLY C6 fallback (renders cropped) until the lane resolves it:
# BISECTED 2026-08-25 (component-removal granularity — expression rewrites
# are NOT valid proof, slint can normalize them back):
#   * Stub ONLY CydBacklightToggle's two instantiations -> the FULL ui/cyd
#     scene LINKS on the S3.
#   * Restore the component but hold out its one float-expression binding
#     (`property <bool> on: root.value > 0.5;`) -> LINKS.
#   * Int-mediate it (Math.round(value*100) >= 50) -> CRASHES again with a
#     new [0.0, 1.0] constpool: ANY float arithmetic in this component's
#     bindings triggers the isel bug; float-free bindings are safe.
# Fix shape (not yet landed): push the on/off bool FROM RUST alongside
# set_brightness (root `backlight-on` property, in-property on the
# component) so the scene carries no float expressions in this component.
# espup 1.98.0.0 was tested and crashes identically — a toolchain bump is
# NOT a path; the minimal .slint repro for upstream is (s3-cyd owns the
# draft doc).
set -euo pipefail
cd "$(dirname "$0")/.."
TRIPLE=xtensa-esp32s3-none-elf

# Sync through fambuild's own rsync path (worktree isolation, config fallback,
# sigil hash hand-off) by running a no-op check first would double-build; do
# the same steps it does, minimally:
HASH="$(bash tools/build_hash.sh 2>/dev/null || echo unknown)"
rsync -az --delete --mkpath --exclude '/target' --exclude '/.git' --exclude '.claude' \
    ./ "familiar:fambuild/esp32c6-watch/" >&2
if [ -f .cargo/config.toml ]; then
    rsync -az .cargo/config.toml familiar:fambuild/esp32c6-watch/.cargo/config.toml >&2
fi

# The ui/cyd scene builds since the CydBacklightToggle workaround (the
# Rust-pushed on-state) landed — the LINK-ONLY C6-scene fallback is retired.
ssh familiar "cd ~/fambuild/esp32c6-watch \
  && export PATH=\$HOME/.cargo/bin:\$PATH && source ~/export-esp.sh \
  && export RUSTFLAGS='' CARGO_PROFILE_RELEASE_OPT_LEVEL=2 WATCH_BUILD_HASH='$HASH' \
  && cargo +esp build --release --no-default-features --features board-esp32s3-cyd \
       --target $TRIPLE --bin esp32c6-watch $*"

mkdir -p "target/$TRIPLE/release"
rsync -az "familiar:fambuild/esp32c6-watch/target/$TRIPLE/release/esp32c6-watch" \
    "target/$TRIPLE/release/esp32c6-watch" >&2
echo "fetched target/$TRIPLE/release/esp32c6-watch" >&2
grep -a -o 'WSIGIL:[^|]*|[^|]*|v[0-9][0-9.]*' "target/$TRIPLE/release/esp32c6-watch" | head -1
