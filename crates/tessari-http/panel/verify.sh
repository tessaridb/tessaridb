#!/usr/bin/env bash
# The panel's build output is committed, so it can drift from its source in two
# directions: somebody edits crates/tessari-http/assets/ directly, or somebody
# edits the panel source and does not re-run the build. Neither fails anything.
#
# The build is deterministic, so running it must leave the working tree exactly
# as it found it. That is the whole check.
#
# There is no CI here and adding one is an owner decision, so this runs locally,
# at the batch boundary, whenever the panel is touched.
set -euo pipefail

panel="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(git -C "$panel" rev-parse --show-toplevel)"
assets="crates/tessari-http/assets"

if [ ! -d "$panel/node_modules" ]; then
  echo "panel: node_modules is missing — run 'npm install' in $panel" >&2
  exit 2
fi

# Hash the bytes, not `git status`. A status line says a file is modified and
# goes on saying it while the content changes underneath, so a hand-edit that
# the build then overwrites would look identical to no change at all.
fingerprint() { find "$root/$assets" -type f -exec shasum {} + | sed "s|$root/||" | sort; }

before="$(fingerprint)"

( cd "$panel" && npm run --silent typecheck && npm run --silent build )

after="$(fingerprint)"

if [ "$before" != "$after" ]; then
  echo "panel: the committed assets do not match what the source builds." >&2
  echo "       Re-run the build and commit the result, or revert the hand-edit." >&2
  git -C "$root" --no-pager diff --stat -- "$assets" >&2
  exit 1
fi

echo "panel: committed assets match the source"
