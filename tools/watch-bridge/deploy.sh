#!/usr/bin/env bash
# Deploy watch_bridge.py to the LAN bridge host.
#
# TARGET: the watch-bridge VM on ubox0 (jp@10.0.6.164, OpenRC) since 2026-10-07. The watch still dials
# 10.0.11.11:8090 (ubox0's VLAN-11 leg); ubox0 proxies it to the VM (~/Projects/ubox0/vms/watch-bridge/).
# That address is hardcoded in the firmware as `voice_stt::default_bridge_ip()`
# and reused by `voice_tts`, so it is not a preference — change one and you must
# change the other.
#
# The bridge serves BOTH directions:
#   POST /stt  raw mono 16 kHz s16le PCM  -> {"text": "..."}      (mic -> Azure)
#   POST /tts  {"text": "..."}            -> raw mono 16 kHz PCM  (Azure -> speaker)
#
# SECRETS: none live here. The bridge imports speech-to-cli's `state.load_config()`
# and reads the Azure key from ~/.config/speech-to-cli/config.json on the BRIDGE
# HOST at runtime. Nothing in this directory contains a credential, and nothing
# here should ever be given one — that is the whole reason the watch (which is
# plain-HTTP only, no TLS) talks to this bridge instead of to Azure.
#
# Usage:
#   ./deploy.sh              # deploy + restart + health-check on ubox0
#   ./deploy.sh --host NAME  # another host (e.g. familiar, for a staging run)
#   ./deploy.sh --dry-run    # diff what WOULD change; touch nothing
set -euo pipefail

HOST=jp@10.0.6.164
DEST_DIR='$HOME/Projects/speech-to-cli'      # expanded remotely, not here
SERVICE=watch-bridge
PORT=8090
DRY_RUN=0
SRC="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/watch_bridge.py"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --host) HOST="$2"; shift 2 ;;
    --dry-run) DRY_RUN=1; shift ;;
    -h|--help) sed -n '2,20p' "$0"; exit 0 ;;
    *) echo "unknown arg: $1" >&2; exit 22 ;;
  esac
done

[[ -f "$SRC" ]] || { echo "missing $SRC" >&2; exit 1; }
python3 -m py_compile "$SRC" || { echo "refusing to deploy: syntax error" >&2; exit 1; }

# Refuse to ship a file that somehow acquired a credential.
if grep -qEi 'Ocp-Apim-Subscription-Key: *[A-Za-z0-9]|key *= *"[A-Za-z0-9]{16}' "$SRC"; then
  echo "refusing to deploy: looks like a hardcoded secret in $SRC" >&2
  exit 1
fi

echo "== diff vs $HOST =="
if ssh "$HOST" "cat $DEST_DIR/watch_bridge.py" 2>/dev/null > /tmp/wb_remote.py; then
  diff -u /tmp/wb_remote.py "$SRC" && echo "  (identical)" || true
else
  echo "  (no existing copy on $HOST)"
fi

if [[ $DRY_RUN -eq 1 ]]; then
  echo "== dry run: nothing changed =="
  exit 0
fi

