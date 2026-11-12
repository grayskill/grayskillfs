#!/bin/sh
set -eu

: "${GRAYSKILLFS_BIN:=grayskillfs}"
: "${GRAYSKILLFS_CONFIG:=/etc/grayskillfs.conf}"
: "${GRAYSKILLFS_MOUNTPOINT:=/run/grayskillfs/mnt}"
: "${GRAYSKILLFS_CACHE_DIR:=/var/cache/grayskillfs}"
: "${GRAYSKILLFS_GIT_USER:=grayskillfs-git}"

if [ "$(id -u)" -ne 0 ]; then
    printf '%s\n' 'Start this example as root; only Git subprocesses drop privileges.' >&2
    exit 1
fi

"$GRAYSKILLFS_BIN" check "$GRAYSKILLFS_CONFIG"
mkdir -p -- "$GRAYSKILLFS_MOUNTPOINT"
install -d -o 0 -g 0 -m 0711 -- "$GRAYSKILLFS_CACHE_DIR"

exec "$GRAYSKILLFS_BIN" mount "$GRAYSKILLFS_CONFIG" "$GRAYSKILLFS_MOUNTPOINT" \
    --user "$GRAYSKILLFS_GIT_USER" --cache-dir "$GRAYSKILLFS_CACHE_DIR" --allow-other
