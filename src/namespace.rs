use crate::backend::{component, errno, Root};
use crate::config::{Config, Device, Kind};
use crate::git::GitStore;
use std::collections::{BTreeSet, HashMap};
use std::ffi::OsString;
use std::fs::{File, Metadata};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

#[derive(Clone, Debug)]
pub struct Location {
    pub root: Arc<Root>,
    pub relative: PathBuf,
    pub writable: bool,
    pub layer: usize,
}

impl Location {
    pub fn writable(&self) -> io::Result<()> {
        if self.writable {
            Ok(())
        } else {
            Err(errno(libc::EROFS))
        }
    }

    pub fn open(&self, flags: i32, mode: u32) -> io::Result<File> {
        if flags & libc::O_ACCMODE != libc::O_RDONLY
            || flags & (libc::O_CREAT | libc::O_TRUNC | libc::O_APPEND) != 0
        {
            self.writable()?;
        }
        if flags & libc::O_CREAT != 0 {
            match self
                .root
                .open(&self.relative, flags | libc::O_EXCL, mode & 0o777)
            {
                Ok(file) => return Ok(file),
                Err(error)
                    if error.raw_os_error() == Some(libc::EEXIST) && flags & libc::O_EXCL == 0 => {}
                Err(error) => return Err(error),
            }
        }
        let pinned = self.root.open(&self.relative, libc::O_PATH, 0)?;
        let metadata = pinned.metadata()?;
        if !metadata.is_file() {
            return Err(errno(if metadata.is_dir() {
                libc::EISDIR
            } else {
                libc::EACCES
            }));
        }
        // Reopen the pinned regular inode, not a path that could become a device.
        let path = std::ffi::CString::new(format!("/proc/self/fd/{}", pinned.as_raw_fd())).unwrap();
        let flags = (flags & !(libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW)) | libc::O_CLOEXEC;
        let fd = unsafe { libc::open(path.as_ptr(), flags) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

#[derive(Clone, Debug)]
struct Mount {
    target: PathBuf,
    source: Source,
    writable: bool,
    layer: usize,
}

#[derive(Clone, Debug)]
enum Source {
    Path(Arc<Root>),
    Dev(Device),
}

#[derive(Debug)]
pub struct View {
    mounts: Vec<Mount>,
}

impl View {
    fn mount(&self, path: &Path) -> io::Result<&Mount> {
        self.mounts
            .iter()
            .rev()
            .find(|mount| path.starts_with(&mount.target))
            .ok_or_else(|| errno(libc::ENOENT))
    }

    fn location(&self, path: &Path) -> io::Result<Location> {
        let mount = self.mount(path)?;
        let Source::Path(root) = &mount.source else {
            return Err(errno(libc::EROFS));
        };
        Ok(Location {
            root: root.clone(),
            relative: path.strip_prefix(&mount.target).unwrap().to_owned(),
            writable: mount.writable,
            layer: mount.layer,
        })
    }

    pub fn protected(&self, path: &Path) -> bool {
        self.mounts
            .iter()
            .any(|mount| mount.target.starts_with(path))
    }

    fn entry(self: &Arc<Self>, path: &Path) -> io::Result<Entry> {
        let mount = self.mount(path)?;
        let ancestor = self
            .mounts
            .iter()
            .any(|mount| mount.target != path && mount.target.starts_with(path));
        let node = if let Source::Dev(device) = mount.source {
            if ancestor {
                Node::Directory
            } else if path == mount.target {
                Node::Device(device, mount.writable)
            } else {
                return Err(errno(libc::ENOTDIR));
            }
        } else {
            let location = self.location(path)?;
            match location.root.metadata(&location.relative) {
                Ok(metadata) if !ancestor || metadata.is_dir() => {
                    Node::Physical(location, metadata)
                }
                Ok(_) if ancestor => Node::Directory,
                Err(error)
                    if ancestor
                        && matches!(
                            error.raw_os_error(),
                            Some(libc::ENOENT | libc::ENOTDIR | libc::ELOOP)
                        ) =>
                {
                    Node::Directory
                }
                Err(error) => return Err(error),
                Ok(_) => unreachable!(),
            }
        };
        Ok(Entry {
            node,
            view: Some(self.clone()),
            relative: path.to_owned(),
        })
    }
}

#[derive(Debug)]
pub enum Node {
    Directory,
    Alias(OsString),
    Physical(Location, Metadata),
    Device(Device, bool),
}

#[derive(Debug)]
pub struct Entry {
    pub node: Node,
    pub view: Option<Arc<View>>,
    pub relative: PathBuf,
}

impl Entry {
    pub fn location(&self) -> io::Result<&Location> {
        match &self.node {
            Node::Physical(location, _) => Ok(location),
            _ => Err(errno(libc::EROFS)),
        }
    }

    pub fn is_dir(&self) -> bool {
        match &self.node {
            Node::Directory => true,
            Node::Physical(_, metadata) => metadata.is_dir(),
            Node::Alias(_) | Node::Device(..) => false,
        }
    }

    pub fn children(&self) -> io::Result<Vec<OsString>> {
        if !self.is_dir() {
            return Err(errno(libc::ENOTDIR));
        }
        let mut names = BTreeSet::new();
        if let Node::Physical(location, _) = &self.node {
            names.extend(location.root.entries(&location.relative)?);
        }
        if let Some(view) = &self.view {
            for mount in &view.mounts {
                if let Ok(suffix) = mount.target.strip_prefix(&self.relative) {
                    if let Some(Component::Normal(name)) = suffix.components().next() {
                        names.insert(name.to_owned());
                    }
                }
            }
        }
        Ok(names.into_iter().collect())
    }

    pub fn read_link(&self) -> io::Result<OsString> {
        match &self.node {
            Node::Alias(target) => Ok(target.clone()),
            Node::Physical(location, _) => location.root.read_link(&location.relative),
            _ => Err(errno(libc::EINVAL)),
        }
    }

    pub fn mutation(&self) -> io::Result<&Location> {
        let location = self.location()?;
        location.writable()?;
        if self
            .view
            .as_ref()
            .is_some_and(|view| view.protected(&self.relative))
        {
            return Err(errno(libc::EBUSY));
        }
        Ok(location)
    }
}

pub struct Namespace {
    config: Config,
    git: GitStore,
    views: HashMap<(u32, Vec<OsString>), Arc<View>>,
    roots: HashMap<PathBuf, Arc<Root>>,
}

impl Namespace {
    pub fn new(config: Config, cache: Option<&Path>) -> io::Result<Self> {
        let mut namespace = Self {
            config,
            git: GitStore::new(cache)?,
            views: HashMap::new(),
            roots: HashMap::new(),
        };
        let probe = Root::new(Path::new("/"))?;
        probe.probe_openat2()?;
        // Validate static mappings now, not on the first client access.
        for layer in namespace.config.layers.clone() {
            if matches!(layer.kind, Kind::Path | Kind::PathSubst) && !layer.source.contains("{{") {
                namespace.root(Path::new(&layer.source))?;
            }
        }
        Ok(namespace)
    }

    fn root(&mut self, path: &Path) -> io::Result<Arc<Root>> {
        if let Some(root) = self.roots.get(path) {
            return Ok(root.clone());
        }
        let root = Root::new(path)?;
        self.roots.insert(path.to_owned(), root.clone());
        Ok(root)
    }

    fn substituted_root(&mut self, template: &str, source: &Path) -> io::Result<Arc<Root>> {
        if let Some(root) = self.roots.get(source) {
            return Ok(root.clone());
        }
        let mut prefix = PathBuf::new();
        for part in Path::new(template).components() {
            if part.as_os_str().to_string_lossy().contains("{{") {
                break;
            }
            prefix.push(part);
        }
        let base = self.root(&prefix)?;
        let relative = source
            .strip_prefix(&prefix)
            .map_err(|_| errno(libc::EINVAL))?;
        let root = base.subdir(relative)?;
        self.roots.insert(source.to_owned(), root.clone());
        Ok(root)
    }

    pub fn lookup(&mut self, uid: u32, path: &Path) -> io::Result<Entry> {
        let mut parts = Vec::new();
        for part in path.components() {
            let Component::Normal(name) = part else {
                return Err(errno(libc::EINVAL));
            };
            component(name)?;
            parts.push(name.to_owned());
        }
        let count = self.config.selectors();
        let selectors = parts.iter().take(count).cloned().collect::<Vec<_>>();
        let mut mounts = Vec::new();
        let mut consumed = 0;
        // Complete canonical views do not re-fetch immutable Git object IDs.
        if selectors.len() == count {
            if let Some(view) = self.views.get(&(uid, selectors.clone())) {
                let relative = parts[count..].iter().collect::<PathBuf>();
                return view.entry(&relative);
            }
        }
        for (index, layer) in self.config.layers.clone().into_iter().enumerate() {
            let selector = if layer.consumes {
                if consumed == selectors.len() {
                    return Ok(Entry {
                        node: Node::Directory,
                        view: None,
                        relative: PathBuf::new(),
                    });
                }
                let selector = selectors[consumed]
                    .to_str()
                    .ok_or_else(|| errno(libc::EINVAL))?;
                consumed += 1;
                Some(selector)
            } else {
                None
            };
            let source = layer.source.replace("{{uid}}", &uid.to_string());
            let source = if let Some(selector) = selector {
                source.replace("{{path}}", selector)
            } else {
                source
            };
            let source = if let Kind::Dev(device) = layer.kind {
                Source::Dev(device)
            } else {
                let source = if layer.kind == Kind::Git {
                    let selection = self.git.select(&source)?;
                    if layer.consumes && selection.canonical != source {
                        if parts.len() != consumed {
                            return Err(errno(libc::ENOTDIR));
                        }
                        return Ok(Entry {
                            node: Node::Alias(selection.canonical.into()),
                            view: None,
                            relative: PathBuf::new(),
                        });
                    }
                    selection.root
                } else {
                    PathBuf::from(source)
                };
                let root = if layer.kind == Kind::PathSubst && layer.source.contains("{{") {
                    self.substituted_root(&layer.source, &source)?
                } else {
                    self.root(&source)?
                };
                Source::Path(root)
            };
            let target = layer.target.strip_prefix("/").unwrap().to_owned();
            mounts.retain(|mount: &Mount| !mount.target.starts_with(&target));
            mounts.push(Mount {
                target,
                source,
                writable: layer.writable,
                layer: index,
            });
        }
        let view = Arc::new(View { mounts });
        let relative = parts[count..].iter().collect::<PathBuf>();
        let entry = view.entry(&relative)?;
        self.views.insert((uid, selectors), view);
        Ok(entry)
    }

    pub fn destination(
        &mut self,
        uid: u32,
        parent: &Path,
        name: &std::ffi::OsStr,
    ) -> io::Result<Location> {
        component(name)?;
        let parent = self.lookup(uid, parent)?;
        if !parent.is_dir() {
            return Err(errno(libc::ENOTDIR));
        }
        let view = parent.view.as_ref().ok_or_else(|| errno(libc::EROFS))?;
        let path = parent.relative.join(name);
        if view.protected(&path) {
            return Err(errno(libc::EBUSY));
        }
        let location = view.location(&path)?;
        location.writable()?;
        Ok(location)
    }
}
