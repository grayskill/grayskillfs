use crate::backend::{check, component, errno};
use crate::config::DeviceKind;
use crate::identity::FsCredentials;
use crate::namespace::{Entry, Location, Namespace, Node};
use fuser::{
    FileAttr, FileType, Filesystem, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty,
    ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite, Request, TimeOrNow,
};
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const TTL: Duration = Duration::ZERO;
type DirEntry = (u64, FileType, OsString);

macro_rules! request_credentials {
    ($req:ident, $reply:ident) => {
        let _credentials = match FsCredentials::for_request($req.uid(), $req.gid()) {
            Ok(credentials) => credentials,
            Err(e) => {
                $reply.error(error(e));
                return;
            }
        };
    };
}

#[derive(Clone, Hash, PartialEq, Eq)]
struct ScopedPath {
    uid: u32,
    path: PathBuf,
}

impl ScopedPath {
    fn new(uid: u32, path: PathBuf) -> Self {
        Self { uid, path }
    }
}

#[derive(Hash, PartialEq, Eq)]
struct Identity {
    uid: u32,
    root: usize,
    layer: usize,
    dev: u64,
    ino: u64,
    directory: Option<PathBuf>,
}

struct Handle {
    file: File,
    ino: u64,
    writable: bool,
    uid: u32,
}

pub struct LayerFs {
    namespace: Namespace,
    paths: HashMap<u64, ScopedPath>,
    inodes: HashMap<ScopedPath, u64>,
    identities: HashMap<Identity, u64>,
    files: HashMap<u64, Handle>,
    directories: HashMap<u64, Vec<DirEntry>>,
    next_inode: u64,
    next_handle: u64,
    uid: u32,
    gid: u32,
}

fn error(error: io::Error) -> i32 {
    let code = error.raw_os_error().unwrap_or(libc::EIO);
    if error.raw_os_error().is_none() {
        eprintln!("grayskillfs: {error}");
    }
    code
}

fn timestamp(seconds: i64, nanos: i64) -> SystemTime {
    let value = i128::from(seconds) * 1_000_000_000 + i128::from(nanos);
    let duration = Duration::new(
        (value.unsigned_abs() / 1_000_000_000) as u64,
        (value.unsigned_abs() % 1_000_000_000) as u32,
    );
    if value < 0 {
        UNIX_EPOCH - duration
    } else {
        UNIX_EPOCH + duration
    }
}

fn kind(metadata: &std::fs::Metadata) -> FileType {
    match metadata.mode() & libc::S_IFMT {
        libc::S_IFDIR => FileType::Directory,
        libc::S_IFLNK => FileType::Symlink,
        libc::S_IFCHR => FileType::CharDevice,
        libc::S_IFBLK => FileType::BlockDevice,
        libc::S_IFIFO => FileType::NamedPipe,
        libc::S_IFSOCK => FileType::Socket,
        _ => FileType::RegularFile,
    }
}

impl LayerFs {
    pub fn new(namespace: Namespace) -> Self {
        Self {
            namespace,
            paths: HashMap::from([(1, ScopedPath::new(0, PathBuf::new()))]),
            inodes: HashMap::new(),
            identities: HashMap::new(),
            files: HashMap::new(),
            directories: HashMap::new(),
            next_inode: 2,
            next_handle: 1,
            uid: unsafe { libc::geteuid() },
            gid: unsafe { libc::getegid() },
        }
    }

    fn inode(&mut self, path: PathBuf, uid: u32) -> u64 {
        if path.as_os_str().is_empty() {
            return 1;
        }
        let path = ScopedPath::new(uid, path);
        if let Some(ino) = self.inodes.get(&path) {
            return *ino;
        }
        let ino = self.next_inode;
        self.next_inode += 1;
        self.inodes.insert(path.clone(), ino);
        self.paths.insert(ino, path);
        ino
    }

