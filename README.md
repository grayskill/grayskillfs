# grayskillfs

A Linux/Rust FUSE implementation of [the layering design](docs/design.txt).
It assembles sandbox roots from host directories and immutable Git trees, with
explicit read-only/read-write mappings. A 9p server is not implemented.

See [examples/](examples/README.md) for shell, systemd and NixOS startup files.

## Build and test

Requires Rust/Cargo (Rust 1.85+), a C linker, and Git on `PATH`. FUSE development
libraries are not needed. Mounting requires Linux 5.8+ (`openat2` and
`utimensat(AT_EMPTY_PATH)`), mounted `/proc`, `/dev/fuse`, and `fusermount3`.

```sh
make                  # Build target/release/grayskillfs.
make test             # Automated tests without requiring a FUSE mount.
make do-lint          # Formatting and strict Clippy checks.
# Explicitly exercise a real, unprivileged mount; also requires make on PATH.
make test-mounted
# Additional isolated filesystem/Git credential tests; see requirements below.
make test-privileges
make help
```

Make directs the build and delegates Rust input tracking to Cargo. Build, test,
and lint targets can run together with `make -j`. Required system tools are
checked before running each target. Cargo fetches locked Rust dependencies as
needed. Override `CARGO` or `CARGO_TARGET_DIR` to select the Cargo executable or
build output directory.

`make clean` removes Cargo outputs. `make distclean` is equivalent because all
generated build files are in the Cargo output directory; both preserve
`Cargo.lock` and sources. Run cleanup separately from build/test/lint targets
to avoid deleting outputs in use.

The mounted test creates its own temporary backing directories and local Git
repository, mounts the CLI in the foreground, exercises filesystem operations,
and unmounts/cleans up on completion. It does not need root, change host settings,
or contact external repositories.

`make test-privileges` additionally needs `unshare` (util-linux), `newuidmap` and
`newgidmap`, configured subordinate UID/GID mappings covering UID/GID 65534,
unprivileged user/mount namespaces, and the `nobody` account (UID/GID 65534).
It runs in private user/mount namespaces, not as host root, and verifies the
Git-only credential dropping, scoped filesystem credentials, and client-owned
backing files through a root-run FUSE mount.

## Publishing

`.github/workflows/publish.yml` publishes to crates.io on pushed `v*` tags, or
manual dispatch **on an existing `v*` tag**. The tag must equal `v` plus the
`Cargo.toml` version. It verifies the package before obtaining a short-lived
OIDC token; no long-lived crates.io token belongs in GitHub secrets.
The GitHub `crates-io` environment is restricted to `v*` tags.

Trusted Publishing requires an initial token-based publication and individual
crate ownership; pending publishers are not supported. A verified crates.io
email and linked GitHub account are also required. The package is MIT-licensed.
The publisher identity for this repository is:

| Setting | Value |
| --- | --- |
| Crate | `grayskillfs` |
| Repository owner | `grayskill` |
| Repository name | `grayskillfs` |
| Workflow filename | `publish.yml` |
| Environment | `crates-io` |

