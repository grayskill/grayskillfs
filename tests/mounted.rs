mod common;

use common::Fixture;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{symlink, FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Mount {
    child: Child,
    path: PathBuf,
}

impl Drop for Mount {
    fn drop(&mut self) {
        let output = Command::new("fusermount3")
            .arg("-u")
            .arg(&self.path)
            .output()
            .expect("run fusermount3");
        if !output.status.success() {
            eprintln!("unmount: {}", String::from_utf8_lossy(&output.stderr));
            let status = Command::new("fusermount3")
                .arg("-uz")
                .arg(&self.path)
                .status()
                .expect("lazy unmount");
            assert!(status.success(), "could not unmount test filesystem");
        }
        let _ = self.child.kill();
        self.child.wait().unwrap();
    }
}

fn mount(fixture: &Fixture) -> Mount {
    mount_with_options(fixture, &[])
}

fn mount_with_options(fixture: &Fixture, options: &[&std::ffi::OsStr]) -> Mount {
    let path = fixture.temp.path().join("mount");
    fs::create_dir(&path).unwrap();
    let config = fixture.temp.path().join("config");
    fs::write(&config, &fixture.config).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_grayskillfs"))
        .arg("mount")
        .arg(config)
        .arg(&path)
        .args(options)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            let mut stderr = String::new();
            child
                .stderr
                .take()
                .unwrap()
                .read_to_string(&mut stderr)
                .unwrap();
            panic!("FUSE daemon exited {status}: {stderr}");
        }
        let name = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statfs(name.as_ptr(), &mut stat) } == 0 && stat.f_type == 0x65735546 {
            break;
        }
        if Instant::now() > deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("mount did not become ready");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Mount { child, path }
}