    fn inode_for(&mut self, path: PathBuf, entry: &Entry, uid: u32) -> u64 {
        let Node::Physical(location, metadata) = &entry.node else {
            return self.inode(path, uid);
        };
        let identity = Identity {
            uid,
            root: Arc::as_ptr(&location.root) as usize,
            layer: location.layer,
            dev: metadata.dev(),
            ino: metadata.ino(),
            directory: metadata.is_dir().then(|| path.clone()),
        };
        let existing = self.identities.get(&identity).copied();
        let scoped = ScopedPath::new(uid, path.clone());
        if let Some(mapped) = self.inodes.get(&scoped).copied() {
            if Some(mapped) != existing {
                self.forget_path(&path, uid);
            }
        }
        let ino = if let Some(ino) = existing {
            self.inodes.insert(scoped.clone(), ino);
            self.paths.entry(ino).or_insert(scoped);
            ino
        } else {
            self.inode(path, uid)
        };
        self.identities.insert(identity, ino);
        ino
    }

    fn path(&self, ino: u64) -> io::Result<PathBuf> {
        self.paths
            .get(&ino)
            .map(|scoped| scoped.path.clone())
            .ok_or_else(|| errno(libc::ESTALE))
    }

    fn child(&mut self, parent: u64, name: &OsStr, uid: u32, gid: u32) -> io::Result<PathBuf> {
        component(name)?;
        let parent = self.path(parent)?;
        if !self.namespace.lookup_with_gid(uid, gid, &parent)?.is_dir() {
            return Err(errno(libc::ENOTDIR));
        }
        Ok(parent.join(name))
    }

    fn entry(&mut self, ino: u64, uid: u32, gid: u32) -> io::Result<Entry> {
        let entry = self.namespace.lookup_with_gid(uid, gid, &self.path(ino)?)?;
        // Cached non-directory inodes must not be opened under a different UID:
        // the kernel page cache belongs to the original UID's inode.
        if ino != 1 && self.paths[&ino].uid != uid && !entry.is_dir() {
            return Err(errno(libc::ESTALE));
        }
        Ok(entry)
    }

    fn attr(&self, ino: u64, entry: &Entry) -> FileAttr {
        let mut attr = FileAttr {
            ino,
            size: 0,
            blocks: 0,
            atime: UNIX_EPOCH,
            mtime: UNIX_EPOCH,
            ctime: UNIX_EPOCH,
            crtime: UNIX_EPOCH,
            kind: FileType::Directory,
            perm: 0o555,
            nlink: 2,
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: 4096,
            flags: 0,
        };
        match &entry.node {
            Node::Directory => {}
            Node::Alias(target) => {
                attr.kind = FileType::Symlink;
                attr.perm = 0o777;
                attr.nlink = 1;
                attr.size = target.as_bytes().len() as u64;
            }
            Node::Physical(location, metadata) => {
                attr = self.physical_attr(ino, metadata, location.writable);
            }
            Node::Device(device, writable) => {
                attr.kind = match device.kind {
                    DeviceKind::Char => FileType::CharDevice,
                    DeviceKind::Block => FileType::BlockDevice,
                };
                attr.rdev = device.rdev();
                attr.perm = if *writable { 0o666 } else { 0o444 };
                attr.nlink = 1;
            }
        }
        attr
    }

    fn physical_attr(&self, ino: u64, metadata: &std::fs::Metadata, writable: bool) -> FileAttr {
        FileAttr {
            ino,
            size: metadata.size(),
            blocks: metadata.blocks(),
            atime: timestamp(metadata.atime(), metadata.atime_nsec()),
            mtime: timestamp(metadata.mtime(), metadata.mtime_nsec()),
            ctime: timestamp(metadata.ctime(), metadata.ctime_nsec()),
            crtime: UNIX_EPOCH,
            kind: kind(metadata),
            perm: (metadata.mode() as u16 & 0o777) & if writable { 0o777 } else { 0o555 },
            nlink: metadata.nlink().min(u32::MAX as u64) as u32,
            uid: metadata.uid(),
            gid: metadata.gid(),
            rdev: 0,
            blksize: metadata.blksize() as u32,
            flags: 0,
        }
    }

    fn handle(&mut self, file: File, ino: u64, writable: bool, uid: u32) -> u64 {
        let fh = self.next_handle;
        self.next_handle += 1;
        self.files.insert(
            fh,
            Handle {
                file,
                ino,
                writable,
                uid,
            },
        );
        fh
    }