These settings are managed through
[crates.io Trusted Publishing](https://crates.io/docs/trusted-publishing).

The official API also supports creation with
`POST /api/v1/trusted_publishing/github_configs`; the JSON body is
`{"github_config":{"crate":"grayskillfs","repository_owner":"grayskill","repository_name":"grayskillfs","workflow_filename":"publish.yml","environment":"crates-io"}}`.
Authentication requires a crate owner's API token with the `trusted_publishing`
scope (or an applicable legacy token). The local `crates.io-api-key` is ignored
by Git and explicitly excluded from Cargo packages; never commit it or put it
in a workflow. Workflow changes must be reviewed and pushed before automation
can run. Creating/pushing a matching version tag or dispatching the workflow on
one authorizes a real publish, not a dry run.

Each new release needs a new package version; an already-published version
cannot be published again.

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

Create the backing directories before using them, or explicitly enable
`mkdir` as described below. A layer consumes one selector
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
normal Unix descriptor semantics. `{{uid}}` cannot be overridden at mount time;
`--user` changes only Git subprocess credentials.

The first layer must mount onto `/`. Layers are applied in order. A later mount
replaces the entire earlier subtree at its target, not a union of their files.
Nested mounts are injected into directory listings, and missing intermediate
directories are synthesized. A later parent mount hides any earlier nested
mounts. Host-path sources must be directories and Git layers must be read-only.
UID-dependent directories are resolved on access, not required at mount startup;
a missing directory returns an error unless `mkdir` is enabled, never falling
back to another UID.

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
`mount` runs in the foreground and validates fixed sources immediately, except
sources explicitly configured for lazy creation.
`--cache-dir DIRECTORY` chooses the parent of a private, per-mount temporary
Git cache. The parent must exist; the cache is removed on normal unmount.
The cache is created lazily on the first Git request, under the daemon's runtime
credentials. No other user's cache or Git configuration is used.

### Lazy source directory provisioning

`path` and `pathsubst` layers can opt into creating missing source directories
when the mapping is first accessed:

```text
pathsubst /srv/sessions/{{uid}}/{{path}} onto /tmp as read-write mkdir mode=0700 uid={{uid}} gid={{gid}}
```

This consumes one session-ID selector and exposes that session's directory as
`/SESSION-ID/tmp`. Without `mkdir`, missing source directories still fail.
The optional settings default to `mode=0700 uid={{uid}} gid={{gid}}`.
`mode` is octal, from `0000` through `0777`; special mode bits are not supported.
Both ownership fields accept numeric IDs or `{{uid}}`/`{{gid}}`, resolved from
the current FUSE request's filesystem UID/GID. These ownership substitutions do
not consume selectors; `{{gid}}` is supported in ownership options, not source
paths. Metadata options require `mkdir`; Git and device layers reject them.

All newly created path components receive the configured mode and ownership.
**Existing directories are never chmodded or chowned**, even when their
metadata differs from the configuration. Pre-create shared static prefixes
such as `/srv/sessions` as root-owned, non-user-writable directories; otherwise
the first caller may own newly created ancestors. Source provisioning uses the
daemon's credentials, so a root daemon can provision caller-owned directories
below a root-owned prefix. Non-root daemons cannot assign arbitrary ownership;
creation or ownership failures are reported rather than faking metadata.

Provisioning rejects symlinks and path traversal. New directories are prepared
privately and atomically installed without replacing an existing entry.
This needs a writable, protected staging ancestor on the same filesystem:
root/daemon-owned and not group/other writable unless sticky. Existing sources
need no staging. Read-only layers can provision sources too; their normal FUSE
mutation restrictions still apply. The daemon does not expire or remove
session directories; the launcher owns their lifecycle.

### Git subprocess and filesystem credentials

Use `--user USER` (account name or numeric UID) to run **Git subprocesses only**
as an unprivileged account, keeping the daemon's credentials unchanged:

```sh
sudo target/release/grayskillfs mount /etc/grayskillfs.conf /mnt/grayskillfs \
    --user grayskillfs --cache-dir /var/cache/grayskillfs --allow-other
```

The account must exist with a nonzero UID and primary GID. Starting with
`--user` requires root; omitting it leaves Git running as the daemon's account.
Before each Git command, its child process clears supplementary groups, sets
real/effective/saved GID and UID to the selected account, clears
permitted/effective/inheritable/ambient capabilities, and sets `no_new_privs`.
Git's working directory is `/`. Credential-setting failures prevent the command
from executing; the daemon's identity and working directory do not change.

The daemon creates its private cache lazily, extracts snapshots itself, and
gives Git a separate, account-owned `repositories/` directory. Git cannot replace
daemon-owned snapshots or archive staging directories. The cache parent must
allow the Git account to traverse to its workspace; keep the parent
daemon-owned, non-writable by that account, and outside mount/backing trees
(for example root-owned mode `0711` for `/var/cache/grayskillfs`).
Repositories and their ancestors must be accessible to the Git account.

`{{uid}}` still means the **client request UID**, independently of `--user`.
When the daemon runs as root, backing operations use the request's UID/GID via
thread-local Linux `setfsuid()`/`setfsgid()`, restoring previous IDs after each
request, including errors. New files, directories and symlinks therefore acquire
the caller's actual backing ownership without post-creation chown; normal
backing setgid-directory inheritance also applies. Existing ownership is not
remapped. Git/cache maintenance and opt-in source provisioning temporarily use
the daemon's filesystem IDs,
independently of the requester, while Git children use `--user`.
Supplementary groups are not impersonated; the kernel's `default_permissions`
checks still use the caller's groups. A non-root daemon retains its own backing
credentials and cannot represent arbitrary client ownership.
The mount owner remains the startup user; `--allow-other` is needed for other
client UIDs. This does not drop the daemon's root privileges.

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
`nodev`. Ownership and ordinary mode bits come from backing files. Git snapshots
belong to the daemon; root-run backing operations use the request's filesystem
UID/GID, while non-root daemons create files as themselves. Prepare directories and file
modes/group permissions appropriately for the sandbox users.
`--allow-other` explicitly enables other users and needs `user_allow_other`
in `/etc/fuse.conf` for an unprivileged daemon. Without it only the mounting
user can use the filesystem. Do not run the daemon as root unnecessarily.

Configuration, backing directory administrators and selector callers must be
trusted. A caller that can choose Git selectors can fetch repositories and
use Git's configured filesystem/network access; only file, HTTP(S) and SSH Git
transports are enabled. Git runs without inherited Git configuration,
credential helpers, hooks, SSH config or interactive prompts. SSH may still
use the daemon account's default identity files. There is no remote allowlist,
fetch timeout, disk quota or cache/inode eviction. Git fetch and snapshot
extraction are synchronous, and archives are buffered in memory. Put this
service behind the larger project's authorization/resource controls before
exposing it to untrusted callers. Keep mount/cache directories out of backing
trees to avoid recursive access. Read-only mappings do not prevent changes by
host processes or by another writable mapping of the same host inode.