#[test]
#[ignore = "requires Linux /dev/fuse and fusermount3; run explicitly"]
fn mounted_design_example_and_posix_operations() {
    let fixture = Fixture::new();
    let host_data = fixture.data.join(unsafe { libc::geteuid() }.to_string());
    let mount = mount(&fixture);
    let symbolic = mount.path.join(fixture.symbolic());
    assert_eq!(
        fs::read_link(&symbolic).unwrap(),
        Path::new(&fixture.selector)
    );
    let root = symbolic.join("alice");
    assert_eq!(fs::read_to_string(root.join("base.txt")).unwrap(), "base\n");
    assert_eq!(
        fs::read_to_string(root.join("package/repo.txt")).unwrap(),
        "git\n"
    );
    assert_eq!(
        fs::metadata(root.join("package"))
            .unwrap()
            .permissions()
            .mode()
            & 0o005,
        0o005
    );
    assert_eq!(
        fs::read_to_string(root.join("package/src/user.txt")).unwrap(),
        "alice\n"
    );
    let names = fs::read_dir(root.join("package"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<Vec<_>>();
    for name in ["repo.txt", "src", "data"] {
        assert!(names.contains(&name.into()));
    }

    let output = Command::new(root.join("bin/hello")).output().unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"mounted-exec\n");

    for name in ["base.txt", "package/repo.txt", "package/src/user.txt"] {
        assert!(OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(root.join(name))
            .is_err());
        assert!(fs::remove_file(root.join(name)).is_err());
        assert!(fs::set_permissions(root.join(name), fs::Permissions::from_mode(0o777)).is_err());
    }
    assert!(fs::write(root.join("no-write"), "bad").is_err());
    assert!(fs::remove_dir(root.join("package/data")).is_err());
    let data = root.join("package/data");
    fs::create_dir(data.join("work")).unwrap();
    let path = data.join("work/file");
    let mut file = OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    file.write_all(b"hello world").unwrap();
    file.sync_all().unwrap();
    file.seek(SeekFrom::Start(6)).unwrap();
    file.write_all(b"FUSE!").unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"hello FUSE!");
    file.set_len(5).unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"hello");
    file.set_permissions(fs::Permissions::from_mode(0o600))
        .unwrap();
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    file.set_times(
        std::fs::FileTimes::new()
            .set_modified(std::time::UNIX_EPOCH + Duration::from_secs(1_234_567)),
    )
    .unwrap();
    assert_eq!(
        fs::metadata(&path).unwrap().modified().unwrap(),
        std::time::UNIX_EPOCH + Duration::from_secs(1_234_567)
    );
    fs::rename(&path, data.join("work/renamed")).unwrap();
    file.seek(SeekFrom::End(0)).unwrap();
    file.write_all(b"!").unwrap();
    assert_eq!(fs::read(data.join("work/renamed")).unwrap(), b"hello!");
    fs::hard_link(data.join("work/renamed"), data.join("work/hard")).unwrap();
    assert_eq!(
        fs::metadata(data.join("work/renamed")).unwrap().ino(),
        fs::metadata(data.join("work/hard")).unwrap().ino()
    );
    fs::remove_file(data.join("work/renamed")).unwrap();
    assert_eq!(fs::read(data.join("work/hard")).unwrap(), b"hello!");
    fs::remove_file(data.join("work/hard")).unwrap();
    file.seek(SeekFrom::Start(0)).unwrap();
    let mut contents = String::new();
    file.read_to_string(&mut contents).unwrap();
    assert_eq!(contents, "hello!");
    assert_eq!(file.metadata().unwrap().len(), 6);
    drop(file);
    fs::remove_dir(data.join("work")).unwrap();

    fs::write(data.join("a"), "one").unwrap();
    OpenOptions::new()
        .append(true)
        .open(data.join("a"))
        .unwrap()
        .write_all(b"two")
        .unwrap();
    assert_eq!(fs::read(data.join("a")).unwrap(), b"onetwo");
    fs::write(data.join("b"), "old").unwrap();
    let mut old = File::open(data.join("b")).unwrap();
    fs::rename(data.join("a"), data.join("b")).unwrap();
    assert_eq!(fs::read(data.join("b")).unwrap(), b"onetwo");
    fs::set_permissions(data.join("b"), fs::Permissions::from_mode(0o0)).unwrap();
    fs::set_permissions(data.join("b"), fs::Permissions::from_mode(0o600)).unwrap();
    let mut old_contents = String::new();
    old.read_to_string(&mut old_contents).unwrap();
    assert_eq!(old_contents, "old");
    symlink("b", data.join("relative")).unwrap();
    assert_eq!(fs::read(data.join("relative")).unwrap(), b"onetwo");
    symlink("../repo.txt", data.join("readonly")).unwrap();
    assert_eq!(fs::read_to_string(data.join("readonly")).unwrap(), "git\n");
    assert!(fs::write(data.join("readonly"), "bad").is_err());
    let outside = fixture.temp.path().join("outside");
    fs::write(&outside, "secret").unwrap();
    symlink(&outside, data.join("host-link")).unwrap();
    assert!(OpenOptions::new()
        .write(true)
        .open(root.join("package/repo.txt"))
        .is_err());
    assert_eq!(fs::read_to_string(&outside).unwrap(), "secret");
    assert_eq!(fs::read_to_string(host_data.join("b")).unwrap(), "onetwo");
    assert_eq!(
        fs::read_to_string(fixture.base.join("base.txt")).unwrap(),
        "base\n"
    );

    fs::create_dir(data.join("before")).unwrap();
    fs::write(data.join("before/file"), "renamed directory").unwrap();
    let before = fs::metadata(data.join("before")).unwrap().ino();
    fs::rename(data.join("before"), data.join("after")).unwrap();
    assert_eq!(fs::metadata(data.join("after")).unwrap().ino(), before);
    assert_eq!(
        fs::read(data.join("after/file")).unwrap(),
        b"renamed directory"
    );
    let open_directory = File::open(data.join("after")).unwrap();
    fs::remove_file(data.join("after/file")).unwrap();
    fs::remove_dir(data.join("after")).unwrap();
    assert!(open_directory.metadata().unwrap().is_dir());
    drop(open_directory);

    fs::write(host_data.join("external"), "original").unwrap();
    fs::hard_link(host_data.join("external"), host_data.join("external-hard")).unwrap();
    assert_eq!(
        fs::metadata(data.join("external")).unwrap().ino(),
        fs::metadata(data.join("external-hard")).unwrap().ino()
    );
    let mut original = File::open(data.join("external")).unwrap();
    fs::write(host_data.join("replacement"), "replacement").unwrap();
    fs::rename(host_data.join("replacement"), host_data.join("external")).unwrap();
    assert_eq!(fs::read(data.join("external")).unwrap(), b"replacement");
    let mut contents = String::new();
    original.read_to_string(&mut contents).unwrap();
    assert_eq!(contents, "original");

    let status = Command::new("git")
        .args(["-c", "init.defaultBranch=main"])
        .arg("init")
        .arg(data.join("git-work"))
        .output()
        .unwrap();
    assert!(
        status.status.success(),
        "{}",
        String::from_utf8_lossy(&status.stderr)
    );
    fs::write(
        data.join("Makefile"),
        "all: result\nresult:\n\tprintf built > result\n",
    )
    .unwrap();
    let output = Command::new("make").arg("-C").arg(&data).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fs::read(data.join("result")).unwrap(), b"built");
    let data_name = std::ffi::CString::new(data.as_os_str().as_encoded_bytes()).unwrap();
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    assert_eq!(unsafe { libc::statvfs(data_name.as_ptr(), &mut stat) }, 0);
    assert!(stat.f_blocks > 0);

    // Exercise directory offsets over several FUSE reply buffers.
    for index in 0..400 {
        fs::write(host_data.join(format!("entry-{index:04}")), "").unwrap();
    }
    let names = fs::read_dir(&data)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<Vec<_>>();
    assert_eq!(
        names
            .iter()
            .filter(|name| name.to_string_lossy().starts_with("entry-"))
            .count(),
        400
    );
}

