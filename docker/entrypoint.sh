#!/bin/sh
# Turn the container's environment into the command line the node expects.
#
# The node takes its store as an argument and its addresses as flags, which is
# right for somebody at a terminal and wrong for a container, where the thing
# that configures a process is the environment. This translates one into the
# other and does nothing else — every variable it reads is named in the
# Dockerfile, and anything it does not recognise is passed straight through.
#
# `sh` rather than `bash`, because everything here is POSIX and an interpreter
# is a dependency like any other.
set -eu

# Anything other than the bare word `serve` is a command in its own right, so
# `docker run tessaridb/tessaridb --version` and `docker run -it
# tessaridb/tessaridb --at other-host:9080` both work and neither goes anywhere
# near the serving defaults below. A flag first is the same case: it is a
# command line for the node, and prepending the binary is what lets it be
# written without one.
if [ "$#" -gt 0 ] && [ "$1" != "serve" ]; then
  exec tessaridb "$@"
fi

STORE="${TESSARIDB_STORE:-}"
ADDRESS="${TESSARIDB_ADDRESS:-}"
HTTP_ADDRESS="${TESSARIDB_HTTP_ADDRESS:-}"

# Refused rather than started. With neither surface the node would read
# statements from standard input, which in a container with no terminal means it
# reaches end of file and exits immediately — reported as a container that
# crashed on start, when what happened is that nobody asked it to listen.
if [ -z "${ADDRESS}" ] && [ -z "${HTTP_ADDRESS}" ]; then
  echo "tessaridb: TESSARIDB_ADDRESS and TESSARIDB_HTTP_ADDRESS are both empty," >&2
  echo "tessaridb: so this node was asked to serve nothing. Set at least one." >&2
  exit 2
fi

# Said once, on the way up, because the alternative is discovering it from a
# stranger's write. A store with no users is open; the node closes it when
# TESSARIDB_INITIAL_USER and TESSARIDB_INITIAL_PASSWORD are both set and the
# store has none yet, and says so itself at `info` — this warning is only for
# the case where nothing was asked for at all.
if [ -z "${TESSARIDB_INITIAL_USER:-}" ] && [ -z "${TESSARIDB_INITIAL_PASSWORD:-}" ]; then
  echo "tessaridb: no TESSARIDB_INITIAL_USER, so a store with no users stays OPEN" >&2
  echo "tessaridb: and will run anything for anybody who reaches it. Development only." >&2
fi

# Built as a list rather than a string, so a path with a space in it survives.
# Written as `if` rather than `[ … ] && set -- …`, because under `set -e` an
# and-or list whose test is false is a failing statement and would exit here —
# which is to say the short form would stop the container whenever the store is
# meant to be held in memory.
set --
if [ -n "${STORE}" ]; then
  set -- "${STORE}"
fi
if [ -n "${ADDRESS}" ]; then
  set -- "$@" --serve "${ADDRESS}"
fi
if [ -n "${HTTP_ADDRESS}" ]; then
  set -- "$@" --http "${HTTP_ADDRESS}"
fi

exec tessaridb "$@"
