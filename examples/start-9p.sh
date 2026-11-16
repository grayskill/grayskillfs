#!/bin/sh
set -eu

: "${GRAYSKILLFS_BIN:=grayskillfs}"
: "${GRAYSKILLFS_CONFIG:=/etc/grayskillfs.conf}"
: "${GRAYSKILLFS_SOCKET:=/run/grayskillfs/9p.sock}"
: "${GRAYSKILLFS_CACHE_DIR:=/var/cache/grayskillfs-9p}"
: "${GRAYSKILLFS_GIT_USER:=grayskillfs-git}"

if [ "$(id -u)" -ne 0 ]; then
    printf '%s\n' 'Start this example as root; only Git subprocesses drop privileges.' >&2
    exit 1
fi

"$GRAYSKILLFS_BIN" check "$GRAYSKILLFS_CONFIG"
mkdir -p -- "$(dirname -- "$GRAYSKILLFS_SOCKET")"
if [ -e "$GRAYSKILLFS_SOCKET" ] || [ -L "$GRAYSKILLFS_SOCKET" ]; then
    if [ ! -S "$GRAYSKILLFS_SOCKET" ]; then
        printf 'Refusing to replace non-socket path: %s\n' "$GRAYSKILLFS_SOCKET" >&2
        exit 1
    fi
    rm -- "$GRAYSKILLFS_SOCKET"
fi
install -d -o 0 -g 0 -m 0711 -- "$GRAYSKILLFS_CACHE_DIR"
umask 077

exec "$GRAYSKILLFS_BIN" serve-9p "$GRAYSKILLFS_CONFIG" "$GRAYSKILLFS_SOCKET" \
    --user "$GRAYSKILLFS_GIT_USER" --cache-dir "$GRAYSKILLFS_CACHE_DIR"
