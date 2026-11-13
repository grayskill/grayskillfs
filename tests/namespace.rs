mod common;

use base64::engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD};
use base64::Engine;
use common::{git, Fixture};
use grayskillfs::backend::Root;
use grayskillfs::config::Config;
use grayskillfs::namespace::{Namespace, Node};
use std::ffi::OsStr;
use std::fs;
use std::io::Read;
use std::os::unix::fs::symlink;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

#[test]
fn configuration_semantics_and_errors() {
    let config = Config::parse(
        "# comment\npath /root onto / as read-only\n\
         git {{path}} onto /package as read-only\n\
         pathsubst /users/{{path}} onto /package/src as read-only\n\
         pathsubst /data/{{uid}} onto /package/data as read-write\n",
    )
    .unwrap();
    assert_eq!(config.selectors(), 2);
    for text in [
        "",
        "path /base onto /other as read-only",
        "path relative onto / as read-only",
        "path /base onto /../x as read-only",
        "path /base onto /./x as read-only",
        "path /base onto / as writable",
        "path /{{path}} onto / as read-only",
        "pathsubst /{{unknown}} onto / as read-only",
        "path {{base64Path}} onto / as read-only",
        "git {{base64Path}} onto / as read-only",
        "pathsubst /prefix/{{base64Path}} onto / as read-only",
        "pathsubst {{base64Path}}/{{path}} onto / as read-only",
        "no-consume pathsubst /{{path}} onto / as read-only",
        "git {{path}} onto / as read-write",
        "no-consume git {{path}} onto / as read-only",
        "git {{uid}} onto / as read-only",
        "path /base onto / as read-only mode=0700",
        "path /base onto / as read-only mkdir-p",
        "path /base onto / as read-only mkdir mkdir",
        "path /base onto / as read-only mkdir mode=1777",
        "path /base onto / as read-only mkdir mode=0780",
        "path /base onto / as read-only mkdir mode=0700 mode=0755",
        "path /base onto / as read-only mkdir uid=4294967295",
        "path /base onto / as read-only mkdir gid={{path}}",
        "git {{path}} onto / as read-only mkdir",
        "dev char 1 3 onto / as read-write mkdir",
    ] {
        assert!(Config::parse(text).is_err(), "accepted {text}");
    }
}

#[test]
fn mkdir_p_is_lazy_and_preserves_existing_directory_metadata() {
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path().join("base");
    let sessions = temp.path().join("sessions");
    fs::create_dir(&base).unwrap();
    fs::create_dir(&sessions).unwrap();
    fs::set_permissions(&sessions, fs::Permissions::from_mode(0o711)).unwrap();
    let uid = unsafe { libc::geteuid() };
    let gid = unsafe { libc::getegid() };
    let config = format!(
        "path {} onto / as read-only\n\
         pathsubst {}/{{{{uid}}}}/{{{{path}}}} onto /tmp as read-write mkdir mode=0750 uid={{{{uid}}}} gid={{{{gid}}}}\n",
        base.display(), sessions.display()
    );
    let mut namespace = Namespace::new(Config::parse(&config).unwrap(), None).unwrap();
    assert!(!sessions.join(uid.to_string()).exists());
    namespace.lookup_with_gid(uid, gid, Path::new("")).unwrap();
    assert!(!sessions.join(uid.to_string()).exists());
    let entry = namespace
        .lookup_with_gid(uid, gid, Path::new("first/tmp"))
        .unwrap();
    assert!(entry.is_dir());
    let user = sessions.join(uid.to_string());
    let first = user.join("first");
    for path in [&user, &first] {
        let metadata = fs::metadata(path).unwrap();
        assert_eq!(
            (metadata.uid(), metadata.gid(), metadata.mode() & 0o777),
            (uid, gid, 0o750)
        );
    }
    assert_eq!(fs::metadata(&sessions).unwrap().mode() & 0o777, 0o711);

    fs::set_permissions(&first, fs::Permissions::from_mode(0o711)).unwrap();
    let changed = config.replace(
        "mode=0750 uid={{uid}} gid={{gid}}",
        "mode=0700 uid=12345 gid=23456",
    );
    let mut namespace = Namespace::new(Config::parse(&changed).unwrap(), None).unwrap();
    namespace
        .lookup_with_gid(uid, gid, Path::new("first/tmp"))
        .unwrap();
    let metadata = fs::metadata(&first).unwrap();
    assert_eq!(
        (metadata.uid(), metadata.gid(), metadata.mode() & 0o777),
        (uid, gid, 0o711)
    );
    assert_eq!(fs::metadata(&user).unwrap().mode() & 0o777, 0o750);
}

