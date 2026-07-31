use std::collections::HashMap;

use fuser::FileType;
use xiv_core::file::clut::ClutIndex as ClutMeta;

use crate::registry::{Region, Side, VersionSel};

/// A node in the synthetic filesystem tree. Region/Version levels are static;
/// `Repo` nodes are backed by one or more merged CLUTs.
#[derive(Clone)]
pub enum Node {
    Root,
    Region(Region),
    Version(Region, VersionSel),
    Repo {
        region: Region,
        ver: VersionSel,
        side: Side,
        /// Path relative to the side's root (`""` = the `boot`/`game` node).
        relpath: String,
        is_dir: bool,
        size: u64,
    },
}

impl Node {
    pub fn is_dir(&self) -> bool {
        !matches!(self, Node::Repo { is_dir: false, .. })
    }

    pub fn file_type(&self) -> FileType {
        if self.is_dir() {
            FileType::Directory
        } else {
            FileType::RegularFile
        }
    }
}

/// One entry inside a directory of a [`ClutIndex`].
pub struct Entry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
}

/// A directory listing derived from a single CLUT: maps a directory path
/// (`""` = root, no trailing slash) to its immediate children.
pub struct ClutIndex {
    pub dirs: HashMap<String, Vec<Entry>>,
}

fn split_last_key(path: &str) -> (&str, &str) {
    match path.rfind('/') {
        Some(i) => (&path[..i], &path[i + 1..]),
        None => ("", path),
    }
}

fn ensure_dir(path: &str, dirs: &mut HashMap<String, Vec<Entry>>) {
    if path.is_empty() || dirs.contains_key(path) {
        return;
    }
    let (parent, name) = split_last_key(path);
    ensure_dir(parent, dirs);
    dirs.insert(path.to_string(), Vec::new());
    dirs.entry(parent.to_string()).or_default().push(Entry {
        name: name.to_string(),
        is_dir: true,
        size: 0,
    });
}

/// Build a directory trie from a CLUT's lightweight index (folder set + file
/// sizes).
pub fn build_index(meta: &ClutMeta) -> ClutIndex {
    let mut dirs: HashMap<String, Vec<Entry>> = HashMap::new();
    dirs.insert(String::new(), Vec::new());

    for folder in &meta.folders {
        ensure_dir(folder, &mut dirs);
    }
    for (key, size) in &meta.files {
        let (parent, name) = split_last_key(key);
        ensure_dir(parent, &mut dirs);
        dirs.entry(parent.to_string()).or_default().push(Entry {
            name: name.to_string(),
            is_dir: false,
            size: *size,
        });
    }
    ClutIndex { dirs }
}

/// Append a child name to a relative path.
pub fn join_rel(rel: &str, name: &str) -> String {
    if rel.is_empty() {
        name.to_string()
    } else {
        format!("{rel}/{name}")
    }
}

/// Split a relative path into (parent dir, file name).
pub fn split_last(rel: &str) -> (&str, &str) {
    split_last_key(rel)
}
