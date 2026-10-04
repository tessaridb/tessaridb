#!/usr/bin/env bash
# Stores written by released builds, for `tests/parity.rs` (G059 C3).
#
# Each published image writes `write.tessariql` into a fresh store, reopens it
# to answer `read.tessariql` — which also flushes the write-ahead file into
# sorted files, so the fixture holds the released build's file format — and
# its answers are kept beside the store as the oracle this build is held to.
#
#   crates/tessari-cli/tests/fixtures/released/generate.sh 0.22.0-beta 0.23.0-beta …
#
# Needs Docker and the published images; nothing here builds an old tag.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
for version in "$@"; do
  work="$(mktemp -d)"
  cp "$here/write.tessariql" "$here/read.tessariql" "$work/"
  chmod -R a+rwX "$work"
  image="tessaridb/tessaridb:${version}"
  docker run --rm -v "$work:/var/lib/tessaridb" "$image" \
    /var/lib/tessaridb/store -f /var/lib/tessaridb/write.tessariql > /dev/null
  docker run --rm -v "$work:/var/lib/tessaridb" "$image" \
    /var/lib/tessaridb/store -f /var/lib/tessaridb/read.tessariql > "$here/${version}.answers"
  rm -rf "${here:?}/${version:?}"
  mkdir -p "$here/${version}"
  # The engine's own LOG, the lock and an emptied write-ahead file are not
  # the store; everything else is copied as the released build left it.
  for file in "$work/store"/*; do
    name="$(basename "$file")"
    case "$name" in
      LOG|LOG.old.*|LOCK) continue ;;
    esac
    if [[ "$name" == *.log && ! -s "$file" ]]; then continue; fi
    cp "$file" "$here/${version}/"
  done
  rm -rf "${work:?}"
  echo "${version}: $(ls "$here/${version}" | wc -l | tr -d ' ') files, $(du -sk "$here/${version}" | cut -f1) KiB"
done
