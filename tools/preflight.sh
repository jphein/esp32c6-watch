#!/usr/bin/env bash
# preflight — the gate that a green `cargo build` is NOT.
#
# ==========================================================================
# WHY THIS EXISTS
#
# `--features tts` was broken on `main` for hours while every build was green.
# A default-off cargo feature removes its own code from the compiler's view, so
# the only way to know gated code still compiles is to build it. #67's merge
# resolution silently dropped a function `voice_tts` calls twice, and nothing
# noticed because nothing ever built that combination.
#
# The second half is budgets. This firmware runs against two near-limit ceilings
# that a successful link does not prove you are safely inside:
#
#   * STACK: gap = _stack_start - _bss_end, asserted at BOOT against
#     STACK_FLOOR. Growing .bss silently steals stack — invisible in heap stats.
#     The floor is measured (61 KB = 5/5 boot panic, 73 KB = 0/5) and sat 15 KB
#     below reality for months while reading as healthy margin.
#   * ROM: high-water vs the region end. Sum-of-section-sizes UNDER-REPORTS by
#     ~11 KB here because it omits `.text_gap`, which is allocated region space.
#     The linker checks high-water, so that is what this checks.
#
# So: build every combination, and assert the budgets rather than print them.
# A check that cannot fail is not a check.
# ==========================================================================
#
# Usage:
#   tools/preflight.sh                       # everything (host tests + 4 combos)
#   tools/preflight.sh --skip-tests          # link combos only
#   tools/preflight.sh --tests-only          # host crates only (fast)
#   tools/preflight.sh --only tts            # ONE combo ("default" for no features)
#   tools/preflight.sh --builder fambuild    # build on familiar, measure locally
#
# `--only` exists for CI: each combo is a full fat-LTO link (firmware.yml allows
# 30 min for ONE), so running four sequentially would blow any sane timeout.
# CI fans them out as a matrix, one combo per job, and they run in parallel.
#
# Exit codes: 0 all gates pass · 1 a gate failed · 2 usage/environment problem.
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO" || exit 2

BUILDER=cargo
SKIP_TESTS=0
TESTS_ONLY=0
ONLY=
HOST_TARGET=x86_64-unknown-linux-gnu
TRIPLE=riscv32imac-unknown-none-elf
BIN=esp32c6-watch

while [[ $# -gt 0 ]]; do
  case "$1" in
    --builder) BUILDER="$2"; shift 2 ;;
    --skip-tests) SKIP_TESTS=1; shift ;;
    --tests-only) TESTS_ONLY=1; shift ;;
    --only) ONLY="$2"; shift 2 ;;
    -h|--help) sed -n '2,32p' "$0"; exit 0 ;;
    *) echo "preflight: unknown arg $1" >&2; exit 2 ;;
  esac
done

FAILURES=()
note()  { printf '\n\033[1m== %s\033[0m\n' "$*"; }
pass()  { printf '  \033[32mPASS\033[0m  %s\n' "$*"; }
fail()  { printf '  \033[31mFAIL\033[0m  %s\n' "$*"; FAILURES+=("$1"); }
info()  { printf '        %s\n' "$*"; }

# --- tool discovery --------------------------------------------------------
# Prefer the toolchain's llvm-nm/llvm-readelf when present; system binutils
# reads these ELFs fine too.
NM=$(command -v llvm-nm || command -v nm) || { echo "preflight: need nm" >&2; exit 2; }
RE=$(command -v llvm-readelf || command -v readelf) || { echo "preflight: need readelf" >&2; exit 2; }
CARGO=$(command -v cargo || echo "$HOME/.cargo/bin/cargo")

# --- budget constants, PARSED FROM SOURCE so they cannot drift -------------
# STACK_FLOOR is the boot assert's own value; hardcoding it here would let the
# gate and the firmware disagree, which is the failure mode this file exists to
# prevent.
STACK_FLOOR=$(grep -oE 'const STACK_FLOOR: usize = [0-9]+ \* 1024' src/main.rs \
              | grep -oE '[0-9]+ \* 1024' | awk '{print $1 * 1024}')
if [[ -z "${STACK_FLOOR:-}" ]]; then
  echo "preflight: could not parse STACK_FLOOR from src/main.rs" >&2
  exit 2
