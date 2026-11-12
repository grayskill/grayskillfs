use crate::backend::check;
use std::ffi::{CString, OsStr};
use std::io;
use std::marker::PhantomData;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::process::Command;
use std::ptr;
use std::rc::Rc;

#[derive(Clone, Copy, Debug)]
pub struct Identity {
    uid: u32,
    gid: u32,
}

impl Identity {
    pub fn resolve(user: &OsStr) -> io::Result<Self> {
        let name = CString::new(user.as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "user contains a NUL byte"))?;
        if user.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "user must not be empty",
            ));
        }
        let uid = if user.as_bytes().iter().all(u8::is_ascii_digit) {
            Some(user.to_str().unwrap().parse::<u32>().map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidInput, "invalid numeric user ID")
            })?)
        } else {
            None
        };
        let mut buffer = vec![0; 1024];
        loop {
            let mut account: libc::passwd = unsafe { std::mem::zeroed() };
            let mut result = ptr::null_mut();
            let code = unsafe {
                if let Some(uid) = uid {
                    libc::getpwuid_r(
                        uid,
                        &mut account,
                        buffer.as_mut_ptr(),
                        buffer.len(),
                        &mut result,
                    )
                } else {
                    libc::getpwnam_r(
                        name.as_ptr(),
                        &mut account,
                        buffer.as_mut_ptr(),
                        buffer.len(),
                        &mut result,
                    )
                }
            };
            if code == libc::ERANGE && buffer.len() < 1024 * 1024 {
                buffer.resize(buffer.len() * 2, 0);
                continue;
            }
            if code != 0 {
                return Err(io::Error::from_raw_os_error(code));
            }
            if result.is_null() {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("no system account for user {}", user.to_string_lossy()),
                ));
            }
            if account.pw_uid == 0
                || account.pw_gid == 0
                || account.pw_uid == u32::MAX
                || account.pw_gid == u32::MAX
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "--user requires an unprivileged account with a nonzero UID and primary GID",
                ));
            }
            return Ok(Self {
                uid: account.pw_uid,
                gid: account.pw_gid,
            });
        }
    }

    pub fn require_root(&self) -> io::Result<()> {
        if unsafe { libc::geteuid() } != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "--user requires starting the daemon as root to set Git subprocess credentials",
            ));
        }
        Ok(())
    }

    pub fn configure_command(self, command: &mut Command) -> io::Result<()> {
        self.require_root()?;
        // Only async-signal-safe credential syscalls run between fork and exec.
        unsafe {
            command.pre_exec(move || self.drop_privileges());
        }
        Ok(())
    }

    pub fn own_directory(self, directory: &std::fs::File) -> io::Result<()> {
        check(unsafe { libc::fchown(directory.as_raw_fd(), self.uid, self.gid) })
    }

    fn drop_privileges(self) -> io::Result<()> {
        check(unsafe { libc::prctl(libc::PR_SET_KEEPCAPS, 0, 0, 0, 0) })?;
        check(unsafe { libc::setgroups(0, ptr::null()) })?;
        check(unsafe { libc::setresgid(self.gid, self.gid, self.gid) })?;
        check(unsafe { libc::setresuid(self.uid, self.uid, self.uid) })?;

        #[repr(C)]
        struct Header {
            version: u32,
            pid: i32,
        }
        #[repr(C)]
        #[derive(Clone, Copy)]
        struct Capabilities {
            effective: u32,
            permitted: u32,
            inheritable: u32,
        }
        let header = Header {
            version: 0x2008_0522,
            pid: 0,
        };
        let capabilities = [Capabilities {
            effective: 0,
            permitted: 0,
            inheritable: 0,
        }; 2];
        check(unsafe { libc::syscall(libc::SYS_capset, &header, capabilities.as_ptr()) as i32 })?;
        check(unsafe {
            libc::prctl(
                libc::PR_CAP_AMBIENT,
                libc::PR_CAP_AMBIENT_CLEAR_ALL,
                0,
                0,
                0,
            )
        })?;
        check(unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) })?;

        let mut uids = [0; 3];
        let mut gids = [0; 3];
        check(unsafe { libc::getresuid(&mut uids[0], &mut uids[1], &mut uids[2]) })?;
        check(unsafe { libc::getresgid(&mut gids[0], &mut gids[1], &mut gids[2]) })?;
        let groups = unsafe { libc::getgroups(0, ptr::null_mut()) };
        check(groups)?;
        if uids != [self.uid; 3] || gids != [self.gid; 3] || groups != 0 {
            return Err(io::Error::from_raw_os_error(libc::EPERM));
        }
        Ok(())
    }
}

pub struct FsCredentials {
    uid: u32,
    gid: u32,
    _thread_bound: PhantomData<Rc<()>>,
}

impl FsCredentials {
    pub fn for_request(uid: u32, gid: u32) -> io::Result<Option<Self>> {
        if unsafe { libc::geteuid() } == 0 {
            Self::enter(uid, gid).map(Some)
        } else {
            Ok(None)
        }
    }

    pub fn enter(uid: u32, gid: u32) -> io::Result<Self> {
        let previous = Self {
            uid: unsafe { libc::setfsuid(u32::MAX) } as u32,
            gid: unsafe { libc::setfsgid(u32::MAX) } as u32,
            _thread_bound: PhantomData,
        };
        unsafe {
            libc::setfsgid(gid);
            libc::setfsuid(uid);
        }
        if unsafe { libc::setfsuid(u32::MAX) } as u32 != uid
            || unsafe { libc::setfsgid(u32::MAX) } as u32 != gid
        {
            return Err(io::Error::from_raw_os_error(libc::EPERM));
        }
        Ok(previous)
    }

    pub fn daemon() -> io::Result<Self> {
        Self::enter(unsafe { libc::geteuid() }, unsafe { libc::getegid() })
    }
}

impl Drop for FsCredentials {
    fn drop(&mut self) {
        unsafe {
            libc::setfsuid(self.uid);
            libc::setfsgid(self.gid);
        }
        if unsafe { libc::setfsuid(u32::MAX) } as u32 != self.uid
            || unsafe { libc::setfsgid(u32::MAX) } as u32 != self.gid
        {
            eprintln!("grayskillfs: fatal: could not restore filesystem credentials");
            std::process::abort();
        }
    }
}