#[test]
fn mkdir_p_supports_fixed_sources_and_reports_ownership_failures() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("fixed/nested");
    let uid = unsafe { libc::geteuid() };
    let gid = unsafe { libc::getegid() };
    let config = format!(
        "path {} onto / as read-only mkdir mode=0710 uid={uid} gid={gid}\n",
        source.display()
    );
    let mut namespace = Namespace::new(Config::parse(&config).unwrap(), None).unwrap();
    assert!(!source.exists());
    namespace.lookup(uid, Path::new("")).unwrap();
    assert_eq!(fs::metadata(&source).unwrap().mode() & 0o777, 0o710);
    if uid != 0 {
        let denied = temp.path().join("denied");
        let config = format!(
            "path {} onto / as read-only mkdir uid=0 gid=0\n",
            denied.display()
        );
        let mut namespace = Namespace::new(Config::parse(&config).unwrap(), None).unwrap();
        assert_eq!(
            namespace
                .lookup(uid, Path::new(""))
                .unwrap_err()
                .raw_os_error(),
            Some(libc::EPERM)
        );
        assert!(!denied.exists());
        assert!(fs::read_dir(temp.path()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".grayskillfs-mkdir-")
        }));
    }
}

#[test]
fn mkdir_p_rejects_symlink_and_file_components_without_modifying_them() {
    let temp = tempfile::tempdir().unwrap();
    let outside = temp.path().join("outside");
    fs::create_dir(&outside).unwrap();
    let link = temp.path().join("link");
    symlink(&outside, &link).unwrap();
    let file = temp.path().join("file");
    fs::write(&file, "unchanged").unwrap();
    for prefix in [&link, &file] {
        let config = format!(
            "pathsubst {}/{{{{path}}}} onto / as read-only mkdir\n",
            prefix.display()
        );
        let mut namespace = Namespace::new(Config::parse(&config).unwrap(), None).unwrap();
        let error = namespace
            .lookup(unsafe { libc::geteuid() }, Path::new("session"))
            .unwrap_err();
        assert!(matches!(
            error.raw_os_error(),
            Some(libc::ELOOP | libc::ENOTDIR)
        ));
    }
    assert!(!outside.join("session").exists());
    assert_eq!(fs::read(file).unwrap(), b"unchanged");
}

#[test]
fn mkdir_p_handles_concurrent_creators_without_leaving_staging_directories() {
    let temp = tempfile::tempdir().unwrap();
    let root = Root::new(temp.path()).unwrap();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(4));
    let uid = unsafe { libc::geteuid() };
    let gid = unsafe { libc::getegid() };
    let workers = (0..4)
        .map(|_| {
            let root = root.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                root.mkdir_p(Path::new("shared/session"), 0o750, uid, gid)
                    .unwrap()
                    .metadata(Path::new(""))
                    .unwrap()
                    .ino()
            })
        })
        .collect::<Vec<_>>();
    let inodes = workers
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .collect::<Vec<_>>();
    assert!(inodes.iter().all(|ino| *ino == inodes[0]));
    assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 1);
    assert_eq!(fs::read_dir(temp.path().join("shared")).unwrap().count(), 1);
}

fn read(ns: &mut Namespace, path: &Path) -> String {
    read_as(ns, 42, path)
}

fn read_as(ns: &mut Namespace, uid: u32, path: &Path) -> String {
    let entry = ns.lookup(uid, path).unwrap();
    let mut file = entry.location().unwrap().open(libc::O_RDONLY, 0).unwrap();
    let mut data = String::new();
    file.read_to_string(&mut data).unwrap();
    data
}