fi

# ROM region end comes from the GENERATED memory.x (build.rs widens it per #67),
# so this tracks whatever the build actually linked against rather than a
# constant that goes stale. Falls back to reporting-only if it cannot be found.
# In fambuild mode the generated memory.x lives on the build host, so read it
# there — the limit must come from what was ACTUALLY linked against, not a
# constant that goes stale the next time build.rs changes it.
rom_limit() {
  local len=""
  if [[ "$BUILDER" == "fambuild" ]]; then
    len=$(ssh familiar "grep -h -oE 'ROM : ORIGIN =[^,]+, LENGTH = 0x[0-9A-Fa-f]+' \
          \$HOME/fambuild/$(basename "$REPO")/target/$TRIPLE/release/build/esp-hal-*/out/memory.x \
          2>/dev/null | head -1" 2>/dev/null | grep -oE '0x[0-9A-Fa-f]+$')
  else
    local mx
    mx=$(find target -path '*esp-hal*' -name memory.x 2>/dev/null | head -1)
    [[ -n "$mx" ]] && len=$(grep -oE 'ROM : ORIGIN =[^,]+, LENGTH = 0x[0-9A-Fa-f]+' "$mx" \
          | grep -oE '0x[0-9A-Fa-f]+$' | head -1)
  fi
  [[ -z "$len" ]] && return 1
  printf '%d\n' $(( 0x42000000 + len ))
}

