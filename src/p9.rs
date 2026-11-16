use crate::config::DeviceKind;
use crate::identity::FsCredentials;
use crate::namespace::{Entry, Namespace, Node};
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{FileExt, MetadataExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::{hash::Hash, hash::Hasher};

const MAX_MESSAGE_SIZE: usize = 1024 * 1024;
const NOFID: u32 = u32::MAX;

struct Fid {
    path: PathBuf,
    uid: u32,
    gid: u32,
    file: Option<File>,
    xattr: Option<Xattr>,
}

enum Xattr {
    Read(Vec<u8>),
    Write {
        name: OsString,
        data: Vec<u8>,
        received: Vec<bool>,
        flags: i32,
    },
}

struct Decoder<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Decoder<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn take(&mut self, size: usize) -> io::Result<&'a [u8]> {
        let end = self
            .offset
            .checked_add(size)
            .filter(|end| *end <= self.bytes.len())
            .ok_or_else(invalid_data)?;
        let value = &self.bytes[self.offset..end];
        self.offset = end;
        Ok(value)
    }

    fn u16(&mut self) -> io::Result<u16> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }

    fn u32(&mut self) -> io::Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn u64(&mut self) -> io::Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    fn string(&mut self) -> io::Result<OsString> {
        let len = self.u16()? as usize;
        Ok(OsString::from_vec(self.take(len)?.to_vec()))
    }

    fn data(&mut self) -> io::Result<&'a [u8]> {
        let len = self.u32()? as usize;
        self.take(len)
    }
}

fn invalid_data() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid 9P message")
}

fn put_u8(out: &mut Vec<u8>, value: u8) {
    out.push(value);
}

fn put_u16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put_string(out: &mut Vec<u8>, value: &OsStr) -> io::Result<()> {
    let bytes = value.as_bytes();
    put_u16(
        out,
        bytes
            .len()
            .try_into()
            .map_err(|_| io::Error::from_raw_os_error(libc::ENAMETOOLONG))?,
    );
    out.extend_from_slice(bytes);
    Ok(())
}

fn put_data(out: &mut Vec<u8>, value: &[u8]) -> io::Result<()> {
    put_u32(
        out,
        value
            .len()
            .try_into()
            .map_err(|_| io::Error::from_raw_os_error(libc::E2BIG))?,
    );
    out.extend_from_slice(value);
    Ok(())
}

fn qid_path(path: &Path, identity: impl Hash) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut hasher);
    identity.hash(&mut hasher);
    hasher.finish()
}

fn put_qid(out: &mut Vec<u8>, entry: &Entry, path: &Path) -> io::Result<()> {
    match &entry.node {
        Node::Physical(location, metadata) => {
            let qtype = if metadata.is_dir() {
                0x80
            } else if metadata.file_type().is_symlink() {
                0x02
            } else {
                0
            };
            put_u8(out, qtype);
            put_u32(out, metadata.mtime() as u32);
            put_u64(
                out,
                qid_path(path, (location.layer, metadata.dev(), metadata.ino())),
            );
        }
        Node::Alias(_) => {
            put_u8(out, 0x02);
            put_u32(out, 0);
            put_u64(out, qid_path(path, 1u8));
        }
        Node::Directory => {
            put_u8(out, 0x80);
            put_u32(out, 0);
            put_u64(out, qid_path(path, 0u8));
        }
        Node::Device(device, _) => {
            put_u8(out, 0);
            put_u32(out, 0);
            put_u64(out, u64::from(device.rdev()));
        }
    }
    Ok(())
}

struct StatFields {
    mode: u32,
    uid: u32,
    gid: u64,
    nlink: u64,
    rdev: u64,
    size: u64,
    blksize: u64,
    blocks: u64,
}

