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
pub struct Layer {
    pub kind: Kind,
    pub source: String,
    pub target: PathBuf,
    pub writable: bool,
    pub consumes: bool,
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
                    if words.len() != 6 || words[2] != "onto" || words[4] != "as" {
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
                let remaining = source.replace("{{path}}", "").replace("{{uid}}", "");
                if remaining.contains(['{', '}']) {
                    return Err(invalid("unknown substitution"));
                }
                let source = source.to_owned();
                if matches!(kind, Kind::Path | Kind::PathSubst) && !Path::new(&source).is_absolute()
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
                let consumes = source.contains("{{path}}");
                Ok(Layer {
                    kind,
                    source,
                    target,
                    writable,
                    consumes,
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
