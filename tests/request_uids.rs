use fuser::{Session, SessionACL};
use grayskillfs::{config::Config, fuse::LayerFs, namespace::Namespace};
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::UnixDatagram;
use std::thread::{self, JoinHandle};
use std::time::Duration;

struct Protocol {
    socket: UnixDatagram,
    worker: Option<JoinHandle<std::io::Result<()>>>,
    unique: u64,
}

fn u64_at(bytes: &[u8], offset: usize) -> u64 {
    u64::from_ne_bytes(bytes[offset..offset + 8].try_into().unwrap())
}

impl Protocol {
    fn new(namespace: Namespace) -> Self {
        // A datagram pair preserves FUSE's one-request-per-read framing while
        // exercising the real request dispatcher with arbitrary caller UIDs.
        let (socket, daemon) = UnixDatagram::pair().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let worker = thread::spawn(move || {
            Session::from_fd(LayerFs::new(namespace), daemon.into(), SessionACL::All).run()
        });
        let mut protocol = Self {
            socket,
            worker: Some(worker),
            unique: 0,
        };
        let init = [7u32, 31, 0, 0]
            .into_iter()
            .flat_map(u32::to_ne_bytes)
            .collect::<Vec<_>>();
        protocol.request(0, 26, 0, &init).unwrap();
        protocol
    }

    fn request(&mut self, uid: u32, opcode: u32, ino: u64, payload: &[u8]) -> Result<Vec<u8>, i32> {
        self.unique += 1;
        let mut message = Vec::new();
        message.extend_from_slice(&((40 + payload.len()) as u32).to_ne_bytes());
        message.extend_from_slice(&opcode.to_ne_bytes());
        message.extend_from_slice(&self.unique.to_ne_bytes());
        message.extend_from_slice(&ino.to_ne_bytes());
        for field in [uid, 100, std::process::id(), 0] {
            message.extend_from_slice(&field.to_ne_bytes());
        }
        message.extend_from_slice(payload);
        assert_eq!(self.socket.send(&message).unwrap(), message.len());
        let mut response = vec![0; 65536];
        let size = self.socket.recv(&mut response).unwrap();
        response.truncate(size);
        assert_eq!(u64_at(&response, 8), self.unique);
        assert_eq!(
            u32::from_ne_bytes(response[..4].try_into().unwrap()) as usize,
            size
        );
        let error = i32::from_ne_bytes(response[4..8].try_into().unwrap());
        if error != 0 {
            Err(-error)
        } else {
            Ok(response[16..].to_vec())
        }
    }

    fn lookup(&mut self, uid: u32, parent: u64, name: &str) -> u64 {
        let reply = self
            .request(uid, 1, parent, format!("{name}\0").as_bytes())
            .unwrap();
        u64_at(&reply, 0)
    }

    fn open(&mut self, uid: u32, ino: u64) -> Result<u64, i32> {
        self.request(uid, 14, ino, &[0; 8])
            .map(|reply| u64_at(&reply, 0))
    }

    fn read(&mut self, uid: u32, ino: u64, fh: u64, opcode: u32) -> Vec<u8> {
        let mut payload = vec![0; 40];
        payload[..8].copy_from_slice(&fh.to_ne_bytes());
        payload[16..20].copy_from_slice(&4096u32.to_ne_bytes());
        self.request(uid, opcode, ino, &payload).unwrap()
    }

    fn create(&mut self, uid: u32, parent: u64, name: &str) -> (u64, u64) {
        let mut payload = Vec::new();
        for field in [
            (libc::O_CREAT | libc::O_EXCL | libc::O_RDWR) as u32,
            0o600,
            0,
            0,
        ] {
            payload.extend_from_slice(&field.to_ne_bytes());
        }
        payload.extend_from_slice(format!("{name}\0").as_bytes());
        let response = self.request(uid, 35, parent, &payload).unwrap();
        (u64_at(&response, 0), u64_at(&response, 128))
    }

    fn write(&mut self, uid: u32, ino: u64, fh: u64, data: &[u8]) {
        let mut payload = vec![0; 40];
        payload[..8].copy_from_slice(&fh.to_ne_bytes());
        payload[16..20].copy_from_slice(&(data.len() as u32).to_ne_bytes());
        payload.extend_from_slice(data);
        self.request(uid, 16, ino, &payload).unwrap();
    }
}

impl Drop for Protocol {
    fn drop(&mut self) {
        self.socket.send(&[]).unwrap();
        self.worker.take().unwrap().join().unwrap().unwrap();
    }
}