fn stat_fields(entry: &Entry) -> io::Result<StatFields> {
    match &entry.node {
        Node::Physical(location, metadata) => {
            let mode = (metadata.mode() & libc::S_IFMT)
                | (metadata.mode() & 0o777 & if location.writable { 0o777 } else { !0o222 });
            Ok(StatFields {
                mode,
                uid: metadata.uid(),
                gid: u64::from(metadata.gid()),
                nlink: metadata.nlink(),
                rdev: metadata.rdev(),
                size: metadata.size(),
                blksize: metadata.blksize(),
                blocks: metadata.blocks(),
            })
        }
        Node::Alias(target) => Ok(StatFields {
            mode: libc::S_IFLNK | 0o777,
            uid: unsafe { libc::geteuid() },
            gid: unsafe { libc::getegid() } as u64,
            nlink: 1,
            rdev: 0,
            size: target.as_bytes().len() as u64,
            blksize: 4096,
            blocks: 0,
        }),
        Node::Directory => Ok(StatFields {
            mode: libc::S_IFDIR | 0o555,
            uid: unsafe { libc::geteuid() },
            gid: unsafe { libc::getegid() } as u64,
            nlink: 2,
            rdev: 0,
            size: 0,
            blksize: 4096,
            blocks: 0,
        }),
        Node::Device(device, writable) => Ok(StatFields {
            mode: (if device.kind == DeviceKind::Char {
                libc::S_IFCHR
            } else {
                libc::S_IFBLK
            }) | if *writable { 0o666 } else { 0o444 },
            uid: unsafe { libc::geteuid() },
            gid: unsafe { libc::getegid() } as u64,
            nlink: 1,
            rdev: u64::from(device.rdev()),
            size: 0,
            blksize: 4096,
            blocks: 0,
        }),
    }
}

fn qid_and_attributes(namespace: &mut Namespace, fid: &Fid) -> io::Result<(Entry, Vec<u8>)> {
    let entry = namespace.lookup_with_gid(fid.uid, fid.gid, &fid.path)?;
    let mut out = Vec::new();
    put_u64(&mut out, 0x7ff);
    put_qid(&mut out, &entry, &fid.path)?;
    let attributes = stat_fields(&entry)?;
    put_u32(&mut out, attributes.mode);
    put_u32(&mut out, attributes.uid);
    put_u32(&mut out, attributes.gid as u32);
    put_u64(&mut out, attributes.nlink);
    put_u64(&mut out, attributes.rdev);
    put_u64(&mut out, attributes.size);
    put_u64(&mut out, attributes.blksize);
    put_u64(&mut out, attributes.blocks);
    let (atime, atime_nsec, mtime, mtime_nsec, ctime, ctime_nsec) = match &entry.node {
        Node::Physical(_, metadata) => (
            metadata.atime(),
            metadata.atime_nsec(),
            metadata.mtime(),
            metadata.mtime_nsec(),
            metadata.ctime(),
            metadata.ctime_nsec(),
        ),
        _ => (0, 0, 0, 0, 0, 0),
    };
    for (seconds, nanos) in [
        (atime, atime_nsec),
        (mtime, mtime_nsec),
        (ctime, ctime_nsec),
        (0, 0),
    ] {
        put_u64(&mut out, seconds as u64);
        put_u64(&mut out, nanos as u64);
    }
    put_u64(&mut out, 0);
    put_u64(&mut out, 0);
    Ok((entry, out))
}