echo "== deploying to $HOST =="
# Back up first — this host serves the live voice path.
ssh "$HOST" "cp -f $DEST_DIR/watch_bridge.py $DEST_DIR/watch_bridge.py.bak 2>/dev/null || true"
# the stamp names the code beside it: invalidate it first, so a failed stamp step or a rollback reports "dev",
# never the previous (or the newer) commit for the code that is actually running
ssh "$HOST" "rm -f $DEST_DIR/watch_bridge.hash"
# scp does NOT expand $HOME in the destination (the ssh calls above DO, because
# they run through a remote shell — which is why the cat/cp worked and this did
# not, creating a literal "$HOME" directory). Stage in /tmp, then move it into
# place with a shell that can expand the path.
# one snapshot is checked, shipped and stamped: the bytes deployed are the bytes judged
SNAP=$(mktemp "${XDG_RUNTIME_DIR:-/tmp}/watch_bridge.py.XXXXXX") && cp "$SRC" "$SNAP" || { echo "cannot snapshot $SRC" >&2; exit 1; }
trap 'rm -f "$SNAP"' EXIT
scp -q "$SNAP" "$HOST:/tmp/watch_bridge.py.new"
ssh "$HOST" "mv -f /tmp/watch_bridge.py.new $DEST_DIR/watch_bridge.py"
# realm-sigil /api/version: the copy has no .git, so ship the commit and the realm_sigil package beside it
# (best effort: without them the route answers 503 and /health is unaffected)
SIGIL_PKG="${REALM_SIGIL_PY:-$HOME/Projects/realm-sigil/python}/realm_sigil"
stamp=$(mktemp "${XDG_RUNTIME_DIR:-/tmp}/watch_bridge.hash.XXXXXX") || stamp=""
# stamp only when the shipped snapshot IS HEAD's blob for this path (an untracked or modified copy keeps no stamp and
# reports "dev"/dirty; the old stamp was removed above)
REPO_TOP=$(git -C "$(dirname "$SRC")" rev-parse --show-toplevel 2>/dev/null || true)
REL=${SRC#"$REPO_TOP"/}
COMMIT=$( [[ -n "$REPO_TOP" ]] && git -C "$REPO_TOP" rev-parse --verify --quiet "HEAD^{commit}" 2>/dev/null || true)
head_blob=$( [[ -n "$COMMIT" ]] && git -C "$REPO_TOP" rev-parse --verify --quiet "$COMMIT:$REL" 2>/dev/null || true)
snap_blob=$( [[ -n "$REPO_TOP" ]] && git -C "$REPO_TOP" hash-object "$SNAP" 2>/dev/null || true)
if [[ -n "$stamp" && -n "$head_blob" && "$head_blob" == "$snap_blob" ]] \
   && git -C "$REPO_TOP" rev-parse --short=7 "$COMMIT" > "$stamp" 2>/dev/null \
   && scp -q "$stamp" "$HOST:/tmp/watch_bridge.hash.new" \
   && ssh "$HOST" "mv -f /tmp/watch_bridge.hash.new $DEST_DIR/watch_bridge.hash"; then :
else echo "note: no build stamp shipped; /api/version will say dev" >&2; fi
[[ -n "$stamp" ]] && rm -f "$stamp" || true
if [[ -f "$SIGIL_PKG/__init__.py" ]]; then
  ssh "$HOST" "rm -rf /tmp/realm_sigil.new" && scp -qr "$SIGIL_PKG" "$HOST:/tmp/realm_sigil.new" \
    && ssh "$HOST" "rm -rf $DEST_DIR/realm_sigil && mv /tmp/realm_sigil.new $DEST_DIR/realm_sigil" \
    || echo "note: realm_sigil not shipped; /api/version will answer 503" >&2
else
  echo "note: $SIGIL_PKG missing; /api/version will answer 503" >&2
fi

echo "== restarting $SERVICE =="
ssh "$HOST" "sudo rc-service $SERVICE restart 2>/dev/null || sudo systemctl restart $SERVICE" || {
  echo "restart failed — rolling back" >&2
  ssh "$HOST" "cp -f $DEST_DIR/watch_bridge.py.bak $DEST_DIR/watch_bridge.py && rm -f $DEST_DIR/watch_bridge.hash && sudo rc-service $SERVICE restart 2>/dev/null || sudo systemctl restart $SERVICE" || true
  exit 1
}

echo "== health =="
sleep 2
if ! ssh "$HOST" "curl -fsS --max-time 10 http://127.0.0.1:$PORT/health"; then
  echo; echo "health check FAILED — rolling back" >&2
  ssh "$HOST" "cp -f $DEST_DIR/watch_bridge.py.bak $DEST_DIR/watch_bridge.py && rm -f $DEST_DIR/watch_bridge.hash && sudo rc-service $SERVICE restart 2>/dev/null || sudo systemctl restart $SERVICE" || true
  exit 5
fi
echo
echo "== ok: /stt + /tts live on $HOST:$PORT =="