    fn file(&self, fh: u64, ino: u64, write: bool) -> io::Result<&File> {
        let handle = self.files.get(&fh).ok_or_else(|| errno(libc::EBADF))?;
        if handle.ino != ino || (write && !handle.writable) {
            return Err(errno(libc::EBADF));
        }
        Ok(&handle.file)
    }

    fn forget_path(&mut self, path: &Path, uid: u32) {
        let removed = self
            .inodes
            .keys()
            .filter(|p| p.uid == uid && p.path.starts_with(path))
            .cloned()
            .collect::<Vec<_>>();
        for path in removed {
            if let Some(ino) = self.inodes.remove(&path) {
                if self.paths.get(&ino) == Some(&path) {
                    if let Some((alias, _)) = self.inodes.iter().find(|(_, value)| **value == ino) {
                        self.paths.insert(ino, alias.clone());
                    } else {
                        self.paths.remove(&ino);
                        self.prune_identity(ino);
                    }
                }
            }
        }
    }

    fn prune_identity(&mut self, ino: u64) {
        if !self.paths.contains_key(&ino) && !self.files.values().any(|handle| handle.ino == ino) {
            self.identities.retain(|_, value| *value != ino);
        }
    }

    fn same_mount(a: &Location, b: &Location) -> io::Result<()> {
        if a.layer != b.layer || !Arc::ptr_eq(&a.root, &b.root) {
            return Err(errno(libc::EXDEV));
        }
        Ok(())
    }

    fn remove(
        &mut self,
        parent: u64,
        name: &OsStr,
        directory: bool,
        uid: u32,
        gid: u32,
    ) -> io::Result<()> {
        let path = self.child(parent, name, uid, gid)?;
        let entry = self.namespace.lookup_with_gid(uid, gid, &path)?;
        let location = entry.mutation()?;
        location.root.unlink(&location.relative, directory)?;
        self.forget_path(&path, uid);
        Ok(())
    }

    fn directory(&mut self, ino: u64, uid: u32, gid: u32) -> io::Result<Vec<DirEntry>> {
        let path = self.path(ino)?;
        let entry = self.namespace.lookup_with_gid(uid, gid, &path)?;
        let names = entry.children()?;
        let parent = self.inode(path.parent().unwrap_or(Path::new("")).to_owned(), uid);
        let mut entries = vec![
            (ino, FileType::Directory, OsString::from(".")),
            (parent, FileType::Directory, OsString::from("..")),
        ];
        for name in names {
            let child = path.join(&name);
            let entry = match self.namespace.lookup_with_gid(uid, gid, &child) {
                Ok(entry) => entry,
                Err(error) if error.raw_os_error() == Some(libc::ENOENT) => continue,
                Err(error) => return Err(error),
            };
            let ino = self.inode_for(child, &entry, uid);
            entries.push((ino, self.attr(ino, &entry).kind, name));
        }
        Ok(entries)
    }
}

#[allow(clippy::too_many_arguments)]
impl Filesystem for LayerFs {
    fn lookup(&mut self, req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
        request_credentials!(req, reply);
        let result = (|| {
            let path = self.child(parent, name, req.uid(), req.gid())?;
            let entry = self
                .namespace
                .lookup_with_gid(req.uid(), req.gid(), &path)?;
            let ino = self.inode_for(path, &entry, req.uid());
            Ok(self.attr(ino, &entry))
        })();
        match result {
            Ok(attr) => reply.entry(&TTL, &attr, 0),
            Err(e) => reply.error(error(e)),
        }
    }

    fn getattr(&mut self, req: &Request<'_>, ino: u64, fh: Option<u64>, reply: ReplyAttr) {
        request_credentials!(req, reply);
        let result = (|| {
            if let Some(fh) = fh {
                let handle = self.files.get(&fh).ok_or_else(|| errno(libc::EBADF))?;
                let metadata = self.file(fh, ino, false)?.metadata()?;
                return Ok(self.physical_attr(ino, &metadata, handle.writable));
            }
            if let Some(handle) = self
                .files
                .values()
                .find(|handle| handle.ino == ino && handle.uid == req.uid())
            {
                return Ok(self.physical_attr(ino, &handle.file.metadata()?, handle.writable));
            }
            let entry = self.entry(ino, req.uid(), req.gid())?;
            Ok(self.attr(ino, &entry))
        })();
        match result {
            Ok(attr) => reply.attr(&TTL, &attr),
            Err(e) => reply.error(error(e)),
        }
    }

