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
# Scratch lives in /var/tmp (disk-backed), NOT /tmp — katana's /tmp is a
# 16GB tmpfs (RAM+swap) and build trees starved the machine once (JP
# directive, 2026-08-25). It CANNOT live in the project's own tmp/ either:
# the staging must stay outside the repo or the repo's .cargo/config.toml
# pins the riscv target onto this host build (the header note above — and
# the first version of this change made exactly that mistake and died with
# "can't find crate for std"). TMPDIR still wins if a caller sets it.
stage="${LUNAMETER_STAGE:-$(mktemp -d "${TMPDIR:-/var/tmp}/lunameter-$(id -u)-XXXXXX")}"
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
# Each entry MUST carry a tracking ref, and the count is capped. Both exist because
# a "known failures" list rots into a permanent exemption — which is the one way this
# gate could still end up lying. Requiring a ref makes an addition deliberate; capping
# the count makes it visible in the diff. **NEVER RAISE known_over_max.** If a new
# frame legitimately cannot meet the ceiling, that is a design decision, not a list
# edit — and the fix is to change the frame, since crossing the ceiling reboots the
# watch 10/10.
# EMPTY, and it stayed empty by being earned. This held
# `story(page3,len08):#75 story(page3,len24):#75` — the two frames that requested
# 14,336 B contiguous and rebooted the watch 10/10. luna's windowed CHAR page
# (PR #80) brought them to 136 and 220 textures, and this gate is what said so:
# "a KNOWN-OVER frame is now UNDER the ceiling — remove it from known_over". The
# list shrank on the gate's own instruction rather than on someone remembering to
# check, which is the property it was designed for.
known_over=""
known_over_max=0

n_known=$(printf '%s\n' $known_over | grep -c . || true)
if [ "$n_known" -gt "$known_over_max" ]; then
  echo "lunameter: known_over has $n_known entries, cap is $known_over_max." >&2
  echo "  This list may only SHRINK. A frame that cannot meet the 256-texture" >&2
  echo "  ceiling reboots the watch — change the frame, not this list." >&2
  exit 1
fi
for e in $known_over; do
  case "$e" in
    *:\#*) ;;
    *) echo "lunameter: known_over entry '$e' has no tracking ref (want 'frame:#NN')" >&2
       exit 1 ;;
  esac
done
known_frames=$(printf '%s\n' $known_over | sed 's/:#.*//' | tr '\n' ' ')

pairs=$(paste -d'|' \
          <(grep -o '^--- FRAME .*' "$out" | sed 's/--- FRAME //; s/ ---//') \
          <(grep -o 'textures=[0-9]*' "$out" | cut -d= -f2))
worst=$(printf '%s\n' "$pairs" | awk -F'|' -v k="$known_frames" '
  BEGIN{split(k,a," "); for(i in a) ex[a[i]]=1}
  !($1 in ex) && $2+0>m {m=$2+0} END{print m+0}')
new_over=$(printf '%s\n' "$pairs" | awk -F'|' -v c="$ceiling" -v k="$known_frames" '
  BEGIN{split(k,a," "); for(i in a) ex[a[i]]=1}
  !($1 in ex) && $2+0>c {printf "  %s = %s textures\n", $1, $2}')
fixed=$(printf '%s\n' "$pairs" | awk -F'|' -v c="$ceiling" -v k="$known_frames" '
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
echo "  (known-over exemptions: ${known_frames:-none — every frame is gated})" >&2
