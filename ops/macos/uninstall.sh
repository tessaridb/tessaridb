#!/bin/sh
# uninstall.sh — take the agent out and remove what install.sh put on this machine.
#
# The store is left alone unless --store is given. Deleting somebody's data because they
# wanted the binary gone is not a decision a command makes by itself.

set -eu

PREFIX="${PREFIX:-$HOME/.local/bin}"
HOME_DIR="${TESSARIDB_HOME:-$HOME/.tessaridb}"
LABEL="com.tessaridb.node"
PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"
DOMAIN="gui/$(id -u)"
DROP_STORE="no"

while [ $# -gt 0 ]; do
	case "$1" in
		--store)  DROP_STORE="yes"; shift ;;
		--prefix) PREFIX="${2:?--prefix needs a path}"; shift 2 ;;
		--home)   HOME_DIR="${2:?--home needs a path}"; shift 2 ;;
		*) echo "uninstall: unknown argument: $1" >&2; exit 1 ;;
	esac
done

launchctl bootout "$DOMAIN/$LABEL" >/dev/null 2>&1 || true
rm -f "$PLIST"
echo "agent removed"

rm -f "$PREFIX/tessaridb" "$PREFIX/tessaridbctl"
echo "binaries removed from $PREFIX"

if [ "$DROP_STORE" = "yes" ]; then
	rm -rf "$HOME_DIR"
	echo "store and settings removed from $HOME_DIR"
else
	echo "store kept at $HOME_DIR — pass --store to remove it too"
fi
