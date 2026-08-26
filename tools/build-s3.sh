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
# ✅ RESOLVED (2026-08-26): the full ui/cyd scene builds AND renders on the S3
# (JP glass-verified "gui looks good"). The LINK-ONLY cropped-C6 fallback this
# script once carried is RETIRED — the build command below uses the real
# board-esp32s3-cyd feature, no fallback branch. Do not reintroduce the
# fallback language; it would be a correct-sounding claim about a dead limit.
#
# Historical record, kept because the failure mode is subtle and could recur:
# the ui/cyd scene once crashed rustc's Xtensa isel —
#   LLVM ERROR: Cannot select: XtensaISD::PCREL_WRAPPER TargetConstantPool
#     [2 x float] [1.0, 0.0]   (symbol: InnerCydBacklightToggle...inits0_0)
# at opt 1/2/3 fat LTO (thin LTO: scavenger crash; lto=off: spill crash).
# BISECTED to CydBacklightToggle's one float-expression binding
# (`property <bool> on: root.value > 0.5;`): ANY float arithmetic in that
# component's bindings triggered it; float-free bindings were safe. FIX
# (landed): the on/off bool is pushed FROM RUST alongside set_brightness so
# the component carries no float expressions. A toolchain bump was NOT the
# path — espup 1.95.0.0 and 1.98.0.0 both crashed identically.
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
