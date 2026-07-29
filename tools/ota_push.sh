#!/usr/bin/env bash
# ota_push.sh — push a firmware update to the watch over WiFi, zero-touch.
#
#   tools/ota_push.sh                  # stamp + build + image + upload + announce
#   tools/ota_push.sh --features story # build WITH an opt-in feature. REQUIRED if
#                                      # the target watch is running one, or the
#                                      # OTA silently downgrades it away.
#   tools/ota_push.sh --announce-only  # re-announce the already-uploaded image
#   tools/ota_push.sh --target <sigil> # announce to ONE watch only, via its
#                                      # per-watch topic watch/<sigil>/ota (#34)
#                                      # e.g. --target eldritch-lantern
#                                      # (combines with --announce-only)
#   tools/ota_push.sh --clear          # REMOVE the retained announce (empty
#                                      # retained publish). Use before bench
#                                      # sessions with cable-flashed dev builds
#                                      # (OTA_BUILD=0 accepts ANY announce and
#                                      # zero-touch replaces your build — #55).
#                                      # (combines with --target)
#
# Flow (see docs/ota-deploy.md "Push OTA"):
#   1. Stamp OTA_BUILD=<unix-seconds> into .cargo/config.toml [env] (gitignored)
#      so the new image carries its own build id (the watch's monotonicity gate).
#   2. fambuild build --release --bin esp32c6-watch   (builds on `familiar`)
#   3. Fetch the ELF from familiar, espflash save-image -> watch.bin (app image).
#   4. scp watch.bin ubox0:/home/jp/watch-ota/watch.bin  (the OTA HTTP server).
#   5. mosquitto_pub a RETAINED announce `OTA|<epoch>|<OTA_URL>` to
#      watch/ota/announce — the watch picks it up on its next MQTT window
#      (boot burst or an open Climate/Energy session) and updates itself.
#
# Credentials/config are READ from the gitignored .cargo/config.toml — never
# hardcoded here (this script is committed).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CFG="$ROOT/.cargo/config.toml"
[ -f "$CFG" ] || { echo "ota_push: missing $CFG (gitignored — copy .cargo/config.example.toml and fill it in)" >&2; exit 2; }

# Read KEY="value" from the [env] section (simple grep — the file is flat).
cfg_get() { sed -n "s/^${1}=\"\(.*\)\"/\1/p" "$CFG" | head -1; }

MQTT_BROKER="$(cfg_get MQTT_BROKER)"
MQTT_USER="$(cfg_get MQTT_USER)"
MQTT_PASS="$(cfg_get MQTT_PASS)"
OTA_URL="$(cfg_get OTA_URL)"
[ -n "$MQTT_BROKER" ] || { echo "ota_push: MQTT_BROKER not set in $CFG" >&2; exit 2; }
[ -n "$OTA_URL" ] || { echo "ota_push: OTA_URL not set in $CFG" >&2; exit 2; }
BROKER_HOST="${MQTT_BROKER%%:*}"
BROKER_PORT="${MQTT_BROKER##*:}"

OTA_DEST="ubox0:/home/jp/watch-ota/watch.bin"

# --- args: --announce-only, --clear, --target <sigil> ------------------------
ANNOUNCE_ONLY=0
CLEAR=0
TARGET=""
while [ $# -gt 0 ]; do
    case "$1" in
        --announce-only) ANNOUNCE_ONLY=1 ;;
        --features)
            FEATURES="$2"
            [ -n "$FEATURES" ] || { echo "ota_push: --features needs a value" >&2; exit 2; }
            shift ;;
        --clear) CLEAR=1 ;;
        --target)
            TARGET="${2:-}"
            [ -n "$TARGET" ] || { echo "ota_push: --target needs a sigil (e.g. eldritch-lantern)" >&2; exit 2; }
            shift ;;
        *) echo "ota_push: unknown argument: $1" >&2; exit 2 ;;
    esac
    shift
done

