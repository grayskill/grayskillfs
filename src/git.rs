use crate::backend::{errno, Root};
use crate::identity::{FsCredentials, Identity};
use base64::engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD};
use base64::Engine;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output, Stdio};
use tempfile::TempDir;

pub struct GitStore {
    cache: Option<TempDir>,
    parent: Option<PathBuf>,
    user: Option<Identity>,
    snapshots: HashMap<(String, String), PathBuf>,
}

pub struct Selection {
    pub canonical: String,
    pub root: PathBuf,
}

fn command(repository: &Path, user: Option<Identity>) -> io::Result<Command> {
    let mut cmd = Command::new("git");
    cmd.env_clear();
    // Do not inherit GIT_*, loader overrides, credential helpers, hooks, or SSH
    // configuration from the daemon's environment.
    cmd.env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", repository)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_SSH_COMMAND", "ssh -F /dev/null -oBatchMode=yes")
        .arg("--git-dir")
        .arg(repository)
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "protocol.allow=never",
            "-c",
            "protocol.file.allow=always",
            "-c",
            "protocol.https.allow=always",
            "-c",
            "protocol.http.allow=always",
            "-c",
            "protocol.ssh.allow=always",
        ])
        .stdin(Stdio::null());
    if let Some(user) = user {
        user.configure_command(&mut cmd)?;
        cmd.current_dir("/");
    }
    Ok(cmd)
}

fn output(mut cmd: Command) -> io::Result<Output> {
    let result = cmd.output()?;
    if !result.status.success() {
        return Err(io::Error::other(format!(
            "git failed: {}",
            String::from_utf8_lossy(&result.stderr).trim()
        )));
    }
    Ok(result)
}

impl GitStore {
    pub fn new(parent: Option<&Path>, user: Option<Identity>) -> Self {
        Self {
            cache: None,
            parent: parent.map(Path::to_owned),
            user,
            snapshots: HashMap::new(),
        }
    }

    fn cache(&mut self) -> io::Result<PathBuf> {
        if let Some(cache) = &self.cache {
            return Ok(cache.path().to_owned());
        }
        let mut builder = tempfile::Builder::new();
        builder.prefix("grayskillfs-");
        let cache = match &self.parent {
            Some(parent) => builder.tempdir_in(parent)?,
            None => builder.tempdir()?,
        };
        let repositories = cache.path().join("repositories");
        fs::create_dir(&repositories)?;
        fs::set_permissions(&repositories, fs::Permissions::from_mode(0o700))?;
        if let Some(user) = self.user {
            user.own_directory(&fs::File::open(&repositories)?)?;
            // Git can traverse to its own workspace but cannot replace the
            // daemon-owned snapshot trees alongside it.
            fs::set_permissions(cache.path(), fs::Permissions::from_mode(0o711))?;
        }
        let path = cache.path().to_owned();
        self.cache = Some(cache);
        Ok(path)
    }

    pub fn select(&mut self, selector: &str) -> io::Result<Selection> {
        let _credentials = FsCredentials::daemon()?;
        let bytes = URL_SAFE_NO_PAD
            .decode(selector)
            .or_else(|_| URL_SAFE.decode(selector))
            .map_err(|_| errno(libc::ENOENT))?;
        let decoded = std::str::from_utf8(&bytes).map_err(|_| errno(libc::ENOENT))?;
        let newline = decoded.ends_with('\n');
        let decoded = decoded.strip_suffix('\n').unwrap_or(decoded);
        let (url, reference) = decoded
            .rsplit_once(':')
            .ok_or_else(|| errno(libc::ENOENT))?;
        if url.is_empty()
            || reference.is_empty()
            || reference.starts_with('-')
            || reference.contains(['\n', '\r', '\0'])
            || url.contains(['\n', '\r', '\0'])
        {
            return Err(errno(libc::ENOENT));
        }
        // Permit names, HEAD and object IDs, not revision expressions/refspecs.
        let hash = reference.len() == 40 || reference.len() == 64;
        let hash = hash && reference.bytes().all(|b| b.is_ascii_hexdigit());
        let cache = self.cache()?;
        if !hash && reference != "HEAD" {
            let mut check = command(&cache, self.user)?;
            check.args(["check-ref-format", "--allow-onelevel", reference]);
            if !check.output()?.status.success() {
                return Err(errno(libc::ENOENT));
            }
        }
        let key = format!("{:x}", Sha256::digest(url.as_bytes()));
        let repositories = cache.join("repositories");
        let repository_name = format!("{key}.git");
        let repository = repositories.join(&repository_name);
        if !repository.exists() {
            let mut init = command(&repository, self.user)?;
            init.args(["init", "--bare"]);
            output(init)?;
            // A snapshot represents the tree, not a release archive altered by
            // repository-controlled export-ignore/export-subst attributes.
            let repository_root = Root::new(&repositories)?.subdir(Path::new(&repository_name))?;
            let mut attributes = repository_root.open(
                Path::new("info/attributes"),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
                0o644,
            )?;
            use std::io::Write;
            attributes.write_all(b"* -export-ignore -export-subst\n")?;
        }
        if !hash
            || !self
                .snapshots
                .contains_key(&(url.to_owned(), reference.to_owned()))
        {
            let mut fetch = command(&repository, self.user)?;
            fetch.args([
                "fetch",
                "--quiet",
                "--no-tags",
                "--force",
                "--",
                url,
                reference,
            ]);
            output(fetch)?;
        }
        let mut resolve = command(&repository, self.user)?;
        if hash {
            resolve.args(["rev-parse", "--verify", &format!("{reference}^{{commit}}")]);
        } else {
            resolve.args(["rev-parse", "--verify", "FETCH_HEAD^{commit}"]);
        }
        let resolved = output(resolve)?;
        let commit = String::from_utf8(resolved.stdout)
            .map_err(io::Error::other)?
            .trim()
            .to_owned();
        let snapshot_key = (url.to_owned(), commit.clone());
        let root = if let Some(root) = self.snapshots.get(&snapshot_key) {
            root.clone()
        } else {
            let mut archive = command(&repository, self.user)?;
            archive.args(["archive", "--format=tar", &commit]);
            let data = output(archive)?;
            let staging = tempfile::Builder::new()
                .prefix("tree-")
                .tempdir_in(&cache)?;
            let mut archive = tar::Archive::new(data.stdout.as_slice());
            archive.set_preserve_permissions(false);
            for entry in archive.entries()? {
                let mut entry = entry?;
                let kind = entry.header().entry_type();
                if kind.is_pax_global_extensions() {
                    continue;
                }
                let path = entry.path()?.into_owned();
                if path
                    .components()
                    .any(|c| !matches!(c, Component::Normal(_)))
                    || path
                        .components()
                        .next()
                        .is_some_and(|c| c.as_os_str() == OsStr::new(".git"))
                {
                    return Err(io::Error::other("unsafe path in git archive"));
                }
                if !(kind.is_file() || kind.is_dir() || kind.is_symlink()) {
                    return Err(io::Error::other("unsupported entry type in git archive"));
                }
                if !entry.unpack_in(staging.path())? {
                    return Err(io::Error::other("git archive entry escaped snapshot"));
                }
            }
            let destination = cache.join(format!("{key}-{commit}"));
            fs::set_permissions(staging.path(), fs::Permissions::from_mode(0o755))?;
            fs::rename(staging.path(), &destination)?;
            self.snapshots.insert(snapshot_key, destination.clone());
            destination
        };
        let canonical =
            URL_SAFE_NO_PAD.encode(format!("{url}:{commit}{}", if newline { "\n" } else { "" }));
        Ok(Selection { canonical, root })
    }
}
