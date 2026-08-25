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

# .cargo/config.toml is GITIGNORED (it holds WiFi/MQTT credentials), so every
# `git worktree add` checkout silently lacks it — and without it cargo builds
# for the HOST, which fails as 9 misleading esp-sync link errors AND silently
# adds ~21 host-only dependencies to Cargo.lock (a worktree agent nearly
# committed that poisoned lockfile as part of a "fix", 2026-08-25). Fail
# loudly here instead of letting either happen. fambuild is exempt only from
# the target half — the [env] credentials still come from this file — so the
# guard applies to both builders.
if [[ ! -f .cargo/config.toml ]]; then
  echo "preflight: no .cargo/config.toml in this checkout — it is gitignored and" >&2
  echo "does NOT follow git worktrees/clones. Without it cargo targets the HOST" >&2
  echo "(bogus esp-sync errors + host-only Cargo.lock poisoning). Copy it from" >&2
  echo "the main checkout (~/Projects/esp32c6-watch/.cargo/config.toml) or run" >&2
  echo "from the main checkout. NEVER commit it (credentials)." >&2
  exit 2
fi

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
# NOTE: main.rs carries one STACK_FLOOR per board (cfg-split); the values are
# equal today (the C5's is PROVISIONAL, inheriting the C6 number until its own
# radio-up soak). This gate runs the C6 matrix, so it takes the C6 value — but
# if the consts ever DIVERGE, taking "the first grep hit" would silently gate
# one board against the other's floor. Fail loudly instead: at that point this
# script needs a --board argument, not a lucky ordering.
FLOORS=$(grep -oE 'const STACK_FLOOR: usize = [0-9]+ \* 1024' src/main.rs \
              | grep -oE '[0-9]+ \* 1024' | awk '{print $1 * 1024}' | sort -u)
if [[ -z "${FLOORS:-}" ]]; then
  echo "preflight: could not parse STACK_FLOOR from src/main.rs" >&2
  exit 2
fi
if [[ $(wc -l <<<"$FLOORS") -gt 1 ]]; then
  echo "preflight: main.rs has DIVERGING per-board STACK_FLOOR values ($(tr '\n' ' ' <<<"$FLOORS")) — this script gates one board and must be told which (add --board before trusting any verdict)" >&2
  exit 2
fi
STACK_FLOOR=$FLOORS

# ROM region comes from the GENERATED memory.x (build.rs widens it per #67), so
# this tracks whatever the build actually linked against rather than a constant
# that goes stale. In fambuild mode the generated memory.x lives on the build
# host, so read it there. Falls back to reporting-only if it cannot be found.
#
# Emits "base length" (decimal) for the ROM region — BOTH parsed, because the
# base is a CHIP fact, not a constant of this script (#cyd-c5: the window was
# hardcoded 0x42000000..0x42800000, the C6's map; a C5 arm would have measured
# flash high-water against the WRONG window and still printed a verdict —
# nebula's finding: the ROM arm computed from zero matching sections and
# passed). Cached after the first call: one matrix run links one board, so the
# region cannot change mid-run.
ROM_REGION_CACHE=""
rom_region() {
  if [[ -z "$ROM_REGION_CACHE" ]]; then
    local line=""
    if [[ "$BUILDER" == "fambuild" ]]; then
      line=$(ssh familiar "grep -h -oE 'ROM : ORIGIN =[^,]+, LENGTH = [^,]+' \
            \$HOME/fambuild/$(basename "$REPO")/target/$TRIPLE/release/build/esp-hal-*/out/memory.x \
            2>/dev/null | head -1" 2>/dev/null)
    else
      local mx
      mx=$(find target -path '*esp-hal*' -name memory.x 2>/dev/null | head -1)
      [[ -n "$mx" ]] && line=$(grep -oE 'ROM : ORIGIN =[^,]+, LENGTH = [^,]+' "$mx" | head -1)
    fi
    [[ -z "$line" ]] && return 1
    # "ROM : ORIGIN = 0x42000000 + 0x20, LENGTH = 0x600000 - 0x20" — take the
    # first hex of each side; the alignment nudge cancels at region scale.
    local base len
    base=$(grep -oE '0x[0-9A-Fa-f]+' <<<"${line%%,*}" | head -1)
    len=$(grep -oE '0x[0-9A-Fa-f]+' <<<"${line#*LENGTH}" | head -1)
    [[ -z "$base" || -z "$len" ]] && return 1
    ROM_REGION_CACHE="$(( base )) $(( len ))"
  fi
  printf '%s\n' "$ROM_REGION_CACHE"
  return 0
}

