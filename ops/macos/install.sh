#!/bin/sh
# install.sh — build the node, put it somewhere it can be run from anywhere, and register
# it as a launchd agent so it is simply up.
#
#   ops/macos/install.sh                    # build from this checkout and install
#   ops/macos/install.sh --address 127.0.0.1:39500
#
# Nothing here needs privilege. Everything it writes is under $HOME, and uninstall.sh
# undoes all of it.

set -eu

PREFIX="${PREFIX:-$HOME/.local/bin}"
HOME_DIR="${TESSARIDB_HOME:-$HOME/.tessaridb}"
ADDRESS="127.0.0.1:39500"
HTTP_ADDRESS=""
BUILD="yes"

LABEL="com.tessaridb.node"
PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"
DOMAIN="gui/$(id -u)"

REPO="$(cd "$(dirname "$0")/../.." && pwd)"

die() { echo "install: $1" >&2; exit 1; }

while [ $# -gt 0 ]; do
	case "$1" in
		--address) ADDRESS="${2:?--address needs host:port}"; shift 2 ;;
		--http)    HTTP_ADDRESS="${2:?--http needs host:port}"; shift 2 ;;
		--prefix)  PREFIX="${2:?--prefix needs a path}"; shift 2 ;;
		--home)    HOME_DIR="${2:?--home needs a path}"; shift 2 ;;
		--no-build) BUILD="no"; shift ;;
		*) die "unknown argument: $1" ;;
	esac
done

case "$(uname -s)" in
	Darwin) ;;
	*) die "this installs a launchd agent, which is macOS. On Linux the equivalent is a systemd unit and it is not written yet" ;;
esac

HOST="${ADDRESS%:*}"
PORT="${ADDRESS##*:}"
[ "$HOST" != "$ADDRESS" ] || die "--address wants host:port, got '$ADDRESS'"

if [ "$BUILD" = "yes" ]; then
	echo "building the node…"
	( cd "$REPO" && cargo build --release )
fi

BINARY="$REPO/target/release/tessaridb"
[ -x "$BINARY" ] || die "$BINARY is missing — build first, or drop --no-build"

mkdir -p "$PREFIX" "$HOME_DIR/data" "$HOME_DIR/logs" "$HOME/Library/LaunchAgents"

echo "installing into ${PREFIX}…"
install -m 0755 "$BINARY" "$PREFIX/tessaridb"
install -m 0755 "$REPO/ops/macos/tessaridbctl" "$PREFIX/tessaridbctl"

CONFIG="$HOME_DIR/config.env"
if [ -f "$CONFIG" ]; then
	echo "keeping the settings already at $CONFIG"
else
	cat > "$CONFIG" <<CONFIGURATION
# The node this machine keeps running. Read by tessaridbctl and by the launchd agent.
# Change a value and restart: tessaridbctl restart
#
# The names are the ones the container path uses, so a node configured here and a node
# configured there are configured the same way.

TESSARIDB_BIN="$PREFIX/tessaridb"
TESSARIDB_STORE="$HOME_DIR/data"
TESSARIDB_ADDRESS="$ADDRESS"
TESSARIDB_LOG_DIR="$HOME_DIR/logs"

# Turn the HTTP surface on and health becomes a question the node answers rather than one
# the kernel's accept queue answers. Off by default: it is a second port on the machine.
#TESSARIDB_HTTP_ADDRESS="$HOST:$((PORT + 1))"
CONFIGURATION
	[ -n "$HTTP_ADDRESS" ] && printf 'TESSARIDB_HTTP_ADDRESS="%s"\n' "$HTTP_ADDRESS" >> "$CONFIG"
	echo "wrote $CONFIG"
fi

# The store is not optional in practice. A node started without one keeps everything in
# memory and loses it when it stops, and nothing says so at the time.
{
	echo "installed:  $(date -u '+%Y-%m-%dT%H:%M:%SZ')"
	echo "engine:     $("$PREFIX/tessaridb" --version 2>/dev/null || echo unknown)"
	echo "built-from: $REPO @ $(git -C "$REPO" rev-parse --short HEAD 2>/dev/null || echo 'not a checkout')"
	echo "address:    $ADDRESS"
	echo "store:      $HOME_DIR/data"
} > "$HOME_DIR/INSTALLED"

sed -e "s|@CTL@|$PREFIX/tessaridbctl|g" \
    -e "s|@HOME_DIR@|$HOME_DIR|g" \
    -e "s|@LOG_DIR@|$HOME_DIR/logs|g" \
    "$REPO/ops/macos/$LABEL.plist.template" > "$PLIST"
echo "wrote $PLIST"

launchctl bootout "$DOMAIN/$LABEL" >/dev/null 2>&1 || true

# Tearing an agent down is asynchronous, so a bootstrap issued straight after a bootout
# meets a label launchd has not finished releasing and fails with `Input/output error` —
# observed, not guessed at, and it is not a brief window: the node it is tearing down goes
# on serving for five seconds and then gives requests already in flight up to twenty more,
# so the budget here is that shutdown plus room, and a shorter one just fails later.
attempt=1
while :; do
	launchctl bootstrap "$DOMAIN" "$PLIST" 2>/tmp/tessaridb-bootstrap.$$ && break
	[ "$attempt" -lt 30 ] || die "could not load the agent: $(cat /tmp/tessaridb-bootstrap.$$)"
	attempt=$((attempt + 1))
	sleep 1
done
rm -f /tmp/tessaridb-bootstrap.$$

waited=0
while [ "$waited" -lt 15 ]; do
	if nc -z "$HOST" "$PORT" >/dev/null 2>&1; then
		echo
		cat "$HOME_DIR/INSTALLED"
		echo
		echo "the node is up. tessaridbctl status | stop | start | health"
		exit 0
	fi
	sleep 1
	waited=$((waited + 1))
done

die "the agent was loaded but nothing answers on $ADDRESS — see $HOME_DIR/logs/node.err"
