#![allow(dead_code)]

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use grayskillfs::{config::Config, namespace::Namespace};
use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

pub struct Fixture {
    pub temp: TempDir,
    pub base: PathBuf,
    pub repo: PathBuf,
    pub users: PathBuf,
    pub data: PathBuf,
    pub config: String,
    pub selector: String,
    pub commit: String,
}

pub fn git(repo: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .args([
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
        ])
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

impl Fixture {
    pub fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().join("base");
        let repo = temp.path().join("repo");
        let users = temp.path().join("users");
        let data = temp.path().join("data");
        for path in [&base, &repo, &users.join("alice"), &data.join("42")] {
            fs::create_dir_all(path).unwrap();
        }
        fs::write(base.join("base.txt"), "base\n").unwrap();
        fs::create_dir(base.join("bin")).unwrap();
        fs::write(
            base.join("bin/hello"),
            "#!/bin/sh\nprintf 'mounted-exec\\n'\n",
        )
        .unwrap();
        fs::set_permissions(base.join("bin/hello"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(users.join("alice/user.txt"), "alice\n").unwrap();
        fs::write(data.join("42/existing"), "writable\n").unwrap();
        let caller_data = data.join(unsafe { libc::geteuid() }.to_string());
        fs::create_dir_all(&caller_data).unwrap();
        fs::write(caller_data.join("existing"), "writable\n").unwrap();
        git(&repo, &["init", "-b", "main"]);
        fs::write(repo.join("repo.txt"), "git\n").unwrap();
        fs::write(repo.join("ignored.txt"), "present\n").unwrap();
        fs::write(repo.join("subst.txt"), "$Format:%H$\n").unwrap();
        fs::write(
            repo.join(".gitattributes"),
            "ignored.txt export-ignore\nsubst.txt export-subst\n",
        )
        .unwrap();
        symlink("/base.txt", repo.join("absolute-link")).unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-m", "fixture"]);
        let commit = git(&repo, &["rev-parse", "HEAD"]);
        let selector = URL_SAFE_NO_PAD.encode(format!("{}:{commit}", repo.display()));
        let config = format!(
            "path {} onto / as read-only\n\
             git {{{{path}}}} onto /package as read-only\n\
             pathsubst {}/{{{{path}}}} onto /package/src as read-only\n\
             pathsubst {}/{{{{uid}}}} onto /package/data as read-write\n",
            base.display(),
            users.display(),
            data.display()
        );
        Self {
            temp,
            base,
            repo,
            users,
            data,
            config,
            selector,
            commit,
        }
    }

    pub fn namespace(&self) -> Namespace {
        Namespace::new(Config::parse(&self.config).unwrap(), None).unwrap()
    }

    pub fn root(&self) -> PathBuf {
        PathBuf::from(&self.selector).join("alice")
    }

    pub fn symbolic(&self) -> String {
        URL_SAFE_NO_PAD.encode(format!("{}:main", self.repo.display()))
    }
}
