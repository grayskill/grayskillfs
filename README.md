# grayskillfs

A Linux/Rust FUSE implementation of [the layering design](docs/design.txt).
It assembles sandbox roots from host directories and immutable Git trees, with
explicit read-only/read-write mappings. A 9p server is not implemented.

## Build and test

Requires Rust/Cargo (Rust 1.85+), a C linker, and Git on `PATH`. FUSE development
libraries are not needed. Mounting requires Linux 5.8+ (`openat2` and
`utimensat(AT_EMPTY_PATH)`), mounted `/proc`, `/dev/fuse`, and `fusermount3`.

```sh
cargo build --release --locked
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --all-targets --locked
# Explicitly exercise a real, unprivileged mount; also requires make on PATH.
cargo test --locked --test mounted -- --ignored --nocapture
```

The mounted test creates its own temporary backing directories and local Git
repository, mounts the CLI in the foreground, exercises filesystem operations,
and unmounts/cleans up on completion. It does not need root, change host settings,
or contact external repositories.

## Configuration and mounting

Configuration is whitespace-separated, one layer per line. Blank lines and
whole-line `#` comments are allowed; quoting, spaces within paths and inline
comments are not supported.

```text
path /srv/sandbox-root onto / as read-only
git {{path}} onto /package as read-only
pathsubst /srv/users/{{path}} onto /package/src as read-only
pathsubst /srv/data/{{uid}} onto /package/data as read-write
dev char 1 3 onto /dev/null as read-write
```

Create the backing directories before using them. A layer consumes one selector
component **only if its source contains `{{path}}`**, regardless of how many
times that placeholder appears. `{{path}}` is that layer's single selector
component, not a path or a previous selector. Fixed sources and sources using
only `{{uid}}` consume nothing. The old `no-consume` keyword is no longer accepted.
`path` is a fixed mapping; use `pathsubst` for host-path substitutions.

`{{uid}}` is the accessing process's filesystem UID from each FUSE request
(`Request::uid()`, equivalent to `fuse_get_context()->uid`), **not the daemon's
UID**. Namespace views and inode identities are scoped by requester UID, with
zero entry/attribute cache lifetimes. Directory traversal through a directory
descriptor resolves new names for the current requester. Already-open file and
directory descriptors stay bound to the mapping opened originally, following
normal Unix descriptor semantics. There is no mount-time UID option.

The first layer must mount onto `/`. Layers are applied in order. A later mount
replaces the entire earlier subtree at its target, not a union of their files.
Nested mounts are injected into directory listings, and missing intermediate
directories are synthesized. A later parent mount hides any earlier nested
mounts. Host-path sources must be directories and Git layers must be read-only.
UID-dependent directories are resolved on access, not required at mount startup;
a missing directory returns an error rather than falling back to another UID.

```sh
target/release/grayskillfs check /path/to/config
mkdir -p /path/to/mount
target/release/grayskillfs mount /path/to/config /path/to/mount
# In another terminal:
selector=$(target/release/grayskillfs encode /path/to/repository main)
readlink "/path/to/mount/$selector"
ls "/path/to/mount/$selector/user1234/package"
fusermount3 -u /path/to/mount
```

`check` validates syntax without fetching repositories or opening sources.
`mount` runs in the foreground and validates fixed sources immediately.
`--cache-dir DIRECTORY` chooses the parent of a private, per-mount temporary
Git cache. The parent must exist; the cache is removed on normal unmount.
No other user's cache or Git configuration is used.

Git sources can be `{{path}}` (consume an encoded selector), or a fixed encoded
selector (consume nothing and pin that tree on first access for each UID).
Only consuming Git layers expose symbolic-ref symlinks in the selector namespace.
Git selectors are URL-safe base64, without padding, of `REPOSITORY:REF`, split
at the last colon. Padded input and a single trailing newline in the encoded
payload (as produced by `echo | base64`) are accepted and canonicalized.
Symbolic refs resolve to relative sibling symlinks whose names encode the same
repository and the full peeled commit ID. Looking up the alias again refreshes
the ref; commit views remain pinned for the mount's lifetime. Aliases must be
followed by the client before looking up subsequent selector components.
Refs are names, `HEAD`, or full SHA-1/SHA-256 object IDs, not revision
expressions. Git submodules are not recursively fetched and LFS objects are
not downloaded. Repository export-ignore/export-subst attributes do not alter
the tree. Selectors must fit the kernel's 255-byte filename limit.

Selector directories cannot enumerate all possible repositories or user
selectors: their listings contain only `.` and `..`. Use known selector names
directly. The assembled tree's ordinary directories are enumerable.

