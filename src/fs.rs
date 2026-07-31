use std::collections::BTreeMap;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, UNIX_EPOCH};

use dashmap::DashMap;
use dashmap::mapref::entry::Entry;
use fuser::{
    Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation, INodeNo, InitFlags,
    KernelConfig, LockOwner, OpenFlags, ReplyAttr, ReplyData, ReplyDirectory, ReplyEmpty,
    ReplyEntry, ReplyOpen, ReplyXattr, Request,
};
use tokio::runtime::Handle;
use tokio::sync::{Mutex as AsyncMutex, Notify};
use xiv_cache::file::CacheFile;
use xiv_cache::server::Server;
use xiv_core::file::{slug::Slug, version::GameVersion};

use crate::registry::{Region, Registry, Side, VersionSel};
use crate::tree::{ClutIndex, Node, build_index, join_rel, split_last};

/// The registry is read once at startup and every version but `latest` is
/// immutable anyway, so nothing the kernel caches can go stale under it.
const TTL: Duration = Duration::from_secs(86400);
const ROOT_INO: u64 = 1;
/// Amount of a file to fetch per cache miss. FFXIV files are reconstructed from
/// many small patch-ref ranges scattered across dozens of `.patch` versions; a
/// 128 KiB kernel read would download a handful of high-latency range requests.
/// Fetching a large window at once lets the downloader issue many concurrent
/// (and range-merged) requests, amortizing latency and the CDN rate limit.
const READAHEAD: u64 = 8 << 20;
/// Concurrent CLUT fetch+parse ceiling. Each transiently holds its compressed
/// payload plus the index it builds, so this bounds what a file manager fanning a
/// browse across versions can cost.
const MAX_CLUT_BUILDS: usize = 8;
/// How long a CLUT parse burst has to go quiet before its arenas are returned.
const TRIM_DELAY: Duration = Duration::from_secs(5);

/// Signalled when a CLUT parse frees its scratch. glibc's heap is process-global,
/// so this is too.
static TRIM_WANTED: Notify = Notify::const_new();

/// Optional restriction of the exposed tree to one region and/or version.
#[derive(Default, Clone)]
pub struct Filter {
    pub region: Option<Region>,
    pub version: Option<VersionSel>,
    /// Also list `H`-prefixed historic patches (the intermediate/superseded ones).
    /// They're always reachable by explicit path; this only controls listing.
    pub history: bool,
    /// For a concrete version, backfill each expansion/boot repo to its newest
    /// version at or before it, reconstructing a coherent point-in-time install.
    pub backfill: bool,
}

/// An open file handle with a readahead window buffered in memory.
struct OpenFile {
    file: CacheFile,
    window: AsyncMutex<Window>,
}

struct Window {
    start: u64,
    data: Vec<u8>,
}

impl Window {
    fn end(&self) -> u64 {
        self.start + self.data.len() as u64
    }
}

impl OpenFile {
    /// Move the window to cover `offset`.
    async fn seek(&self, w: &mut Window, offset: u64) -> io::Result<()> {
        let mut data = vec![0u8; READAHEAD.min(self.file.len() - offset) as usize];
        self.file.pread(offset, &mut data).await?;
        w.start = offset;
        w.data = data;
        Ok(())
    }
}

struct Inner {
    server: Server,
    handle: Handle,
    registry: Registry,
    filter: Filter,

    inodes: DashMap<u64, Arc<Node>>,
    by_name: DashMap<(u64, String), u64>,
    parents: DashMap<u64, u64>,
    next_ino: AtomicU64,

    index_cache: moka::future::Cache<(Slug, GameVersion), Arc<ClutIndex>>,
    build_sem: tokio::sync::Semaphore,
    open_files: DashMap<u64, Arc<OpenFile>>,
    next_fh: AtomicU64,

    uid: u32,
    gid: u32,
}

impl Inner {
    fn region_allowed(&self, region: Region) -> bool {
        self.registry.region_exists(region) && self.filter.region.is_none_or(|r| r == region)
    }

    fn version_allowed(&self, sel: &VersionSel) -> bool {
        self.filter.version.as_ref().is_none_or(|v| v == sel)
    }

    fn alloc(&self, parent: u64, name: &str, node: Node) -> u64 {
        match self.by_name.entry((parent, name.to_string())) {
            Entry::Occupied(e) => *e.get(),
            Entry::Vacant(e) => {
                let ino = self.next_ino.fetch_add(1, Ordering::Relaxed);
                e.insert(ino);
                self.inodes.insert(ino, Arc::new(node));
                self.parents.insert(ino, parent);
                ino
            }
        }
    }