fn handle_request(
    kind: u8,
    request: &mut Decoder<'_>,
    namespace: &mut Namespace,
    fids: &mut HashMap<u32, Fid>,
    msize: &mut u32,
    peer: (u32, u32),
) -> io::Result<(u8, Vec<u8>)> {
    let mut reply = Vec::new();
    let response = match kind {
        100 => {
            let requested = request.u32()?;
            let version = request.string()?;
            let version = if version == "9P2000.L" {
                version
            } else {
                OsString::from("unknown")
            };
            *msize = requested.clamp(256, MAX_MESSAGE_SIZE as u32);
            put_u32(&mut reply, *msize);
            put_string(&mut reply, &version)?;
            101
        }
        104 => {
            let fid = request.u32()?;
            let afid = request.u32()?;
            let _uname = request.string()?;
            let aname = request.string()?;
            let _n_uname = request.u32()?;
            if afid != NOFID || !aname.is_empty() || fids.contains_key(&fid) {
                return Err(io::Error::from_raw_os_error(libc::EINVAL));
            }
            let (uid, gid) = peer;
            let root = Fid {
                path: PathBuf::new(),
                uid,
                gid,
                file: None,
                xattr: None,
            };
            let (entry, _) = qid_and_attributes(namespace, &root)?;
            put_qid(&mut reply, &entry, &root.path)?;
            fids.insert(fid, root);
            105
        }
        110 => {
            let fid = request.u32()?;
            let newfid = request.u32()?;
            let count = request.u16()? as usize;
            let original = fids
                .get(&fid)
                .ok_or_else(|| io::Error::from_raw_os_error(libc::EBADF))?;
            if newfid != fid && fids.contains_key(&newfid) {
                return Err(io::Error::from_raw_os_error(libc::EEXIST));
            }
            let mut walked = Fid {
                path: original.path.clone(),
                uid: original.uid,
                gid: original.gid,
                file: None,
                xattr: None,
            };
            let mut qids = Vec::new();
            let mut failed = false;
            for index in 0..count {
                let name = request.string()?;
                let entry = namespace.lookup_with_gid(walked.uid, walked.gid, &walked.path)?;
                if !entry.is_dir() {
                    if index == 0 {
                        return Err(io::Error::from_raw_os_error(libc::ENOTDIR));
                    }
                    failed = true;
                    break;
                }
                if name == "." {
                } else if name == ".." {
                    walked.path.pop();
                } else {
                    if name.is_empty()
                        || name.as_bytes().contains(&b'/')
                        || name.as_bytes().contains(&0)
                    {
                        if index == 0 {
                            return Err(io::Error::from_raw_os_error(libc::EINVAL));
                        }
                        failed = true;
                        break;
                    }
                    walked.path.push(name);
                }
                match namespace.lookup_with_gid(walked.uid, walked.gid, &walked.path) {
                    Ok(entry) => put_qid(&mut qids, &entry, &walked.path)?,
                    Err(error) if index == 0 => return Err(error),
                    Err(_) => {
                        walked.path.pop();
                        failed = true;
                        break;
                    }
                }
            }
            if count == 0 || !failed || !qids.is_empty() {
                fids.insert(newfid, walked);
            }
            put_u16(&mut reply, (qids.len() / 13).min(u16::MAX as usize) as u16);
            reply.extend_from_slice(&qids);
            111
        }
        12 => {
            let fid = request.u32()?;
            let flags = request.u32()?;
            let target = fids
                .get_mut(&fid)
                .ok_or_else(|| io::Error::from_raw_os_error(libc::EBADF))?;
            let entry = namespace.lookup_with_gid(target.uid, target.gid, &target.path)?;
            let flags = map_open_flags(flags)?;
            let file = if entry.is_dir() {
                if flags & libc::O_ACCMODE != libc::O_RDONLY || flags & libc::O_TRUNC != 0 {
                    return Err(io::Error::from_raw_os_error(libc::EISDIR));
                }
                None
            } else {
                Some(entry.location()?.open(flags, 0)?)
            };
            put_qid(&mut reply, &entry, &target.path)?;
            put_u32(&mut reply, 0);
            target.file = file;
            13
        }
        116 => {
            let fid = request.u32()?;
            let offset = request.u64()?;
            let count = request.u32()? as usize;
            let target = fids
                .get(&fid)
                .ok_or_else(|| io::Error::from_raw_os_error(libc::EBADF))?;
            let limit = count.min(msize.saturating_sub(11) as usize);
            let data = if let Some(Xattr::Read(data)) = &target.xattr {
                let start = usize::try_from(offset)
                    .unwrap_or(usize::MAX)
                    .min(data.len());
                data[start..start.saturating_add(limit).min(data.len())].to_vec()
            } else {
                let file = target
                    .file
                    .as_ref()
                    .ok_or_else(|| io::Error::from_raw_os_error(libc::EBADF))?;
                let mut data = vec![0; limit];
                let read = file.read_at(&mut data, offset)?;
                data.truncate(read);
                data
            };
            put_data(&mut reply, &data)?;
            117
        }
        118 => {
            let fid = request.u32()?;
            let offset = request.u64()?;
            let data = request.data()?;
            let target = fids
                .get_mut(&fid)
                .ok_or_else(|| io::Error::from_raw_os_error(libc::EBADF))?;
            let size = if let Some(Xattr::Write {
                name,
                data: value,
                received,
                flags,
            }) = &mut target.xattr
            {
                let start = usize::try_from(offset)
                    .map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
                let end = start
                    .checked_add(data.len())
                    .filter(|end| *end <= value.len())
                    .ok_or_else(|| io::Error::from_raw_os_error(libc::EINVAL))?;
                value[start..end].copy_from_slice(data);
                received[start..end].fill(true);
                let size = data.len();
                if received.iter().all(|received| *received) {
                    let entry = namespace.lookup_with_gid(target.uid, target.gid, &target.path)?;
                    let location = entry.mutation()?;
                    location
                        .root
                        .set_xattr(&location.relative, name, value, *flags)?;
                    target.xattr = None;
                }
                size
            } else {
                let file = target
                    .file
                    .as_ref()
                    .ok_or_else(|| io::Error::from_raw_os_error(libc::EBADF))?;
                file.write_at(data, offset)?
            };
            put_u32(
                &mut reply,
                size.try_into()
                    .map_err(|_| io::Error::from_raw_os_error(libc::EIO))?,
            );
            119
        }
        120 => {
            let fid = request.u32()?;
            if fids.remove(&fid).is_none() {
                return Err(io::Error::from_raw_os_error(libc::EBADF));
            }
            121
        }
        22 => {
            let fid = request.u32()?;
            let target = fids
                .get(&fid)
                .ok_or_else(|| io::Error::from_raw_os_error(libc::EBADF))?;
            let entry = namespace.lookup_with_gid(target.uid, target.gid, &target.path)?;
            put_string(&mut reply, &entry.read_link()?)?;
            23
        }
        24 => {
            let fid = request.u32()?;
            let _mask = request.u64()?;
            let target = fids
                .get(&fid)
                .ok_or_else(|| io::Error::from_raw_os_error(libc::EBADF))?;
            let (_, attributes) = qid_and_attributes(namespace, target)?;
            reply = attributes;
            25
        }
        40 => {
            let fid = request.u32()?;
            let offset = request.u64()?;
            let count = request.u32()? as usize;
            let target = fids
                .get(&fid)
                .ok_or_else(|| io::Error::from_raw_os_error(libc::EBADF))?;
            let entry = namespace.lookup_with_gid(target.uid, target.gid, &target.path)?;
            if !entry.is_dir() {
                return Err(io::Error::from_raw_os_error(libc::ENOTDIR));
            }
            let mut entries = vec![
                (OsString::from("."), target.path.clone()),
                (
                    OsString::from(".."),
                    target.path.parent().unwrap_or(Path::new("")).to_owned(),
                ),
            ];
            entries.extend(
                entry
                    .children()?
                    .into_iter()
                    .map(|name| (name.clone(), target.path.join(name))),
            );
            let start = usize::try_from(offset).unwrap_or(usize::MAX);
            let mut encoded = Vec::new();
            for (index, (name, path)) in entries.into_iter().enumerate().skip(start) {
                let child = match namespace.lookup_with_gid(target.uid, target.gid, &path) {
                    Ok(child) => child,
                    Err(error) if error.raw_os_error() == Some(libc::ENOENT) => continue,
                    Err(error) => return Err(error),
                };
                let mut record = Vec::new();
                put_qid(&mut record, &child, &path)?;
                put_u64(&mut record, (index + 1) as u64);
                put_string(&mut record, &name)?;
                if record.len() + encoded.len() > count
                    || record.len() + encoded.len() > msize.saturating_sub(11) as usize
                {
                    break;
                }
                encoded.extend_from_slice(&record);
            }
            put_data(&mut reply, &encoded)?;
            41
        }
        8 => {
            let fid = request.u32()?;
            let target = fids
                .get(&fid)
                .ok_or_else(|| io::Error::from_raw_os_error(libc::EBADF))?;
            let entry = namespace.lookup_with_gid(target.uid, target.gid, &target.path)?;
            let stat = match &entry.node {
                Node::Physical(location, _) => {
                    let file = location.root.open(&location.relative, libc::O_PATH, 0)?;
                    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
                    crate::backend::check(unsafe { libc::fstatvfs(file.as_raw_fd(), &mut stat) })?;
                    Some(stat)
                }
                _ => None,
            };
            put_u32(&mut reply, 0);
            put_u32(&mut reply, stat.as_ref().map_or(4096, |s| s.f_bsize as u32));
            for value in [
                stat.as_ref().map_or(0, |s| s.f_blocks),
                stat.as_ref().map_or(0, |s| s.f_bfree),
                stat.as_ref().map_or(0, |s| s.f_bavail),
                stat.as_ref().map_or(0, |s| s.f_files),
                stat.as_ref().map_or(0, |s| s.f_ffree),
                0,
            ] {
                put_u64(&mut reply, value);
            }
            put_u32(
                &mut reply,
                stat.as_ref().map_or(255, |s| s.f_namemax as u32),
            );
            9
        }
        72 => {
            let fid = request.u32()?;
            let name = request.string()?;
            let mode = request.u32()?;
            let gid = request.u32()?;
            let parent = fids
                .get(&fid)
                .ok_or_else(|| io::Error::from_raw_os_error(libc::EBADF))?;
            let location =
                namespace.destination_with_gid(parent.uid, parent.gid, &parent.path, &name)?;
            location.root.mkdir(&location.relative, mode & 0o777)?;
            let path = parent.path.join(name);
            let created = namespace.lookup_with_gid(parent.uid, parent.gid, &path)?;
            let _ = gid;
            put_qid(&mut reply, &created, &path)?;
            73
        }
        14 => {
            let fid = request.u32()?;
            let name = request.string()?;
            let flags = request.u32()?;
            let mode = request.u32()?;
            let _gid = request.u32()?;
            let parent = fids
                .get_mut(&fid)
                .ok_or_else(|| io::Error::from_raw_os_error(libc::EBADF))?;
            let location =
                namespace.destination_with_gid(parent.uid, parent.gid, &parent.path, &name)?;
            let file = location.open(map_open_flags(flags)? | libc::O_CREAT, mode & 0o777)?;
            parent.path.push(name);
            let entry = namespace.lookup_with_gid(parent.uid, parent.gid, &parent.path)?;
            put_qid(&mut reply, &entry, &parent.path)?;
            put_u32(&mut reply, 0);
            parent.file = Some(file);
            15
        }
        16 => {
            let fid = request.u32()?;
            let name = request.string()?;
            let target = request.string()?;
            let _gid = request.u32()?;
            let parent = fids
                .get(&fid)
                .ok_or_else(|| io::Error::from_raw_os_error(libc::EBADF))?;
            let location =
                namespace.destination_with_gid(parent.uid, parent.gid, &parent.path, &name)?;
            location
                .root
                .symlink(&location.relative, Path::new(&target))?;
            let path = parent.path.join(name);
            let entry = namespace.lookup_with_gid(parent.uid, parent.gid, &path)?;
            put_qid(&mut reply, &entry, &path)?;
            17
        }
        76 => {
            let fid = request.u32()?;
            let name = request.string()?;
            let flags = request.u32()?;
            if flags & !(libc::AT_REMOVEDIR as u32) != 0 {
                return Err(io::Error::from_raw_os_error(libc::EINVAL));
            }
            let parent = fids
                .get(&fid)
                .ok_or_else(|| io::Error::from_raw_os_error(libc::EBADF))?;
            let path = parent.path.join(name);
            let entry = namespace.lookup_with_gid(parent.uid, parent.gid, &path)?;
            let location = entry.mutation()?;
            location
                .root
                .unlink(&location.relative, flags & libc::AT_REMOVEDIR as u32 != 0)?;
            77
        }
        74 => {
            let oldfid = request.u32()?;
            let oldname = request.string()?;
            let newfid = request.u32()?;
            let newname = request.string()?;
            let old_parent = fids
                .get(&oldfid)
                .ok_or_else(|| io::Error::from_raw_os_error(libc::EBADF))?;
            let new_parent = fids
                .get(&newfid)
                .ok_or_else(|| io::Error::from_raw_os_error(libc::EBADF))?;
            if old_parent.uid != new_parent.uid || old_parent.gid != new_parent.gid {
                return Err(io::Error::from_raw_os_error(libc::EXDEV));
            }
            let source_path = old_parent.path.join(oldname);
            let source = namespace.lookup_with_gid(old_parent.uid, old_parent.gid, &source_path)?;
            let source_location = source.mutation()?;
            let destination = namespace.destination_with_gid(
                new_parent.uid,
                new_parent.gid,
                &new_parent.path,
                &newname,
            )?;
            if source_location.layer != destination.layer
                || !std::sync::Arc::ptr_eq(&source_location.root, &destination.root)
            {
                return Err(io::Error::from_raw_os_error(libc::EXDEV));
            }
            source_location.root.rename(
                &source_location.relative,
                &destination.root,
                &destination.relative,
                0,
            )?;
            75
        }
        50 => {
            let fid = request.u32()?;
            let datasync = request.u32()? != 0;
            let target = fids
                .get(&fid)
                .ok_or_else(|| io::Error::from_raw_os_error(libc::EBADF))?;
            let file = target
                .file
                .as_ref()
                .ok_or_else(|| io::Error::from_raw_os_error(libc::EBADF));
            let Ok(file) = file else {
                let entry = namespace.lookup_with_gid(target.uid, target.gid, &target.path)?;
                if entry.is_dir() {
                    return Ok((51, Vec::new()));
                }
                return Err(io::Error::from_raw_os_error(libc::EBADF));
            };
            if datasync {
                file.sync_data()?;
            } else {
                file.sync_all()?;
            }
            51
        }
        26 => {
            let fid = request.u32()?;
            let valid = request.u32()?;
            let mode = request.u32()?;
            let uid = request.u32()?;
            let gid = request.u32()?;
            let size = request.u64()?;
            let atime_sec = request.u64()?;
            let atime_nsec = request.u64()?;
            let mtime_sec = request.u64()?;
            let mtime_nsec = request.u64()?;
            let target = fids
                .get(&fid)
                .ok_or_else(|| io::Error::from_raw_os_error(libc::EBADF))?;
            let entry = namespace.lookup_with_gid(target.uid, target.gid, &target.path)?;
            let location = entry.mutation()?;
            let mut flags = if entry.is_dir() {
                libc::O_RDONLY | libc::O_DIRECTORY
            } else {
                libc::O_RDONLY
            };
            if valid & 8 != 0 {
                if entry.is_dir() {
                    return Err(io::Error::from_raw_os_error(libc::EISDIR));
                }
                flags = libc::O_WRONLY;
            }
            let file = location.root.open(&location.relative, flags, 0)?;
            if valid & 1 != 0 {
                crate::backend::check(unsafe { libc::fchmod(file.as_raw_fd(), mode) })?;
            }
            if valid & (2 | 4) != 0 {
                crate::backend::check(unsafe {
                    libc::fchown(
                        file.as_raw_fd(),
                        if valid & 2 != 0 { uid } else { u32::MAX },
                        if valid & 4 != 0 { gid } else { u32::MAX },
                    )
                })?;
            }
            if valid & 8 != 0 {
                let size = libc::off_t::try_from(size)
                    .map_err(|_| io::Error::from_raw_os_error(libc::EFBIG))?;
                crate::backend::check(unsafe { libc::ftruncate(file.as_raw_fd(), size) })?;
            }
            if valid & (0x10 | 0x20) != 0 {
                let times = [
                    libc::timespec {
                        tv_sec: if valid & 0x10 != 0 {
                            libc::time_t::try_from(atime_sec)
                                .map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?
                        } else {
                            0
                        },
                        tv_nsec: if valid & 0x10 == 0 {
                            libc::UTIME_OMIT
                        } else if valid & 0x80 == 0 {
                            libc::UTIME_NOW
                        } else {
                            libc::c_long::try_from(atime_nsec)
                                .map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?
                        },
                    },
                    libc::timespec {
                        tv_sec: if valid & 0x20 != 0 {
                            libc::time_t::try_from(mtime_sec)
                                .map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?
                        } else {
                            0
                        },
                        tv_nsec: if valid & 0x20 == 0 {
                            libc::UTIME_OMIT
                        } else if valid & 0x100 == 0 {
                            libc::UTIME_NOW
                        } else {
                            libc::c_long::try_from(mtime_nsec)
                                .map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?
                        },
                    },
                ];
                crate::backend::check(unsafe { libc::futimens(file.as_raw_fd(), times.as_ptr()) })?;
            }
            27
        }
        30 => {
            let fid = request.u32()?;
            let newfid = request.u32()?;
            let name = request.string()?;
            if fids.contains_key(&newfid) {
                return Err(io::Error::from_raw_os_error(libc::EEXIST));
            }
            let original = fids
                .get(&fid)
                .ok_or_else(|| io::Error::from_raw_os_error(libc::EBADF))?;
            let entry = namespace.lookup_with_gid(original.uid, original.gid, &original.path)?;
            let location = entry.location()?;
            let value = if name.is_empty() {
                location.root.list_xattrs(&location.relative)?
            } else {
                location.root.get_xattr(&location.relative, &name)?
            };
            fids.insert(
                newfid,
                Fid {
                    path: original.path.clone(),
                    uid: original.uid,
                    gid: original.gid,
                    file: None,
                    xattr: Some(Xattr::Read(value.clone())),
                },
            );
            put_u64(
                &mut reply,
                value
                    .len()
                    .try_into()
                    .map_err(|_| io::Error::from_raw_os_error(libc::E2BIG))?,
            );
            31
        }
        32 => {
            let fid = request.u32()?;
            let name = request.string()?;
            let size = request.u64()?;
            let flags = request.u32()?;
            if size > 65536 || flags & !3 != 0 {
                return Err(io::Error::from_raw_os_error(libc::EINVAL));
            }
            let target = fids
                .get_mut(&fid)
                .ok_or_else(|| io::Error::from_raw_os_error(libc::EBADF))?;
            let entry = namespace.lookup_with_gid(target.uid, target.gid, &target.path)?;
            let location = entry.mutation()?;
            if size == 0 {
                location
                    .root
                    .set_xattr(&location.relative, &name, &[], flags as i32)?;
            } else {
                target.xattr = Some(Xattr::Write {
                    name,
                    data: vec![0; size as usize],
                    received: vec![false; size as usize],
                    flags: flags as i32,
                });
            }
            33
        }
        70 => {
            let directory_fid = request.u32()?;
            let source_fid = request.u32()?;
            let name = request.string()?;
            let parent = fids
                .get(&directory_fid)
                .ok_or_else(|| io::Error::from_raw_os_error(libc::EBADF))?;
            let source_fid = fids
                .get(&source_fid)
                .ok_or_else(|| io::Error::from_raw_os_error(libc::EBADF))?;
            let source =
                namespace.lookup_with_gid(source_fid.uid, source_fid.gid, &source_fid.path)?;
            let source_location = source.mutation()?;
            let destination =
                namespace.destination_with_gid(parent.uid, parent.gid, &parent.path, &name)?;
            if parent.uid != source_fid.uid
                || source_location.layer != destination.layer
                || !std::sync::Arc::ptr_eq(&source_location.root, &destination.root)
            {
                return Err(io::Error::from_raw_os_error(libc::EXDEV));
            }
            source_location
                .root
                .link(&source_location.relative, &destination.relative)?;
            71
        }
        _ => return Err(io::Error::from_raw_os_error(libc::EOPNOTSUPP)),
    };
    Ok((response, reply))
}