## Device layers

```text
dev char 1 3 onto /dev/null as read-write
dev char 1 5 onto /dev/zero as read-only
dev block 8 0 onto /dev/disk as read-only
```

`dev char|block MAJOR MINOR onto /TARGET as read-only|read-write` adds one
configured virtual device inode. Numbers are decimal, limited to Linux's
12-bit major and 20-bit minor ranges. Device layers consume no selectors,
synthesize missing parent directories, and follow the same replacement order
as other layers. They cannot replace `/`. No backing device nodes are created
on the host, and clients cannot create or modify device nodes through mknod.

Device inodes report the configured type and device number, with permissions
`0666` for read-write or `0444` for read-only. Ownership is the daemon's.
Mounts remain `nodev` by default: device metadata is visible, but opens fail.
To enable native device I/O, mount with **`--allow-dev`**; this normally requires
`CAP_SYS_ADMIN` in the initial user namespace/root, because unprivileged
`fusermount3` refuses device-enabled mounts. The daemon warns when devices are
configured without this option.

Linux opens these devices directly; FUSE read/write handlers do not proxy them.
Consequently their read-only setting is a native Unix permission restriction,
not an `EROFS` check on the driver. Privileged callers can bypass those mode
bits, and driver ioctls have their normal semantics. In particular, exposing a
block device exposes host storage, not a virtual sandbox disk. Configure only
devices appropriate for the sandbox, and apply the wrapper's capability,
device-cgroup and seccomp restrictions as needed. Do not treat a read-only
device node as protection against privileged writes or destructive ioctls.

## Filesystem and sandbox boundaries

Regular file I/O, execution, create, mkdir, unlink/rmdir, rename (including
no-replace), hard links within one mapping, symlinks, truncate, chmod, timestamps,
directory streams, fsync and backing filesystem statistics are implemented.
Cross-mapping renames/hard links are rejected. Mount points and ancestors of
nested mounts cannot be removed or renamed. Configured virtual devices are
supported as described above; ordinary backing-file opens remain limited to
regular files. Ownership changes, client special-file creation, FIFO opens,
xattrs/ACLs and rename-exchange/whiteouts are not implemented.
Setuid/setgid/sticky mode bits are not exposed or set.

Host source directories are pinned by descriptors. Substituted sources are
opened beneath their fixed prefix without following symlinks. Subsequent
backing operations use descriptor-relative `openat2` resolution that rejects
host symlink traversal and escapes. Symlinks are returned unchanged for the
client kernel to resolve **inside its own namespace**, not the daemon's.
Regular files are pinned before reopening so a replacement cannot redirect an
open to a device. Read-only policy is checked in mutation handlers, not merely
represented by permission bits. Fixed, administrator-configured source paths
may themselves contain symlinks and are resolved once when pinned.

**This is root assembly, not a complete process sandbox.** A process using the
mount without chroot/pivot_root can still follow absolute symlinks to its host
root, or `..` out of a selected tree. The privileged wrapper in the broader
sandbox plan must set up the root/mount/network namespaces, per-run `/tmp`,
UID/GID, limits, seccomp and no-new-privileges. This project does not install a
setuid wrapper or change the Grayskill One project.

Mounts use `default_permissions`, `nosuid` and, unless `--allow-dev` is specified,
`nodev`. Ownership and ordinary mode bits come from backing files (Git snapshots
and newly created host files belong to the daemon); UID substitution does not
impersonate callers or remap ownership. Prepare writable directories and file
modes/group permissions appropriately for the sandbox users.
`--allow-other` explicitly enables other users and needs `user_allow_other`
in `/etc/fuse.conf` for an unprivileged daemon. Without it only the mounting
user can use the filesystem. Do not run the daemon as root unnecessarily.

Configuration, backing directory administrators and selector callers must be
trusted. A caller that can choose Git selectors can fetch repositories and
use the daemon's filesystem/network access; only file, HTTP(S) and SSH Git
transports are enabled. Git runs without inherited Git configuration,
credential helpers, hooks, SSH config or interactive prompts. SSH may still
use the daemon account's default identity files. There is no remote allowlist,
fetch timeout, disk quota or cache/inode eviction. Git fetch and snapshot
extraction are synchronous, and archives are buffered in memory. Put this
service behind the larger project's authorization/resource controls before
exposing it to untrusted callers. Keep mount/cache directories out of backing
trees to avoid recursive access. Read-only mappings do not prevent changes by
host processes or by another writable mapping of the same host inode.