    async fn index_for(&self, slug: Slug, version: &GameVersion) -> Option<Arc<ClutIndex>> {
        // Single-flight: a burst of concurrent readdir/lookup on the same cold
        // (slug, version) coalesces into one fetch+parse instead of stampeding.
        self.index_cache
            .try_get_with((slug, version.clone()), async {
                let _permit = self.build_sem.acquire().await;
                let meta = self.server.get_clut_index(slug, version.clone()).await?;
                let index = Arc::new(build_index(&meta));
                drop(meta);
                TRIM_WANTED.notify_one();
                anyhow::Ok(index)
            })
            .await
            .inspect_err(|e| log::error!("CLUT index for {slug} {version}: {e:#}"))
            .ok()
    }

    fn attr(&self, ino: u64, node: &Node) -> FileAttr {
        let (kind, size, perm, nlink) = match node {
            Node::Repo {
                is_dir: false,
                size,
                ..
            } => (FileType::RegularFile, *size, 0o444, 1),
            _ => (FileType::Directory, 0, 0o555, 2),
        };
        FileAttr {
            ino: INodeNo(ino),
            size,
            blocks: size.div_ceil(512),
            atime: UNIX_EPOCH,
            mtime: UNIX_EPOCH,
            ctime: UNIX_EPOCH,
            crtime: UNIX_EPOCH,
            kind,
            perm,
            nlink,
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: 512,
            flags: 0,
        }
    }

    /// Immediate children of a directory node as (name, node) pairs.
    async fn children(&self, node: &Node) -> Vec<(String, Node)> {
        match node {
            Node::Root => Region::ALL
                .into_iter()
                .filter(|r| self.region_allowed(*r))
                .map(|r| (r.dir_name().to_string(), Node::Region(r)))
                .collect(),
            Node::Region(region) => {
                let mut out = Vec::new();
                if self.version_allowed(&VersionSel::Latest) {
                    out.push((
                        "latest".to_string(),
                        Node::Version(*region, VersionSel::Latest),
                    ));
                }
                for v in self.registry.game_versions(*region) {
                    // Hide `H`-prefixed historic patches unless opted in; an explicit
                    // --patch pin is itself opt-in to that one version. They stay
                    // reachable by path either way.
                    if is_historic(&v) && !self.filter.history && self.filter.version.is_none() {
                        continue;
                    }
                    let sel = VersionSel::Concrete(v);
                    if self.version_allowed(&sel) {
                        out.push((sel_name(&sel), Node::Version(*region, sel)));
                    }
                }
                out
            }
            Node::Version(region, sel) => {
                let mut out = Vec::new();
                for (name, side) in [("boot", Side::Boot), ("game", Side::Game)] {
                    if !self
                        .registry
                        .resolve(*region, side, sel, self.filter.backfill)
                        .is_empty()
                    {
                        out.push((
                            name.to_string(),
                            Node::Repo {
                                region: *region,
                                ver: sel.clone(),
                                side,
                                relpath: String::new(),
                                is_dir: true,
                                size: 0,
                            },
                        ));
                    }
                }
                out
            }
            Node::Repo {
                region,
                ver,
                side,
                relpath,
                is_dir: true,
                ..
            } => self
                .repo_children(*region, ver, *side, relpath)
                .await
                .into_iter()
                .map(|(name, is_dir, size)| {
                    (
                        name.clone(),
                        Node::Repo {
                            region: *region,
                            ver: ver.clone(),
                            side: *side,
                            relpath: join_rel(relpath, &name),
                            is_dir,
                            size,
                        },
                    )
                })
                .collect(),
            Node::Repo { is_dir: false, .. } => Vec::new(),
        }
    }

    /// Merge the directory listing at `relpath` across all backing slugs.
    async fn repo_children(
        &self,
        region: Region,
        ver: &VersionSel,
        side: Side,
        relpath: &str,
    ) -> Vec<(String, bool, u64)> {
        let mut merged: BTreeMap<String, (bool, u64)> = BTreeMap::new();
        for c in self
            .registry
            .resolve(region, side, ver, self.filter.backfill)
        {
            let Some(idx) = self.index_for(c.slug, &c.version).await else {
                continue;
            };
            let Some(entries) = idx.dirs.get(relpath) else {
                continue;
            };
            for e in entries {
                let slot = merged.entry(e.name.clone()).or_insert((e.is_dir, e.size));
                if e.is_dir {
                    slot.0 = true;
                } else {
                    slot.1 = e.size;
                }
            }
        }
        merged
            .into_iter()
            .map(|(name, (is_dir, size))| (name, is_dir, size))
            .collect()
    }