fn map_open_flags(flags: u32) -> io::Result<i32> {
    let access = match flags & 3 {
        0 => libc::O_RDONLY,
        1 => libc::O_WRONLY,
        2 => libc::O_RDWR,
        _ => return Err(io::Error::from_raw_os_error(libc::EINVAL)),
    };
    let mut mapped = access;
    for (source, destination) in [
        (0o00001000, libc::O_TRUNC),
        (0o00002000, libc::O_APPEND),
        (0o00000200, libc::O_EXCL),
        (0o00000100, libc::O_CREAT),
        (0o00200000, libc::O_DIRECTORY),
        (0o00400000, libc::O_NOFOLLOW),
        (0o00004000, libc::O_NONBLOCK),
    ] {
        if flags & source != 0 {
            mapped |= destination;
        }
    }
    Ok(mapped)
}

fn send_frame(stream: &mut UnixStream, tag: u16, kind: u8, payload: &[u8]) -> io::Result<()> {
    let size = 7usize
        .checked_add(payload.len())
        .filter(|size| *size <= MAX_MESSAGE_SIZE)
        .ok_or_else(invalid_data)?;
    stream.write_all(&(size as u32).to_le_bytes())?;
    stream.write_all(&[kind])?;
    stream.write_all(&tag.to_le_bytes())?;
    stream.write_all(payload)
}

