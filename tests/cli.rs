use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use std::fs;
use std::process::Command;

#[test]
fn cli_encoding_validation_and_errors() {
    let binary = env!("CARGO_BIN_EXE_grayskillfs");
    let result = Command::new(binary)
        .args(["encode", "https://example.invalid/repo.git", "main"])
        .output()
        .unwrap();
    assert!(result.status.success());
    let text = std::str::from_utf8(&result.stdout).unwrap().trim();
    assert_eq!(
        URL_SAFE_NO_PAD.decode(text).unwrap(),
        b"https://example.invalid/repo.git:main"
    );
    let temp = tempfile::tempdir().unwrap();
    let config = temp.path().join("config");
    fs::write(&config, "path /nonexistent onto / as read-only\n").unwrap();
    let result = Command::new(binary)
        .arg("check")
        .arg(&config)
        .output()
        .unwrap();
    assert!(result.status.success());
    assert!(String::from_utf8_lossy(&result.stdout).contains("1 layers, 0 selectors"));
    let result = Command::new(binary)
        .arg("mount")
        .arg(&config)
        .arg(temp.path())
        .args(["--uid", "42"])
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("unknown option: --uid"));
    for (arguments, expected) in [
        (vec!["--user"], "--user requires a user name or numeric UID"),
        (vec!["--user", "0"], "unprivileged account"),
        (
            vec!["--user", "grayskillfs-no-such-user"],
            "no system account",
        ),
        (vec!["--user", "4294967296"], "invalid numeric user ID"),
    ] {
        let result = Command::new(binary)
            .arg("mount")
            .arg(&config)
            .arg(temp.path())
            .args(arguments)
            .output()
            .unwrap();
        assert!(!result.status.success());
        assert!(
            String::from_utf8_lossy(&result.stderr).contains(expected),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
    if unsafe { libc::geteuid() } != 0 {
        let result = Command::new(binary)
            .arg("mount")
            .arg(&config)
            .arg(temp.path())
            .arg("--user")
            .arg(unsafe { libc::geteuid() }.to_string())
            .output()
            .unwrap();
        assert!(!result.status.success());
        assert!(String::from_utf8_lossy(&result.stderr)
            .contains("--user requires starting the daemon as root"));
    }
    fs::write(&config, "path /base onto / as read-only\nno-consume pathsubst /data/{{uid}} onto /data as read-write\n").unwrap();
    let result = Command::new(binary)
        .arg("check")
        .arg(&config)
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("line 2"));
    fs::write(&config, "path /root onto relative as read-only\n").unwrap();
    let result = Command::new(binary)
        .arg("check")
        .arg(&config)
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("line 1"));
    let result = Command::new(binary).args(["unknown"]).output().unwrap();
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("Usage:"));
}
