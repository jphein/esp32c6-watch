#!/usr/bin/env bash
# trackermeter — count Slint PROPERTY-DEPENDENCY NODES per screen, on the host.
#
#   tools/trackermeter/measure.sh                              # this checkout
#   WATCH_UI_ROOT=/other/checkout tools/trackermeter/measure.sh # A/B another tree
#   TRACKERMETER_STAGE=/tmp/tm tools/trackermeter/measure.sh    # pin for build reuse
#
# See README.md. Host build only — touches no hardware, opens no serial port, and
# is NOT a fambuild target: like lunameter it has to build AND RUN locally to emit
# frames, so the "compile on familiar" rule does not apply.
set -euo pipefail

# `cargo` missing from a non-login shell's PATH produced ZERO frames and exit 0
# under `set -e` in lunameter — a measurement tool that silently measures nothing
# is worse than one that fails, because empty output reads as "no change".
command -v cargo >/dev/null || export PATH="$HOME/.cargo/bin:$PATH"
command -v cargo >/dev/null || { echo "trackermeter: cargo not found on PATH" >&2; exit 127; }

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd "$here/../.." && pwd)"

# Stage OUTSIDE the repo: the repo's .cargo/config.toml pins
# target = riscv32imac-unknown-none-elf for everything beneath it, and this is a
# host binary. Building in-tree fails with "can't find crate for `std`".
#
# A FIXED path races itself — two concurrent runs `rm -rf` each other's staging
# tree and one dies with "cannot remove …: Directory not empty". That was observed
# live 2026-07-29 in a session running several agents in parallel, so the default
# is unique per run; TRACKERMETER_STAGE still pins it for anyone who wants build
# reuse and knows they are the only runner.
stage="${TRACKERMETER_STAGE:-$(mktemp -d "${TMPDIR:-/tmp}/trackermeter-$(id -u)-XXXXXX")}"
rm -rf "$stage"
mkdir -p "$stage/src"
cp "$here/Cargo.toml" "$here/build.rs" "$stage/"

# The harness is DERIVED from lunameter's on every run, so the frame list can never
# drift from the one the texture ceiling is gated against. Asserts its anchors.
python3 "$here/instrument.py" "$stage/src"
# One renderer, one patch set — reuse lunameter's rather than keeping a second.
# A side effect worth having: each frame then reports both costs, scene items from
# the instrumented renderer and dependency nodes from the allocator.
python3 "$root/tools/lunameter/instrument.py" "$stage/renderer-fork"

cd "$stage"
WATCH_UI_ROOT="${WATCH_UI_ROOT:-$root}" \
  cargo run --release --quiet 2>&1 >/dev/null \
  | grep -E '^(--- FRAME|LUNAMETER|TRACKER)'