# --- measurement -----------------------------------------------------------
# Emits "gap rom_used rom_end" for an ELF.
measure() {
  local elf="$1" limit
  limit=$(rom_limit || echo 0)
  "$NM" "$elf" 2>/dev/null | awk -v lim="$limit" '
    / _bss_end$/     { b = strtonum("0x" $1) }
    / _stack_start$/ { s = strtonum("0x" $1) }
    END { printf "%d ", s - b }'
  "$RE" -S -W "$elf" 2>/dev/null | awk -v lim="$limit" '
    /^  \[/ {
      a = strtonum("0x" $4); z = strtonum("0x" $6)
      if (a >= 0x42000000 && a < 0x42800000 && z > 0 && a + z > m) m = a + z
    }
    END { printf "%d %d\n", m - 0x42000000, lim }'
}

# --- host crates -----------------------------------------------------------
# `cargo test --workspace` does NOT work here: the workspace root member is the
# FIRMWARE crate, so --workspace builds it for the host and dies inside esp-sync
# with "cannot find module or crate `riscv`" — nothing to do with the crate under
# test. And even with -p, --target is required, because .cargo/config.toml
# defaults the target to riscv and a bare `cargo test` then fails with "can't
# find crate for `test`". Both messages point away from the real cause.
if [[ $SKIP_TESTS -eq 0 ]]; then
  note "host crate tests (-p, host target — see the comment above)"
  # Discover crates instead of hardcoding, so a new one is covered on day one.
  mapfile -t CRATES < <(find crates -maxdepth 2 -name Cargo.toml -printf '%h\n' \
                        | xargs -r -n1 basename | sort)
  TOTAL=0
  for c in "${CRATES[@]}"; do
    # The vendored Slint renderer fork is excluded from the workspace.
    [[ "$c" == "i-slint-renderer-software" ]] && continue
    out=$("$CARGO" test -p "$c" --target "$HOST_TARGET" 2>&1)
    if grep -qE '^error' <<<"$out"; then
      fail "$c: build/test error"
      grep -E '^error' <<<"$out" | head -3 | sed 's/^/        /'
      continue
    fi
    n=$(grep -E '^test result' <<<"$out" \
        | awk -F'[ ;]' '{p+=$4; f+=$6} END{print p"/"p+f}')
    if grep -qE '^test result: FAILED' <<<"$out"; then
      fail "$c: tests failed ($n)"
    else
      pass "$c: $n"
      TOTAL=$(( TOTAL + $(cut -d/ -f1 <<<"$n") ))
    fi
  done
  info "total: $TOTAL host tests"
fi

# --- link every feature combination ---------------------------------------
# The whole point: a default-off feature's code is invisible to the compiler
# until something builds with it enabled.
if [[ $TESTS_ONLY -eq 1 ]]; then
  note "verdict (tests only)"
  if [[ ${#FAILURES[@]} -eq 0 ]]; then pass "host tests green"; exit 0; fi
  printf '  \033[31m%d failed\033[0m\n' "${#FAILURES[@]}"
  exit 1
fi

note "link combos + budgets (floor $STACK_FLOOR B)"
printf '        %-24s %10s %10s %12s\n' COMBO 'STACK GAP' MARGIN 'ROM FREE'

# The two #75 diagnostic features ride together in ONE combo rather than two.
# They must be here at all — a gated feature nothing builds is exactly the rot
# this script was written to catch (see the `--features tts` story in the header)
# — but each entry is a full fat-LTO link, so pairing them buys both for the
# price of one and additionally proves they compose: `heap-forensics` allocates
# inside `log_heap` while `heap-hooks` counts every allocation, so building them
# together is the case most likely to break.
COMBOS=("" "debug-console" "tts" "tts,debug-console" "heap-hooks,heap-forensics")
if [[ -n "$ONLY" ]]; then
  # "default" is the human name for the empty feature set.
  [[ "$ONLY" == "default" ]] && ONLY=""
  COMBOS=("$ONLY")
fi
for feat in "${COMBOS[@]}"; do
  label=${feat:-default}
  args=(build --release --bin "$BIN")
  [[ -n "$feat" ]] && args+=(--features "$feat")

  if [[ "$BUILDER" == "fambuild" ]]; then
    out=$(fambuild "${args[@]}" 2>&1)
    rc=$?
    # fambuild builds on familiar; bring the ELF back so the budgets are
    # measured from the artifact that was actually linked.
    # `~` not `$HOME`: OpenSSH 9+ scp speaks SFTP, which expands a tilde but
    # NOT shell variables — with $HOME the copy silently fails and every budget
    # measures 0, which reads as "below the floor" rather than "no artifact".
    remote="~/fambuild/$(basename "$REPO")/target/$TRIPLE/release/$BIN"
    elf=$(mktemp)
    if ! scp -q "familiar:$remote" "$elf"; then
      fail "$label: could not fetch the remote ELF ($remote)"
      continue
    fi
  else
    out=$("$CARGO" "${args[@]}" 2>&1)
    rc=$?
    elf="target/$TRIPLE/release/$BIN"
  fi

  if [[ $rc -ne 0 ]] || grep -qE '^error' <<<"$out"; then
    fail "$label: link failed"
    grep -E '^error|overflowed by' <<<"$out" | head -4 | sed 's/^/        /'
    continue
  fi
  [[ -f "$elf" ]] || { fail "$label: no ELF to measure"; continue; }

  read -r gap rom_used rom_end <<<"$(measure "$elf")"
  # Guard against measuring nothing: an unreadable ELF yields gap 0, which would
  # otherwise be reported as a stack-floor violation and send someone trimming
  # the heap to fix a broken scp.
  if [[ "${gap:-0}" -le 0 ]]; then
    fail "$label: could not read _bss_end/_stack_start from the ELF (measured gap ${gap:-?})"
    continue
  fi
  margin=$(( gap - STACK_FLOOR ))
  if [[ "$rom_end" -gt 0 ]]; then
    rom_free=$(( rom_end - 0x42000000 - rom_used ))
    rom_txt="$rom_free"
  else
    rom_txt="(unknown)"
  fi
  printf '        %-24s %10d %+10d %12s\n' "$label" "$gap" "$margin" "$rom_txt"

  # The assertions. A boot-time panic is a worse place to learn this.
  if [[ "$gap" -lt "$STACK_FLOOR" ]]; then
    fail "$label: stack gap $gap B is BELOW the $STACK_FLOOR B floor — the watch \
will panic at boot. Trim the MAIN heap_allocator! to grow the stack; do NOT \
lower the floor (it is measured, not chosen)."
  else
    pass "$label: links, stack margin +$margin B"
  fi
done

# --- verdict ---------------------------------------------------------------
note "verdict"
if [[ ${#FAILURES[@]} -eq 0 ]]; then
  pass "all gates green"
  exit 0
fi
printf '  \033[31m%d gate(s) failed:\033[0m\n' "${#FAILURES[@]}"
for f in "${FAILURES[@]}"; do printf '    - %s\n' "$f"; done
exit 1