#[test]
#[ignore = "requires Linux /dev/fuse and fusermount3; run explicitly"]
fn mounted_virtual_devices_report_types_numbers_and_nodev() {
    let mut fixture = Fixture::new();
    fixture.config.push_str(
        "dev char 1 3 onto /dev/null as read-write\n\
         dev block 259 300 onto /dev/disk as read-only\n",
    );
    let mount = mount(&fixture);
    let root = mount.path.join(fixture.root());
    let null = fs::metadata(root.join("dev/null")).unwrap();
    assert!(null.file_type().is_char_device());
    assert_eq!(libc::major(null.rdev()), 1);
    assert_eq!(libc::minor(null.rdev()), 3);
    assert_eq!(null.permissions().mode() & 0o777, 0o666);
    let disk = fs::metadata(root.join("dev/disk")).unwrap();
    assert!(disk.file_type().is_block_device());
    assert_eq!(libc::major(disk.rdev()), 259);
    assert_eq!(libc::minor(disk.rdev()), 300);
    assert_eq!(disk.permissions().mode() & 0o777, 0o444);
    assert_eq!(
        File::open(root.join("dev/null"))
            .unwrap_err()
            .raw_os_error(),
        Some(libc::EACCES)
    );
    assert!(fs::remove_file(root.join("dev/null")).is_err());
    assert_eq!(fs::read_dir(root.join("dev")).unwrap().count(), 2);
}

