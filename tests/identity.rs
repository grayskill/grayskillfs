mod common;

use grayskillfs::identity::{FsCredentials, Identity};
use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::process::Command;

#[test]
fn identity_rejects_root_and_missing_accounts() {
    for user in [
        "",
        "root",
        "0",
        "4294967295",
        "4294967296",
        "grayskillfs-no-such-user",
    ] {
        assert!(
            Identity::resolve(OsStr::new(user)).is_err(),
            "accepted {user}"
        );
    }
    if unsafe { libc::geteuid() } != 0 {
        let uid = unsafe { libc::geteuid() }.to_string();
        let account = Identity::resolve(OsStr::new(&uid)).unwrap();
        assert!(account.require_root().is_err());
        assert!(account.configure_command(&mut Command::new("id")).is_err());
    }
}

#[test]
#[ignore = "requires unshare, newuidmap/newgidmap, subordinate IDs, and a nobody account"]
fn privilege_drop_is_permanent_and_clears_groups_and_capabilities() {
    if !common::in_user_namespace(
        "privilege_drop_is_permanent_and_clears_groups_and_capabilities",
        "GRAYSKILLFS_IDENTITY_TEST_CHILD",
    ) {
        return;
    }
    assert_eq!(unsafe { libc::geteuid() }, 0);
    Identity::resolve(OsStr::new("65534")).unwrap();
    let groups = [0, 1];
    assert_eq!(unsafe { libc::setgroups(groups.len(), groups.as_ptr()) }, 0);
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
    let mut capabilities = [Capabilities {
        effective: 0,
        permitted: 0,
        inheritable: 0,
    }; 2];
    assert_eq!(
        unsafe { libc::syscall(libc::SYS_capget, &header, capabilities.as_mut_ptr()) },
        0
    );
    capabilities[0].inheritable |= 1;
    assert_eq!(
        unsafe { libc::syscall(libc::SYS_capset, &header, capabilities.as_ptr()) },
        0
    );
    assert_eq!(
        unsafe {
            libc::prctl(
                libc::PR_CAP_AMBIENT,
                libc::PR_CAP_AMBIENT_RAISE,
                0, // CAP_CHOWN
                0,
                0,
            )
        },
        0
    );
    assert_eq!(unsafe { libc::prctl(libc::PR_SET_KEEPCAPS, 1, 0, 0, 0) }, 0);
    let before = fs::read_to_string("/proc/thread-self/status").unwrap();
    let temp = tempfile::tempdir().unwrap();
    fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o755)).unwrap();
    let executable = temp.path().join("identity-probe");
    fs::copy(std::env::current_exe().unwrap(), &executable).unwrap();
    let mut command = Command::new(executable);
    command
        .args([
            "--exact",
            "git_child_identity_probe",
            "--ignored",
            "--nocapture",
        ])
        .env("GRAYSKILLFS_IDENTITY_PROBE", "1")
        .current_dir("/");
    Identity::resolve(OsStr::new("nobody"))
        .unwrap()
        .configure_command(&mut command)
        .unwrap();
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let after = fs::read_to_string("/proc/thread-self/status").unwrap();
    for field in [
        "Uid:",
        "Gid:",
        "Groups:",
        "CapInh:",
        "CapPrm:",
        "CapEff:",
        "CapAmb:",
        "NoNewPrivs:",
    ] {
        assert_eq!(
            before.lines().find(|line| line.starts_with(field)),
            after.lines().find(|line| line.starts_with(field)),
            "parent's {field} changed"
        );
    }
}

#[test]
#[ignore = "launched by the isolated child credential test"]
fn git_child_identity_probe() {
    assert!(std::env::var_os("GRAYSKILLFS_IDENTITY_PROBE").is_some());
    assert_eq!(unsafe { libc::getuid() }, 65534);
    assert_eq!(unsafe { libc::geteuid() }, 65534);
    assert_eq!(unsafe { libc::getgid() }, 65534);
    assert_eq!(unsafe { libc::getegid() }, 65534);
    assert_eq!(unsafe { libc::getgroups(0, std::ptr::null_mut()) }, 0);
    assert_eq!(unsafe { libc::setuid(0) }, -1);
    assert_eq!(unsafe { libc::setgid(0) }, -1);
    let status = fs::read_to_string("/proc/thread-self/status").unwrap();
    for field in ["Uid:", "Gid:"] {
        assert_eq!(
            status
                .lines()
                .find(|line| line.starts_with(field))
                .unwrap()
                .split_whitespace()
                .skip(1)
                .collect::<Vec<_>>(),
            ["65534"; 4]
        );
    }
    for field in ["CapInh:", "CapPrm:", "CapEff:", "CapAmb:"] {
        assert_eq!(
            status
                .lines()
                .find(|line| line.starts_with(field))
                .unwrap()
                .split_whitespace()
                .nth(1)
                .unwrap(),
            "0000000000000000"
        );
    }
    assert!(status.lines().any(|line| line == "NoNewPrivs:\t1"));
    let output = Command::new("id").arg("-u").output().unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"65534\n");
}

#[test]
#[ignore = "requires user namespaces and subordinate UID/GID mappings"]
fn privilege_drop_filesystem_credentials_are_scoped() {
    if !common::in_user_namespace(
        "privilege_drop_filesystem_credentials_are_scoped",
        "GRAYSKILLFS_FSUID_TEST_CHILD",
    ) {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o777)).unwrap();
    let private = temp.path().join("private");
    fs::write(&private, "private").unwrap();
    fs::set_permissions(&private, fs::Permissions::from_mode(0o600)).unwrap();
    let setgid = temp.path().join("setgid");
    fs::create_dir(&setgid).unwrap();
    common::chown_tree(&setgid, 0, 65532);
    fs::set_permissions(&setgid, fs::Permissions::from_mode(0o2777)).unwrap();
    {
        let _credentials = FsCredentials::for_request(65534, 65533).unwrap().unwrap();
        assert_eq!(unsafe { libc::geteuid() }, 0);
        assert_eq!(unsafe { libc::getegid() }, 0);
        fs::write(temp.path().join("owned"), "owned").unwrap();
        let metadata = fs::metadata(temp.path().join("owned")).unwrap();
        assert_eq!((metadata.uid(), metadata.gid()), (65534, 65533));
        fs::write(setgid.join("inherited"), "inherited").unwrap();
        assert_eq!(fs::metadata(setgid.join("inherited")).unwrap().gid(), 65532);
        assert_eq!(
            fs::read(&private).unwrap_err().raw_os_error(),
            Some(libc::EACCES)
        );
        assert_eq!(
            fs::set_permissions(&private, fs::Permissions::from_mode(0o666))
                .unwrap_err()
                .raw_os_error(),
            Some(libc::EPERM)
        );
        {
            let _daemon = FsCredentials::daemon().unwrap();
            assert_eq!(fs::read(&private).unwrap(), b"private");
        }
        assert_eq!(
            fs::read(&private).unwrap_err().raw_os_error(),
            Some(libc::EACCES)
        );
    }
    assert_eq!(fs::read(&private).unwrap(), b"private");
    assert!(FsCredentials::enter(70000, 65533).is_err());
    let restored = temp.path().join("restored");
    fs::write(&restored, "root").unwrap();
    let metadata = fs::metadata(restored).unwrap();
    assert_eq!((metadata.uid(), metadata.gid()), (0, 0));
}
