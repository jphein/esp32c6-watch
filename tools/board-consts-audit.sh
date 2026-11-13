#!/usr/bin/env bash
# board-consts-audit.sh: list board constants nothing reads (#92).
#
# A declared constant with a doc comment looks wired. This lists every
# `pub const` in src/board/<board>.rs that has no reader anywhere else in src/
# (src/board/mod.rs counts; the board files themselves do not), ignoring
# references that sit inside `//` comments, since a comment is not a reader.
# Pin constants (`*_GPIO`, `*_GPIO_*`) are excluded: esp-hal pins are typed
# fields, so those are documentation of record by design (see board/mod.rs).
#
# Exit 0: none unread. Exit 1: prints "UNREAD <file> <NAME>" per finding.
set -u
cd "$(dirname "${BASH_SOURCE[0]}")/.." || exit 2
boards=(src/board/waveshare_c6.rs src/board/cyd_c5.rs src/board/esp32s3_cyd.rs)
for b in "${boards[@]}"; do [[ -f "$b" ]] || { echo "UNKNOWN: no $b" >&2; exit 2; }; done
# Positive control: the scan must find the constants it is about to judge.
total=0; bad=0
for b in "${boards[@]}"; do
  while read -r name; do
    total=$((total + 1))
    [[ "$name" =~ _GPIO($|_) ]] && continue
    # Readers: every .rs under src/ except the three board files, comments cut.
    n=$(grep -rn --include='*.rs' -w "$name" src \
          | grep -v -e '^src/board/waveshare_c6.rs:' -e '^src/board/cyd_c5.rs:' -e '^src/board/esp32s3_cyd.rs:' \
          | sed -E 's#^([^:]*:[0-9]+:)##; s#//.*$##' \
          | grep -cw "$name")
    if [[ "$n" -eq 0 ]]; then echo "UNREAD $b $name"; bad=$((bad + 1)); fi
  done < <(grep -oE '^\s*pub const [A-Z_0-9]+' "$b" | awk '{print $3}' | sort -u)
done
if [[ "$total" -lt 30 ]]; then
  echo "UNKNOWN: only $total board constants found; the scan is not reading the board files" >&2; exit 2
fi
echo "board-consts-audit: $total constants scanned, $bad unread (pin constants excluded by design)"
[[ "$bad" -eq 0 ]]