#[test]
fn design_example_layers_and_git_tree_are_exact() {
    let fixture = Fixture::new();
    let mut ns = fixture.namespace();
    assert!(ns.lookup(42, Path::new("")).unwrap().is_dir());
    assert!(ns
        .lookup(42, Path::new(&fixture.selector))
        .unwrap()
        .is_dir());
    let root = fixture.root();
    assert_eq!(read(&mut ns, &root.join("base.txt")), "base\n");
    assert_eq!(read(&mut ns, &root.join("package/repo.txt")), "git\n");
    assert_eq!(
        read(&mut ns, &root.join("package/ignored.txt")),
        "present\n"
    );
    assert_eq!(
        read(&mut ns, &root.join("package/subst.txt")),
        "$Format:%H$\n"
    );
    assert_eq!(read(&mut ns, &root.join("package/src/user.txt")), "alice\n");
    assert_eq!(
        read(&mut ns, &root.join("package/data/existing")),
        "writable\n"
    );
    let names = ns.lookup(42, &root).unwrap().children().unwrap();
    assert!(names.contains(&"package".into()));
    assert!(names.contains(&"base.txt".into()));
    let names = ns
        .lookup(42, &root.join("package"))
        .unwrap()
        .children()
        .unwrap();
    assert!(names.contains(&"src".into()));
    assert!(names.contains(&"data".into()));
    assert_eq!(
        ns.lookup(42, &root.join("package/absolute-link"))
            .unwrap()
            .read_link()
            .unwrap(),
        "/base.txt"
    );
    assert_eq!(
        ns.lookup(42, &root.join("package/.git"))
            .unwrap_err()
            .raw_os_error(),
        Some(libc::ENOENT)
    );
}

#[test]
fn base64_path_selectors_map_writable_directories_and_reject_invalid_paths() {
    let fixture = Fixture::new();
    let repo = fixture.temp.path().join("repo with spaces");
    fs::rename(&fixture.repo, &repo).unwrap();
    let config = format!(
        "path {} onto / as read-only\n\
         pathsubst {{{{base64Path}}}} onto /app as read-write\n",
        fixture.base.display()
    );
    let config = Config::parse(&config).unwrap();
    assert_eq!(config.selectors(), 1);
    let mut ns = Namespace::new(config, None).unwrap();
    for selector in [
        URL_SAFE_NO_PAD.encode(repo.as_os_str().as_encoded_bytes()),
        URL_SAFE.encode(repo.as_os_str().as_encoded_bytes()),
    ] {
        let root = Path::new(&selector);
        assert_eq!(read(&mut ns, &root.join("app/repo.txt")), "git\n");
        assert!(ns.lookup(42, &root.join("app/.git")).unwrap().is_dir());
        let location = ns
            .destination(42, &root.join("app"), OsStr::new("created"))
            .unwrap();
        location
            .open(libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC, 0o600)
            .unwrap();
        assert!(repo.join("created").is_file());
    }
    let link = fixture.temp.path().join("link");
    symlink(&repo, &link).unwrap();
    let selector = URL_SAFE_NO_PAD.encode(link.as_os_str().as_encoded_bytes());
    assert!(ns.lookup(42, Path::new(&selector)).is_err());
    for invalid in [
        "relative/path".to_owned(),
        format!("{}/../repo with spaces", fixture.temp.path().display()),
        format!("{}/./repo with spaces", fixture.temp.path().display()),
        format!("{}\n", repo.display()),
        "/tmp/\0".to_owned(),
    ] {
        let selector = URL_SAFE_NO_PAD.encode(invalid.as_bytes());
        assert_eq!(
            ns.lookup(42, Path::new(&selector))
                .unwrap_err()
                .raw_os_error(),
            Some(libc::EINVAL)
        );
    }
    for selector in ["not-base64!", &URL_SAFE_NO_PAD.encode([0xff])] {
        assert_eq!(
            ns.lookup(42, Path::new(selector))
                .unwrap_err()
                .raw_os_error(),
            Some(libc::EINVAL)
        );
    }
}