# Fleet topic by default; per-watch topic (watch/<sigil>/ota, both watches'
# firmware subscribes its own alongside the fleet topic) when targeted. The
# sigil for each watch is printed at boot: `[SIGIL] <name> (node id N, ...)`,
# and shown on the System page.
ANNOUNCE_TOPIC="watch/ota/announce"
if [ -n "$TARGET" ]; then
    ANNOUNCE_TOPIC="watch/$TARGET/ota"
    echo "ota_push: targeting ONE watch: $TARGET ($ANNOUNCE_TOPIC)"
fi

if [ "$CLEAR" = 1 ]; then
    # Empty retained publish deletes the retained announce; the firmware's
    # handle_announce treats an empty payload as a retained-clear, not an
    # announce. (Same ubox0 publish path as below — see that comment.)
    ssh ubox0 "mosquitto_pub -h '$BROKER_HOST' -p '$BROKER_PORT' \
        ${MQTT_USER:+-u '$MQTT_USER'} ${MQTT_PASS:+-P '$MQTT_PASS'} \
        -r -n -t '$ANNOUNCE_TOPIC'"
    echo "ota_push: retained announce CLEARED on $ANNOUNCE_TOPIC"
    exit 0
fi

if [ "$ANNOUNCE_ONLY" = 1 ]; then
    # Re-announce the current stamped build (image must already be uploaded).
    EPOCH="$(cfg_get OTA_BUILD)"
    [ -n "$EPOCH" ] || { echo "ota_push: no OTA_BUILD in $CFG — run a full push first" >&2; exit 2; }
else
    EPOCH="$(date +%s)"

    # 1. Stamp OTA_BUILD into [env] (replace an existing line, else append
    #    directly under the [env] header).
    if grep -q '^OTA_BUILD=' "$CFG"; then
        sed -i "s|^OTA_BUILD=.*|OTA_BUILD=\"$EPOCH\"|" "$CFG"
    else
        sed -i "/^\[env\]/a # Push-OTA build id (unix-seconds), stamped by tools/ota_push.sh.\nOTA_BUILD=\"$EPOCH\"" "$CFG"
    fi
    echo "ota_push: stamped OTA_BUILD=$EPOCH"

    # 2. Build on familiar (fambuild syncs this worktree incl. .cargo/config.toml).
    # --features MATTERS AND ITS ABSENCE IS A TRAP. Opt-in features (`story`,
    # `tts`) are OFF by default, so a plain push builds WITHOUT them and the
    # zero-touch OTA then SILENTLY REMOVES a feature the watch was running. That
    # is a downgrade the wearer never asked for and cannot see coming.
    #
    # Caught the first time an OTA was attempted for a watch running `--features
    # story`: the push would have replaced it with a default image and taken the
    # Story app away mid-use.
    if [ -n "$FEATURES" ]; then
        echo "ota_push: building WITH --features $FEATURES"
        (cd "$ROOT" && fambuild build --release --bin esp32c6-watch --features "$FEATURES")
    else
        echo "ota_push: building with DEFAULT features (no --features given)"
        echo "ota_push: NOTE if the target watch runs an opt-in feature (story/tts),"
        echo "ota_push:      this push will REMOVE it. Pass --features to keep it."
        (cd "$ROOT" && fambuild build --release --bin esp32c6-watch)
    fi

    # 3. ELF -> app image. fambuild keeps target/ on familiar, so fetch the ELF.
    WORKTREE_NAME="$(basename "$ROOT")"
    ELF_REMOTE="fambuild/$WORKTREE_NAME/target/riscv32imac-unknown-none-elf/release/esp32c6-watch"
    TMP="$(mktemp -d)"
    trap 'rm -rf "$TMP"' EXIT
    scp -q "familiar:$ELF_REMOTE" "$TMP/esp32c6-watch.elf"
    # espflash may be absent from a non-login shell's PATH; fall back to cargo bin.
    ESPFLASH="$(command -v espflash || echo "$HOME/.cargo/bin/espflash")"
    "$ESPFLASH" save-image --chip esp32c6 --flash-size 16mb --partition-table "$ROOT/partitions.csv" "$TMP/esp32c6-watch.elf" "$TMP/watch.bin"
    # Slot-fit gate: the A/B app slots are 4,128,768 B; refuse early with a
    # clear message instead of espflash's mid-flow error (and keep margin).
    BIN_SIZE=$(stat -c%s "$TMP/watch.bin")
    if [ "$BIN_SIZE" -gt 6291456 ]; then
        echo "ota_push: ABORT - image ${BIN_SIZE}B exceeds the 6291456B OTA slot (see the partition-grow issue)" >&2
        exit 3
    fi
    echo "ota_push: image ${BIN_SIZE}B fits the slot ($((6291456 - BIN_SIZE))B headroom)"

    # Read the build sigil OUT OF THE IMAGE (marker: src/net/sigil.rs BUILD_STAMP)
    # rather than recomputing it from the working tree. The tree may have moved on
    # since the remote build, and a label that disagrees with the bytes is worse
    # than none. This line is what to compare against the watch's SYSTEM page.
    # `strings` splits on the NUL terminator, which a portable grep pattern
    # cannot express; the marker is NUL-terminated for exactly this reason.
    SIGIL_STAMP=$(strings -a "$TMP/watch.bin" | grep -o 'WSIGIL:.*' | head -1 | cut -c8-)
    if [ -n "$SIGIL_STAMP" ]; then
        echo "ota_push: ============================================"
        echo "ota_push:  BUILD  $(echo "$SIGIL_STAMP" | cut -d'|' -f1)"
        echo "ota_push:  HASH   $(echo "$SIGIL_STAMP" | cut -d'|' -f2)"
        echo "ota_push:  VER    $(echo "$SIGIL_STAMP" | cut -d'|' -f3)"
        echo "ota_push: ---- compare on SYSTEM page / Settings p4 ----"
    else
        # An image with no marker predates this change (or LTO dropped it) — say
        # so, because silence would read as "sigil matches".
        echo "ota_push: WARNING no WSIGIL marker in the image — it predates the build stamp;"
        echo "ota_push:         the watch's BUILD row cannot be compared for this push."
    fi

    # 4. Publish the image to the OTA HTTP server.
    scp -q "$TMP/watch.bin" "$OTA_DEST"
    echo "ota_push: image uploaded -> $OTA_DEST ($(stat -c%s "$TMP/watch.bin") bytes)"