    fn readlink(&mut self, req: &Request<'_>, ino: u64, reply: ReplyData) {
        request_credentials!(req, reply);
        match self
            .entry(ino, req.uid(), req.gid())
            .and_then(|entry| entry.read_link())
        {
            Ok(target) => reply.data(target.as_bytes()),
            Err(e) => reply.error(error(e)),
        }
    }

    fn open(&mut self, req: &Request<'_>, ino: u64, flags: i32, reply: ReplyOpen) {
        request_credentials!(req, reply);
        let result = (|| {
            let entry = self.entry(ino, req.uid(), req.gid())?;
            let location = entry.location()?;
            let file = location.open(flags, 0)?;
            Ok(self.handle(file, ino, location.writable, req.uid()))
        })();
        match result {
            Ok(fh) => reply.opened(fh, 0),
            Err(e) => reply.error(error(e)),
        }
    }

    fn read(
        &mut self,
        req: &Request<'_>,
        ino: u64,
        fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyData,
    ) {
        request_credentials!(req, reply);
        let result = (|| {
            if offset < 0 {
                return Err(errno(libc::EINVAL));
            }
            let mut data = vec![0; size as usize];
            let file = self.file(fh, ino, false)?;
            let mut done = 0;
            while done < data.len() {
                match file.read_at(&mut data[done..], offset as u64 + done as u64) {
                    Ok(0) => break,
                    Ok(size) => done += size,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(e),
                }
            }
            data.truncate(done);
            Ok(data)
        })();
        match result {
            Ok(data) => reply.data(&data),
            Err(e) => reply.error(error(e)),
        }
    }

    fn write(
        &mut self,
        req: &Request<'_>,
        ino: u64,
        fh: u64,
        offset: i64,
        data: &[u8],
        _write_flags: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyWrite,
    ) {
        request_credentials!(req, reply);
        let result = (|| {
            if offset < 0 {
                return Err(errno(libc::EINVAL));
            }
            self.file(fh, ino, true)?
                .write_all_at(data, offset as u64)?;
            Ok(data.len() as u32)
        })();
        match result {
            Ok(size) => reply.written(size),
            Err(e) => reply.error(error(e)),
        }
    }

    fn flush(&mut self, req: &Request<'_>, ino: u64, fh: u64, _lock_owner: u64, reply: ReplyEmpty) {
        request_credentials!(req, reply);
        let result = self.file(fh, ino, false).and_then(|file| {
            let fd = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            check(unsafe { libc::close(fd) })
        });
        match result {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(error(e)),
        }
    }

    fn release(
        &mut self,
        req: &Request<'_>,
        _ino: u64,
        fh: u64,
        _flags: i32,
        _lock_owner: Option<u64>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        request_credentials!(req, reply);
        if let Some(handle) = self.files.remove(&fh) {
            self.prune_identity(handle.ino);
        }
        reply.ok();
    }

    fn fsync(&mut self, req: &Request<'_>, ino: u64, fh: u64, datasync: bool, reply: ReplyEmpty) {
        request_credentials!(req, reply);
        let result = self.file(fh, ino, false).and_then(|file| {
            if datasync {
                file.sync_data()
            } else {
                file.sync_all()
            }
        });
        match result {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(error(e)),
        }
    }

