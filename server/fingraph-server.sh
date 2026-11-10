#!/bin/sh
# Runs the server (or one of its commands) as PUID:PGID, so the files it writes belong to you and not to
# root, also when started with `docker exec` (which runs as root).
set -e
# what FinGraph writes is yours alone (statements it files are group-readable)
umask 027
BIN=/usr/local/lib/fingraph/fingraph-server
PUID="${PUID:-1000}"
PGID="${PGID:-1000}"
if [ "$(id -u)" = "0" ]; then
    mkdir -p "${FINGRAPH_DATA:-/data}" "${FINGRAPH_STATEMENTS:-/statements}"
    # FinGraph's own folder is all its own; the statements folder may be a share, so only its top level
    chown -R "$PUID:$PGID" "${FINGRAPH_DATA:-/data}" 2>/dev/null || true
    chmod 700 "${FINGRAPH_DATA:-/data}" 2>/dev/null || true
    chown "$PUID:$PGID" "${FINGRAPH_STATEMENTS:-/statements}" 2>/dev/null || true
    exec setpriv --reuid="$PUID" --regid="$PGID" --clear-groups --inh-caps=-all --bounding-set=-all "$BIN" "$@"
fi
exec "$BIN" "$@"