#[test]
#[ignore = "requires user/mount namespaces, subordinate IDs, nobody account, and FUSE"]
fn mounted_privilege_drop_is_git_only() {
    use std::ffi::OsStr;
    use std::os::unix::process::CommandExt;
    if !common::in_user_namespace(
        "mounted_privilege_drop_is_git_only",
        "GRAYSKILLFS_MOUNT_DROP_TEST_CHILD",
    ) {
        return;
    }
    let mut fixture = Fixture::new();
    let sessions = fixture.temp.path().join("sessions");
    fs::create_dir(&sessions).unwrap();
    fs::set_permissions(&sessions, fs::Permissions::from_mode(0o711)).unwrap();
    fixture.config.push_str(&format!(
        "pathsubst {}/{{{{uid}}}}/{{{{path}}}} onto /tmp as read-write mkdir mode=0700 uid={{{{uid}}}} gid={{{{gid}}}}\n",
        sessions.display()
    ));
    let cache = fixture.temp.path().join("cache");
    fs::create_dir(&cache).unwrap();
    common::chown_tree(&fixture.repo, 65534, 65534);
    fs::set_permissions(&cache, fs::Permissions::from_mode(0o711)).unwrap();
    fs::set_permissions(fixture.temp.path(), fs::Permissions::from_mode(0o755)).unwrap();
    let data = fixture.data.join("65533");
    fs::create_dir(&data).unwrap();
    fs::set_permissions(&data, fs::Permissions::from_mode(0o777)).unwrap();
    fs::write(data.join("existing"), "root-owned").unwrap();
    fs::set_permissions(data.join("existing"), fs::Permissions::from_mode(0o666)).unwrap();
    fs::create_dir(data.join("sticky")).unwrap();
    fs::set_permissions(data.join("sticky"), fs::Permissions::from_mode(0o1777)).unwrap();
    fs::write(data.join("sticky/secret"), "root-owned").unwrap();
    let mount = mount_with_options(
        &fixture,
        &[
            OsStr::new("--user"),
            OsStr::new("nobody"),
            OsStr::new("--allow-other"),
            OsStr::new("--cache-dir"),
            cache.as_os_str(),
        ],
    );
    assert_eq!(
        fs::read_dir(&cache).unwrap().count(),
        0,
        "Git cache must be lazy"
    );
    let root = mount.path.join(fixture.symbolic()).join("alice/session");
    assert_eq!(fs::read(root.join("package/repo.txt")).unwrap(), b"git\n");
    let status = fs::read_to_string(format!("/proc/{}/status", mount.child.id())).unwrap();
    for field in ["Uid:", "Gid:"] {
        let values = status
            .lines()
            .find(|line| line.starts_with(field))
            .unwrap()
            .split_whitespace()
            .skip(1)
            .collect::<Vec<_>>();
        assert_eq!(values, ["0"; 4], "daemon credentials must remain unchanged");
    }
    let synthetic = fs::metadata(root.join("package/src")).unwrap();
    assert_eq!(synthetic.uid(), 0, "backing ownership must not be remapped");
    let output = Command::new("sh")
        .args([
            "-ec",
            r#"
            cd "$1"
            test "$(cat package/repo.txt)" = git
            test "$(cat base.txt)" = base
            printf created > package/data/created
            mkdir package/data/directory
            ln -s created package/data/symlink
            ln package/data/created package/data/hardlink
            mv package/data/created package/data/renamed
            chmod 640 package/data/renamed
            printf updated > package/data/existing
            printf session > tmp/created
            if rm package/data/sticky/secret; then exit 1; fi
            ls package/data >/dev/null
        "#,
            "--",
        ])
        .arg(&root)
        .uid(65533)
        .gid(65532)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    for path in ["renamed", "directory", "symlink", "hardlink"] {
        let metadata = fs::symlink_metadata(data.join(path)).unwrap();
        assert_eq!((metadata.uid(), metadata.gid()), (65533, 65532), "{path}");
    }
    let existing = fs::metadata(data.join("existing")).unwrap();
    assert_eq!((existing.uid(), existing.gid()), (0, 0));
    assert_eq!(fs::read(data.join("existing")).unwrap(), b"updated");
    assert_eq!(fs::read(data.join("sticky/secret")).unwrap(), b"root-owned");
    for path in [sessions.join("65533"), sessions.join("65533/session")] {
        let metadata = fs::metadata(path).unwrap();
        assert_eq!(
            (metadata.uid(), metadata.gid(), metadata.mode() & 0o777),
            (65533, 65532, 0o700)
        );
    }
    assert_eq!(
        fs::read(sessions.join("65533/session/created")).unwrap(),
        b"session"
    );
    assert_eq!(fs::read(root.join("package/repo.txt")).unwrap(), b"git\n");
    for entry in fs::read_dir(&cache).unwrap() {
        let entry = entry.unwrap();
        let metadata = entry.metadata().unwrap();
        assert_eq!((metadata.uid(), metadata.gid()), (0, 0));
        let repositories = entry.path().join("repositories");
        let metadata = fs::metadata(&repositories).unwrap();
        assert_eq!((metadata.uid(), metadata.gid()), (65534, 65534));
        for repository in fs::read_dir(&repositories).unwrap() {
            let repository = repository.unwrap();
            if repository.file_name().to_string_lossy().ends_with(".git") {
                let metadata = fs::metadata(repository.path().join("HEAD")).unwrap();
                assert_eq!(
                    metadata.uid(),
                    65534,
                    "Git subprocess must run as the target user"
                );
                assert_eq!(metadata.gid(), 65534);
            }
        }
        for snapshot in fs::read_dir(entry.path()).unwrap() {
            let snapshot = snapshot.unwrap();
            if snapshot.file_name() == "repositories" {
                continue;
            }
            assert_eq!(snapshot.metadata().unwrap().uid(), 0);
            assert_eq!(
                fs::metadata(snapshot.path().join("repo.txt"))
                    .unwrap()
                    .uid(),
                0
            );
            let mut command = Command::new("sh");
            command
                .args([
                    "-ec",
                    r#"test ! -w "$1"; test ! -w "$2"; test -w "$3""#,
                    "--",
                ])
                .arg(entry.path())
                .arg(snapshot.path())
                .arg(&repositories);
            grayskillfs::identity::Identity::resolve(OsStr::new("nobody"))
                .unwrap()
                .configure_command(&mut command)
                .unwrap();
            let result = command.output().unwrap();
            assert!(
                result.status.success(),
                "Git user can modify daemon snapshots"
            );
        }
    }
    // Git cannot use the daemon's retained access to root-only repositories.
    let secret = fixture.temp.path().join("root-only");
    fs::create_dir(&secret).unwrap();
    common::git(&secret, &["init", "-b", "main"]);
    fs::write(secret.join("secret"), "must not be accessible to Git").unwrap();
    common::git(&secret, &["add", "."]);
    common::git(&secret, &["commit", "-m", "private"]);
    fs::set_permissions(&secret, fs::Permissions::from_mode(0o700)).unwrap();
    let selector = Command::new(env!("CARGO_BIN_EXE_grayskillfs"))
        .arg("encode")
        .arg(&secret)
        .arg("HEAD")
        .output()
        .unwrap();
    let selector = String::from_utf8(selector.stdout).unwrap();
    assert!(fs::read_link(mount.path.join(selector.trim())).is_err());
}
