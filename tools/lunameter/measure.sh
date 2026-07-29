#!/usr/bin/env bash
# lunameter — measure the per-frame scene cost of every watch screen.
#
#   tools/lunameter/measure.sh                             # this checkout
#   LUNAMETER_OUT=/tmp/after tools/lunameter/measure.sh     # + dump PPM renders
#   WATCH_UI_ROOT=/other/checkout tools/lunameter/measure.sh # a different tree
#
# See README.md. Host build only — touches no hardware, opens no serial port.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd "$here/../.." && pwd)"

# Stage OUTSIDE the repo: the repo's .cargo/config.toml pins
# target = riscv32imac-unknown-none-elf for everything beneath it, and this is a
# host binary. Building in-tree fails with "can't find crate for `std`".
stage="${LUNAMETER_STAGE:-${TMPDIR:-/tmp}/lunameter-$(id -u)}"
rm -rf "$stage"
mkdir -p "$stage"
cp "$here/Cargo.toml" "$here/build.rs" "$stage/"
cp -r "$here/src" "$stage/src"

# Regenerated from the real vendored crate every run, so it can never be a stale
# copy of a renderer that has since changed. Asserts every patch anchor.
python3 "$here/instrument.py" "$stage/renderer-fork"

cd "$stage"
WATCH_UI_ROOT="${WATCH_UI_ROOT:-$root}" \
  cargo run --release --quiet 2>&1 >/dev/null \
  | grep -E '^(--- FRAME|LUNAMETER)'