    fn opendir(&mut self, req: &Request<'_>, ino: u64, _flags: i32, reply: ReplyOpen) {
        request_credentials!(req, reply);
        let result = (|| {
            let entries = self.directory(ino, req.uid(), req.gid())?;
            let entry = self.entry(ino, req.uid(), req.gid())?;
            let pinned = if let Node::Physical(location, _) = entry.node {
                Some((
                    location.root.open(
                        &location.relative,
                        libc::O_RDONLY | libc::O_DIRECTORY,
                        0,
                    )?,
                    location.writable,
                ))
            } else {
                None
            };
            Ok((entries, pinned))
        })();
        match result {
            Ok((entries, pinned)) => {
                let fh = self.next_handle;
                self.next_handle += 1;
                self.directories.insert(fh, entries);
                if let Some((file, writable)) = pinned {
                    self.files.insert(
                        fh,
                        Handle {
                            file,
                            ino,
                            writable,
                            uid: req.uid(),
                        },
                    );
                }
                reply.opened(fh, 0);
            }
            Err(e) => reply.error(error(e)),
        }
    }

    fn readdir(
        &mut self,
        req: &Request<'_>,
        _ino: u64,
        fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        request_credentials!(req, reply);
        if offset < 0 {
            reply.error(libc::EINVAL);
            return;
        }
        let Some(entries) = self.directories.get(&fh) else {
            reply.error(libc::EBADF);
            return;
        };
        for (index, (ino, kind, name)) in entries.iter().enumerate().skip(offset as usize) {
            if reply.add(*ino, (index + 1) as i64, *kind, name) {
                break;
            }
        }
        reply.ok();
    }

    fn releasedir(
        &mut self,
        req: &Request<'_>,
        _ino: u64,
        fh: u64,
        _flags: i32,
        reply: ReplyEmpty,
    ) {
        request_credentials!(req, reply);
        self.directories.remove(&fh);
        if let Some(handle) = self.files.remove(&fh) {
            self.prune_identity(handle.ino);
        }
        reply.ok();
    }

    fn fsyncdir(
        &mut self,
        req: &Request<'_>,
        ino: u64,
        fh: u64,
        datasync: bool,
        reply: ReplyEmpty,
    ) {
        request_credentials!(req, reply);
        if self.directories.contains_key(&fh) && !self.files.contains_key(&fh) {
            reply.ok();
            return;
        }
        let result = self.file(fh, ino, false).and_then(|file| {
            if datasync {
                file.sync_data()
            } else {
                file.sync_all()
            }
        });
        match result {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(error(e)),
        }
    }

    fn statfs(&mut self, req: &Request<'_>, ino: u64, reply: ReplyStatfs) {
        request_credentials!(req, reply);
        let result = (|| {
            let entry = self.entry(ino, req.uid(), req.gid())?;
            let location = match &entry.node {
                Node::Physical(location, _) => location,
                _ => return Ok(None),
            };
            let file = location.root.open(&location.relative, libc::O_PATH, 0)?;
            let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
            check(unsafe { libc::fstatvfs(file.as_raw_fd(), &mut stat) })?;
            Ok(Some(stat))
        })();
        match result {
            Ok(Some(stat)) => reply.statfs(
                stat.f_blocks,
                stat.f_bfree,
                stat.f_bavail,
                stat.f_files,
                stat.f_ffree,
                stat.f_bsize as u32,
                stat.f_namemax as u32,
                stat.f_frsize as u32,
            ),
            Ok(None) => reply.statfs(0, 0, 0, 0, 0, 4096, 255, 4096),
            Err(e) => reply.error(error(e)),
        }
    }

    fn create(
        &mut self,
        req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        mode: u32,
        umask: u32,
        flags: i32,
        reply: ReplyCreate,
    ) {
        request_credentials!(req, reply);
        let result = (|| {
            let parent = self.path(parent)?;
            let location =
                self.namespace
                    .destination_with_gid(req.uid(), req.gid(), &parent, name)?;
            let file = location.open(flags | libc::O_CREAT, mode & !umask)?;
            let path = parent.join(name);
            let entry = self
                .namespace
                .lookup_with_gid(req.uid(), req.gid(), &path)?;
            let ino = self.inode_for(path, &entry, req.uid());
            let attr = self.attr(ino, &entry);
            let fh = self.handle(file, ino, true, req.uid());
            Ok((attr, fh))
        })();
        match result {
            Ok((attr, fh)) => reply.created(&TTL, &attr, 0, fh, 0),
            Err(e) => reply.error(error(e)),
        }
    }

