use std::collections::HashMap;

use anyhow::{Result, bail};
use xiv_cache::server::Server;
use xiv_core::file::{slug::Slug, version::GameVersion};

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Region {
    Global,
    Korea,
    China,
}

impl Region {
    pub const ALL: [Region; 3] = [Region::Global, Region::Korea, Region::China];

    pub fn dir_name(self) -> &'static str {
        match self {
            Region::Global => "global",
            Region::Korea => "korea",
            Region::China => "china",
        }
    }

    pub fn from_dir(s: &str) -> Option<Self> {
        Region::ALL.into_iter().find(|r| r.dir_name() == s)
    }

    fn from_publisher(publisher: &str) -> Option<Self> {
        match publisher {
            "ffxivneo" => Some(Region::Global),
            "actoz" => Some(Region::Korea),
            "shanda" => Some(Region::China),
            _ => None,
        }
    }
}

/// A single upstream repository within a region.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Repo {
    Boot,
    Game,
    Ex(u8),
}

impl Repo {
    fn from_node(node: &str) -> Option<Self> {
        match node {
            "boot" => Some(Repo::Boot),
            "game" => Some(Repo::Game),
            _ => node
                .strip_prefix("ex")
                .and_then(|n| n.parse::<u8>().ok())
                .map(Repo::Ex),
        }
    }
}

/// The two branches directly under a version: `boot/` and `game/`. Both key their
/// CLUT entries relative to their own install directory (`ffxivboot.exe`,
/// `sqpack/ffxiv/...`), so a node's relative path is already the CLUT key; the
/// `boot`/`game` names come from the tree, not from the keys.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Side {
    Boot,
    Game,
}

impl Side {
    fn repos(self) -> &'static [Repo] {
        match self {
            Side::Boot => &[Repo::Boot],
            Side::Game => &[
                Repo::Game,
                Repo::Ex(1),
                Repo::Ex(2),
                Repo::Ex(3),
                Repo::Ex(4),
                Repo::Ex(5),
            ],
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum VersionSel {
    Latest,
    Concrete(GameVersion),
}

/// A resolved (slug, version) that contributes files to a merged tree node.
pub struct Contribution {
    pub slug: Slug,
    pub version: GameVersion,
}

struct RepoInfo {
    slug: Slug,
    latest: GameVersion,
    versions: Vec<GameVersion>,
}

pub struct Registry {
    repos: HashMap<(Region, Repo), RepoInfo>,
}

impl Registry {
    pub async fn load(server: &Server) -> Result<Self> {
        // Ensure the slug list is populated; the background updater may not have
        // run its first tick yet.
        server.update_slugs().await?;

        let mut repos = HashMap::new();
        for slug in server.get_slug_list().await? {
            let Ok(data) = server.get_slug(slug).await else {
                continue;
            };
            let parts: Vec<&str> = data.repository.split('/').collect();
            let (Some(publisher), Some(node)) = (parts.first(), parts.last()) else {
                continue;
            };
            let (Some(region), Some(repo)) =
                (Region::from_publisher(publisher), Repo::from_node(node))
            else {
                continue;
            };
            repos.insert(
                (region, repo),
                RepoInfo {
                    slug,
                    latest: data.latest_version,
                    versions: data.versions,
                },
            );
        }

        if repos.is_empty() {
            bail!("no known repositories found in slug list");
        }
        Ok(Self { repos })
    }

    pub fn len(&self) -> usize {
        self.repos.len()
    }

    pub fn region_exists(&self, region: Region) -> bool {
        self.repos.contains_key(&(region, Repo::Game))
    }

    /// Versions listed under `<region>/` (the base game repo's chain).
    pub fn game_versions(&self, region: Region) -> Vec<GameVersion> {
        self.repos
            .get(&(region, Repo::Game))
            .map(|r| r.versions.clone())
            .unwrap_or_default()
    }

    /// Whether any repo in the region published exactly this version.
    pub fn version_valid(&self, region: Region, version: &GameVersion) -> bool {
        self.repos
            .iter()
            .any(|((r, _), info)| *r == region && info.versions.contains(version))
    }

    /// The (slug, version) pairs backing a `side` at a version selector. A slug
    /// is dropped if it lacks the requested concrete version.
    pub fn resolve(
        &self,
        region: Region,
        side: Side,
        sel: &VersionSel,
        backfill: bool,
    ) -> Vec<Contribution> {
        let mut out = Vec::new();
        for &repo in side.repos() {
            let Some(info) = self.repos.get(&(region, repo)) else {
                continue;
            };
            let version = match sel {
                VersionSel::Latest => info.latest.clone(),
                VersionSel::Concrete(v) if info.versions.contains(v) => v.clone(),
                VersionSel::Concrete(v) if backfill => {
                    match info.versions.iter().filter(|x| *x <= v).max() {
                        Some(x) => x.clone(),
                        None => continue,
                    }
                }
                VersionSel::Concrete(_) => continue,
            };
            out.push(Contribution {
                slug: info.slug,
                version,
            });
        }
        out
    }
}
