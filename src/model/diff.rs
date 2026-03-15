use crate::model::entry::{Entry, EntryKind, Metadata};
use crate::model::path::RelativePath;
use std::path::PathBuf;
use serde::{Deserialize, Serialize};

/// Diff representing all changes between source and target
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Diff {
    pub version: u32,
    pub created_at: i64,
    /// Hash of input snapshot (for integrity check)
    pub source_snapshot_hash: [u8; 32],
    pub changes: Vec<Change>,
}

impl Diff {
    pub const CURRENT_VERSION: u32 = 1;
}

/// Single change in the diff
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Change {
    pub path: RelativePath,
    pub kind: ChangeKind,
}

/// Kind of change in a single entry
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ChangeKind {
    Added(AddedEntry),
    Modified(ModifiedEntry),
    Removed(EntryKind),
}

/// Added entry with its metadata and optional content
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AddedEntry {
    pub entry: Entry,
    /// Whether file content is included in the diff
    pub has_content: bool,
}

/// Modified entry with only the changed fields
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModifiedEntry {
    pub new_metadata: Option<Metadata>,
    pub new_hash: Option<[u8; 32]>,
    pub has_content: bool,
    pub new_symlink_target: Option<PathBuf>,
}