fn peer_credentials(stream: &UnixStream) -> io::Result<(u32, u32)> {
    let mut credentials: libc::ucred = unsafe { std::mem::zeroed() };
    let mut size = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    crate::backend::check(unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut credentials as *mut libc::ucred).cast(),
            &mut size,
        )
    })?;
    if size as usize != std::mem::size_of::<libc::ucred>() {
        return Err(invalid_data());
    }
    Ok((credentials.uid, credentials.gid))
}

fn serve_connection(mut stream: UnixStream, namespace: Arc<Mutex<Namespace>>) -> io::Result<()> {
    let mut fids: HashMap<u32, Fid> = HashMap::new();
    let mut msize = 8192u32;
    let peer = peer_credentials(&stream)?;
    loop {
        let mut size = [0; 4];
        match stream.read_exact(&mut size) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(error) => return Err(error),
        }
        let size = u32::from_le_bytes(size) as usize;
        if !(7..=MAX_MESSAGE_SIZE).contains(&size) || size > msize as usize {
            return Err(invalid_data());
        }
        let mut frame = vec![0; size - 4];
        stream.read_exact(&mut frame)?;
        let kind = frame[0];
        let tag = u16::from_le_bytes([frame[1], frame[2]]);
        let mut request = Decoder::new(&frame[3..]);
        let identity = if kind == 104 {
            Some(peer)
        } else {
            let fid = request.u32()?;
            fids.get(&fid).map(|fid| (fid.uid, fid.gid))
        };
        let _credentials = identity
            .map(|(uid, gid)| FsCredentials::for_request(uid, gid))
            .transpose()?;
        let result = {
            let mut namespace = namespace
                .lock()
                .map_err(|_| io::Error::other("9P namespace lock poisoned"))?;
            handle_request(
                kind,
                &mut request,
                &mut namespace,
                &mut fids,
                &mut msize,
                peer,
            )
        };
        match result {
            Ok((response, payload)) => send_frame(&mut stream, tag, response, &payload)?,
            Err(error) => {
                let code = error.raw_os_error().unwrap_or(libc::EIO) as u32;
                send_frame(&mut stream, tag, 7, &code.to_le_bytes())?;
            }
        }
    }
}

