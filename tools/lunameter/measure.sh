#!/usr/bin/env bash
# lunameter — measure the per-frame scene cost of every watch screen.
#
#   tools/lunameter/measure.sh                             # this checkout
#   LUNAMETER_OUT=/tmp/after tools/lunameter/measure.sh     # + dump PPM renders
#   WATCH_UI_ROOT=/other/checkout tools/lunameter/measure.sh # a different tree
#
# See README.md. Host build only — touches no hardware, opens no serial port.
set -euo pipefail

# `cargo` missing from a non-login shell's PATH produced ZERO frames and exit 0
# under `set -e` — a measurement tool that silently measures nothing is worse
# than one that fails, because its empty output reads as "no change".
command -v cargo >/dev/null || export PATH="$HOME/.cargo/bin:$PATH"
command -v cargo >/dev/null || { echo "lunameter: cargo not found on PATH" >&2; exit 127; }

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd "$here/../.." && pwd)"

# Stage OUTSIDE the repo: the repo's .cargo/config.toml pins
# target = riscv32imac-unknown-none-elf for everything beneath it, and this is a
# host binary. Building in-tree fails with "can't find crate for `std`".
# A FIXED path here races itself: two concurrent runs `rm -rf` each other's
# staging tree and one dies with "cannot remove …: Directory not empty" — observed
# live 2026-07-29 in a session running several agents in parallel. Default to a
# unique dir; `LUNAMETER_STAGE` still pins it for anyone who wants build reuse.
stage="${LUNAMETER_STAGE:-$(mktemp -d "${TMPDIR:-/tmp}/lunameter-$(id -u)-XXXXXX")}"
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
