use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
pub struct FileHeader {
    /// "snapshot" or "diff"
    pub file_type: String,
    pub version: u32,
    pub created_at: i64,
    /// For diffs: hash of the source snapshot
    pub source_snapshot_hash: Option<[u8; 32]>,
    /// For snapshots: informational root directory
    pub root_dir: Option<String>,
}