pub fn serve(socket: &Path, namespace: Namespace) -> io::Result<()> {
    let listener = UnixListener::bind(socket)?;
    let namespace = Arc::new(Mutex::new(namespace));
    for connection in listener.incoming() {
        let stream = connection?;
        let namespace = namespace.clone();
        std::thread::Builder::new()
            .name("grayskillfs-9p".into())
            .spawn(move || {
                if let Err(error) = serve_connection(stream, namespace) {
                    eprintln!("grayskillfs: 9P connection: {error}");
                }
            })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn call(
        kind: u8,
        body: &[u8],
        namespace: &mut Namespace,
        fids: &mut HashMap<u32, Fid>,
        msize: &mut u32,
    ) -> io::Result<(u8, Vec<u8>)> {
        handle_request(
            kind,
            &mut Decoder::new(body),
            namespace,
            fids,
            msize,
            (unsafe { libc::geteuid() }, unsafe { libc::getegid() }),
        )
    }

    #[test]
    fn serves_read_only_namespace_over_9p2000_l() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("hello"), b"9P works").unwrap();
        let config = Config::parse(&format!(
            "path {} onto / as read-only\n",
            directory.path().display()
        ))
        .unwrap();
        let mut namespace = Namespace::new(config, None).unwrap();
        let mut fids = HashMap::new();
        let mut msize = 8192;

        let mut body = Vec::new();
        put_u32(&mut body, 8192);
        put_string(&mut body, OsStr::new("9P2000.L")).unwrap();
        let (kind, _) = call(100, &body, &mut namespace, &mut fids, &mut msize).unwrap();
        assert_eq!(kind, 101);

        body.clear();
        put_u32(&mut body, 1);
        put_u32(&mut body, NOFID);
        put_string(&mut body, OsStr::new("ignored")).unwrap();
        put_string(&mut body, OsStr::new("")).unwrap();
        put_u32(&mut body, 0);
        let (kind, _) = call(104, &body, &mut namespace, &mut fids, &mut msize).unwrap();
        assert_eq!(kind, 105);
        assert_eq!(fids[&1].uid, unsafe { libc::geteuid() });
        assert_eq!(fids[&1].gid, unsafe { libc::getegid() });

        body.clear();
        put_u32(&mut body, 1);
        put_u64(&mut body, 0);
        put_u32(&mut body, 24);
        let (kind, reply) = call(40, &body, &mut namespace, &mut fids, &mut msize).unwrap();
        assert_eq!(kind, 41);
        assert_eq!(u32::from_le_bytes(reply[..4].try_into().unwrap()), 24);

        body.clear();
        put_u32(&mut body, 1);
        put_u32(&mut body, 2);
        put_u16(&mut body, 1);
        put_string(&mut body, OsStr::new("hello")).unwrap();
        let (kind, reply) = call(110, &body, &mut namespace, &mut fids, &mut msize).unwrap();
        assert_eq!(kind, 111);
        assert_eq!(u16::from_le_bytes(reply[..2].try_into().unwrap()), 1);

        body.clear();
        put_u32(&mut body, 2);
        put_u32(&mut body, 0);
        let (kind, _) = call(12, &body, &mut namespace, &mut fids, &mut msize).unwrap();
        assert_eq!(kind, 13);

        body.clear();
        put_u32(&mut body, 2);
        put_u64(&mut body, 0);
        put_u32(&mut body, 32);
        let (kind, reply) = call(116, &body, &mut namespace, &mut fids, &mut msize).unwrap();
        assert_eq!(kind, 117);
        assert_eq!(&reply[4..], b"9P works");

        body.clear();
        put_u32(&mut body, 1);
        put_u32(&mut body, 2);
        put_u16(&mut body, 1);
        put_string(&mut body, OsStr::new("hello")).unwrap();
        assert_eq!(
            call(110, &body, &mut namespace, &mut fids, &mut msize)
                .unwrap_err()
                .raw_os_error(),
            Some(libc::EEXIST)
        );

        body.clear();
        put_u32(&mut body, 1);
        put_string(&mut body, OsStr::new("new")).unwrap();
        put_u32(&mut body, libc::O_WRONLY as u32 | 0o00000100);
        put_u32(&mut body, 0o644);
        put_u32(&mut body, unsafe { libc::getegid() });
        assert_eq!(
            call(14, &body, &mut namespace, &mut fids, &mut msize)
                .unwrap_err()
                .raw_os_error(),
            Some(libc::EROFS)
        );
        assert!(!directory.path().join("new").exists());
    }
}