    fn mkdir(
        &mut self,
        req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        mode: u32,
        umask: u32,
        reply: ReplyEntry,
    ) {
        request_credentials!(req, reply);
        let result = (|| {
            let parent = self.path(parent)?;
            let location =
                self.namespace
                    .destination_with_gid(req.uid(), req.gid(), &parent, name)?;
            location
                .root
                .mkdir(&location.relative, mode & !umask & 0o777)?;
            let path = parent.join(name);
            let entry = self
                .namespace
                .lookup_with_gid(req.uid(), req.gid(), &path)?;
            let ino = self.inode_for(path, &entry, req.uid());
            Ok(self.attr(ino, &entry))
        })();
        match result {
            Ok(attr) => reply.entry(&TTL, &attr, 0),
            Err(e) => reply.error(error(e)),
        }
    }

    fn unlink(&mut self, req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        request_credentials!(req, reply);
        match self.remove(parent, name, false, req.uid(), req.gid()) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(error(e)),
        }
    }

    fn rmdir(&mut self, req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        request_credentials!(req, reply);
        match self.remove(parent, name, true, req.uid(), req.gid()) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(error(e)),
        }
    }

    fn symlink(
        &mut self,
        req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        target: &Path,
        reply: ReplyEntry,
    ) {
        request_credentials!(req, reply);
        let result = (|| {
            let parent = self.path(parent)?;
            let location =
                self.namespace
                    .destination_with_gid(req.uid(), req.gid(), &parent, name)?;
            location.root.symlink(&location.relative, target)?;
            let path = parent.join(name);
            let entry = self
                .namespace
                .lookup_with_gid(req.uid(), req.gid(), &path)?;
            let ino = self.inode_for(path, &entry, req.uid());
            Ok(self.attr(ino, &entry))
        })();
        match result {
            Ok(attr) => reply.entry(&TTL, &attr, 0),
            Err(e) => reply.error(error(e)),
        }
    }

    fn rename(
        &mut self,
        req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        newparent: u64,
        newname: &OsStr,
        flags: u32,
        reply: ReplyEmpty,
    ) {
        request_credentials!(req, reply);
        let result = (|| {
            if flags & !libc::RENAME_NOREPLACE != 0 {
                return Err(errno(libc::EOPNOTSUPP));
            }
            let uid = req.uid();
            let gid = req.gid();
            let from = self.child(parent, name, uid, gid)?;
            let to_parent = self.path(newparent)?;
            let to = to_parent.join(newname);
            let entry = self.namespace.lookup_with_gid(uid, gid, &from)?;
            let source = entry.mutation()?;
            let destination = self
                .namespace
                .destination_with_gid(uid, gid, &to_parent, newname)?;
            Self::same_mount(source, &destination)?;
            source.root.rename(
                &source.relative,
                &destination.root,
                &destination.relative,
                flags,
            )?;
            if from == to
                || self
                    .inodes
                    .get(&ScopedPath::new(uid, from.clone()))
                    .is_some_and(|ino| {
                        self.inodes.get(&ScopedPath::new(uid, to.clone())) == Some(ino)
                    })
            {
                return Ok(());
            }
            self.forget_path(&to, uid);
            let moved =
                self.inodes
                    .iter()
                    .filter(|(path, _)| path.uid == uid)
                    .filter_map(|(path, ino)| {
                        path.path.strip_prefix(&from).ok().map(|suffix| {
                            (*ino, path.clone(), ScopedPath::new(uid, to.join(suffix)))
                        })
                    })
                    .collect::<Vec<_>>();
            for (ino, old, new) in moved {
                self.inodes.remove(&old);
                self.inodes.insert(new.clone(), ino);
                self.paths.insert(ino, new);
            }
            let identities = std::mem::take(&mut self.identities);
            for (mut identity, ino) in identities {
                if let Some(path) = &identity.directory {
                    if let Ok(suffix) = path.strip_prefix(&from) {
                        if identity.uid == uid {
                            identity.directory = Some(to.join(suffix));
                        }
                    }
                }
                self.identities.insert(identity, ino);
            }
            Ok(())
        })();
        match result {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(error(e)),
        }
    }

    fn link(
        &mut self,
        req: &Request<'_>,
        ino: u64,
        newparent: u64,
        newname: &OsStr,
        reply: ReplyEntry,
    ) {
        request_credentials!(req, reply);
        let result = (|| {
            let entry = self.entry(ino, req.uid(), req.gid())?;
            let source = entry.mutation()?;
            let parent = self.path(newparent)?;
            let destination =
                self.namespace
                    .destination_with_gid(req.uid(), req.gid(), &parent, newname)?;
            Self::same_mount(source, &destination)?;
            source.root.link(&source.relative, &destination.relative)?;
            let path = parent.join(newname);
            let entry = self
                .namespace
                .lookup_with_gid(req.uid(), req.gid(), &path)?;
            let ino = self.inode_for(path, &entry, req.uid());
            Ok(self.attr(ino, &entry))
        })();
        match result {
            Ok(attr) => reply.entry(&TTL, &attr, 0),
            Err(e) => reply.error(error(e)),
        }
    }

    fn setattr(
        &mut self,
        req: &Request<'_>,
        ino: u64,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        fh: Option<u64>,
        crtime: Option<SystemTime>,
        chgtime: Option<SystemTime>,
        bkuptime: Option<SystemTime>,
        flags: Option<u32>,
        reply: ReplyAttr,
    ) {
        request_credentials!(req, reply);
        let result = (|| {
            if uid.is_some()
                || gid.is_some()
                || crtime.is_some()
                || chgtime.is_some()
                || bkuptime.is_some()
                || flags.is_some()
            {
                return Err(errno(libc::EOPNOTSUPP));
            }
            let owned;
            let file = if let Some(fh) = fh {
                self.file(fh, ino, true)?
            } else {
                let entry = self.entry(ino, req.uid(), req.gid())?;
                let location = entry.location()?;
                location.writable()?;
                owned = if size.is_some() {
                    location.open(libc::O_WRONLY, 0)?
                } else {
                    location.root.open(&location.relative, libc::O_PATH, 0)?
                };
                &owned
            };
            if let Some(mode) = mode {
                if file.metadata()?.file_type().is_symlink() {
                    return Err(errno(libc::EOPNOTSUPP));
                }
                let pinned =
                    std::ffi::CString::new(format!("/proc/self/fd/{}", file.as_raw_fd())).unwrap();
                check(unsafe { libc::chmod(pinned.as_ptr(), mode & 0o777) })?;
            }
            if let Some(size) = size {
                file.set_len(size)?;
            }
            if atime.is_some() || mtime.is_some() {
                fn timespec(value: Option<TimeOrNow>) -> io::Result<libc::timespec> {
                    match value {
                        None => Ok(libc::timespec {
                            tv_sec: 0,
                            tv_nsec: libc::UTIME_OMIT,
                        }),
                        Some(TimeOrNow::Now) => Ok(libc::timespec {
                            tv_sec: 0,
                            tv_nsec: libc::UTIME_NOW,
                        }),
                        Some(TimeOrNow::SpecificTime(time)) => {
                            let nanos = match time.duration_since(UNIX_EPOCH) {
                                Ok(value) => value.as_nanos() as i128,
                                Err(e) => -(e.duration().as_nanos() as i128),
                            };
                            Ok(libc::timespec {
                                tv_sec: nanos
                                    .div_euclid(1_000_000_000)
                                    .try_into()
                                    .map_err(|_| errno(libc::EINVAL))?,
                                tv_nsec: nanos.rem_euclid(1_000_000_000) as _,
                            })
                        }
                    }
                }
                let times = [timespec(atime)?, timespec(mtime)?];
                check(unsafe {
                    libc::utimensat(
                        file.as_raw_fd(),
                        c"".as_ptr(),
                        times.as_ptr(),
                        libc::AT_EMPTY_PATH | libc::AT_SYMLINK_NOFOLLOW,
                    )
                })?;
            }
            let metadata = file.metadata()?;
            Ok(self.physical_attr(ino, &metadata, true))
        })();
        match result {
            Ok(attr) => reply.attr(&TTL, &attr),
            Err(e) => reply.error(error(e)),
        }
    }
}
