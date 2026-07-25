#!/usr/bin/env bash
# watchctl_selftest.sh — hardware pass for watchctl's MUTATING paths.
#
# Run by the DEVICE OWNER (agents without a hardware pass: hand this to the
# orchestrator — see docs/debugging.md "Rule for agents without hardware
# access"). It resets the watch several times and, if an ELF is given,
# rewrites the booting slot.
#
#   tools/watchctl_selftest.sh <sigil> [elf] [--with-console]
#
#   <sigil>          eldritch-lantern | mythic-throne
#   [elf]            optional firmware ELF: exercises `deploy` (writes the
#                    BOOTING slot — flash wear + a reboot; use a known-good
#                    build, ideally --features debug-console)
#   --with-console   also run `test` suite + `console` ping (requires the
#                    RUNNING build to have the debug-console feature)
#
# Read-only paths (list/logs/ota-status/endpoint-absent) are covered by the
# build-time verification; this script is about the paths that touch state.
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WATCHCTL="$HERE/watchctl"
SIGIL="${1:-}"
[ -n "$SIGIL" ] || { echo "usage: $0 <sigil> [elf] [--with-console]" >&2; exit 2; }
shift
ELF=""
WITH_CONSOLE=0
for a in "$@"; do
    case "$a" in
        --with-console) WITH_CONSOLE=1 ;;
        *) ELF="$a" ;;
    esac
done

PASS=0; FAIL=0
step() {  # step <name> <expected-exit> <cmd...>
    local name="$1" want="$2"; shift 2
    echo
    echo "=== $name"
    echo "    \$ $*"
    "$@"; local rc=$?
    if [ "$rc" -eq "$want" ]; then
        echo "--- PASS ($name, exit $rc)"; PASS=$((PASS+1))
    else
        echo "--- FAIL ($name: exit $rc, wanted $want)"; FAIL=$((FAIL+1))
    fi
    return 0
}

echo "watchctl self-test on $SIGIL — $(date '+%F %T')"
echo "watchctl: $WATCHCTL"

# 0. Presence (read-only sanity before mutating anything).
step "list shows the fleet"           0 "$WATCHCTL" list
step "watch is on USB (json list ok)" 0 "$WATCHCTL" --json list

# 1. reset — verified reset, banner seen.
step "reset (verified boot banner)"   0 "$WATCHCTL" reset "$SIGIL"

# 2. slot — booting slot + fingerprint (another reset).
step "slot (booting slot + fingerprint)" 0 "$WATCHCTL" slot "$SIGIL"

# 3. logs --reset — the capture-before-reset race: MUST see a boot burst.
step "logs --reset captures the boot burst" 0 \
    "$WATCHCTL" logs "$SIGIL" --reset --seconds 8

# 4. recover — on a healthy watch rung 1 must succeed (exit 0).
step "recover (rung 1 on a healthy watch)" 0 "$WATCHCTL" recover "$SIGIL"

# 5. deploy — only when an ELF was provided.
if [ -n "$ELF" ]; then
    step "deploy ELF into the BOOTING slot" 0 "$WATCHCTL" deploy "$SIGIL" "$ELF"
    step "slot fingerprint after deploy"    0 "$WATCHCTL" slot "$SIGIL"
else
    echo
    echo "=== deploy SKIPPED (no ELF given) — rerun with an ELF to cover it"
fi

# 6. debug-console paths (need a debug-console build running).
if [ "$WITH_CONSOLE" -eq 1 ]; then
    step "test suite (debug-console)" 0 "$WATCHCTL" test "$SIGIL"
    step "test hotpaths"              0 "$WATCHCTL" test "$SIGIL" hotpaths
else
    echo
    echo "=== console/test SKIPPED (pass --with-console when the running"
    echo "    build has the debug-console feature)"
fi

# 7. Infra read-backs (mutate nothing, but exercise the ssh/MQTT plumbing
#    from THIS machine's config).
step "ota-status (journal + retained announce)" 0 "$WATCHCTL" ota-status
# endpoint: exit 2 (= no retained endpoint) is CORRECT until the firmware
# debug server lands; flip the expected code to 0 after that task ships.
step "endpoint (expected absent until fw lands)" 2 "$WATCHCTL" endpoint "$SIGIL"

# NOT covered here: flash-full (provisioning-only — run manually on a scratch
# device) and the USBDEVFS/power-cycle rungs (need a genuinely wedged port).

echo
echo "================================================"
echo "watchctl self-test: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