#[test]
fn incomplete_selector_skips_unrelated_non_consuming_sources() {
    let fixture = Fixture::new();
    let config = format!(
        "path {} onto / as read-only\n\
         git {{{{path}}}} onto /package as read-only\n\
         pathsubst {}/{{{{uid}}}} onto /data as read-write\n\
         pathsubst {}/{{{{path}}}} onto /tmp as read-write\n",
        fixture.base.display(),
        fixture.temp.path().join("missing-data").display(),
        fixture.users.display()
    );
    let mut ns = Namespace::new(Config::parse(&config).unwrap(), None).unwrap();
    let target = ns
        .lookup(42, Path::new(&fixture.symbolic()))
        .unwrap()
        .read_link()
        .unwrap();
    assert_eq!(target, OsStr::new(&fixture.selector));
    assert!(ns
        .lookup(42, Path::new(&fixture.selector))
        .unwrap()
        .is_dir());
    assert_eq!(
        ns.lookup(42, &Path::new(&fixture.selector).join("alice"))
            .unwrap_err()
            .raw_os_error(),
        Some(libc::ENOENT)
    );
}

#[test]
fn symbolic_refs_are_live_sibling_aliases_and_commits_are_immutable() {
    let fixture = Fixture::new();
    let mut ns = fixture.namespace();
    let symbolic = fixture.symbolic();
    let target = ns
        .lookup(42, Path::new(&symbolic))
        .unwrap()
        .read_link()
        .unwrap();
    assert_eq!(target, OsStr::new(&fixture.selector));
    assert_eq!(
        read(&mut ns, &fixture.root().join("package/repo.txt")),
        "git\n"
    );
    fs::write(fixture.repo.join("repo.txt"), "new\n").unwrap();
    git(&fixture.repo, &["add", "."]);
    git(&fixture.repo, &["commit", "-m", "advance"]);
    let new_commit = git(&fixture.repo, &["rev-parse", "HEAD"]);
    let new_selector = URL_SAFE_NO_PAD.encode(format!("{}:{new_commit}", fixture.repo.display()));
    assert_eq!(
        ns.lookup(42, Path::new(&symbolic))
            .unwrap()
            .read_link()
            .unwrap(),
        OsStr::new(&new_selector)
    );
    assert_eq!(
        read(&mut ns, &fixture.root().join("package/repo.txt")),
        "git\n"
    );
    assert_eq!(
        read(
            &mut ns,
            &Path::new(&new_selector).join("alice/package/repo.txt")
        ),
        "new\n"
    );
    let with_newline = URL_SAFE_NO_PAD.encode(format!("{}:main\n", fixture.repo.display()));
    let target = ns
        .lookup(42, Path::new(&with_newline))
        .unwrap()
        .read_link()
        .unwrap();
    assert_eq!(
        target,
        OsStr::new(&URL_SAFE_NO_PAD.encode(format!("{}:{new_commit}\n", fixture.repo.display())))
    );
    let padded = URL_SAFE.encode(format!("{}:{}", fixture.repo.display(), fixture.commit));
    let entry = ns.lookup(42, Path::new(&padded)).unwrap();
    if padded != fixture.selector {
        assert!(matches!(entry.node, Node::Alias(_)));
    }
    for invalid in [
        "not-base64",
        "Li4",
        &URL_SAFE_NO_PAD.encode("repo:HEAD~1"),
        &URL_SAFE_NO_PAD.encode("repo:--upload-pack=bad"),
    ] {
        assert!(ns.lookup(42, Path::new(invalid)).is_err());
    }
}

#[test]
fn writes_are_confined_to_writable_layers_and_mounts_are_protected() {
    let fixture = Fixture::new();
    let mut ns = fixture.namespace();
    let root = fixture.root();
    for name in ["base.txt", "package/repo.txt", "package/src/user.txt"] {
        let entry = ns.lookup(42, &root.join(name)).unwrap();
        assert_eq!(
            entry
                .location()
                .unwrap()
                .open(libc::O_WRONLY | libc::O_TRUNC, 0)
                .unwrap_err()
                .raw_os_error(),
            Some(libc::EROFS)
        );
    }
    assert_eq!(
        ns.destination(42, &root, OsStr::new("new"))
            .unwrap_err()
            .raw_os_error(),
        Some(libc::EROFS)
    );
    let data = root.join("package/data");
    let location = ns.destination(42, &data, OsStr::new("new")).unwrap();
    let file = location
        .open(libc::O_CREAT | libc::O_RDWR | libc::O_EXCL, 0o640)
        .unwrap();
    use std::os::unix::fs::FileExt;
    file.write_all_at(b"hello", 0).unwrap();
    assert_eq!(fs::read(fixture.data.join("42/new")).unwrap(), b"hello");
    assert_eq!(
        ns.lookup(42, &data)
            .unwrap()
            .mutation()
            .unwrap_err()
            .raw_os_error(),
        Some(libc::EBUSY)
    );
    assert!(ns.destination(42, &data, OsStr::new("../escape")).is_err());
    assert!(ns.lookup(42, Path::new("../escape")).is_err());
    assert_eq!(
        fs::read_to_string(fixture.base.join("base.txt")).unwrap(),
        "base\n"
    );
}