#[test]
fn fuse_requests_select_uid_views_inodes_and_mutation_destinations() {
    let temp = tempfile::tempdir().unwrap();
    fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o755)).unwrap();
    let base = temp.path().join("base");
    let data = temp.path().join("data");
    fs::create_dir(&base).unwrap();
    for (uid, content) in [(42, "first user"), (43, "second user, different length")] {
        fs::create_dir_all(data.join(uid.to_string())).unwrap();
        fs::write(data.join(format!("{uid}/shared")), content).unwrap();
        fs::write(data.join(format!("{uid}/only-{uid}")), "").unwrap();
        if unsafe { libc::geteuid() } == 0 {
            common::chown_tree(&data.join(uid.to_string()), uid, 100);
        }
    }
    let config = format!(
        "path {} onto / as read-only\npathsubst {}/{{{{uid}}}} onto /data as read-write\n",
        base.display(),
        data.display()
    );
    let namespace = Namespace::new(Config::parse(&config).unwrap(), None).unwrap();
    let mut fs = Protocol::new(namespace);
    let dir42 = fs.lookup(42, 1, "data");
    let dir43 = fs.lookup(43, 1, "data");
    assert_ne!(dir42, dir43);
    let file42 = fs.lookup(42, dir42, "shared");
    let file43 = fs.lookup(43, dir42, "shared");
    assert_ne!(file42, file43);
    assert_eq!(fs.lookup(43, dir43, "shared"), file43);
    let handle42 = fs.open(42, file42).unwrap();
    let handle43 = fs.open(43, file43).unwrap();
    assert_eq!(fs.read(42, file42, handle42, 15), b"first user");
    assert_eq!(
        fs.read(43, file43, handle43, 15),
        b"second user, different length"
    );
    assert_eq!(fs.open(43, file42), Err(libc::ESTALE));
    assert_eq!(fs.request(43, 3, file42, &[0; 16]), Err(libc::ESTALE));
    assert_eq!(fs.lookup(42, dir43, "shared"), file42);

    let (created42, fh42) = fs.create(42, dir43, "created");
    let (created43, fh43) = fs.create(43, dir42, "created");
    assert_ne!(created42, created43);
    fs.write(42, created42, fh42, b"42");
    fs.write(43, created43, fh43, b"43");
    assert_eq!(fs::read(data.join("42/created")).unwrap(), b"42");
    assert_eq!(fs::read(data.join("43/created")).unwrap(), b"43");
    if unsafe { libc::geteuid() } == 0 {
        for uid in [42, 43] {
            let metadata = fs::metadata(data.join(format!("{uid}/created"))).unwrap();
            assert_eq!((metadata.uid(), metadata.gid()), (uid, 100));
        }
    }

    let mut rename = dir42.to_ne_bytes().to_vec();
    rename.extend_from_slice(b"created\0renamed\0");
    fs.request(43, 12, dir42, &rename).unwrap();
    assert!(data.join("42/created").exists());
    assert!(!data.join("42/renamed").exists());
    assert_eq!(fs::read(data.join("43/renamed")).unwrap(), b"43");
    fs.request(43, 10, dir42, b"renamed\0").unwrap();
    assert!(data.join("42/created").exists());
    assert!(!data.join("43/renamed").exists());

    let reply = fs.request(43, 27, dir42, &[0; 8]).unwrap();
    let entries = fs.read(43, dir42, u64_at(&reply, 0), 28);
    let mut names = Vec::new();
    let mut offset = 0;
    while offset < entries.len() {
        let size =
            u32::from_ne_bytes(entries[offset + 16..offset + 20].try_into().unwrap()) as usize;
        names.push(std::str::from_utf8(&entries[offset + 24..offset + 24 + size]).unwrap());
        offset += (24 + size + 7) & !7;
    }
    assert!(names.contains(&"only-43"));
    assert!(!names.contains(&"only-42"));
}

#[test]
#[ignore = "requires user namespaces and subordinate UID/GID mappings"]
fn privilege_drop_fuse_requests_preserve_caller_ownership() {
    if common::in_user_namespace(
        "privilege_drop_fuse_requests_preserve_caller_ownership",
        "GRAYSKILLFS_REQUEST_CREDENTIALS_TEST_CHILD",
    ) {
        fuse_requests_select_uid_views_inodes_and_mutation_destinations();
    }
}
mod common;
