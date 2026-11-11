use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use fuser::MountOption;
use grayskillfs::{config::Config, fuse::LayerFs, namespace::Namespace};
use std::io;
use std::path::PathBuf;

const USAGE: &str = "\
Usage:
  grayskillfs mount CONFIG MOUNTPOINT [--cache-dir DIRECTORY] [--allow-other] [--allow-dev]
  grayskillfs check CONFIG
  grayskillfs encode REPOSITORY REF

Mount runs in the foreground. Unmount with: fusermount3 -u MOUNTPOINT
{{uid}} is the UID of the process making each filesystem request.
--allow-other requires user_allow_other in /etc/fuse.conf for unprivileged mounts.
--allow-dev enables native device I/O; normally requires a privileged mount.";

fn run() -> io::Result<()> {
    let mut args = std::env::args_os().skip(1);
    let action = args.next().unwrap_or_default();
    if action == "--help" || action == "-h" {
        println!("{USAGE}");
        return Ok(());
    }
    if action == "encode" {
        let repository = args.next().ok_or_else(|| invalid(USAGE))?;
        let reference = args.next().ok_or_else(|| invalid(USAGE))?;
        if args.next().is_some() {
            return Err(invalid(USAGE));
        }
        let repository = repository
            .to_str()
            .ok_or_else(|| invalid("repository must be UTF-8"))?;
        let reference = reference
            .to_str()
            .ok_or_else(|| invalid("ref must be UTF-8"))?;
        println!(
            "{}",
            URL_SAFE_NO_PAD.encode(format!("{repository}:{reference}"))
        );
        return Ok(());
    }
    if action != "check" && action != "mount" {
        return Err(invalid(USAGE));
    }
    let config = args.next().ok_or_else(|| invalid(USAGE))?;
    let config = Config::parse(&std::fs::read_to_string(config)?)?;
    if action == "check" {
        if args.next().is_some() {
            return Err(invalid(USAGE));
        }
        println!(
            "valid configuration: {} layers, {} selectors",
            config.layers.len(),
            config.selectors()
        );
        return Ok(());
    }
    let mountpoint = PathBuf::from(args.next().ok_or_else(|| invalid(USAGE))?);
    let mut cache = None;
    let mut allow_other = false;
    let mut allow_dev = false;
    while let Some(option) = args.next() {
        if option == "--cache-dir" {
            cache =
                Some(PathBuf::from(args.next().ok_or_else(|| {
                    invalid("--cache-dir requires a directory")
                })?));
        } else if option == "--allow-other" {
            allow_other = true;
        } else if option == "--allow-dev" {
            allow_dev = true;
        } else {
            return Err(invalid(format!(
                "unknown option: {}",
                option.to_string_lossy()
            )));
        }
    }
    if config.has_devices() && !allow_dev {
        eprintln!("grayskillfs: configured device nodes are visible but I/O is disabled by nodev; use --allow-dev for native device I/O");
    }
    let namespace = Namespace::new(config, cache.as_deref())?;
    let mut options = vec![
        MountOption::FSName("grayskillfs".into()),
        MountOption::DefaultPermissions,
        MountOption::NoSuid,
        if allow_dev {
            MountOption::Dev
        } else {
            MountOption::NoDev
        },
        MountOption::RW,
    ];
    if allow_other {
        options.push(MountOption::AllowOther);
    }
    fuser::mount2(LayerFs::new(namespace), mountpoint, &options)
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("grayskillfs: {error}");
        std::process::exit(1);
    }
}
