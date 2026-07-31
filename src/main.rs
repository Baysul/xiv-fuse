mod fs;
mod registry;
mod tree;

use std::path::PathBuf;

use anyhow::{Context, bail};
use clap::Parser;
use fuser::{Config, MountOption, SessionACL};
use xiv_cache::builder::ServerBuilder;
use xiv_core::file::version::GameVersion;

use shadow_rs::shadow;

use crate::fs::Filter;
use crate::registry::{Region, Registry, VersionSel};

shadow!(build);

const MIN_CLUT_CACHE_MIB: u64 = 32;

/// Mount every FFXIV game file, for every region and patch version, as a
/// read-only directory tree.
#[derive(Parser)]
#[command(name = "xiv-fuse", version = build::CLAP_LONG_VERSION, about)]
struct Args {
    /// Directory to mount the filesystem at.
    mountpoint: PathBuf,

    /// On-disk cache for downloaded patch data (default: $HOME/.cache/xiv-fuse).
    #[arg(long, value_name = "DIR")]
    cache_dir: Option<PathBuf>,

    /// Size of the on-disk cache, in GiB. A ceiling rather than a reservation -
    /// the slab files are sparse - but a working set larger than it evicts and
    /// re-downloads, so size it to what you intend to read.
    #[arg(long, default_value_t = 10, value_name = "GIB")]
    cache_size: u64,

    /// How much RAM to spend on parsed CLUTs, in MiB. Each holds its compressed
    /// payload plus an index and decodes data references on demand, so an indexed
    /// CLUT costs about 7 MB for the base game and kilobytes for a small repository.
    #[arg(long, default_value_t = 512, value_name = "MIB")]
    clut_cache: u64,

    /// Base URL to fetch `.clut` files from, as `<base>/<repo>/<version>.clut`.
    /// Point this at your own copy to serve locally converted CLUTs.
    #[arg(long, value_name = "URL")]
    clut_path: Option<String>,

    /// Let other users access the mount (needs `user_allow_other` in fuse.conf).
    #[arg(long)]
    allow_other: bool,

    /// Only expose one region: global, korea, or china.
    #[arg(long)]
    region: Option<String>,

    /// Only expose one patch: `latest` or a version string like 2026.06.18.0000.0000.
    #[arg(long)]
    patch: Option<String>,

    /// Also list `H`-prefixed historic patches (hidden by default).
    /// (They stay reachable by explicit path regardless.)
    #[arg(long)]
    history: bool,

    /// Under a specific patch, backfill each expansion/boot repo to its newest
    /// version at or before it (a coherent point-in-time install), instead of
    /// showing only the repos that shipped on exactly that date.
    #[arg(long)]
    backfill: bool,
}

fn parse_filter(args: &Args) -> anyhow::Result<Filter> {
    let region = match &args.region {
        Some(r) => Some(Region::from_dir(r).ok_or_else(|| {
            anyhow::anyhow!("invalid --region {r:?}; expected global, korea, or china")
        })?),
        None => None,
    };
    let version = match args.patch.as_deref() {
        Some("latest") => Some(VersionSel::Latest),
        Some(v) => Some(VersionSel::Concrete(
            GameVersion::new(v).with_context(|| format!("invalid --patch {v:?}"))?,
        )),
        None => None,
    };
    Ok(Filter {
        region,
        version,
        history: args.history,
        backfill: args.backfill,
    })
}

fn default_cache_dir() -> PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(std::env::temp_dir);
    base.join("xiv-fuse")
}

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("info,xiv_fuse=info"),
    )
    .init();

    // glibc spreads allocations over one arena per core and only ever returns
    // memory from an arena's top, so a CLUT parse storm leaves hundreds of MB of
    // freed-but-resident pages. Four arenas is the knee: RSS as low as a tighter
    // cap gives, without the malloc contention that costs the read path ~30%.
    #[cfg(target_os = "linux")]
    unsafe {
        libc::mallopt(libc::M_ARENA_MAX, 4);
    }

    let args = Args::parse();
    let filter = parse_filter(&args)?;
    if args.clut_cache < MIN_CLUT_CACHE_MIB {
        log::warn!(
            "--clut-cache {} MiB cannot hold the CLUTs backing a single version",
            args.clut_cache
        );
    }
    let cache_dir = args.cache_dir.clone().unwrap_or_else(default_cache_dir);
    std::fs::create_dir_all(&cache_dir)
        .with_context(|| format!("creating cache dir {}", cache_dir.display()))?;

    let rt = tokio::runtime::Runtime::new()?;

    log::info!("building cache server (cache dir: {})", cache_dir.display());
    let mut builder = ServerBuilder::default()
        .storage_directory(cache_dir)
        .max_concurrent_downloads(32)
        .storage_capacity_bytes(usize::try_from(args.cache_size << 30)?)
        .clut_ram_bytes(args.clut_cache << 20);
    if let Some(path) = &args.clut_path {
        builder = builder.clut_path(path.clone());
    }
    let server = rt.block_on(builder.build())?;
    let server_handle = server.clone();

    log::info!("loading repository list from Thaliak");
    let registry = rt.block_on(Registry::load(&server))?;
    log::info!("loaded {} repositories", registry.len());

    let uid = unsafe { libc::getuid() };
    let gid = unsafe { libc::getgid() };
    if let Some(r) = filter.region
        && !registry.region_exists(r)
    {
        bail!("region {} has no repositories", r.dir_name());
    }
    let filesystem = fs::XivFs::new(server, registry, filter, rt.handle().clone(), uid, gid);

    let mut config = Config::default();
    config.mount_options = vec![
        MountOption::RO,
        MountOption::FSName("xiv-fuse".into()),
        MountOption::Subtype("xivfuse".into()),
        MountOption::DefaultPermissions,
    ];
    if args.allow_other {
        config.acl = SessionACL::All;
    }

    log::info!("mounting at {}", args.mountpoint.display());
    fuser::mount(filesystem, &args.mountpoint, &config)?;

    log::info!("flushing cache");
    rt.block_on(server_handle.close())?;
    Ok(())
}