#[test]
fn host_symlinks_and_special_files_cannot_be_followed_by_daemon() {
    let fixture = Fixture::new();
    let outside = fixture.temp.path().join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("secret"), "secret").unwrap();
    symlink(&outside, fixture.users.join("escape")).unwrap();
    let mut ns = fixture.namespace();
    assert!(ns
        .lookup(42, &Path::new(&fixture.selector).join("escape"))
        .is_err());
    symlink(&outside, fixture.data.join("42/link")).unwrap();
    let root = fixture.root();
    let path = root.join("package/data/link/secret");
    assert!(ns.lookup(42, &path).is_err());
    let path = root.join("package/data/link");
    let entry = ns.lookup(42, &path).unwrap();
    assert_eq!(entry.read_link().unwrap(), outside.as_os_str());
    assert!(entry
        .location()
        .unwrap()
        .open(libc::O_WRONLY | libc::O_TRUNC, 0)
        .is_err());
    let fifo = fixture.data.join("42/fifo");
    let name = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    let entry = ns.lookup(42, &root.join("package/data/fifo")).unwrap();
    assert!(entry.location().unwrap().open(libc::O_RDONLY, 0).is_err());
    assert_eq!(
        fs::read_to_string(outside.join("secret")).unwrap(),
        "secret"
    );
}

#[test]
fn later_mounts_replace_subtrees_and_synthesize_ancestors() {
    let fixture = Fixture::new();
    let config = format!(
        "path {} onto / as read-only\n\
         path {} onto /new/deep as read-only\n\
         path {} onto /replace/deep as read-only\n\
         path {} onto /replace as read-write\n",
        fixture.base.display(),
        fixture.users.join("alice").display(),
        fixture.users.join("alice").display(),
        fixture.data.join("42").display()
    );
    let mut ns = Namespace::new(Config::parse(&config).unwrap(), None).unwrap();
    assert!(ns.lookup(42, Path::new("new")).unwrap().is_dir());
    assert_eq!(
        ns.lookup(42, Path::new("new")).unwrap().children().unwrap(),
        vec![OsStr::new("deep")]
    );
    assert_eq!(read(&mut ns, Path::new("new/deep/user.txt")), "alice\n");
    assert!(ns.lookup(42, Path::new("replace/deep")).is_err());
    assert_eq!(read(&mut ns, Path::new("replace/existing")), "writable\n");
    let root = Root::new(&fixture.base).unwrap();
    for path in ["../repo", "/etc/passwd"] {
        assert!(root.open(Path::new(path), libc::O_RDONLY, 0).is_err());
    }
}

#[test]
fn nested_mounts_override_non_directory_ancestors() {
    let fixture = Fixture::new();
    symlink("/nonexistent", fixture.base.join("obstacle")).unwrap();
    let config = format!(
        "path {} onto / as read-only\npath {} onto /obstacle/middle/deep as read-only\n",
        fixture.base.display(),
        fixture.users.join("alice").display()
    );
    let mut ns = Namespace::new(Config::parse(&config).unwrap(), None).unwrap();
    for path in ["obstacle", "obstacle/middle"] {
        assert!(ns.lookup(42, Path::new(path)).unwrap().is_dir());
    }
    assert_eq!(
        read(&mut ns, Path::new("obstacle/middle/deep/user.txt")),
        "alice\n"
    );
}

