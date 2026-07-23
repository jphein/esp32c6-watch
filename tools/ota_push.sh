#!/usr/bin/env bash
# ota_push.sh — push a firmware update to the watch over WiFi, zero-touch.
#
#   tools/ota_push.sh                  # stamp + build + image + upload + announce
#   tools/ota_push.sh --announce-only  # re-announce the already-uploaded image
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

ANNOUNCE_TOPIC="watch/ota/announce"
OTA_DEST="ubox0:/home/jp/watch-ota/watch.bin"

if [ "${1:-}" = "--announce-only" ]; then
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
    (cd "$ROOT" && fambuild build --release --bin esp32c6-watch)

    # 3. ELF -> app image. fambuild keeps target/ on familiar, so fetch the ELF.
    WORKTREE_NAME="$(basename "$ROOT")"
    ELF_REMOTE="fambuild/$WORKTREE_NAME/target/riscv32imac-unknown-none-elf/release/esp32c6-watch"
    TMP="$(mktemp -d)"
    trap 'rm -rf "$TMP"' EXIT
    scp -q "familiar:$ELF_REMOTE" "$TMP/esp32c6-watch.elf"
    espflash save-image --chip esp32c6 "$TMP/esp32c6-watch.elf" "$TMP/watch.bin"

    # 4. Publish the image to the OTA HTTP server.
    scp -q "$TMP/watch.bin" "$OTA_DEST"
    echo "ota_push: image uploaded -> $OTA_DEST ($(stat -c%s "$TMP/watch.bin") bytes)"
fi

# 5. RETAINED announce: the watch triggers only if <epoch> > its running
#    OTA_BUILD (monotonic gate), so re-announces and the post-reboot retained
#    copy are harmless.
mosquitto_pub -h "$BROKER_HOST" -p "$BROKER_PORT" \
    ${MQTT_USER:+-u "$MQTT_USER"} ${MQTT_PASS:+-P "$MQTT_PASS"} \
    -r -t "$ANNOUNCE_TOPIC" -m "OTA|$EPOCH|$OTA_URL"
echo "ota_push: retained announce published: OTA|$EPOCH|$OTA_URL"
echo "ota_push: the watch updates on its next MQTT window (reboot it, or open Climate/Energy)"
