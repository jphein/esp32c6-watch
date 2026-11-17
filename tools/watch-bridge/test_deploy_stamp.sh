#!/usr/bin/env bash
# deploy.sh stamps the commit only when the shipped snapshot is HEAD's blob (2026-11-17). Stub ssh/scp, a temp repo.
set -uo pipefail
here="$(cd "$(dirname "$0")" && pwd)"; fails=0
T=$(mktemp -d "${XDG_RUNTIME_DIR:-/tmp}/wb-deploy-test.XXXXXX"); trap 'rm -rf "$T"' EXIT
mkdir -p "$T/bin" "$T/remote" "$T/repo/tools/watch-bridge"
cat > "$T/bin/ssh" <<S
#!/bin/sh
shift; cmd="\$*"; cmd=\$(printf '%s' "\$cmd" | sed "s#\\\$DEST_DIR#$T/remote#g; s#\\\$HOME/Projects/speech-to-cli#$T/remote#g; s#/tmp/#$T/remote/tmp-#g; s#sudo rc-service[^|]*||##; s#sudo systemctl restart [a-z-]*#true#")
case "\$cmd" in *curl*) echo '{"ok": true}'; exit 0;; esac
sh -c "\$cmd"
S
cat > "$T/bin/scp" <<S
#!/bin/sh
for a; do last=\$a; done; src=""; for a; do case "\$a" in -*) ;; "\$last") ;; *) src="\$a";; esac; done
dst=\${last#*:}; dst=\$(printf '%s' "\$dst" | sed "s#^/tmp/#$T/remote/tmp-#"); cp -r "\$src" "\$dst"
S
chmod +x "$T/bin/ssh" "$T/bin/scp"
cp "$here/deploy.sh" "$here/watch_bridge.py" "$T/repo/tools/watch-bridge/"
git -C "$T/repo" init -q && git -C "$T/repo" add -A && git -C "$T/repo" -c user.email=t@t -c user.name=t commit -qm init
H=$(git -C "$T/repo" rev-parse --short HEAD)
run() {   # the deploy must succeed and the remote file must be exactly the source shipped
  : > "$T/remote/watch_bridge.py"
  PATH="$T/bin:$PATH" REALM_SIGIL_PY="$T/none" bash "$T/repo/tools/watch-bridge/deploy.sh" --host x >"$T/out" 2>&1 \
    || { echo "FAIL: deploy exited non-zero: $(tail -3 "$T/out")"; fails=$((fails+1)); return 1; }
  cmp -s "$T/remote/watch_bridge.py" "$T/repo/tools/watch-bridge/watch_bridge.py" \
    || { echo "FAIL: remote bytes differ from the source"; fails=$((fails+1)); return 1; }
}
run; [ "$(cat "$T/remote/watch_bridge.hash" 2>/dev/null)" = "$H" ] && echo "ok: clean HEAD file is stamped $H" || { echo "FAIL: clean not stamped"; fails=$((fails+1)); }
echo "# local edit" >> "$T/repo/tools/watch-bridge/watch_bridge.py"; run
[ ! -e "$T/remote/watch_bridge.hash" ] && echo "ok: modified copy ships no stamp" || { echo "FAIL: modified copy stamped"; fails=$((fails+1)); }
git -C "$T/repo" checkout -q -- tools/watch-bridge/watch_bridge.py
git -C "$T/repo" rm -q --cached tools/watch-bridge/watch_bridge.py && git -C "$T/repo" -c user.email=t@t -c user.name=t commit -qm untrack
run; [ ! -e "$T/remote/watch_bridge.hash" ] && echo "ok: untracked copy ships no stamp" || { echo "FAIL: untracked copy stamped"; fails=$((fails+1)); }
# one HEAD resolution: the blob is checked against, and the stamp derived from, the same commit (xreview r4)
n=$(grep -v '^\s*#' "$here/deploy.sh" | grep -cE 'git [^|]*rev-parse[^|]*HEAD')
[ "$n" = 1 ] && echo "ok: deploy.sh resolves HEAD once" || { echo "FAIL: deploy.sh resolves HEAD $n times"; fails=$((fails+1)); }
[ "$fails" = 0 ] && echo "ALL PASS" || { echo "$fails FAILED"; exit 1; }