# --- measurement -----------------------------------------------------------
# Emits "gap rom_used rom_end" for an ELF. rom_used/rom_end are 0 when the ROM
# region cannot be parsed — the caller treats that as report-only, never PASS.
measure() {
  local elf="$1" base=0 len=0
  read -r base len <<<"$(rom_region || echo '0 0')"
  "$NM" "$elf" 2>/dev/null | awk '
    / _bss_end$/     { b = strtonum("0x" $1) }
    / _stack_start$/ { s = strtonum("0x" $1) }
    END { printf "%d ", s - b }'
  "$RE" -S -W "$elf" 2>/dev/null | awk -v base="$base" -v len="$len" '
    /^  \[/ {
      a = strtonum("0x" $4); z = strtonum("0x" $6)
      if (base > 0 && a >= base && a < base + len && z > 0 && a + z > m) m = a + z
    }
    END { printf "%d %d\n", (m ? m - base : 0), (len ? base + len : 0) }'
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
# `story` combos are here for the reason in this file's header: a default-off
# feature's code is invisible until something links it, so leaving `story` out of
# this list would let it rot exactly as `tts` once did. `story,tts` is included
# because the two features' `.bss` is additive and the stack gap is the binding
# budget — testing them only in isolation would miss the case that overruns it.
# `story,tts,debug-console` is the thinnest combination in the tree and was
# ungated. The file's own justification for testing `tts,debug-console` — that the
# features' .bss is additive and the stack gap is the binding budget, so testing
# them only in isolation misses the case that overruns it — applies with more force
# once story is measured at 5,584 B of stack on its own. Estimated at ~+2,240 B of
# margin from single-feature deltas; that estimate is exactly what needs replacing
# with a link.
# NOTE feature lists here are ADDITIVE to `default`, and `tts` is now IN default
# (2026-07-29). So "" already includes tts, and the explicit "tts" / "story,tts" /
# "story,tts,debug-console" entries are duplicates of "" / "story" /
# "story,debug-console". Kept rather than pruned: they cost one link each and they
# are the combos someone will type by hand when checking whether tts is the thing
# that broke their budget. `--no-default-features` is how you measure WITHOUT it.
COMBOS=("" "debug-console" "tts" "tts,debug-console" "heap-hooks,heap-forensics" "story" "story,debug-console" "story,tts" "story,tts,debug-console")

# NEVER-SHIP features must not appear in the gated matrix. `story-stub-slots` forces
# the character page's worst frame so the 512 pool rung can be measured at all — it is
# not inert test code, it makes the watch permanently render the crash regime. A
# Cargo.toml comment cannot stop it drifting into the matrix; this can. The
# deploy-time twin lives in `watchctl` (`refuse_if_never_ship`), which is the check
# that matters, because preflight is run by a person and deploy is what puts bytes on
# a wrist.
for _c in "${COMBOS[@]}"; do
  case ",$_c," in
    *,story-stub-slots,*|*"story-stub-slots"*)
      echo "preflight: REFUSING — combo '$_c' contains a never-ship feature" >&2
      exit 2 ;;
  esac
done
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
  # The build STAMP must MATCH THE TREE, not merely be present.
  #
  # Presence alone was the weaker check. `crates/**` was missing from build.rs's
  # `rerun-if-changed` set, so an edit there produced new bytes wearing the
  # PREVIOUS label — and if `crates/` was the only dirt, git called the tree clean
  # and the watch reported a clean HEAD hash with no `*`. A dirty build wearing a
  # clean label.
  #
  # The fix declared the missing paths. This check exists because that fix cannot
  # be PROVEN complete: cargo's own dep-info (`<bin>.d`) reproduces the hole
  # mechanically, but it is blind to the vendored Slint renderer entirely — that
  # crate is `exclude`d from the workspace and reaches the build through
  # `[patch.crates-io]`, so it appears in no dep list at all. Also absent from
  # `.d`: linkall.x/memory.x, partitions.csv, Cargo.lock, rust-toolchain.toml, and
  # every dependency build script's own inputs (`.cargo/config.toml`'s ESP_LOG
  # feeds esp-println's build script and changes the binary leaving no trace).
  #
  # So ANY answer of the form "here is the list of inputs" has the same failure
  # mode as the original bug: right until someone adds an input. Comparing the
  # shipped bytes against the tree is immune to every input nobody enumerated,
  # and retires the class instead of the instance.
  #
  # DRIFT WARNING: the two commands below MUST stay identical to
  # `build.rs::stamp_build_sigil`. Only the HASH is compared, not the name — the
  # name is a pure function of the hash, so a hash match is sufficient and avoids
  # duplicating the word tables here.
  # ONE implementation, shared with build.rs and fambuild — see tools/build_hash.sh.
  # A recomputation that drifted from the baked-in one would fail this check
  # spuriously, which is worse than not checking.
  # FAIL CLOSED. The first version of this read
  #   stamp_want_hash="$(bash tools/build_hash.sh 2>/dev/null || true)"
  # which swallowed EVERY failure — script missing, script broken, git absent,
  # permission denied — into an empty string, and an empty string made the
  # comparison false. The gate was therefore skipped silently, with no output at
  # all. The worry had been that it would cry wolf and get disabled; the actual
  # behaviour was the opposite and worse: it would never bark and nobody would
  # notice it had stopped. The rest of this script fails closed on exactly this
  # class ("could not fetch the remote ELF"), and so does smol's equivalent, which
  # refuses to package an image whose stack was never measured.
  #
  # Three states, distinguished:
  #   script unusable        -> FAIL (we cannot know, so we do not pass)
  #   script says "no git"   -> a real answer; the image must agree ("unknown")
  #   script returns a hash  -> must match the image, or the stamp is stale
  # PIPE-FREE on purpose. The first version piped `strings -a | grep -q`, and
  # under `pipefail` that FAILS ON SUCCESS: grep -q exits at the first match and
  # closes the pipe, strings dies of SIGPIPE (141), and the pipeline reports
  # failure precisely because the marker was found early. Verified by bash -x on
  # the first real fambuild-mode run — where stamp_got had ALREADY extracted the
  # full stamp two lines above the "no stamp" verdict. grep -a reads the ELF
  # itself: no pipe, no SIGPIPE, no lie. (And a confession the trace forced: this
  # gate shipped bash -n'd but never EXECUTED in fambuild mode — the acceptance
  # run was its first, which is the exact unexecuted-gate sin it polices.)
  stamp_got=$(grep -a -o 'WSIGIL:[^|]*|[^|]*|v[0-9][0-9.]*' "$elf" 2>/dev/null | head -1)
  stamp_got_hash=$(printf '%s' "$stamp_got" | cut -d'|' -f2)
  if [[ ! -f "$REPO/tools/build_hash.sh" ]]; then
    fail "$label: tools/build_hash.sh is missing, so the build-stamp gate cannot \
run. It is the single implementation shared by build.rs, this script and fambuild \
— if it is absent here it is absent from the build too, and every image is \
stamped 'no-git'."
  elif ! stamp_want_hash=$(bash "$REPO/tools/build_hash.sh" 2>&1); then
    fail "$label: tools/build_hash.sh failed (${stamp_want_hash:-no output}), so \
the build stamp cannot be verified."
  elif [[ -z "$stamp_got_hash" ]]; then
    : # absence is caught by the WSIGIL presence check below, with its own message
  elif [[ -z "$stamp_want_hash" ]]; then
    # No git in this tree. That is a legitimate state (source tarball), but then
    # the image must SAY so rather than carry a hash from somewhere else.
    if [[ "$stamp_got_hash" != "unknown" ]]; then
      fail "$label: no git in this tree, yet the image claims hash \
'$stamp_got_hash' — it was built from a different tree than the one being measured."
    fi
  elif [[ "$stamp_want_hash" != "$stamp_got_hash" ]]; then
    fail "$label: build sigil is STALE — the image says '$stamp_got_hash' but the \
tree is '$stamp_want_hash'. If the image says 'unknown' and came from a REMOTE \
build, the cause is a missing WATCH_BUILD_HASH (fambuild excludes /.git, so \
build.rs cannot see git on the build host) — NOT a missing rerun-if-changed. \
Otherwise build.rs did not re-run for an input it does not declare; find it and \
add it to the list in stamp_build_sigil()."
  fi

  # The build STAMP must be present in every image. `#[used]` stops LLVM's DCE
  # but NOT the ELF linker's --gc-sections, and nothing passes that flag today —
  # so the marker survives only because no one garbage-collects sections. #67
  # (ROM ceiling) makes adding --gc-sections an attractive future diet, and the
  # failure would be silent: the flash tooling would just stop printing a sigil
  # and every image would look like a pre-stamp build. This is the one check that
  # cannot rot, because it reads the shipped bytes.
  if ! grep -aq 'WSIGIL:' "$elf"; then
    fail "$label: no WSIGIL build stamp in the ELF — \`#[used]\` on src/net/sigil.rs::BUILD_STAMP was defeated (a new --gc-sections link arg is the likely cause). Without it, flash/OTA cannot report which image it wrote."
  fi

  margin=$(( gap - STACK_FLOOR ))
  if [[ "$rom_end" -gt 0 ]]; then
    # rom_used is already region-relative (measure subtracts the parsed base),
    # so free is just LENGTH - used — no hardcoded base (#cyd-c5).
    read -r _rr_base _rr_len <<<"$(rom_region)"
    rom_free=$(( _rr_len - rom_used ))
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