    /// Resolve a single child of a directory node.
    async fn resolve_child(&self, parent: &Node, name: &str) -> Option<Node> {
        match parent {
            Node::Root => Region::from_dir(name)
                .filter(|r| self.region_allowed(*r))
                .map(Node::Region),
            Node::Region(region) => {
                let sel = if name == "latest" {
                    VersionSel::Latest
                } else {
                    let v = GameVersion::new(name).ok()?;
                    self.registry.version_valid(*region, &v).then_some(())?;
                    VersionSel::Concrete(v)
                };
                self.version_allowed(&sel)
                    .then_some(Node::Version(*region, sel))
            }
            Node::Version(region, sel) => {
                let side = match name {
                    "boot" => Side::Boot,
                    "game" => Side::Game,
                    _ => return None,
                };
                (!self
                    .registry
                    .resolve(*region, side, sel, self.filter.backfill)
                    .is_empty())
                .then(|| Node::Repo {
                    region: *region,
                    ver: sel.clone(),
                    side,
                    relpath: String::new(),
                    is_dir: true,
                    size: 0,
                })
            }
            Node::Repo {
                region,
                ver,
                side,
                relpath,
                is_dir: true,
                ..
            } => {
                for c in self
                    .registry
                    .resolve(*region, *side, ver, self.filter.backfill)
                {
                    let Some(idx) = self.index_for(c.slug, &c.version).await else {
                        continue;
                    };
                    if let Some(e) = idx
                        .dirs
                        .get(relpath)
                        .and_then(|es| es.iter().find(|e| e.name == name))
                    {
                        return Some(Node::Repo {
                            region: *region,
                            ver: ver.clone(),
                            side: *side,
                            relpath: join_rel(relpath, name),
                            is_dir: e.is_dir,
                            size: e.size,
                        });
                    }
                }
                None
            }
            Node::Repo { is_dir: false, .. } => None,
        }
    }

    async fn open_file(&self, node: &Node) -> io::Result<CacheFile> {
        let Node::Repo {
            region,
            ver,
            side,
            relpath,
            is_dir: false,
            ..
        } = node
        else {
            return Err(io::Error::from(io::ErrorKind::InvalidInput));
        };
        let (dir, fname) = split_last(relpath);
        for c in self
            .registry
            .resolve(*region, *side, ver, self.filter.backfill)
        {
            let Some(idx) = self.index_for(c.slug, &c.version).await else {
                continue;
            };
            let owns = idx
                .dirs
                .get(dir)
                .is_some_and(|es| es.iter().any(|e| !e.is_dir && e.name == fname));
            if owns {
                return CacheFile::new(
                    self.server.clone(),
                    c.slug,
                    c.version.clone(),
                    relpath.clone(),
                )
                .await;
            }
        }
        Err(io::Error::from(io::ErrorKind::NotFound))
    }
}

/// Directory-entry name for a version selector (`latest` or the version string).
fn sel_name(sel: &VersionSel) -> String {
    match sel {
        VersionSel::Latest => "latest".to_string(),
        VersionSel::Concrete(v) => v.to_string(),
    }
}

/// Whether a version is a historic patch (its display string is `H`-prefixed).
fn is_historic(v: &GameVersion) -> bool {
    v.to_string().starts_with('H')
}

pub struct XivFs {
    inner: Arc<Inner>,
}

impl XivFs {
    pub fn new(
        server: Server,
        registry: Registry,
        filter: Filter,
        handle: Handle,
        uid: u32,
        gid: u32,
    ) -> Self {
        let inodes = DashMap::new();
        inodes.insert(ROOT_INO, Arc::new(Node::Root));
        let parents = DashMap::new();
        parents.insert(ROOT_INO, ROOT_INO);
        // A CLUT parse is short-lived but big enough that glibc keeps its arenas
        // resident afterwards, settling a browse across versions at several hundred
        // MB that isn't live. Trimming inside the parse costs several times the
        // parse itself, so do it once a burst goes quiet.
        #[cfg(target_os = "linux")]
        handle.spawn(async {
            loop {
                TRIM_WANTED.notified().await;
                tokio::time::sleep(TRIM_DELAY).await;
                unsafe { libc::malloc_trim(0) };
            }
        });
        Self {
            inner: Arc::new(Inner {
                server,
                handle,
                registry,
                filter,
                inodes,
                by_name: DashMap::new(),
                parents,
                next_ino: AtomicU64::new(2),
                // Derived indexes are small (names + sizes); keep plenty so we
                // rarely re-parse a CLUT.
                index_cache: moka::future::Cache::new(4096),
                build_sem: tokio::sync::Semaphore::new(MAX_CLUT_BUILDS),
                open_files: DashMap::new(),
                next_fh: AtomicU64::new(1),
                uid,
                gid,
            }),
        }
    }
}

fn errno(e: &io::Error) -> Errno {
    match e.kind() {
        io::ErrorKind::NotFound => Errno::ENOENT,
        io::ErrorKind::InvalidInput => Errno::EINVAL,
        _ => Errno::from_i32(e.raw_os_error().unwrap_or(libc::EIO)),
    }
}