#[test]
fn uid_mappings_and_cached_views_follow_each_request() {
    let fixture = Fixture::new();
    fs::create_dir(fixture.data.join("43")).unwrap();
    fs::write(fixture.data.join("43/existing"), "other-user\n").unwrap();
    let mut ns = fixture.namespace();
    let data = fixture.root().join("package/data");
    for (uid, expected) in [(42, "writable\n"), (43, "other-user\n"), (42, "writable\n")] {
        assert_eq!(read_as(&mut ns, uid, &data.join("existing")), expected);
        let destination = ns.destination(uid, &data, OsStr::new("new")).unwrap();
        let file = destination
            .open(libc::O_CREAT | libc::O_WRONLY | libc::O_TRUNC, 0o600)
            .unwrap();
        use std::os::unix::fs::FileExt;
        file.write_all_at(uid.to_string().as_bytes(), 0).unwrap();
    }
    assert_eq!(fs::read(fixture.data.join("42/new")).unwrap(), b"42");
    assert_eq!(fs::read(fixture.data.join("43/new")).unwrap(), b"43");
    assert_eq!(
        ns.lookup(44, &data).unwrap_err().raw_os_error(),
        Some(libc::ENOENT)
    );
    symlink(fixture.data.join("42"), fixture.data.join("44")).unwrap();
    assert!(ns.lookup(44, &data).is_err());
    assert_eq!(read_as(&mut ns, 43, &data.join("existing")), "other-user\n");
}

#[test]
fn layers_without_path_substitution_do_not_consume_selectors() {
    let fixture = Fixture::new();
    let config = format!(
        "pathsubst {} onto / as read-only\n\
             git {} onto /package as read-only\n\
             pathsubst {}/{{{{uid}}}} onto /data as read-write\n",
        fixture.base.display(),
        fixture.symbolic(),
        fixture.data.display()
    );
    let config = Config::parse(&config).unwrap();
    assert_eq!(config.selectors(), 0);
    let mut ns = Namespace::new(config, None).unwrap();
    assert_eq!(read(&mut ns, Path::new("package/repo.txt")), "git\n");
    assert_eq!(read(&mut ns, Path::new("data/existing")), "writable\n");
    assert!(ns.lookup(99, Path::new("data/existing")).is_err());
}

#[test]
fn device_layers_validate_numbers_and_replace_only_their_targets() {
    use grayskillfs::config::{DeviceKind, Kind};
    let fixture = Fixture::new();
    let config = format!(
        "path {} onto / as read-only\n\
             dev char 1 3 onto /dev/null as read-write\n\
             dev block 4095 1048575 onto /dev/disk as read-only\n",
        fixture.base.display()
    );
    let config = Config::parse(&config).unwrap();
    assert_eq!(config.selectors(), 0);
    let Kind::Dev(device) = config.layers[2].kind else {
        panic!("expected device")
    };
    assert_eq!(libc::major(device.rdev().into()), 4095);
    assert_eq!(libc::minor(device.rdev().into()), 1048575);
    let mut ns = Namespace::new(config, None).unwrap();
    let entry = ns.lookup(42, Path::new("dev/null")).unwrap();
    assert!(
        matches!(entry.node, Node::Device(device, true) if device.kind == DeviceKind::Char && device.major == 1 && device.minor == 3)
    );
    assert!(entry.mutation().is_err());
    assert_eq!(
        ns.lookup(42, Path::new("dev/null/child"))
            .unwrap_err()
            .raw_os_error(),
        Some(libc::ENOTDIR)
    );
    assert_eq!(
        ns.lookup(42, Path::new("dev")).unwrap().children().unwrap(),
        vec![OsStr::new("disk"), OsStr::new("null")]
    );
    assert_eq!(read(&mut ns, Path::new("base.txt")), "base\n");
    for line in [
        "dev pipe 1 3 onto /dev/null as read-write",
        "dev char -1 3 onto /dev/null as read-write",
        "dev char 1 4294967296 onto /dev/null as read-write",
        "dev block 4096 0 onto /dev/disk as read-only",
        "dev char 1 1048576 onto /dev/null as read-write",
        "dev char {{path}} 3 onto /dev/null as read-write",
        "dev char 1 onto /dev/null as read-write",
        "dev char 1 3 onto / as read-write",
    ] {
        let text = format!("path /base onto / as read-only\n{line}\n");
        assert!(Config::parse(&text).is_err(), "accepted {line}");
    }
    assert!(Config::parse("dev char 1 3 onto / as read-only").is_err());
}
