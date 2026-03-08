use super::path::RelativePath;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Serialize, Deserialize)]
pub enum EntryKind {
    File,
    Directory,
    Symlink,
}

#[derive(Serialize, Deserialize)]
pub struct Metadata {
    pub size: u64,
    pub mtime_sec: i64, // negative values are actually used for timestamps that date before 1970 :)
    pub permissions: u32,
    pub uid: u32,
    pub gid: u32,
}

impl Metadata {
    pub fn size_and_mtime_match(&self, other: &Metadata) -> bool {
        self.size == other.size && self.mtime_sec == other.mtime_sec
    }
}

#[derive(Serialize, Deserialize)]
pub struct Entry {
    pub path: RelativePath,
    pub kind: EntryKind,
    pub metadata: Metadata,
    pub hash: Option<String>,
    pub symlink_target: Option<PathBuf>,
}