impl Filesystem for XivFs {
    fn init(&mut self, _req: &Request, config: &mut KernelConfig) -> io::Result<()> {
        let _ = config.add_capabilities(InitFlags::FUSE_PARALLEL_DIROPS);
        let _ = config.set_max_background(64);
        if let Err(max) = config.set_max_readahead(8 * 1024 * 1024) {
            let _ = config.set_max_readahead(max);
        }
        Ok(())
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &std::ffi::OsStr, reply: ReplyEntry) {
        let name = name.to_str().map(str::to_string);
        let inner = self.inner.clone();
        inner.handle.clone().spawn(async move {
            let Some(name) = name else {
                return reply.error(Errno::ENOENT);
            };
            let Some(pnode) = inner.inodes.get(&parent.0).map(|n| n.clone()) else {
                return reply.error(Errno::ENOENT);
            };
            match inner.resolve_child(&pnode, &name).await {
                Some(child) => {
                    let ino = inner.alloc(parent.0, &name, child.clone());
                    reply.entry(&TTL, &inner.attr(ino, &child), Generation(0));
                }
                None => reply.error(Errno::ENOENT),
            }
        });
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        match self.inner.inodes.get(&ino.0) {
            Some(node) => reply.attr(&TTL, &self.inner.attr(ino.0, &node)),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let inner = self.inner.clone();
        inner.handle.clone().spawn(async move {
            let Some(node) = inner.inodes.get(&ino.0).map(|n| n.clone()) else {
                return reply.error(Errno::ENOENT);
            };
            if !node.is_dir() {
                return reply.error(Errno::ENOTDIR);
            }
            let parent = inner.parents.get(&ino.0).map(|p| *p).unwrap_or(ROOT_INO);

            let mut entries: Vec<(u64, FileType, String)> = vec![
                (ino.0, FileType::Directory, ".".to_string()),
                (parent, FileType::Directory, "..".to_string()),
            ];
            for (name, child) in inner.children(&node).await {
                let cino = inner.alloc(ino.0, &name, child.clone());
                entries.push((cino, child.file_type(), name));
            }

            for (i, (eino, kind, name)) in entries.into_iter().enumerate().skip(offset as usize) {
                if reply.add(INodeNo(eino), (i + 1) as u64, kind, name) {
                    break;
                }
            }
            reply.ok();
        });
    }

    fn open(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        let inner = self.inner.clone();
        inner.handle.clone().spawn(async move {
            let Some(node) = inner.inodes.get(&ino.0).map(|n| n.clone()) else {
                return reply.error(Errno::ENOENT);
            };
            if node.is_dir() {
                return reply.error(Errno::EISDIR);
            }
            match inner.open_file(&node).await {
                Ok(file) => {
                    let fh = inner.next_fh.fetch_add(1, Ordering::Relaxed);
                    inner.open_files.insert(
                        fh,
                        Arc::new(OpenFile {
                            file,
                            window: AsyncMutex::new(Window {
                                start: 0,
                                data: Vec::new(),
                            }),
                        }),
                    );
                    reply.opened(FileHandle(fh), FopenFlags::FOPEN_KEEP_CACHE);
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => reply.error(errno(&e)),
                Err(e) => {
                    log::error!("open ino {}: {e:#}", ino.0);
                    reply.error(errno(&e))
                }
            }
        });
    }

    fn read(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        let inner = self.inner.clone();
        inner.handle.clone().spawn(async move {
            let Some(of) = inner.open_files.get(&fh.0).map(|f| f.clone()) else {
                return reply.error(Errno::EBADF);
            };
            let len = of.file.len();
            if offset >= len {
                return reply.data(&[]);
            }
            let end = (offset + size as u64).min(len);

            let mut w = of.window.lock().await;
            if (offset < w.start || end > w.end())
                && let Err(e) = of.seek(&mut w, offset).await
            {
                log::error!("read {} at {offset}: {e:#}", of.file.name());
                return reply.error(errno(&e));
            }
            let s = (offset - w.start) as usize;
            let e = (end - w.start) as usize;
            reply.data(&w.data[s..e]);
        });
    }

    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        self.inner.open_files.remove(&fh.0);
        reply.ok();
    }

    // We have no extended attributes. Answer ENOSYS (quietly, without the default's
    // per-call warning) so the kernel stops probing them on every access.
    fn getxattr(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _name: &std::ffi::OsStr,
        _size: u32,
        reply: ReplyXattr,
    ) {
        reply.error(Errno::ENOSYS);
    }

    fn listxattr(&self, _req: &Request, _ino: INodeNo, _size: u32, reply: ReplyXattr) {
        reply.error(Errno::ENOSYS);
    }

    fn flush(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _lock_owner: LockOwner,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }
}
