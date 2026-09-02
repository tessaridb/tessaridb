#!/bin/bash
# Is this node well?
#
# `bash` rather than `sh`, and the reason is `/dev/tcp`: it is a bash feature
# that dash does not have, and it is what lets this ask a real question over
# HTTP without putting an HTTP client into a database image.
#
# Two checks, and which one runs depends on what the node was asked to serve.
# When the HTTP surface is on, `/health` is the better question — it is answered
# by the node itself rather than by the kernel's accept queue, so a process that
# is listening but wedged fails it. When HTTP was turned off, an open port is
# the only question left, and saying so is more honest than reporting healthy
# because nothing was checked.
set -euo pipefail

if [ -n "${TESSARIDB_HTTP_ADDRESS:-}" ]; then
  port="${TESSARIDB_HTTP_ADDRESS##*:}"
  exec 3<>"/dev/tcp/127.0.0.1/${port}"
  # `Connection: close` so the node finishes the response rather than holding
  # the socket open for a second request that never comes; without it the read
  # below waits for the timeout on every probe.
  printf 'GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n' >&3
  # The status line, and only the status line. A body could say anything; the
  # code is the part the node commits to.
  #
  # Read into a variable rather than piped into `grep -q`, because `grep -q`
  # exits the moment it matches and `head` then dies of SIGPIPE — which under
  # `pipefail` fails the pipeline on exactly the successful case.
  status="$(head -n 1 <&3)"
  case "${status}" in
    *' 200 '*) exit 0 ;;
    *) echo "tessaridb: /health answered ${status:-nothing}" >&2 ; exit 1 ;;
  esac
elif [ -n "${TESSARIDB_ADDRESS:-}" ]; then
  port="${TESSARIDB_ADDRESS##*:}"
  exec 3<>"/dev/tcp/127.0.0.1/${port}"
else
  # Neither surface is on, so this container is not serving anything and the
  # entrypoint refused to start it. Reaching here at all is a defect.
  echo "tessaridb: neither surface is configured, so there is nothing to check" >&2
  exit 1
fi
