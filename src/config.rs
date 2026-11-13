use std::io;
use std::path::{Component, Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    Path,
    PathSubst,
    Git,
    Dev(Device),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceKind {
    Char,
    Block,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Device {
    pub kind: DeviceKind,
    pub major: u32,
    pub minor: u32,
}

impl Device {
    pub fn rdev(self) -> u32 {
        libc::makedev(self.major, self.minor) as u32
    }
}

#[derive(Clone, Debug)]
pub enum OwnerId {
    Uid,
    Gid,
    Fixed(u32),
}

impl OwnerId {
    fn parse(value: &str) -> io::Result<Self> {
        match value {
            "{{uid}}" => Ok(Self::Uid),
            "{{gid}}" => Ok(Self::Gid),
            _ if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) => {
                match value.parse::<u32>() {
                    Ok(id) if id != u32::MAX => Ok(Self::Fixed(id)),
                    _ => Err(invalid("invalid ownership ID")),
                }
            }
            _ => Err(invalid(
                "ownership requires a numeric ID, {{uid}}, or {{gid}}",
            )),
        }
    }

    pub fn resolve(&self, uid: u32, gid: u32) -> u32 {
        match self {
            Self::Uid => uid,
            Self::Gid => gid,
            Self::Fixed(id) => *id,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Provision {
    pub mode: u32,
    pub uid: OwnerId,
    pub gid: OwnerId,
}

impl Provision {
    fn parse(options: &[&str]) -> io::Result<Option<Self>> {
        let mut mkdir = false;
        let mut mode = None;
        let mut uid = None;
        let mut gid = None;
        for option in options {
            if *option == "mkdir" && !mkdir {
                mkdir = true;
            } else if let Some(value) = option.strip_prefix("mode=") {
                if mode.is_some()
                    || value.is_empty()
                    || !value.bytes().all(|byte| (b'0'..=b'7').contains(&byte))
                {
                    return Err(invalid("expected one octal mode between 0000 and 0777"));
                }
                let value =
                    u32::from_str_radix(value, 8).map_err(|_| invalid("invalid directory mode"))?;
                if value > 0o777 {
                    return Err(invalid("directory mode must be between 0000 and 0777"));
                }
                mode = Some(value);
            } else if let Some(value) = option.strip_prefix("uid=") {
                if uid.is_some() {
                    return Err(invalid("duplicate uid option"));
                }
                uid = Some(OwnerId::parse(value)?);
            } else if let Some(value) = option.strip_prefix("gid=") {
                if gid.is_some() {
                    return Err(invalid("duplicate gid option"));
                }
                gid = Some(OwnerId::parse(value)?);
            } else {
                return Err(invalid(format!(
                    "unknown or duplicate layer option: {option}"
                )));
            }
        }
        if !mkdir {
            if !options.is_empty() {
                return Err(invalid("mode, uid, and gid options require mkdir"));
            }
            return Ok(None);
        }
        Ok(Some(Self {
            mode: mode.unwrap_or(0o700),
            uid: uid.unwrap_or(OwnerId::Uid),
            gid: gid.unwrap_or(OwnerId::Gid),
        }))
    }
}

#[derive(Clone, Debug)]
pub struct Layer {
    pub kind: Kind,
    pub source: String,
    pub target: PathBuf,
    pub writable: bool,
    pub consumes: bool,
    pub provision: Option<Provision>,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub layers: Vec<Layer>,
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

impl Config {
    pub fn parse(text: &str) -> io::Result<Self> {
        let mut layers = Vec::new();
        for (index, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let parse = || -> io::Result<Layer> {
                let words = line.split_whitespace().collect::<Vec<_>>();
                let (kind, source, target, access) = if words.first() == Some(&"dev") {
                    if words.len() != 8 || words[4] != "onto" || words[6] != "as" {
                        return Err(invalid(
                            "expected dev char|block MAJOR MINOR onto /TARGET as read-only|read-write",
                        ));
                    }
                    let kind = match words[1] {
                        "char" => DeviceKind::Char,
                        "block" => DeviceKind::Block,
                        _ => return Err(invalid("device type must be char or block")),
                    };
                    let major = words[2]
                        .parse::<u32>()
                        .map_err(|_| invalid("invalid device major"))?;
                    let minor = words[3]
                        .parse::<u32>()
                        .map_err(|_| invalid("invalid device minor"))?;
                    if major > 0xfff || minor > 0xfffff {
                        return Err(invalid(
                            "device numbers exceed Linux's 12-bit major/20-bit minor limits",
                        ));
                    }
                    (
                        Kind::Dev(Device { kind, major, minor }),
                        "",
                        words[5],
                        words[7],
                    )
                } else {
                    if words.len() < 6 || words[2] != "onto" || words[4] != "as" {
                        return Err(invalid(
                            "expected KIND SOURCE onto /TARGET as read-only|read-write",
                        ));
                    }
                    let kind = match words[0] {
                        "path" => Kind::Path,
                        "pathsubst" => Kind::PathSubst,
                        "git" => Kind::Git,
                        _ => return Err(invalid("unknown layer kind")),
                    };
                    (kind, words[1], words[3], words[5])
                };
                let writable = match access {
                    "read-only" => false,
                    "read-write" => true,
                    _ => return Err(invalid("unknown access mode")),
                };
                let provision = if matches!(kind, Kind::Path | Kind::PathSubst) {
                    Provision::parse(&words[6..])?
                } else {
                    if kind == Kind::Git && words.len() != 6 {
                        return Err(invalid(
                            "directory provisioning is only supported for path and pathsubst",
                        ));
                    }
                    None
                };
                if kind == Kind::Git
                    && (writable || (source.contains("{{") && source != "{{path}}"))
                {
                    return Err(invalid(
                        "git requires read-only and either {{path}} or a fixed encoded selector",
                    ));
                }
                if kind == Kind::Path && source.contains("{{") {
                    return Err(invalid("use pathsubst for substitutions"));
                }
                if source.contains("{{base64Path}}")
                    && (kind != Kind::PathSubst || source != "{{base64Path}}")
                {
                    return Err(invalid(
                        "{{base64Path}} must be the entire source of a pathsubst layer",
                    ));
                }
                let remaining = source
                    .replace("{{path}}", "")
                    .replace("{{uid}}", "")
                    .replace("{{base64Path}}", "");
                if remaining.contains(['{', '}']) {
                    return Err(invalid("unknown substitution"));
                }
                let source = source.to_owned();
                if matches!(kind, Kind::Path | Kind::PathSubst)
                    && source != "{{base64Path}}"
                    && !Path::new(&source).is_absolute()
                {
                    return Err(invalid("source must be absolute"));
                }
                let raw_target = target;
                let target = PathBuf::from(target);
                if !target.is_absolute()
                    || target
                        .components()
                        .any(|c| matches!(c, Component::ParentDir))
                    || raw_target.split('/').any(|c| c == ".")
                {
                    return Err(invalid("target must be absolute and contain no . or .."));
                }
                if matches!(kind, Kind::Dev(_)) && target == Path::new("/") {
                    return Err(invalid("a device cannot replace the filesystem root"));
                }
                let consumes = source.contains("{{path}}") || source == "{{base64Path}}";
                Ok(Layer {
                    kind,
                    source,
                    target,
                    writable,
                    consumes,
                    provision,
                })
            };
            layers.push(parse().map_err(|e| invalid(format!("line {}: {e}", index + 1)))?);
        }
        if layers.is_empty()
            || layers[0].target != Path::new("/")
            || matches!(layers[0].kind, Kind::Dev(_))
        {
            return Err(invalid("the first layer must mount onto /"));
        }
        Ok(Self { layers })
    }

    pub fn selectors(&self) -> usize {
        self.layers.iter().filter(|l| l.consumes).count()
    }

    pub fn has_devices(&self) -> bool {
        self.layers
            .iter()
            .any(|layer| matches!(layer.kind, Kind::Dev(_)))
    }
}