fi

# 5. RETAINED announce: the watch triggers only if <epoch> > its running
#    OTA_BUILD (monotonic gate), so re-announces and the post-reboot retained
#    copy are harmless FOR STAMPED BUILDS. A cable-flashed dev build
#    (OTA_BUILD=0) accepts any retained announce and zero-touch replaces
#    itself on its next MQTT window — run `tools/ota_push.sh --clear` before
#    bench-debugging. (#55 post-mortem: with the pre-fix firmware this
#    combination plus stale otadata self-overwrote the RUNNING slot and
#    bricked the watch; the booted_partition fix removes the brick, the
#    surprise self-update remains.)
#    Published FROM ubox0 (VLAN-11, same subnet as the broker's reachable leg):
#    publishing from katana (VLAN-6) connects but stalls mid-handshake
#    ("Keepalive exceeded") — an asymmetric-routing quirk on the katana→VLAN-11
#    path. ubox0 already hosts the image, so the announce rides the same ssh.
ssh ubox0 "mosquitto_pub -h '$BROKER_HOST' -p '$BROKER_PORT' \
    ${MQTT_USER:+-u '$MQTT_USER'} ${MQTT_PASS:+-P '$MQTT_PASS'} \
    -r -t '$ANNOUNCE_TOPIC' -m 'OTA|$EPOCH|$OTA_URL'"
echo "ota_push: retained announce published: OTA|$EPOCH|$OTA_URL"
echo "ota_push: the watch updates on its next MQTT window (reboot it, or open Climate/Energy)"
