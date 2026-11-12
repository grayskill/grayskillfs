# Startup examples

These examples keep the daemon root-run, use `grayskillfs-git` only for Git
subprocesses, and allow other UIDs to use the mount. They leave native device I/O
disabled. The daemon is privileged; do not treat these launchers as a sandbox.

## Shell or ordinary systemd

Build with `make`, install `target/release/grayskillfs` as
`/usr/local/bin/grayskillfs`, and install Git, OpenSSH and FUSE 3. Create an
unprivileged system account and primary group named `grayskillfs-git`, with
nonzero UID/GID. Then prepare the example configuration and source directory:

```sh
sudo install -m 0644 examples/grayskillfs.conf /etc/grayskillfs.conf
sudo install -d -m 0755 /srv/grayskillfs/base
# Populate /srv/grayskillfs/base with the desired sandbox root.
sudo sh examples/start.sh
```

The script stays in the foreground using `exec`. Its `GRAYSKILLFS_BIN`,
`GRAYSKILLFS_CONFIG`, `GRAYSKILLFS_MOUNTPOINT`, `GRAYSKILLFS_CACHE_DIR`, and
`GRAYSKILLFS_GIT_USER` environment variables override the defaults. The cache
parent is root-owned mode `0711`: Git can traverse it, but cannot replace the
daemon's cache directories. Manually unmount with
`sudo fusermount3 -u /run/grayskillfs/mnt`.

Alternatively, install the unit instead of running the script:

```sh
sudo install -m 0644 examples/grayskillfs.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now grayskillfs.service
```

Adjust the unit's executable paths for your distribution if necessary.
Systemd creates the runtime and cache directories, and logs to the journal.
Stopping the service lazily detaches the mount, including when clients hold
files open; stop sandbox workloads before stopping the filesystem. `Type=exec`
confirms process startup, not mount readiness; dependent workloads must check
that the FUSE mount is ready before accessing it.

## NixOS

Copy this directory into your NixOS configuration tree and add this import to
`configuration.nix`:

```nix
{
  imports = [ ./examples/nixos.nix ];
}
```

Run `sudo nixos-rebuild switch`. The module downloads `grayskillfs` 0.1.0 from
crates.io with a pinned SHA-256 checksum, builds it with the packaged Makefile
and locked Cargo dependencies, and runs the ordinary tests
(not the opt-in mounted/namespace tests), installs the CLI, provisions the Git
account and base directory, and enables the same unit shown above. Git, SSH and
FUSE helper paths are included in the package wrapper and service environment;
the daemon still runs as root, not as the Git account.

The module is a native Linux build example, not a flake or cross-build setup.
To upgrade, update `version` and the `pkgs.fetchCrate` source hash together.

## Using the mount

The example configuration consumes a Git selector:

```sh
selector=$(grayskillfs encode https://github.com/OWNER/REPOSITORY.git main)
ls "/run/grayskillfs/mnt/$selector/package"
```

The base root's files appear alongside `package` below each selector. Git runs
without inherited credential helpers, SSH configuration or interactive prompts;
use an accessible repository. For local repositories, the Git account must be
able to traverse every ancestor and read the repository.

For writable per-caller storage, create `/srv/grayskillfs/data/UID` owned by
that caller's UID/GID, with appropriate permissions, and uncomment the
`pathsubst` layer in the configuration. Include UID 0 if root will browse the
selected trees; missing per-UID directories fail rather than falling back.
