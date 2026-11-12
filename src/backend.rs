use std::ffi::{CString, OsStr, OsString};
use std::fs::{self, File, Metadata};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path};
use std::sync::Arc;

pub fn errno(code: i32) -> io::Error {
    io::Error::from_raw_os_error(code)
}

fn cpath(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes()).map_err(|_| errno(libc::EINVAL))
}

pub fn component(name: &OsStr) -> io::Result<()> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.as_bytes().contains(&b'/')
        || name.as_bytes().contains(&0)
    {
        return Err(errno(libc::EINVAL));
    }
    Ok(())
}

#[derive(Debug)]
pub struct Root {
    directory: File,
}

#[repr(C)]
struct OpenHow {
    flags: u64,
    mode: u64,
    resolve: u64,
}

impl Root {
    pub fn new(path: &Path) -> io::Result<Arc<Self>> {
        // Pin the configured directory; subsequent operations never resolve host symlinks.
        let name = cpath(path)?;
        let fd = unsafe {
            libc::open(
                name.as_ptr(),
                libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Arc::new(Self {
            directory: unsafe { File::from_raw_fd(fd) },
        }))
    }

    pub fn open(&self, path: &Path, flags: i32, mode: u32) -> io::Result<File> {
        if path
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
        {
            return Err(errno(libc::EINVAL));
        }
        let name = cpath(if path.as_os_str().is_empty() {
            Path::new(".")
        } else {
            path
        })?;
        let how = OpenHow {
            flags: (flags | libc::O_CLOEXEC | libc::O_NOFOLLOW) as u64,
            mode: mode as u64,
            // RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS. The FUSE client resolves links
            // in the assembled namespace, never in the daemon's host namespace.
            resolve: 0x08 | 0x04,
        };
        let fd = unsafe {
            libc::syscall(
                libc::SYS_openat2,
                self.directory.as_raw_fd(),
                name.as_ptr(),
                &how,
                std::mem::size_of::<OpenHow>(),
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(unsafe { File::from_raw_fd(fd as i32) })
    }

    pub fn metadata(&self, path: &Path) -> io::Result<Metadata> {
        self.open(path, libc::O_PATH, 0)?.metadata()
    }

    pub fn read_link(&self, path: &Path) -> io::Result<OsString> {
        let file = self.open(path, libc::O_PATH, 0)?;
        let mut data = vec![0; 4096];
        let size = unsafe {
            libc::readlinkat(
                file.as_raw_fd(),
                c"".as_ptr(),
                data.as_mut_ptr().cast(),
                data.len(),
            )
        };
        if size < 0 {
            return Err(io::Error::last_os_error());
        }
        if size as usize == data.len() {
            return Err(errno(libc::ENAMETOOLONG));
        }
        data.truncate(size as usize);
        Ok(OsStr::from_bytes(&data).to_owned())
    }

    pub fn entries(&self, path: &Path) -> io::Result<Vec<OsString>> {
        let directory = self.open(path, libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
        fs::read_dir(format!("/proc/self/fd/{}", directory.as_raw_fd()))?
            .map(|entry| entry.map(|e| e.file_name()))
            .collect()
    }

    fn parent(&self, path: &Path) -> io::Result<(File, CString)> {
        let name = path.file_name().ok_or_else(|| errno(libc::EBUSY))?;
        component(name)?;
        let parent = self.open(
            path.parent().unwrap_or(Path::new("")),
            libc::O_PATH | libc::O_DIRECTORY,
            0,
        )?;
        Ok((parent, CString::new(name.as_bytes()).unwrap()))
    }

    pub fn mkdir(&self, path: &Path, mode: u32) -> io::Result<()> {
        let (parent, name) = self.parent(path)?;
        check(unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), mode) })
    }

    pub fn mkdir_p(
        self: &Arc<Self>,
        path: &Path,
        mode: u32,
        uid: u32,
        gid: u32,
    ) -> io::Result<Arc<Self>> {
        if mode > 0o777 || uid == u32::MAX || gid == u32::MAX {
            return Err(errno(libc::EINVAL));
        }
        if path
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
        {
            return Err(errno(libc::EINVAL));
        }
        let mut current = self.clone();
        let mut staging: Option<(Arc<Self>, u64)> = None;
        for part in path.components() {
            let metadata = current.directory.metadata()?;
            if staging
                .as_ref()
                .is_some_and(|(_, dev)| *dev != metadata.dev())
            {
                staging = None;
            }
            // A protected ancestor keeps an attacker from replacing the staging
            // directory before it is pinned. Sticky directories protect its owner.
            if (metadata.uid() == unsafe { libc::geteuid() } || metadata.uid() == 0)
                && (metadata.mode() & 0o022 == 0 || metadata.mode() & libc::S_ISVTX != 0)
            {
                let access = unsafe {
                    libc::syscall(
                        libc::SYS_faccessat2,
                        current.directory.as_raw_fd(),
                        c"".as_ptr(),
                        libc::W_OK | libc::X_OK,
                        libc::AT_EMPTY_PATH | libc::AT_EACCESS,
                    )
                };
                if access == 0 {
                    staging = Some((current.clone(), metadata.dev()));
                } else {
                    let error = io::Error::last_os_error();
                    if !matches!(error.raw_os_error(), Some(libc::EACCES | libc::EROFS)) {
                        return Err(error);
                    }
                }
            }
            let name = Path::new(part.as_os_str());
            match current.subdir(name) {
                Ok(directory) => current = directory,
                Err(error) if error.raw_os_error() == Some(libc::ENOENT) => {
                    let (protected, _) = staging.as_ref().ok_or_else(|| {
                        io::Error::other(
                            "mkdir requires a protected staging ancestor on the same filesystem",
                        )
                    })?;
                    current.provision_directory(name, protected, mode, uid, gid)?;
                    current = current.subdir(name)?;
                }
                Err(error) => return Err(error),
            }
        }
        Ok(current)
    }

    fn provision_directory(
        &self,
        name: &Path,
        protected: &Root,
        mode: u32,
        uid: u32,
        gid: u32,
    ) -> io::Result<()> {
        let staging = tempfile::Builder::new()
            .prefix(".grayskillfs-mkdir-")
            .tempdir_in(format!("/proc/self/fd/{}", protected.directory.as_raw_fd()))?;
        let root = Root::new(staging.path())?;
        let child = Path::new("directory");
        root.mkdir(child, 0o700)?;
        let result = (|| {
            let file = root.open(child, libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
            check(unsafe { libc::fchown(file.as_raw_fd(), uid, gid) })?;
            check(unsafe { libc::fchmod(file.as_raw_fd(), mode) })?;
            root.rename(child, self, name, libc::RENAME_NOREPLACE)
        })();
        if let Err(error) = result {
            root.unlink(child, true).map_err(|cleanup| {
                io::Error::other(format!(
                    "mkdir failed: {error}; staging cleanup failed: {cleanup}"
                ))
            })?;
            if error.raw_os_error() != Some(libc::EEXIST) {
                return Err(error);
            }
        }
        staging.close()?;
        Ok(())
    }

    pub fn unlink(&self, path: &Path, directory: bool) -> io::Result<()> {
        let (parent, name) = self.parent(path)?;
        check(unsafe {
            libc::unlinkat(
                parent.as_raw_fd(),
                name.as_ptr(),
                if directory { libc::AT_REMOVEDIR } else { 0 },
            )
        })
    }

    pub fn symlink(&self, path: &Path, target: &Path) -> io::Result<()> {
        let (parent, name) = self.parent(path)?;
        let target = cpath(target)?;
        check(unsafe { libc::symlinkat(target.as_ptr(), parent.as_raw_fd(), name.as_ptr()) })
    }

    pub fn rename(&self, from: &Path, destination: &Root, to: &Path, flags: u32) -> io::Result<()> {
        let (from_parent, from_name) = self.parent(from)?;
        let (to_parent, to_name) = destination.parent(to)?;
        check(unsafe {
            libc::renameat2(
                from_parent.as_raw_fd(),
                from_name.as_ptr(),
                to_parent.as_raw_fd(),
                to_name.as_ptr(),
                flags,
            )
        })
    }

    pub fn link(&self, from: &Path, to: &Path) -> io::Result<()> {
        let (from_parent, from_name) = self.parent(from)?;
        let (to_parent, to_name) = self.parent(to)?;
        check(unsafe {
            libc::linkat(
                from_parent.as_raw_fd(),
                from_name.as_ptr(),
                to_parent.as_raw_fd(),
                to_name.as_ptr(),
                0,
            )
        })
    }

    pub fn probe_openat2(&self) -> io::Result<()> {
        self.open(Path::new(""), libc::O_PATH | libc::O_DIRECTORY, 0)?;
        Ok(())
    }

    pub fn subdir(&self, path: &Path) -> io::Result<Arc<Self>> {
        Ok(Arc::new(Self {
            directory: self.open(path, libc::O_PATH | libc::O_DIRECTORY, 0)?,
        }))
    }
}

pub fn check(result: i32) -> io::Result<()> {
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}
