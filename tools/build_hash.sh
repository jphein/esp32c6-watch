#!/usr/bin/env bash
# build_hash.sh — the ONE implementation of "which source tree is this?".
#
# Prints a 7-hex-char identifier for the current working tree, with a trailing `*`
# when the tree is dirty, or nothing at all when there is no git. That string is
# the seed for the realm-sigil `forge` build name shown on the watch's SYSTEM page
# and printed by every flash/OTA path.
#
# ## Why this is a script and not three copies
#
# The value is needed in three places that cannot share code any other way:
#
#   * `build.rs`         — bakes it into the image (`BUILD_HASH` rustc-env)
#   * `tools/preflight.sh` — compares it against the `WSIGIL:` marker in the ELF,
#                            which is how a stale stamp gets caught
#   * `~/.local/bin/fambuild` — computes it on katana and exports it, because
#                            fambuild rsyncs the tree to familiar EXCLUDING
#                            `/.git`, so git is unavailable at the far end and a
#                            remote build would otherwise stamp `no-git`
#
# Three hand-copies of the same five commands is exactly the failure realm-sigil's
# Rust binding exists to end (smol hand-copied its corpus twice and both copies
# drifted with nothing to catch it). preflight compares build.rs's output against
# its own recomputation, so any drift between two of these would surface as a
# spurious STALE failure — a check that fails when the checker disagrees with the
# checked is worse than no check.
#
# ## Dirty trees get a CONTENT hash, deliberately
#
# Nearly every flash in this project is of an uncommitted tree. Reporting HEAD's
# hash for those would label every debug flash in a session identically, which is
# the exact bug the build sigil exists to fix. So a dirty tree is identified by
# hashing `HEAD + status + diff` — two dirty builds differ if and only if their
# sources differ.
#
# The flags are load-bearing, not decoration:
#   --binary        `git diff` renders a tracked binary as "Binary files differ"
#                   with NO content, so two different binaries would hash the same.
#                   None are tracked today, but Slint embeds resources from `ui/`.
#   --no-ext-diff / --no-textconv / -c diff.external=
#                   the diff TEXT is otherwise sensitive to the invoking user's git
#                   config, making "same sources -> same name" a per-host property.
#                   This project builds on both katana and familiar.
#   --untracked-files=all
#                   lists files inside untracked directories, not just the dir.
#
# Known and documented gap: the CONTENTS of a never-`git add`ed file reach the hash
# only as a filename, and `.gitignore`d files not at all. `git add` closes it.
#
# `git hash-object` is called WITHOUT `-w`: it computes the id and writes nothing.
# A build must not litter the user's object database.
set -uo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.." || exit 0

head_full=$(git rev-parse HEAD 2>/dev/null) || exit 0
[ -n "$head_full" ] || exit 0

status=$(git status --porcelain=v1 --untracked-files=all 2>/dev/null || true)
diff=$(git -c diff.external= diff --no-ext-diff --no-textconv --binary HEAD 2>/dev/null || true)

if [ -z "$status" ] && [ -z "$diff" ]; then
    printf '%s' "${head_full:0:7}"
else
    h=$(printf '%s\n%s\n%s' "$head_full" "$status" "$diff" \
        | git hash-object --stdin 2>/dev/null | cut -c1-7)
    # Dirty but unhashable: still say dirty rather than present HEAD as if it were
    # what got built.
    [ -n "$h" ] && printf '%s*' "$h" || printf '%s*' "${head_full:0:7}"
fi
