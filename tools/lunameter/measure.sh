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
out="$stage/frames.txt"
WATCH_UI_ROOT="${WATCH_UI_ROOT:-$root}" \
  cargo run --release --quiet 2>&1 >/dev/null \
  | grep -E '^(--- FRAME|LUNAMETER)' | tee "$out"

# ---------------------------------------------------------------------------
# THE TEXTURE CEILING — a measured crash turned into a host-side invariant
# ---------------------------------------------------------------------------
#
# 256 `SceneTexture`s is a HARD ceiling on this hardware, not a budget. Crossing it
# makes the vector double to 512, which asks for 512 x 28 = 14,336 B CONTIGUOUS while
# the old 7,168 B buffer is still live — and reclaimed can never yield that while it
# also holds the items vector, the rounded-rects, the glyph caches and the story
# payload. Measured 2026-07-29: rendering story's CHARACTER page with 17 populated
# 24-char slots reboots the watch, **10 of 10 trials**, identically from a cold pool
# (items=128 tex=64) and a warm one (256/256):
#
#   memory allocation of 14336 bytes failed
#     RawVec::grow_one -> Vec<SceneTexture>::push_mut
#     -> SceneBuilder<PrepareScene>::draw_text_paragraph::<PixelFont>  (lib.rs:2791)
#
# Host frames bracket it exactly: page3 at 6-char values = 245 textures (safe), at
# 8-char values = 279 (crosses, reboots). The lower rungs always succeed; it is one
# specific allocation that can never be satisfied.
#
# This gate exists because the property is COUNTABLE ON THE HOST. The crash needed a
# watch, four hours and ~20 arms to find; the invariant needs one `grep`. That is the
# whole argument for putting it here rather than in a comment or a design doc.
ceiling=256
# KNOWN-OVER frames: arms that exist precisely to DOCUMENT the cliff. They must not
# fail the gate — otherwise it fails forever and gets ignored, which is the one thing
# a gate must never do. They are not hypothetical: they are the open bug, reachable the
# moment the daemon sends non-null equipment slots.
#
# When page 3 is fixed these should drop under the ceiling, and the gate says so
# explicitly rather than silently passing — a gate that tells you when it can be
# TIGHTENED is worth more than one that only tells you when it broke.
known_over="story(page3,len08) story(page3,len24)"

pairs=$(paste -d'|' \
          <(grep -o '^--- FRAME .*' "$out" | sed 's/--- FRAME //; s/ ---//') \
          <(grep -o 'textures=[0-9]*' "$out" | cut -d= -f2))
worst=$(printf '%s\n' "$pairs" | awk -F'|' -v k="$known_over" '
  BEGIN{split(k,a," "); for(i in a) ex[a[i]]=1}
  !($1 in ex) && $2+0>m {m=$2+0} END{print m+0}')
new_over=$(printf '%s\n' "$pairs" | awk -F'|' -v c="$ceiling" -v k="$known_over" '
  BEGIN{split(k,a," "); for(i in a) ex[a[i]]=1}
  !($1 in ex) && $2+0>c {printf "  %s = %s textures\n", $1, $2}')
fixed=$(printf '%s\n' "$pairs" | awk -F'|' -v c="$ceiling" -v k="$known_over" '
  BEGIN{split(k,a," "); for(i in a) ex[a[i]]=1}
  ($1 in ex) && $2+0<=c {printf "  %s = %s textures\n", $1, $2}')

if [ -n "$new_over" ]; then
  echo "" >&2
  echo "lunameter: TEXTURE CEILING EXCEEDED — these frames reboot the watch:" >&2
  printf '%s\n' "$new_over" >&2
  echo "  ceiling is $ceiling; crossing it requests 14,336 B contiguous, which this" >&2
  echo "  hardware cannot serve (measured 10/10 reboots, cold and warm)." >&2
  echo "  Reduce rendered rows or glyphs per row — a value-length cap does not help" >&2
  echo "  above 6 characters." >&2
  exit 1
fi
if [ -n "$fixed" ]; then
  echo "lunameter: a KNOWN-OVER frame is now UNDER the ceiling — remove it from" >&2
  echo "  known_over so it is gated from here on:" >&2
  printf '%s\n' "$fixed" >&2
fi
echo "lunameter: texture ceiling OK — worst gated frame ${worst}/${ceiling}" >&2
echo "  (known-over, the OPEN page-3 bug, not gated: $known_over)" >&2
