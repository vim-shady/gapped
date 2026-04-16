use crate::model::diff::Diff;
use crate::model::snapshot::Snapshot;
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Magic bytes identifying a gapped file
pub const MAGIC: &[u8; 9] = b"GAPPED\x00\x03\x00";

/// Magic bytes for identifying a zstd-compressed gapped file
pub const MAGIC_COMPRESSED: &[u8; 9] = b"GAPPEDZ03";

/// End of record marker
pub const EOR: [u8; 9] = [0u8; 9];

/// Record types in binary format
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum RecordType {
    /// File content record
    SnapshotEntry = 1,
    /// Diff change metadata
    DiffChange = 2,
    /// Raw file content following a DiffChange or SnapshotEntry
    FileContent = 3,
}

impl RecordType {
    pub fn from_u8(value: u8) -> Option<Self> {
        match value {
            1 => Some(RecordType::SnapshotEntry),
            2 => Some(RecordType::DiffChange),
            3 => Some(RecordType::FileContent),
            _ => None,
        }
    }
}

/// File header stored after magic bytes
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FileHeader {
    /// "snapshot" or "diff"
    pub file_type: String,
    /// Version of the format
    pub version: u32,
    pub created_at: i64,
    /// For diff files: hash of source snapshot
    pub source_snapshot_hash: Option<[u8; 16]>,
    /// For snapshots: informational root directory
    pub root_dir: Option<String>,
    /// For split diffs: chunk number (1-based, matches filename suffix)
    pub chunk_index: Option<u32>,
}

fn now_unix_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

impl FileHeader {
    pub fn snapshot(root_dir: &Path) -> Self {
        Self {
            file_type: "snapshot".to_string(),
            version: Snapshot::CURRENT_VERSION,
            created_at: now_unix_secs(),
            source_snapshot_hash: None,
            root_dir: Some(root_dir.to_string_lossy().into_owned()),
            chunk_index: None,
        }
    }

    pub fn diff(source_snapshot_hash: [u8; 16], chunk_index: Option<u32>) -> Self {
        Self {
            file_type: "diff".to_string(),
            version: Diff::CURRENT_VERSION,
            created_at: now_unix_secs(),
            source_snapshot_hash: Some(source_snapshot_hash),
            root_dir: None,
            chunk_index,
        }
    }
}
